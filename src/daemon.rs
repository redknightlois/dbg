use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::backend::Backend;
use crate::profile::ProfileData;
use crate::pty::DebuggerProcess;

const CMD_TIMEOUT: Duration = Duration::from_secs(60);

fn cleanup_and_exit() -> ! {
    let _ = std::fs::remove_file(socket_path());
    let _ = std::fs::remove_file(pid_path());
    let session_dir = session_tmp("").parent().map(|p| p.to_path_buf());
    if let Some(dir) = session_dir {
        let _ = std::fs::remove_dir_all(&dir);
    }
    std::process::exit(0);
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

    let session = Mutex::new(Session {
        proc,
        events: VecDeque::new(),
        profile,
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

fn handle_command(cmd: &str, backend: &dyn Backend, session: &Mutex<Session>, cached_help: &str) -> String {
    // Serve help from cache — no lock needed, works even when the debugger is busy.
    if cmd == "help" {
        return cached_help.to_string();
    }

    if cmd == "quit" {
        if let Ok(guard) = session.try_lock() {
            guard.proc.quit(backend.quit_command());
        }
        // If the lock is held (debugger busy), process::exit in the event
        // loop will handle cleanup via DebuggerProcess::Drop.
        return "stopped".to_string();
    }

    if cmd == "events" {
        let mut guard = session.lock().unwrap();
        if guard.events.is_empty() {
            return "none".to_string();
        }
        let events: Vec<String> = guard.events.drain(..).collect();
        return events.join("\n");
    }

    // Profile mode: handle commands from in-memory profile data
    {
        let mut guard = session.lock().unwrap();
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

    let mut guard = session.lock().unwrap();
    match guard.proc.send_and_wait(cmd, CMD_TIMEOUT) {
        Ok(raw) => {
            let result = backend.clean(cmd, &raw);
            for event in result.events {
                guard.events.push_back(event);
            }
            result.output
        }
        Err(e) => format!("[error: {e}]"),
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
