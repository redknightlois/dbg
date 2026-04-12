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

    let cleanup = || {
        let _ = std::fs::remove_file(&socket_path());
        let _ = std::fs::remove_file(&pid_path());
        let session_dir = session_tmp("").parent().map(|p| p.to_path_buf());
        if let Some(dir) = session_dir {
            let _ = std::fs::remove_dir_all(&dir);
        }
    };

    ctrlc::set_handler(move || {
        let _ = std::fs::remove_file(&socket_path());
        let _ = std::fs::remove_file(&pid_path());
        std::process::exit(0);
    })
    .ok();

    let profile = backend
        .profile_output()
        .and_then(|path| ProfileData::load(Path::new(&path)).ok());

    let session = Mutex::new(Session {
        proc,
        events: VecDeque::new(),
        profile,
    });

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };

        let mut data = vec![0u8; 4096];
        let n = stream.read(&mut data).unwrap_or(0);
        let cmd = String::from_utf8_lossy(&data[..n]).trim().to_string();

        if cmd.is_empty() {
            continue;
        }

        let response = handle_command(&cmd, backend, &session);

        if cmd == "quit" {
            let _ = stream.write_all(response.as_bytes());
            cleanup();
            std::process::exit(0);
        }

        let _ = stream.write_all(response.as_bytes());
    }

    cleanup();
    Ok(())
}

fn handle_command(cmd: &str, backend: &dyn Backend, session: &Mutex<Session>) -> String {
    if cmd == "quit" {
        let guard = session.lock().unwrap();
        guard.proc.quit(backend.quit_command());
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

    if cmd == "help" {
        let guard = session.lock().unwrap();
        let raw = guard
            .proc
            .send_and_wait(backend.help_command(), CMD_TIMEOUT)
            .unwrap_or_default();
        return backend.parse_help(&raw);
    }

    if let Some(topic) = cmd.strip_prefix("help ") {
        let help_cmd = backend.help_command();
        let guard = session.lock().unwrap();
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
