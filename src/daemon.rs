use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use dbg_cli::session_db::{self, CreateOptions, SessionDb, SessionKind, TargetClass};
use rusqlite::params;

use crate::backend::Backend;
use crate::commands::{self, Dispatched, crosstrack, debug as debug_cmds, lifecycle as lifecycle_cmds};
use crate::profile::ProfileData;
use crate::pty::DebuggerProcess;
use dbg_cli::session_db::LiveDebugger;

const CMD_TIMEOUT: Duration = Duration::from_secs(60);

fn cleanup_and_exit() -> ! {
    let _ = std::fs::remove_file(socket_path());
    let _ = std::fs::remove_file(pid_path());
    let session_dir = session_tmp_dir();
    let _ = std::fs::remove_dir_all(&session_dir);
    std::process::exit(0);
}

/// Return the session-scoped temp directory (without appending a filename).
fn session_tmp_dir() -> PathBuf {
    // session_tmp("x") gives <dir>/x — take the parent to get <dir>
    let with_file = session_tmp("x");
    with_file.parent().unwrap_or(&with_file).to_path_buf()
}

/// Pick the best directory for IPC files.
/// Prefers $XDG_RUNTIME_DIR (per-user, tmpfs), falls back to /tmp.
fn runtime_dir() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

fn socket_path() -> PathBuf {
    runtime_dir().join("dbg.sock")
}

fn pid_path() -> PathBuf {
    runtime_dir().join("dbg.pid")
}

/// Session-scoped temp directory for profile data etc.
/// Uses a random ID to avoid collisions between concurrent sessions.
pub fn session_tmp(filename: &str) -> PathBuf {
    use std::sync::OnceLock;
    static SESSION_ID: OnceLock<String> = OnceLock::new();
    let id = SESSION_ID.get_or_init(|| {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use std::time::SystemTime;
        let mut h = DefaultHasher::new();
        SystemTime::now().hash(&mut h);
        std::process::id().hash(&mut h);
        format!("{:08x}", h.finish() as u32)
    });
    let dir = runtime_dir().join(format!("dbg-{id}"));
    let _ = std::fs::create_dir_all(&dir);
    dir.join(filename)
}

struct Session {
    proc: DebuggerProcess,
    events: VecDeque<String>,
    profile: Option<ProfileData>,
    /// Per-run SessionDb. `None` if DB creation failed on startup — we
    /// keep the debugger session alive either way; capture is best-effort.
    db: Option<SessionDb>,
    /// Monotonic hit counter per canonical `location_key`. Drives
    /// `breakpoint_hits.hit_seq` so agents can reference "the 5th hit
    /// at main.c:42".
    hit_seq: HashMap<String, u32>,
    /// Where to back the DB up on graceful shutdown (if the session
    /// accumulated any captured data and is still marked auto).
    save_to: Option<PathBuf>,
    /// Target path — used by crosstrack collectors (dbg disasm, etc.).
    target: String,
    /// Working directory at session start — used to resolve
    /// `.dbg/sessions/<label>.db` paths and on-demand collectors.
    cwd: PathBuf,
    /// Classification chosen at startup from the backend name; picks
    /// which collector (lldb / jitdasm / go-objdump / …) runs.
    target_class: TargetClass,
}

/// LiveDebugger impl that hands `crosstrack::run` a narrow handle onto
/// the session PTY. Only used while the daemon holds the session lock
/// — so this is called synchronously from `handle_command`.
struct ProcLive<'a> {
    proc: &'a DebuggerProcess,
    tool_name: &'static str,
}

impl LiveDebugger for ProcLive<'_> {
    fn send(&self, cmd: &str) -> anyhow::Result<String> {
        self.proc.send_and_wait(cmd, CMD_TIMEOUT)
    }
    fn tool_name(&self) -> &'static str {
        self.tool_name
    }
}

/// Infer which `TargetClass` to stamp on the session row from the
/// backend name. Profilers and backends that don't fit the debug
/// taxonomy fall through to `NativeCpu` — the class is informational
/// for those, not driving any per-class domain tables.
fn backend_target_class(name: &str) -> TargetClass {
    match name {
        "pdb" => TargetClass::Python,
        "netcoredbg" | "dotnet-trace" | "jitdasm" => TargetClass::ManagedDotnet,
        "jdb" => TargetClass::Jvm,
        "node-inspect" | "nodeprof" | "node-proto" => TargetClass::JsNode,
        "rdbg" | "stackprof" => TargetClass::Ruby,
        "phpdbg" | "xdebug-profile" => TargetClass::Php,
        // lldb, delve, perf, callgrind, massif, memcheck,
        // ghci, ocamldebug, etc. — all native-style by default.
        _ => TargetClass::NativeCpu,
    }
}

/// Is `cmd` a control flow command whose output may contain a stop
/// banner worth feeding through `parse_hit`? We don't gate hit capture
/// on this (safe: `parse_hit` only returns `Some` on an actual banner),
/// but it lets us avoid synthesizing expensive locals roundtrips after
/// clearly non-stopping commands like `breakpoint list` or `version`.
fn command_may_stop(cmd: &str) -> bool {
    let first = cmd.trim().split_whitespace().next().unwrap_or("");
    matches!(
        first,
        // Canonical verbs that may cause a stop...
        "run" | "continue" | "step" | "next" | "finish"
            | "c" | "s" | "n" | "r"
            // ... and the most common native forms across backends.
            | "process" | "thread" | "stepout" | "return" | "restart"
    )
}

/// Start the daemon: spawn the debugger, listen on socket.
/// This function does NOT return on success — it runs the event loop.
pub fn run_daemon(backend: &dyn Backend, target: &str, args: &[String]) -> Result<()> {
    let config = backend.spawn_config(target, args)?;

    let proc = DebuggerProcess::spawn(
        &config.bin,
        &config.args,
        &config.env,
        backend.prompt_pattern(),
    )
    .context("failed to spawn debugger")?;

    // Wait for initial prompt
    proc.wait_for_prompt(Duration::from_secs(120))
        .context("debugger did not produce prompt")?;

    // Run init commands
    for cmd in &config.init_commands {
        proc.send_and_wait(cmd, CMD_TIMEOUT)?;
    }

    // Write PID file
    std::fs::write(&pid_path(), std::process::id().to_string())?;

    // Clean up stale socket
    let _ = std::fs::remove_file(&socket_path());

    // Bind socket
    let listener = UnixListener::bind(&socket_path()).context("failed to bind socket")?;

    ctrlc::set_handler(move || {
        cleanup_and_exit();
    })
    .ok();

    // Cache help output now while the debugger is idle and responsive.
    // Stored outside the session mutex so it can be served even when
    // the debugger is busy running a command.
    let cached_help = proc
        .send_and_wait(backend.help_command(), CMD_TIMEOUT)
        .map(|raw| backend.parse_help(&raw))
        .unwrap_or_default();

    let profile = backend
        .profile_output()
        .and_then(|path| ProfileData::load(Path::new(&path)).ok());

    // Build a per-run SessionDb alongside the debugger session. Create
    // failure is non-fatal — we proceed without persistence rather than
    // refusing to start the debugger.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let target_class = backend_target_class(backend.name());
    let tmp_db = session_tmp("session.db");
    let _ = std::fs::remove_file(&tmp_db); // fresh DB on every start
    let (db, save_to) = match SessionDb::create(CreateOptions {
        kind: SessionKind::Debug,
        target,
        target_class,
        cwd: &cwd,
        db_path: Some(&tmp_db),
        label: None,
        target_hash: None,
    }) {
        Ok(db) => {
            let final_path =
                session_db::sessions_dir(&cwd).join(format!("{}.db", db.label()));
            (Some(db), Some(final_path))
        }
        Err(e) => {
            eprintln!("[dbg] warning: session DB unavailable ({e}); proceeding without capture");
            (None, None)
        }
    };

    let session = Mutex::new(Session {
        proc,
        events: VecDeque::new(),
        profile,
        db,
        hit_seq: HashMap::new(),
        save_to,
        target: target.to_string(),
        cwd: cwd.clone(),
        target_class,
    });

    // Non-blocking accept so threads can handle connections concurrently.
    listener.set_nonblocking(true)?;

    std::thread::scope(|scope| {
        loop {
            let stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(_) => continue,
            };

            let session = &session;
            let cached_help = &cached_help;

            scope.spawn(move || {
                let mut stream = stream;
                // Accepted sockets inherit non-blocking from the listener; reset to blocking
                let _ = stream.set_nonblocking(false);
                let mut data = String::new();
                let _ = stream.read_to_string(&mut data);
                let cmd = data.trim().to_string();

                if cmd.is_empty() {
                    return;
                }

                if cmd == "quit" {
                    let response = handle_command(&cmd, backend, session, cached_help);
                    let _ = stream.write_all(response.as_bytes());
                    // Exit immediately — scoped threads would otherwise wait
                    // for any blocked command (e.g. `continue`) to finish.
                    cleanup_and_exit();
                }

                let response = handle_command(&cmd, backend, session, cached_help);
                let _ = stream.write_all(response.as_bytes());
            });
        }
    });

    cleanup_and_exit();
}

/// Lock the session mutex, recovering from poisoning so the daemon
/// stays responsive after a thread panic.
fn lock_session(session: &Mutex<Session>) -> std::sync::MutexGuard<'_, Session> {
    session.lock().unwrap_or_else(|e| e.into_inner())
}

fn handle_command(cmd: &str, backend: &dyn Backend, session: &Mutex<Session>, cached_help: &str) -> String {
    // Serve help from cache — no lock needed, works even when the debugger is busy.
    if cmd == "help" {
        return cached_help.to_string();
    }

    if cmd == "quit" {
        if let Ok(mut guard) = session.try_lock() {
            // Best-effort persist the SessionDb to its final location so
            // the agent can reopen with `dbg sessions` / `dbg diff`.
            persist_session_on_exit(&mut guard);
            guard.proc.quit(backend.quit_command());
        }
        // If the lock is held (debugger busy), process::exit in the event
        // loop will handle cleanup via DebuggerProcess::Drop. We lose
        // the capture in that case — by design; the agent interrupted.
        return "stopped".to_string();
    }

    if cmd == "events" {
        let mut guard = lock_session(session);
        if guard.events.is_empty() {
            return "none".to_string();
        }
        let events: Vec<String> = guard.events.drain(..).collect();
        return events.join("\n");
    }

    // Profile mode: handle commands from in-memory profile data
    {
        let mut guard = lock_session(session);
        if let Some(ref mut profile) = guard.profile {
            return profile.handle_command(cmd);
        }
    }

    if let Some(topic) = cmd.strip_prefix("help ") {
        let help_cmd = backend.help_command();
        let guard = match session.try_lock() {
            Ok(g) => g,
            Err(_) => return "[busy] debugger is running a command — try again".to_string(),
        };
        return guard
            .proc
            .send_and_wait(&format!("{help_cmd} {topic}"), CMD_TIMEOUT)
            .unwrap_or_else(|e| format!("[error: {e}]"));
    }

    // Route through the unified dispatcher: cross-track verbs first
    // (hits/hit-diff/cross/disasm/source/at-hit), canonical debug
    // verbs next (break/step/continue/…), then Fallthrough to the
    // legacy passthrough path for anything else.
    match commands::dispatch(cmd, backend) {
        Dispatched::Immediate(resp) => {
            let mut guard = lock_session(session);
            log_command(&mut guard, cmd, &resp, Some("meta"));
            resp
        }
        Dispatched::Native {
            canonical_op,
            native_cmd,
            decorate,
        } => {
            let mut guard = lock_session(session);
            match guard.proc.send_and_wait(&native_cmd, CMD_TIMEOUT) {
                Ok(raw) => {
                    let result = backend.clean(&native_cmd, &raw);
                    for event in result.events {
                        guard.events.push_back(event);
                    }
                    let mut cleaned = result.output;
                    if decorate {
                        cleaned = debug_cmds::decorate_output(backend, &cleaned);
                    }
                    log_command(&mut guard, cmd, &cleaned, Some(canonical_op));
                    if command_may_stop(cmd) {
                        capture_hit_if_stopped(&mut guard, backend, &cleaned);
                    }
                    cleaned
                }
                Err(e) => format!("[error: {e}]"),
            }
        }
        Dispatched::Query(q) => {
            let canonical_op = q.canonical_op();
            let mut guard = lock_session(session);
            let response = run_crosstrack(&mut guard, backend, &q);
            log_command(&mut guard, cmd, &response, Some(canonical_op));
            response
        }
        Dispatched::Lifecycle(l) => {
            let canonical_op = l.canonical_op();
            let mut guard = lock_session(session);
            let ctx = lifecycle_cmds::LifeCtx {
                cwd: &guard.cwd,
                active: guard.db.as_ref(),
            };
            let response = lifecycle_cmds::run(&l, &ctx);
            log_command(&mut guard, cmd, &response, Some(canonical_op));
            response
        }
        Dispatched::Fallthrough => {
            let mut guard = lock_session(session);
            match guard.proc.send_and_wait(cmd, CMD_TIMEOUT) {
                Ok(raw) => {
                    let result = backend.clean(cmd, &raw);
                    for event in result.events {
                        guard.events.push_back(event);
                    }
                    let cleaned = result.output;
                    log_command(&mut guard, cmd, &cleaned, None);
                    if command_may_stop(cmd) {
                        capture_hit_if_stopped(&mut guard, backend, &cleaned);
                    }
                    cleaned
                }
                Err(e) => format!("[error: {e}]"),
            }
        }
    }
}

/// Record a `commands` row for the just-issued command. `canonical_op`
/// is `Some(...)` when the command went through the canonical
/// dispatcher (e.g. `"break"`, `"step"`, `"raw"`, `"meta"`) and `None`
/// for legacy passthrough.
fn log_command(session: &mut Session, input: &str, output: &str, canonical_op: Option<&str>) {
    let db = match session.db.as_ref() {
        Some(d) => d,
        None => return,
    };
    let head: String = output.chars().take(4096).collect();
    let output_bytes = output.len() as i64;
    let _ = db.conn().execute(
        "INSERT INTO commands (session_id, input, output_head, output_bytes, ts, canonical_op)
         VALUES (
             (SELECT id FROM sessions LIMIT 1),
             ?1, ?2, ?3, datetime('now'), ?4
         )",
        params![input, head, output_bytes, canonical_op],
    );
}

/// Parse `cleaned` through the active backend's `CanonicalOps::parse_hit`
/// and, on a match, synthesize `op_locals` + `op_stack` roundtrips so
/// we can attach a rich hit record to the SessionDb without asking the
/// agent to run extra commands.
fn capture_hit_if_stopped(session: &mut Session, backend: &dyn Backend, cleaned: &str) {
    let ops = match backend.canonical_ops() {
        Some(o) => o,
        None => return,
    };
    let hit = match ops.parse_hit(cleaned) {
        Some(h) => h,
        None => return,
    };
    let db = match session.db.as_ref() {
        Some(d) => d,
        None => return,
    };

    // Advance the per-location hit counter. First hit is #1.
    let seq = session.hit_seq.entry(hit.location_key.clone()).or_insert(0);
    *seq += 1;
    let hit_seq = *seq as i64;

    // Synthesize the locals roundtrip; tolerate all failures since the
    // debugger may be in a transient state after a stop.
    let locals_json = ops
        .op_locals()
        .ok()
        .and_then(|op| session.proc.send_and_wait(&op, CMD_TIMEOUT).ok())
        .and_then(|raw| ops.parse_locals(&raw))
        .map(|v| v.to_string());

    // And the stack roundtrip. We store the raw cleaned text; a full
    // structured parse is out of scope for Phase 1.
    let stack_text = ops
        .op_stack(Some(20))
        .ok()
        .and_then(|op| session.proc.send_and_wait(&op, CMD_TIMEOUT).ok());
    let stack_json = stack_text.map(|s| {
        serde_json::json!({ "raw": s, "frame_symbol": hit.frame_symbol }).to_string()
    });

    let _ = db.conn().execute(
        "INSERT INTO breakpoint_hits
            (session_id, location_key, hit_seq, thread, ts, locals_json, stack_json)
         VALUES (
             (SELECT id FROM sessions LIMIT 1),
             ?1, ?2, ?3, datetime('now'), ?4, ?5
         )",
        params![hit.location_key, hit_seq, hit.thread, locals_json, stack_json],
    );
}

/// Execute a cross-track query against the session's DB, threading
/// a LiveDebugger handle so collectors can reuse the existing PTY
/// when they want to. Returns a formatted report ready for the agent.
fn run_crosstrack(session: &mut Session, backend: &dyn Backend, q: &crosstrack::Query) -> String {
    let db = match session.db.as_ref() {
        Some(d) => d,
        None => return "[session DB unavailable — crosstrack queries need capture]".into(),
    };
    let tool_name = backend
        .canonical_ops()
        .map(|c| c.tool_name())
        .unwrap_or("unknown");
    let live = ProcLive {
        proc: &session.proc,
        tool_name,
    };
    let ctx = crosstrack::RunCtx {
        target: &session.target,
        target_class: session.target_class,
        cwd: &session.cwd,
        live: Some(&live),
    };
    crosstrack::run(q, db, &ctx)
}

/// Back up the session DB to `.dbg/sessions/<label>.db` iff the
/// session is still marked auto and has captured any data. If the
/// session is empty we leave nothing behind — the tmpdir is wiped by
/// `cleanup_and_exit`.
fn persist_session_on_exit(session: &mut Session) {
    let db = match session.db.as_ref() {
        Some(d) => d,
        None => return,
    };
    let has_data = db.has_captured_data().unwrap_or(false);
    if !has_data {
        return;
    }
    if let Some(path) = session.save_to.as_ref() {
        if let Err(e) = db.save_to(path) {
            eprintln!("[dbg] warning: failed to save session DB to {}: {e}", path.display());
        }
    }
}

/// Send a command to a running daemon. Returns the response.
pub fn send_command(cmd: &str) -> Result<String> {
    use std::os::unix::net::UnixStream;

    let mut stream =
        UnixStream::connect(&socket_path()).context("no session running — use: dbg start")?;
    stream.set_read_timeout(Some(CMD_TIMEOUT))?;

    stream.write_all(format!("{cmd}\n").as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

/// Check if a daemon is running.
pub fn is_running() -> bool {
    if let Ok(pid_str) = std::fs::read_to_string(&pid_path()) {
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            return nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
        }
    }
    false
}

/// Kill the running daemon.
pub fn kill_daemon() -> Result<String> {
    if is_running() {
        send_command("quit")
    } else {
        let _ = std::fs::remove_file(&socket_path());
        let _ = std::fs::remove_file(&pid_path());
        Ok("stopped".into())
    }
}

/// Wait for the socket file to appear.
pub fn wait_for_socket(timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if Path::new(&socket_path()).exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn backend_target_class_mapping() {
        assert_eq!(backend_target_class("lldb"), TargetClass::NativeCpu);
        assert_eq!(backend_target_class("delve"), TargetClass::NativeCpu);
        assert_eq!(backend_target_class("pdb"), TargetClass::Python);
        assert_eq!(backend_target_class("netcoredbg"), TargetClass::ManagedDotnet);
        assert_eq!(backend_target_class("jdb"), TargetClass::Jvm);
        assert_eq!(backend_target_class("node-inspect"), TargetClass::JsNode);
        assert_eq!(backend_target_class("node-proto"), TargetClass::JsNode);
        assert_eq!(backend_target_class("rdbg"), TargetClass::Ruby);
        assert_eq!(backend_target_class("phpdbg"), TargetClass::Php);
        // Unknown backends fall through to native-cpu — informational only.
        assert_eq!(backend_target_class("perf"), TargetClass::NativeCpu);
    }

    #[test]
    fn command_may_stop_covers_canonical_and_native_verbs() {
        // Canonical verbs.
        assert!(command_may_stop("continue"));
        assert!(command_may_stop("step"));
        assert!(command_may_stop("next"));
        assert!(command_may_stop("finish"));
        assert!(command_may_stop("run"));
        assert!(command_may_stop("restart"));
        // Native / aliased forms the daemon is likely to see on raw.
        assert!(command_may_stop("process continue"));
        assert!(command_may_stop("thread step-in"));
        assert!(command_may_stop("stepout"));
        assert!(command_may_stop("return"));
        assert!(command_may_stop("c"));
        assert!(command_may_stop("s"));
        assert!(command_may_stop("n"));
        // Definitely non-stopping commands.
        assert!(!command_may_stop("print x"));
        assert!(!command_may_stop("breakpoint list"));
        assert!(!command_may_stop("frame variable"));
        assert!(!command_may_stop("help"));
    }

    /// End-to-end of the SessionDb capture path without spawning a real
    /// debugger: we drive the same SQL the daemon uses and assert the
    /// rows land + `has_captured_data` flips.
    #[test]
    fn capture_path_writes_rows() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("s.db");
        let db = SessionDb::create(CreateOptions {
            kind: SessionKind::Debug,
            target: "./app",
            target_class: TargetClass::NativeCpu,
            cwd: tmp.path(),
            db_path: Some(&db_path),
            label: Some("t".into()),
            target_hash: Some("h".into()),
        })
        .unwrap();

        // Simulate `log_command`.
        db.conn()
            .execute(
                "INSERT INTO commands (session_id, input, output_head, output_bytes, ts, canonical_op)
                 VALUES ((SELECT id FROM sessions LIMIT 1), ?1, ?2, ?3, datetime('now'), NULL)",
                params!["continue", "stopped at main.c:42", 20_i64],
            )
            .unwrap();

        // Simulate `capture_hit_if_stopped`.
        db.conn()
            .execute(
                "INSERT INTO breakpoint_hits
                    (session_id, location_key, hit_seq, thread, ts, locals_json, stack_json)
                 VALUES ((SELECT id FROM sessions LIMIT 1), ?1, ?2, ?3, datetime('now'), ?4, ?5)",
                params!["main.c:42", 1_i64, "1", r#"{"x":{"value":"42"}}"#, r#"{"raw":"..."}"#],
            )
            .unwrap();

        assert!(db.has_captured_data().unwrap());
        let (cmds, hits): (i64, i64) = db
            .conn()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM commands),
                        (SELECT COUNT(*) FROM breakpoint_hits)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cmds, 1);
        assert_eq!(hits, 1);
    }
}
