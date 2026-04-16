use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
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

/// Does this canonical operation cause an execution-state transition?
/// Only these ops can produce a *new* breakpoint hit. Inspection ops
/// (`stack`, `locals`, `print`, `breaks`) echo the current stop state
/// without transitioning and must NOT feed into `capture_hit_if_stopped`
/// or they create duplicate hit rows.
fn op_may_stop(canonical_op: &str) -> bool {
    matches!(
        canonical_op,
        "run" | "continue" | "step" | "next" | "finish"
    )
}

/// Same idea for raw/fallthrough commands that bypass the canonical
/// dispatcher — match on the native command verb.
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

    // Expose the child PID outside the session mutex so the quit handler
    // can SIGINT the child to interrupt a blocked `send_and_wait` (e.g.
    // jdb waiting on a `continue` that hasn't hit a breakpoint yet).
    let child_pid = AtomicI32::new(proc.child_pid().as_raw());

    // Pre-clone the event-log handle outside the session mutex. `dbg
    // events --wait` blocks on the log's condvar without touching the
    // session lock, so live-tailing works concurrently even while a
    // `continue` holds the mutex for minutes.
    let log_handle = proc.log();

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
            let child_pid = &child_pid;
            let log_handle = &log_handle;

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
                    let response = handle_quit(backend, session, child_pid);
                    let _ = stream.write_all(response.as_bytes());
                    // Exit immediately — scoped threads would otherwise wait
                    // for any blocked command (e.g. `continue`) to finish.
                    cleanup_and_exit();
                }

                let response =
                    handle_command(&cmd, backend, session, cached_help, log_handle);
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

/// Graceful quit: interrupt the child so any blocked `send_and_wait`
/// returns, acquire the session lock, persist the DB, then quit the
/// debugger. This fixes the jdb timing issue where the 2s lock timeout
/// wasn't enough for a slow `continue` to finish.
fn handle_quit(backend: &dyn Backend, session: &Mutex<Session>, child_pid: &AtomicI32) -> String {
    // SIGINT the child to interrupt any blocked PTY read (e.g. jdb
    // waiting for a breakpoint during `continue`). This causes the
    // `send_and_wait` in the command thread to return promptly,
    // releasing the session lock so we can acquire it.
    let pid = nix::unistd::Pid::from_raw(child_pid.load(Ordering::Relaxed));
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT);

    // Give the interrupted command thread time to release the lock.
    // 5s is generous — after the SIGINT the prompt usually appears
    // within 100-200ms, so the lock drops well before the deadline.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(mut guard) = session.try_lock() {
            persist_session_on_exit(&mut guard);
            guard.proc.quit(backend.quit_command());
            break;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    "stopped".to_string()
}

fn handle_command(
    cmd: &str,
    backend: &dyn Backend,
    session: &Mutex<Session>,
    cached_help: &str,
    log_handle: &crate::pty::LogHandle,
) -> String {
    // Serve help from cache — no lock needed, works even when the debugger is busy.
    if cmd == "help" {
        return cached_help.to_string();
    }

    if cmd == "events" || cmd.starts_with("events ") {
        // Uses the pre-cloned log handle — does NOT touch the session
        // mutex, so live-tailing is concurrent with a blocked command.
        return handle_events(cmd, log_handle);
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
            // Drain deferred stop banners even for immediate commands.
            // node-inspect's `locals` dispatches as Immediate (unsupported),
            // but a prior `continue` may have left a banner in the PTY.
            drain_pending_events(&mut guard, backend);
            log_command(&mut guard, cmd, &resp, Some("meta"));
            resp
        }
        Dispatched::Native {
            canonical_op,
            native_cmd,
            decorate,
        } => {
            let mut guard = lock_session(session);
            // Drain any deferred stop banner from a prior async
            // execution command before sending the next one.
            drain_pending_events(&mut guard, backend);

            match guard.proc.send_and_wait(&native_cmd, CMD_TIMEOUT) {
                Ok(raw) => {
                    if op_may_stop(canonical_op) {
                        capture_hit_if_stopped(&mut guard, backend, &raw);
                    }
                    // When the agent explicitly runs `dbg locals`,
                    // retroactively fill in the most recent hit's
                    // locals_json if it was NULL. This enables sparklines
                    // for backends where auto_capture_locals=false: the
                    // agent's natural workflow (continue → locals →
                    // continue → locals) populates the data.
                    if canonical_op == "locals" {
                        backfill_locals(&mut guard, backend, &raw);
                    }

                    let result = backend.clean(&native_cmd, &raw);
                    let mut cleaned = result.output;
                    if decorate {
                        cleaned = debug_cmds::decorate_output(backend, &cleaned);
                    }
                    log_command(&mut guard, cmd, &cleaned, Some(canonical_op));
                    cleaned
                }
                Err(e) => format!("[error: {e}]"),
            }
        }
        Dispatched::Query(q) => {
            let canonical_op = q.canonical_op();
            let mut guard = lock_session(session);
            // Drain first: a deferred stop banner from a prior async
            // execution may still be pending in the channel. Without
            // draining here, the query would report stale hit counts.
            drain_pending_events(&mut guard, backend);
            let response = run_crosstrack(&mut guard, backend, &q);
            log_command(&mut guard, cmd, &response, Some(canonical_op));
            response
        }
        Dispatched::Lifecycle(l) => {
            let canonical_op = l.canonical_op();
            let mut guard = lock_session(session);
            drain_pending_events(&mut guard, backend);
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
            drain_pending_events(&mut guard, backend);

            match guard.proc.send_and_wait(cmd, CMD_TIMEOUT) {
                Ok(raw) => {
                    if command_may_stop(cmd) {
                        capture_hit_if_stopped(&mut guard, backend, &raw);
                    }
                    let result = backend.clean(cmd, &raw);
                    let cleaned = result.output;
                    log_command(&mut guard, cmd, &cleaned, None);
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

/// Format the reader's event log for `dbg events`. Supports:
///   * `events`                 — full retained log
///   * `events --since=<N>`     — only entries with seq > N
///   * `events --tail=<N>`      — only last N entries
///   * `events --wait=<ms>`     — block up to this many ms for new
///                                 entries past `--since` before
///                                 returning. Capped at 60s.
///
/// Agents use this to tail a session's PTY timeline live. The log is
/// populated by the reader thread, so it updates even while a command
/// is blocked — for `--wait` we clone the log handle, drop the session
/// mutex, then block on the log's condvar. That keeps `dbg events`
/// concurrent with a long-running `continue`.
fn handle_events(cmd: &str, log_handle: &crate::pty::LogHandle) -> String {
    let mut since: u64 = 0;
    let mut tail: Option<usize> = None;
    let mut wait_ms: Option<u64> = None;
    for tok in cmd.split_whitespace().skip(1) {
        if let Some(v) = tok.strip_prefix("--since=") {
            since = v.parse().unwrap_or(0);
        } else if let Some(v) = tok.strip_prefix("--tail=") {
            tail = v.parse().ok();
        } else if let Some(v) = tok.strip_prefix("--wait=") {
            wait_ms = v.parse().ok();
        }
    }

    let mut entries = log_handle.since(since);
    if entries.is_empty() {
        if let Some(ms) = wait_ms {
            // Cap below CMD_TIMEOUT (60s) so the client's socket read
            // timeout always fires after the server responds, not during.
            let capped = ms.min(50_000);
            entries = log_handle.since_wait(since, Duration::from_millis(capped));
        }
    }
    let last_seq = log_handle.last_seq();

    if let Some(n) = tail {
        if entries.len() > n {
            entries.drain(..entries.len() - n);
        }
    }

    if entries.is_empty() {
        return format!("no events (last seq={last_seq})");
    }

    let mut out = String::new();
    for e in &entries {
        let kind = e.kind.as_str();
        let ts = format_ms(e.ts_ms);
        match e.kind {
            crate::pty::EventKind::Output => {
                let preview = output_preview(&e.bytes);
                let nbytes = e.bytes.len();
                out.push_str(&format!(
                    "#{seq:<6} {ts:>9}  {kind:<7} {nbytes:>5}B  {preview}\n",
                    seq = e.seq,
                    ts = ts,
                    kind = kind,
                    nbytes = nbytes,
                    preview = preview,
                ));
            }
            crate::pty::EventKind::Prompt | crate::pty::EventKind::Exit => {
                out.push_str(&format!(
                    "#{seq:<6} {ts:>9}  {kind}\n",
                    seq = e.seq,
                    ts = ts,
                    kind = kind,
                ));
            }
        }
    }
    out.push_str(&format!(
        "({n} entries, last seq={last_seq})\n",
        n = entries.len(),
        last_seq = last_seq,
    ));
    out
}

fn format_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("+{ms}ms")
    } else {
        format!("+{:.2}s", ms as f64 / 1000.0)
    }
}

/// One-line preview of an Output event. Strips ANSI, collapses
/// whitespace, truncates to 80 chars. Used by `dbg events`.
fn output_preview(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let cleaned = s.replace(['\r', '\n'], "⏎").replace('\t', " ");
    let single: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.chars().count() > 80 {
        let truncated: String = single.chars().take(77).collect();
        format!("{truncated}…")
    } else {
        single
    }
}

/// Drain any events that arrived on the reader channel since the last
/// command. For async debuggers (node-inspect) the stop banner for a
/// prior `continue` can arrive *after* the ack prompt, while the daemon
/// is idle. The reader thread captures those bytes regardless; this
/// helper pulls them from the channel and feeds them through
/// `capture_hit_if_stopped` before the next command runs. Non-blocking.
fn drain_pending_events(session: &mut Session, backend: &dyn Backend) {
    if let Some(pending) = session.proc.drain_pending() {
        if !pending.trim().is_empty() {
            capture_hit_if_stopped(session, backend, &pending);
        }
    }
}

/// Parse `output` through the active backend's `CanonicalOps::parse_hit`
/// and, on a match, synthesize `op_locals` + `op_stack` roundtrips so
/// we can attach a rich hit record to the SessionDb without asking the
/// agent to run extra commands.
///
/// With PTY echo disabled, each `send_and_wait` returns exactly the
/// debugger's response for that command.  Stop banners always appear
/// in the execution command's response — no deferred/merged banners.
fn capture_hit_if_stopped(session: &mut Session, backend: &dyn Backend, output: &str) {
    let ops = match backend.canonical_ops() {
        Some(o) => o,
        None => return,
    };
    let hit = match ops.parse_hit(output) {
        Some(h) => h,
        None => return,
    };
    let db = match session.db.as_ref() {
        Some(d) => d,
        None => return,
    };

    let seq = session.hit_seq.entry(hit.location_key.clone()).or_insert(0);
    *seq += 1;
    let hit_seq = *seq as i64;

    // Auto-capture locals + stack when the backend says it's safe.
    // Fragile backends (jdb, ghci, netcoredbg CLI) set
    // `auto_capture_locals = false` because the synthesized roundtrip
    // can destroy their PTY state. Agents use `dbg locals` explicitly.
    let (locals_json, stack_json) = if ops.auto_capture_locals() {
        const CAPTURE_TIMEOUT: Duration = Duration::from_secs(1);
        let lj = ops
            .op_locals()
            .ok()
            .and_then(|op| match session.proc.send_and_wait(&op, CAPTURE_TIMEOUT) {
                Ok(raw) => ops.parse_locals(&raw).map(|v| v.to_string()),
                Err(_) => None,
            });
        let sj = ops.op_stack(Some(20)).ok().and_then(|op| {
            match session.proc.send_and_wait(&op, CAPTURE_TIMEOUT) {
                Ok(raw) => Some(
                    serde_json::json!({
                        "raw": raw,
                        "frame_symbol": hit.frame_symbol,
                    })
                    .to_string(),
                ),
                Err(_) => None,
            }
        });
        (lj, sj)
    } else {
        let sj = hit.frame_symbol.as_ref().map(|s| {
            serde_json::json!({ "frame_symbol": s }).to_string()
        });
        (None, sj)
    };

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

/// Discard any pending PTY output so the next `send_and_wait` starts
/// from a clean buffer. Called after a timed-out synthesized roundtrip
/// to prevent stale data from poisoning the next agent command.
/// When the agent explicitly runs `dbg locals`, retroactively fill in
/// the most recent breakpoint_hits row's `locals_json` if it was NULL.
/// This enables `dbg hit-trend <loc> <field>` sparklines for backends
/// where `auto_capture_locals=false`: the agent's natural workflow
/// (`continue` → `locals` → repeat) populates the data progressively.
fn backfill_locals(session: &mut Session, backend: &dyn Backend, raw_output: &str) {
    let ops = match backend.canonical_ops() {
        Some(o) => o,
        None => return,
    };
    let locals_json = match ops.parse_locals(raw_output) {
        Some(v) => v.to_string(),
        None => return,
    };
    let db = match session.db.as_ref() {
        Some(d) => d,
        None => return,
    };
    // Update the most recent hit that has NULL locals_json.
    let _ = db.conn().execute(
        "UPDATE breakpoint_hits
         SET locals_json = ?1
         WHERE id = (
             SELECT id FROM breakpoint_hits
             WHERE locals_json IS NULL
             ORDER BY id DESC LIMIT 1
         )",
        params![locals_json],
    );
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
