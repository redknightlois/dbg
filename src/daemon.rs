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
use crate::pty::{DebuggerIo, DebuggerProcess};
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

fn users_uid() -> u32 {
    // Read from /proc/self/status; the value only needs to be stable
    // per-user within the host, not authoritative.
    std::env::var("UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("Uid:"))
                        .and_then(|l| l.split_ascii_whitespace().nth(1)?.parse().ok())
                })
                .unwrap_or(0)
        })
}

/// Per-user IPC directory. Prefers `$XDG_RUNTIME_DIR/dbg-<uid>` (tmpfs,
/// already private), falls back to `/tmp/dbg-<uid>` with `0700` perms.
/// Containing the socket+pid in a `0700` dir keeps other local users
/// from racing the socket creation or hijacking the pid file — Unix
/// socket permissions aren't portable enough to rely on the socket
/// inode itself.
fn runtime_dir() -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    // Unix user — used to scope the IPC dir per-user so collisions
    // and permission races are impossible on shared /tmp.
    let uid = users_uid();
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    let dir = base.join(format!("dbg-{uid}"));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        // Fall back — we can't abort here (callers treat this as
        // infallible). The bind() will fail with a clearer error.
        eprintln!("dbg: cannot create {}: {e}", dir.display());
    } else {
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir
}

/// Stable per-working-directory slug used to scope the daemon socket
/// and pid file. Two agents running `dbg start` in different cwds get
/// different sockets and don't stomp each other. `DBG_SESSION` env var
/// overrides the cwd hash — useful for tests and for explicit
/// multi-session workflows within a single cwd.
fn session_slug() -> String {
    if let Ok(v) = std::env::var("DBG_SESSION") {
        let safe: String = v
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        if !safe.is_empty() {
            return safe;
        }
    }
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let mut h = DefaultHasher::new();
    cwd.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn socket_path() -> PathBuf {
    runtime_dir().join(format!("dbg-{}.sock", session_slug()))
}

fn pid_path() -> PathBuf {
    runtime_dir().join(format!("dbg-{}.pid", session_slug()))
}

/// File the forked daemon's stderr is redirected to during startup.
/// The parent reads it back when the daemon dies before binding the
/// socket, so the agent sees the real failure reason instead of
/// "daemon failed to start".
pub fn startup_log_path() -> PathBuf {
    runtime_dir().join(format!("dbg-{}.startup.log", session_slug()))
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
    proc: Box<dyn DebuggerIo>,
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
    proc: &'a dyn DebuggerIo,
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
        "run" | "continue" | "step" | "next" | "finish" | "restart" | "pause"
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
pub fn run_daemon(
    backend: &dyn Backend,
    target: &str,
    args: &[String],
    attach: Option<&crate::backend::AttachSpec>,
) -> Result<()> {
    // Branch on transport kind. Most backends use the default PTY
    // transport; `node-proto` uses the V8 Inspector WebSocket
    // transport; DAP-capable backends (delve-proto, debugpy-proto, …)
    // use the Debug Adapter Protocol over TCP. All paths produce a
    // `Box<dyn DebuggerIo>`. Protocol backends have no post-spawn
    // init commands — their handshake happens inside the transport.
    let (proc, init_commands): (Box<dyn DebuggerIo>, Vec<String>) = if backend.uses_inspector() {
        let t = crate::inspector::InspectorTransport::spawn(target, args)
            .context("failed to spawn inspector transport")?;
        (Box::new(t), Vec::new())
    } else if backend.uses_dap() {
        let cfg = match attach {
            Some(spec) => backend.dap_attach(spec)?,
            None => backend.dap_launch(target, args)?,
        };
        let t = crate::dap::DapTransport::spawn(target, cfg)
            .context("failed to spawn DAP transport")?;
        (Box::new(t), Vec::new())
    } else {
        let config = backend.spawn_config(target, args)?;
        let p = DebuggerProcess::spawn(
            &config.bin,
            &config.args,
            &config.env,
            backend.prompt_pattern(),
        )
        .context("failed to spawn debugger")?;
        (Box::new(p), config.init_commands)
    };

    // Publish the jitdasm capture path so on-demand collectors (which
    // live in the lib crate and can't reach `daemon::session_tmp`) can
    // serve `dbg disasm` from the pre-captured file rather than
    // re-spawning dotnet on every call.
    if backend.name() == "jitdasm" {
        // SAFETY: set_var is only unsafe in multi-threaded contexts due
        // to libc env races. We're still single-threaded here — this
        // runs before the socket listener spawns request handlers.
        unsafe {
            std::env::set_var(
                "DBG_JITDASM_CAPTURE",
                session_tmp("jitdasm").join("capture.asm"),
            );
        }
    }

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

    // Publish pid + socket BEFORE running init commands. Backends
    // like jitdasm have multi-minute init flows (dotnet build +
    // disasm capture + exec-into-REPL); keeping pid/socket gated on
    // init completion made `dbg status` lie ("no session") the whole
    // time, and `dbg start` appeared to hang silently. With pid
    // written and socket bound up-front, connections queue into the
    // listener backlog and are served as soon as the accept loop
    // below picks them up — which happens right after init finishes.
    std::fs::write(&pid_path(), std::process::id().to_string())?;
    let _ = std::fs::remove_file(&socket_path());
    let listener = UnixListener::bind(&socket_path()).context("failed to bind socket")?;

    ctrlc::set_handler(move || {
        cleanup_and_exit();
    })
    .ok();

    // Run init commands (PTY backends only; inspector has none).
    for cmd in &init_commands {
        proc.send_and_wait(cmd, CMD_TIMEOUT)?;
    }

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
                    let _ = stream.flush();
                    // Close the write half so the client's read_to_string
                    // returns EOF cleanly before we drop the socket —
                    // without this the client sees SIGPIPE/EPIPE and
                    // exits 144.
                    let _ = stream.shutdown(std::net::Shutdown::Write);
                    // Give the kernel a brief moment to deliver the last
                    // bytes to the client's socket buffer.
                    std::thread::sleep(Duration::from_millis(20));
                    // Exit immediately — scoped threads would otherwise wait
                    // for any blocked command (e.g. `continue`) to finish.
                    cleanup_and_exit();
                }

                if cmd == "cancel" {
                    let response = handle_cancel(child_pid);
                    let _ = stream.write_all(response.as_bytes());
                    return;
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

/// Interrupt the running command by SIGINT'ing the child without
/// tearing down the session. Unlike `quit`, this keeps the debugger
/// alive: the blocked `send_and_wait` returns (prompt reappears after
/// the interrupt), the session lock drops, and subsequent commands
/// proceed normally. Used to break out of a `continue` that hasn't
/// hit a breakpoint, an infinite loop in the target, etc.
fn handle_cancel(child_pid: &AtomicI32) -> String {
    let pid = nix::unistd::Pid::from_raw(child_pid.load(Ordering::Relaxed));
    match nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT) {
        Ok(()) => "interrupted".to_string(),
        Err(e) => format!("[error: {e}]"),
    }
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
        // dbg's own verbs (start, kill, hits, break, …) — served from
        // a static registry so `dbg help start` actually returns text
        // even when the backend doesn't know about it.
        let topic = topic.trim();
        if let Some(text) = dbg_verb_help(topic) {
            return text.to_string();
        }
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
            structured,
        } => {
            let mut guard = lock_session(session);
            // Drain any deferred stop banner from a prior async
            // execution command before sending the next one.
            drain_pending_events(&mut guard, backend);

            // Post-mortem fallback: once the debuggee has exited, live
            // inspection verbs (stack/locals) can still be answered
            // from the last captured breakpoint hit in the session DB.
            // Execution verbs (step/continue/run/…) have no useful
            // fallback — return a directed exit message instead.
            if !guard.proc.is_alive() {
                // First post-mortem access also flushes the DB to
                // `.dbg/sessions/<label>.db` so `dbg kill` isn't
                // required to make the run discoverable via
                // `dbg sessions` / `dbg replay`.
                persist_session_on_exit(&mut guard);
                let fallback = post_mortem_fallback(&guard, canonical_op);
                log_command(&mut guard, cmd, &fallback, Some(canonical_op));
                return fallback;
            }

            let send_result = match structured.as_ref() {
                Some(req) => match guard.proc.dispatch_structured(req, CMD_TIMEOUT) {
                    Some(r) => r,
                    None => guard.proc.send_and_wait(&native_cmd, CMD_TIMEOUT),
                },
                None => guard.proc.send_and_wait(&native_cmd, CMD_TIMEOUT),
            };
            match send_result {
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

/// Help text for dbg-level verbs. Returns `None` when the verb is
/// unknown — callers fall through to the backend's own help.
pub fn dbg_verb_help(verb: &str) -> Option<&'static str> {
    Some(match verb {
        "start" => "\
dbg start <type> <target> [--break SPEC] [--args ...] [--run]
  Spawn a debugger session. <type> may be omitted when <target>'s
  extension unambiguously identifies a backend (.py, .go, .java, .rb,
  .php, .csproj, .js, .ts, .hs, .ml).
  --break SPEC        set a breakpoint before run (repeatable)
  --args a b c        forward args to the debuggee
  --run               continue past the debugger's startup prompt;
                      breakpoints still fire.
  --attach-pid N      attach to a running process (DAP backends)
  --attach-port H:P   attach via host:port (DAP backends)",
        "kill" | "quit" => "\
dbg kill
  Stop the active session, persisting captured data to .dbg/sessions/
  before exit. Idempotent — safe to run when no session is live.",
        "status" => "\
dbg status
  Show live session info: backend, target, PID, elapsed time. Returns
  \"no session\" when nothing is running.",
        "sessions" => "\
dbg sessions
  List persisted sessions under .dbg/sessions/. The currently-live
  session (if any) is marked with *.",
        "replay" => "\
dbg replay <label>
  Open a persisted session read-only and run crosstrack queries against
  it. Same vocabulary as a live session: hits, hit-trend, hit-diff,
  cross, disasm, source. No live debugger — exec verbs are rejected.",
        "run" => "\
dbg run
  Start the debuggee (or restart it). Stops at the first breakpoint
  that fires.",
        "continue" | "c" => "\
dbg continue
  Resume the debuggee until the next breakpoint, signal, or exit.",
        "step" | "s" => "\
dbg step
  Step into the next line (source-level).",
        "next" | "n" => "\
dbg next
  Step over the next line, not descending into function calls.",
        "finish" => "\
dbg finish
  Run until the current function returns.",
        "break" | "b" => "\
dbg break <spec>
  Set a breakpoint. <spec> is one of:
    file:line               — e.g. main.c:42, broken.py:20
    function                — e.g. MyClass.run, main
    /abs/file:line          — absolute paths always work unambiguously
  For pdb/delve/others, breaking on a `def` / function-header line
  may not fire until the first body line — dbg auto-advances when it
  can detect this; otherwise use file:line with a body line.",
        "locals" => "\
dbg locals
  Print local variables at the current stop. Also backfills
  locals_json on the most recent captured hit, enabling
  `dbg hit-trend`/`--group-by` sparklines progressively.",
        "stack" | "bt" => "\
dbg stack
  Print the call stack at the current stop.",
        "print" | "p" => "\
dbg print <expr>
  Evaluate <expr> in the debuggee's current frame.",
        "hits" => "\
dbg hits <loc> [--group-by FIELD] [--count-by FIELD --top N]
  List captured breakpoint hits at <loc>. With --group-by, aggregate
  by a locals field: `dbg hits broken.py:20 --group-by n` → count per
  distinct n. --count-by is an alias; --top N truncates to the most
  frequent. Dotted paths supported (self.x).",
        "hit-diff" => "\
dbg hit-diff <loc> <seq_a> <seq_b>
  Show a field-by-field diff of captured locals between two hits.",
        "hit-trend" => "\
dbg hit-trend <loc> <field>
  Render a sparkline of a locals field across hits. Accepts dotted
  paths (self.x). When the field isn't captured, enumerates available
  field names in the error so you can pick one.",
        "cross" => "\
dbg cross <symbol>
  Show everything known about <symbol>: hit count, profile samples,
  jit events, disassembly rows, source snapshots.",
        "disasm" => "\
dbg disasm [<symbol>] [--refresh]
  Capture and show disassembly for <symbol>. Omit <symbol> at a stop
  point to disasm the current frame. --refresh forces recollection.",
        "source" => "\
dbg source <symbol> [radius=5]
  Show the source of <symbol> with ±radius lines of context.",
        "events" => "\
dbg events [--since=SEQ] [--tail=N] [--kind=stop,stdout,...] [--wait=MS]
  Live-tail the session's PTY event log. Does not touch the session
  mutex, so it works while a `continue` is blocked.",
        "save" => "\
dbg save [<label> | --label <label>]
  Copy the live session DB to .dbg/sessions/<label>.db so it
  survives `dbg kill` and can be reopened with `dbg replay`.
  Without a label, uses the session's auto-label (see `dbg status`).
  Marks the persisted DB as user-owned so `dbg prune` won't reap it.",
        "cancel" => "\
dbg cancel
  Interrupt the currently-running debugger command without tearing
  down the session (sends SIGINT to the debuggee). Use to break out
  of a `continue` that hasn't hit a breakpoint.",
        _ => return None,
    })
}

/// Format the reader's event log for `dbg events`. Supports:
///   * `events`                     — full retained log
///   * `events --since=<N>`         — only entries with seq > N
///   * `events --tail=<N>`          — only last N entries
///   * `events --kind=stop[,output]` — filter by event kind
///   * `events --wait=<ms>`         — block up to this many ms for
///                                     new entries matching the filter
///                                     (capped at 50s).
///
/// Combined `--kind=stop --wait=N --since=M` means "wake me on the
/// next breakpoint hit past seq M, or timeout after N ms."
///
/// Agents use this to tail a session's PTY timeline live. The log is
/// populated by the reader thread + daemon (Stop events), so it updates
/// even while a command is blocked — `handle_events` never touches the
/// session mutex, so live-tailing is concurrent with a blocked
/// `continue`.
fn handle_events(cmd: &str, log_handle: &crate::pty::LogHandle) -> String {
    let mut since: u64 = 0;
    let mut tail: Option<usize> = None;
    let mut wait_ms: Option<u64> = None;
    let mut kinds: Option<Vec<crate::pty::EventKind>> = None;
    for tok in cmd.split_whitespace().skip(1) {
        if let Some(v) = tok.strip_prefix("--since=") {
            since = v.parse().unwrap_or(0);
        } else if let Some(v) = tok.strip_prefix("--tail=") {
            tail = v.parse().ok();
        } else if let Some(v) = tok.strip_prefix("--wait=") {
            wait_ms = v.parse().ok();
        } else if let Some(v) = tok.strip_prefix("--kind=") {
            let mut parsed = Vec::new();
            for name in v.split(',') {
                match crate::pty::EventKind::parse(name) {
                    Some(k) => parsed.push(k),
                    None => {
                        return format!(
                            "unknown event kind '{name}' in --kind={v} (valid: stdout, output, prompt, exit, stop)"
                        );
                    }
                }
            }
            if !parsed.is_empty() {
                kinds = Some(parsed);
            }
        }
    }

    let matches_filter = |kind: crate::pty::EventKind| -> bool {
        match &kinds {
            None => true,
            Some(ks) => ks.contains(&kind),
        }
    };

    // Snapshot + filter.
    let mut entries: Vec<_> = log_handle
        .since(since)
        .into_iter()
        .filter(|e| matches_filter(e.kind))
        .collect();

    // If no matching entries and --wait is set, loop-wait on the
    // condvar until a matching event arrives or the deadline expires.
    // Plain wait (no filter) wakes on any new event — fine; wake-check
    // is cheap. With a filter, unrelated events can wake the wait, so
    // we re-check and re-wait against the remaining deadline.
    if entries.is_empty() {
        if let Some(ms) = wait_ms {
            let capped = ms.min(50_000);
            let deadline = std::time::Instant::now() + Duration::from_millis(capped);
            let mut cursor = since;
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let woke = log_handle.since_wait(cursor, remaining);
                if woke.is_empty() {
                    break;
                }
                cursor = woke.last().map(|e| e.seq).unwrap_or(cursor);
                entries.extend(woke.into_iter().filter(|e| matches_filter(e.kind)));
                if !entries.is_empty() {
                    break;
                }
            }
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
            crate::pty::EventKind::Output | crate::pty::EventKind::Stdout => {
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
            crate::pty::EventKind::Stop => {
                out.push_str(&format!(
                    "#{seq:<6} {ts:>9}  {kind:<7}   {payload}\n",
                    seq = e.seq,
                    ts = ts,
                    kind = kind,
                    payload = format_stop_payload(&e.bytes),
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

/// Format a Stop event's JSON payload as a readable one-line summary.
/// Emitted by `capture_hit_if_stopped`; fields are location_key,
/// hit_seq, thread, file, line, frame_symbol.
fn format_stop_payload(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let v: serde_json::Value = match serde_json::from_str(&s) {
        Ok(v) => v,
        Err(_) => return format!("(unparseable stop: {s})"),
    };
    let loc = v.get("location_key").and_then(|x| x.as_str()).unwrap_or("?");
    let seq = v.get("hit_seq").and_then(|x| x.as_i64()).unwrap_or(0);
    let frame = v
        .get("frame_symbol")
        .and_then(|x| x.as_str())
        .map(|s| format!(" in {s}"))
        .unwrap_or_default();
    let thread = v
        .get("thread")
        .and_then(|x| x.as_str())
        .map(|s| format!(" thread={s}"))
        .unwrap_or_default();
    format!("{loc} #{seq}{frame}{thread}")
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
/// After the debuggee exits, live inspection verbs have no live
/// debugger to ask. For stack/locals we serve the last captured hit
/// from the session DB so the agent can still look at the program's
/// final observable state. Execution verbs get a clear "you're
/// post-mortem" message pointing at `dbg hits` / `dbg cross`.
fn post_mortem_fallback(session: &Session, canonical_op: &str) -> String {
    let db = match session.db.as_ref() {
        Some(d) => d,
        None => {
            return "(debuggee has exited and no session DB is available — \
                    start a fresh session with `dbg start`)"
                .to_string();
        }
    };
    match canonical_op {
        "locals" => {
            let row: Option<(i64, String, Option<String>)> = db
                .conn()
                .query_row(
                    "SELECT hit_seq, location_key, locals_json
                     FROM breakpoint_hits
                     WHERE locals_json IS NOT NULL
                     ORDER BY id DESC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .ok();
            match row {
                Some((seq, loc, Some(json))) => format!(
                    "[post-mortem] locals at last captured hit {loc} #{seq}\n{json}\n\
                     (debuggee has exited; use `dbg hits <loc>` to browse all hits)"
                ),
                _ => "[post-mortem] debuggee has exited and no locals were \
                      captured — run a new session with `dbg start`"
                    .to_string(),
            }
        }
        "stack" | "bt" | "backtrace" => {
            let row: Option<(i64, String, Option<String>)> = db
                .conn()
                .query_row(
                    "SELECT hit_seq, location_key, stack_json
                     FROM breakpoint_hits
                     WHERE stack_json IS NOT NULL
                     ORDER BY id DESC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .ok();
            match row {
                Some((seq, loc, Some(json))) => format!(
                    "[post-mortem] stack at last captured hit {loc} #{seq}\n{json}"
                ),
                _ => "[post-mortem] debuggee has exited and no stack was \
                      captured — run a new session with `dbg start`"
                    .to_string(),
            }
        }
        "run" | "continue" | "step" | "next" | "finish" | "restart" | "pause" => {
            "debuggee has exited — cannot resume. Use `dbg hits <loc>`, \
             `dbg stack`, `dbg locals`, `dbg cross <sym>` to inspect \
             captured state, or `dbg start` for a fresh run."
                .to_string()
        }
        _ => "debuggee has exited — live inspection unavailable. Use \
              `dbg hits`, `dbg cross`, `dbg replay <label>` against the \
              captured DB, or `dbg start` for a new session."
            .to_string(),
    }
}

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
    // Structured path first: protocol transports (V8 Inspector, DAP)
    // deliver paused events with frame data inline; we skip text
    // banner parsing entirely. Falls back to `parse_hit` for PTY
    // transports that only surface stop info as debugger output.
    let hit = match session.proc.pending_hit() {
        Some(h) => h,
        None => match ops.parse_hit(output) {
            Some(h) => h,
            None => return,
        },
    };

    let seq = session.hit_seq.entry(hit.location_key.clone()).or_insert(0);
    *seq += 1;
    let hit_seq = *seq as i64;

    // Emit a structured Stop event on the timeline so `dbg events
    // --kind=stop` can tail program stops. Lean payload — callers who
    // need locals/stack query the session DB by location_key + hit_seq.
    let stop_payload = serde_json::json!({
        "location_key": hit.location_key,
        "hit_seq": hit_seq,
        "thread": hit.thread,
        "file": hit.file,
        "line": hit.line,
        "frame_symbol": hit.frame_symbol,
    })
    .to_string();
    session
        .proc
        .log()
        .push(crate::pty::EventKind::Stop, stop_payload.into_bytes());

    let db = match session.db.as_ref() {
        Some(d) => d,
        None => return,
    };

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
        proc: &*session.proc,
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
    let Ok(pid_str) = std::fs::read_to_string(&pid_path()) else { return false };
    let Ok(pid) = pid_str.trim().parse::<i32>() else { return false };
    let alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
    if !alive {
        // Orphaned pid file — clean up so the next `dbg start`/`dbg
        // replay` doesn't fight it. Ignore errors (race with another
        // process that may also be clearing it).
        let _ = std::fs::remove_file(pid_path());
        let _ = std::fs::remove_file(socket_path());
        return false;
    }
    // A live pid is necessary but not sufficient — the kernel may
    // have recycled the pid for an unrelated process after our
    // daemon died uncleanly. Require the socket to exist too; the
    // daemon unlinks it on graceful exit.
    if !Path::new(&socket_path()).exists() {
        // Pid is live but belongs to somebody else — drop the
        // stale pid file.
        let _ = std::fs::remove_file(pid_path());
        return false;
    }
    true
}

/// Kill the running daemon. Blocks until the process is gone and
/// socket/pid files are cleared — callers that immediately spawn a
/// new daemon need this to be synchronous.
pub fn kill_daemon() -> Result<String> {
    if !is_running() {
        let _ = std::fs::remove_file(&socket_path());
        let _ = std::fs::remove_file(&pid_path());
        return Ok("stopped".into());
    }
    let response = send_command("quit").unwrap_or_else(|_| "stopped".into());
    // Wait for the pid to actually die. The daemon's quit handler
    // releases the socket before exit but the kernel still needs a
    // moment to reap the process.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline && is_running() {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_file(&socket_path());
    let _ = std::fs::remove_file(&pid_path());
    Ok(response)
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
    fn pid_and_socket_bind_before_init_commands() {
        // Regression: jitdasm (and any other backend with a long-init
        // flow) made `dbg status` return "no session" and `dbg start`
        // appear to hang for minutes, because pid_path + socket bind
        // were gated on init_commands completing. Assert the source
        // order here: pid write + socket bind must precede the
        // `for cmd in &init_commands` loop inside run_daemon.
        let src = include_str!("daemon.rs");
        let pid_write = src.find("std::fs::write(&pid_path()").expect("pid_path write");
        let bind = src.find("UnixListener::bind(&socket_path()").expect("socket bind");
        let init_loop = src.find("for cmd in &init_commands").expect("init loop");
        assert!(
            pid_write < init_loop,
            "pid file write must precede init_commands loop"
        );
        assert!(
            bind < init_loop,
            "socket bind must precede init_commands loop"
        );
    }

    #[test]
    fn is_running_clears_orphaned_pid_file() {
        // Regression: `dbg replay` saw "live session running" after a
        // crashed daemon left a pid file behind. is_running() must
        // reap such orphans so subsequent commands work.
        //
        // Isolate the runtime dir so concurrent tests don't collide.
        let tmp = TempDir::new().unwrap();
        // Safety: tests are single-threaded with respect to env via
        // `cargo test -- --test-threads=1` if needed; these tests
        // don't run concurrently with anything reading these vars.
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", tmp.path());
            std::env::set_var("DBG_SESSION", "testorphan");
        }
        // Write a PID that cannot exist (the kernel never hands out 0).
        std::fs::write(pid_path(), "2147483000").unwrap();
        assert!(!is_running(), "dead PID should read as not running");
        assert!(!pid_path().exists(), "orphan pid file should be cleaned up");
        unsafe { std::env::remove_var("DBG_SESSION"); }
    }

    #[test]
    fn is_running_requires_socket_when_pid_live() {
        let tmp = TempDir::new().unwrap();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", tmp.path());
            std::env::set_var("DBG_SESSION", "testlive");
        }
        let my_pid = std::process::id();
        std::fs::write(pid_path(), my_pid.to_string()).unwrap();
        // No socket — our PID is "alive" but it's us, the test.
        assert!(!is_running(), "live PID without socket shouldn't register as daemon");
        assert!(!pid_path().exists(), "stale pid file should be cleaned up");
        unsafe { std::env::remove_var("DBG_SESSION"); }
    }

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
