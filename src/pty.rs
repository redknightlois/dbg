use std::collections::VecDeque;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::poll::{PollFd, PollFlags, poll};
use nix::pty::{OpenptyResult, openpty};
use nix::sys::signal::Signal;
use nix::unistd::{ForkResult, Pid, close, dup2, execvp, fork, setsid};
use regex::Regex;

static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]|\x1b\[K|\x1b\[2K").unwrap());

/// An event emitted by the reader thread.
///
/// The reader owns the PTY master read side and produces a stream of
/// events that the daemon consumes. This decouples reading from
/// command dispatch so async debuggers (node-inspect, async gdb) don't
/// lose stop banners that arrive between commands.
pub enum PtyEvent {
    /// A chunk of raw output bytes. Multiple `Data` events may precede
    /// a single `Prompt`; the daemon concatenates them.
    Data(Vec<u8>),
    /// The prompt regex matched the accumulated output. The debugger
    /// is ready for input. The reader resets its internal match buffer
    /// after emitting this.
    Prompt,
    /// The reader detected EOF or a fatal read error. Child is gone.
    Exit,
}

/// Kind of entry stored in the event log. The log is a tamer,
/// persistent view of the channel — same information, but retained so
/// `dbg events` can replay what happened.
///
/// `Output`, `Prompt`, `Exit` are pushed by the reader thread. `Stop`
/// is pushed by the daemon after parse_hit succeeds on an execution
/// command's output — the bytes field carries a JSON HitEvent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Output,
    Prompt,
    Exit,
    Stop,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Output => "output",
            EventKind::Prompt => "prompt",
            EventKind::Exit => "exit",
            EventKind::Stop => "stop",
        }
    }

    /// Parse a kind name (lowercase) for filtering. Returns None if
    /// the string doesn't match any known kind.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "output" => Some(EventKind::Output),
            "prompt" => Some(EventKind::Prompt),
            "exit" => Some(EventKind::Exit),
            "stop" => Some(EventKind::Stop),
            _ => None,
        }
    }
}

/// An entry in the event log. `seq` is monotonic and session-unique;
/// agents pass it as `--since` to query incrementally.
#[derive(Clone, Debug)]
pub struct EventEntry {
    pub seq: u64,
    /// Milliseconds since the session started.
    pub ts_ms: u64,
    pub kind: EventKind,
    /// Raw bytes. Empty for Prompt/Exit.
    pub bytes: Vec<u8>,
}

/// Bounded ring buffer of events. Capped at `MAX_EVENTS`; older entries
/// are dropped silently. The `last_seq` counter keeps incrementing even
/// across drops so agents can tell if they missed events.
const MAX_EVENTS: usize = 2048;

struct EventLog {
    entries: VecDeque<EventEntry>,
    last_seq: u64,
    started: Instant,
}

impl EventLog {
    fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(MAX_EVENTS),
            last_seq: 0,
            started: Instant::now(),
        }
    }

    fn push(&mut self, kind: EventKind, bytes: Vec<u8>) {
        self.last_seq += 1;
        let ts_ms = self.started.elapsed().as_millis() as u64;
        if self.entries.len() == MAX_EVENTS {
            self.entries.pop_front();
        }
        self.entries.push_back(EventEntry {
            seq: self.last_seq,
            ts_ms,
            kind,
            bytes,
        });
    }

    /// Return entries with `seq > since`. `since = 0` returns the full log.
    fn since(&self, since: u64) -> Vec<EventEntry> {
        self.entries
            .iter()
            .filter(|e| e.seq > since)
            .cloned()
            .collect()
    }
}

/// Shared event-log handle. Hands out snapshots of the log and supports
/// blocking until new events arrive via an internal `Condvar`. Cloning
/// the handle is an Arc bump; all clones see the same log.
///
/// The handle is a separate type from `DebuggerProcess` so daemon
/// handlers can clone it, drop the session mutex, and wait on the
/// condvar without blocking other commands.
#[derive(Clone)]
pub struct LogHandle(Arc<(Mutex<EventLog>, Condvar)>);

impl LogHandle {
    fn new() -> Self {
        Self(Arc::new((Mutex::new(EventLog::new()), Condvar::new())))
    }

    /// Append an event and notify all waiters. Used both by the reader
    /// thread (Output / Prompt / Exit) and by the daemon (Stop, emitted
    /// after parse_hit succeeds on an execution command's output).
    pub fn push(&self, kind: EventKind, bytes: Vec<u8>) {
        let (lock, cvar) = &*self.0;
        lock.lock().unwrap().push(kind, bytes);
        cvar.notify_all();
    }

    /// Non-blocking snapshot of entries with `seq > since`.
    pub fn since(&self, since: u64) -> Vec<EventEntry> {
        self.0.0.lock().unwrap().since(since)
    }

    /// Current highest assigned seq (even if that entry was evicted).
    pub fn last_seq(&self) -> u64 {
        self.0.0.lock().unwrap().last_seq
    }

    /// Block up to `timeout` for any entry with `seq > since`. If one
    /// already exists, returns immediately. Spurious wakeups loop
    /// internally; the closure re-checks the predicate each wake.
    pub fn since_wait(&self, since: u64, timeout: Duration) -> Vec<EventEntry> {
        let (lock, cvar) = &*self.0;
        let guard = lock.lock().unwrap();
        let (guard, _result) = cvar
            .wait_timeout_while(guard, timeout, |log| log.last_seq <= since)
            .unwrap();
        guard.since(since)
    }
}

/// A debugger process running in a PTY. The reader thread owns the
/// read side of the master fd; the daemon holds this struct and writes
/// commands + consumes events from the channel.
pub struct DebuggerProcess {
    master: OwnedFd,
    child_pid: Pid,
    /// Wrapped in a Mutex so `DebuggerProcess: Sync`. The Receiver
    /// itself isn't `Sync`, but all access paths hold the daemon's
    /// session lock, so contention here is zero.
    rx: Mutex<Receiver<PtyEvent>>,
    /// Shared handle to the reader's event log. Clonable — daemon
    /// handlers grab their own clone so they can wait on the condvar
    /// without pinning the session mutex.
    log: LogHandle,
    shutdown: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    prompt_re: Regex,
}

impl DebuggerProcess {
    /// Spawn a debugger in a PTY and start the reader thread.
    pub fn spawn(
        bin: &str,
        args: &[String],
        env_extra: &[(String, String)],
        prompt_pattern: &str,
    ) -> Result<Self> {
        let OpenptyResult { master, slave } = openpty(None, None)?;

        // Safety: fork is unsafe because it duplicates the process.
        let fork_result = unsafe { fork() }?;
        match fork_result {
            ForkResult::Child => {
                drop(master);
                setsid().ok();

                let slave_fd = slave.as_raw_fd();
                dup2(slave_fd, 0).ok();
                dup2(slave_fd, 1).ok();
                dup2(slave_fd, 2).ok();
                if slave_fd > 2 {
                    close(slave_fd).ok();
                }

                // Mutate the child's environment in place then exec.
                // Safe: the child is single-threaded immediately after fork().
                // Portable across Linux and macOS (macOS libc has no execvpe).
                unsafe {
                    for (k, v) in env_extra {
                        std::env::set_var(k, v);
                    }
                    std::env::set_var("TERM", "dumb");
                }

                let c_bin =
                    std::ffi::CString::new(bin).unwrap_or_else(|_| std::process::exit(127));
                let mut c_args = vec![c_bin.clone()];
                for a in args {
                    c_args.push(
                        std::ffi::CString::new(a.as_str())
                            .unwrap_or_else(|_| std::process::exit(127)),
                    );
                }

                execvp(&c_bin, &c_args).ok();
                std::process::exit(127);
            }
            ForkResult::Parent { child } => {
                drop(slave);

                let prompt_re =
                    Regex::new(prompt_pattern).context("invalid prompt pattern")?;
                let reader_prompt_re = prompt_re.clone();
                let master_fd = master.as_raw_fd();
                let (tx, rx) = mpsc::channel::<PtyEvent>();
                let shutdown = Arc::new(AtomicBool::new(false));
                let reader_shutdown = shutdown.clone();
                let log = LogHandle::new();
                let reader_log = log.clone();

                let reader = std::thread::Builder::new()
                    .name("dbg-pty-reader".into())
                    .spawn(move || {
                        reader_loop(
                            master_fd,
                            reader_prompt_re,
                            tx,
                            reader_shutdown,
                            reader_log,
                        )
                    })
                    .context("failed to spawn reader thread")?;

                Ok(Self {
                    master,
                    child_pid: child,
                    rx: Mutex::new(rx),
                    log,
                    shutdown,
                    reader: Some(reader),
                    prompt_re,
                })
            }
        }
    }

    /// Write bytes to the master fd without creating a File (which would
    /// close the fd on drop or panic).
    fn write_master(&self, data: &[u8]) -> Result<()> {
        let fd = self.master.as_raw_fd();
        let mut written = 0;
        while written < data.len() {
            match nix::unistd::write(
                unsafe { BorrowedFd::borrow_raw(fd) },
                &data[written..],
            ) {
                Ok(n) => written += n,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Drain any events that arrived since the last `send_and_wait` or
    /// `drain_pending` call. Returns the accumulated output bytes (ANSI
    /// stripped). Non-blocking — never waits for new data.
    ///
    /// Used by the daemon at the head of each command to process stop
    /// banners that arrived asynchronously from the last execution
    /// command (e.g., node-inspect delivers `break in …` after having
    /// already ack-prompted the `cont`).
    pub fn drain_pending(&self) -> Option<String> {
        let rx = self.rx.lock().unwrap();
        let mut accumulated: Vec<u8> = Vec::new();
        let mut saw_data = false;
        loop {
            match rx.try_recv() {
                Ok(PtyEvent::Data(bytes)) => {
                    saw_data = true;
                    accumulated.extend(bytes);
                }
                Ok(PtyEvent::Prompt) => {}
                Ok(PtyEvent::Exit) => break,
                Err(_) => break,
            }
        }
        if !saw_data {
            return None;
        }
        Some(strip_ansi(&String::from_utf8_lossy(&accumulated)))
    }

    /// Wait for the initial prompt after spawn.
    pub fn wait_for_prompt(&self, timeout: Duration) -> Result<String> {
        let rx = self.rx.lock().unwrap();
        let mut collected: Vec<u8> = Vec::new();
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("timeout waiting for initial prompt");
            }
            match rx.recv_timeout(remaining) {
                Ok(PtyEvent::Data(bytes)) => collected.extend(bytes),
                Ok(PtyEvent::Prompt) => {
                    return Ok(strip_ansi(&String::from_utf8_lossy(&collected)));
                }
                Ok(PtyEvent::Exit) => bail!("debugger exited before producing prompt"),
                Err(RecvTimeoutError::Timeout) => {
                    bail!("timeout waiting for initial prompt")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("reader thread died before initial prompt")
                }
            }
        }
    }

    /// Send a command and wait for the prompt. Returns the debugger's
    /// response between our command and the next prompt.
    ///
    /// Call sites that need to handle async stop events should call
    /// `drain_pending()` first; this method only collects events that
    /// arrive after the command is written.
    pub fn send_and_wait(&self, cmd: &str, timeout: Duration) -> Result<String> {
        // Sticky "session has exited" guard. Once the child is gone,
        // the reader-thread channel is drained/closed and the loop
        // below would bail with "reader thread disconnected" — loudly
        // and for every subsequent verb. Return a clean, recognizable
        // status instead so agents can distinguish a dead session
        // (typical after the debuggee runs to completion) from a
        // genuine protocol error.
        if !self.is_alive() {
            return Ok("(session has exited — start a new one with `dbg start`)".to_string());
        }
        if let Err(e) = self.write_master(format!("{cmd}\n").as_bytes()) {
            // EIO / EPIPE on write almost always means the PTY master
            // closed under us because the debugger exited between the
            // alive-check above and the write. Surface the same clean
            // sticky message rather than the raw errno.
            if !self.is_alive() {
                return Ok("(session has exited — start a new one with `dbg start`)".to_string());
            }
            return Err(e);
        }

        let rx = self.rx.lock().unwrap();
        let mut collected: Vec<u8> = Vec::new();
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("timeout waiting for prompt");
            }
            match rx.recv_timeout(remaining) {
                Ok(PtyEvent::Data(bytes)) => collected.extend(bytes),
                Ok(PtyEvent::Prompt) => break,
                Ok(PtyEvent::Exit) => break,
                Err(RecvTimeoutError::Timeout) => bail!("timeout waiting for prompt"),
                Err(RecvTimeoutError::Disconnected) => {
                    // Reader thread exited — child is gone. Return the
                    // sticky status so the agent sees a consistent
                    // message regardless of which verb first noticed.
                    return Ok(
                        "(session has exited — start a new one with `dbg start`)".to_string(),
                    );
                }
            }
        }

        let raw = String::from_utf8_lossy(&collected).to_string();
        let clean = strip_ansi(&raw);
        let no_prompts = self.prompt_re.replace_all(&clean, "");

        let lines: Vec<&str> = no_prompts.lines().collect();
        let start = if !lines.is_empty() && lines[0].contains(cmd.trim()) {
            1
        } else {
            0
        };
        let mut end = lines.len();
        while end > start && lines[end - 1].trim().is_empty() {
            end -= 1;
        }
        Ok(lines[start..end].join("\n").trim().to_string())
    }

    /// Clone a shared handle to the event log. Handlers that need to
    /// wait for new events drop the session mutex first, then call
    /// `since_wait` on the handle — otherwise a blocking wait would
    /// pin the session.
    pub fn log(&self) -> LogHandle {
        self.log.clone()
    }

    /// The PID of the child process, for out-of-band signalling (e.g.
    /// interrupting a running command from the quit handler).
    pub fn child_pid(&self) -> Pid {
        self.child_pid
    }

    /// Check if the child process is still alive.
    pub fn is_alive(&self) -> bool {
        nix::sys::wait::waitpid(self.child_pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG))
            .is_ok_and(|s| matches!(s, nix::sys::wait::WaitStatus::StillAlive))
    }

    /// Send quit command and wait for exit.
    pub fn quit(&self, quit_cmd: &str) {
        if self.is_alive() {
            let _ = self.write_master(format!("{quit_cmd}\n").as_bytes());
            std::thread::sleep(Duration::from_millis(500));
            if self.is_alive() {
                let _ = nix::sys::signal::kill(self.child_pid, Signal::SIGKILL);
            }
        }
    }
}

/// Reader thread entry point. Reads PTY bytes, coalesces them into
/// Output chunks at prompt boundaries, and emits events on the channel
/// and into the persistent log. Exits when the shutdown flag is set or
/// EOF. Coalescing keeps the event log readable — one Output entry per
/// "command response" instead of one per 4KB PTY read.
fn reader_loop(
    master_fd: std::os::fd::RawFd,
    prompt_re: Regex,
    tx: Sender<PtyEvent>,
    shutdown: Arc<AtomicBool>,
    log: LogHandle,
) {
    let mut buf = [0u8; 4096];
    // Pending output bytes not yet emitted. Flushed to a single Output
    // event when a prompt is detected, when it grows past 64KB, or on
    // exit.
    let mut pending: Vec<u8> = Vec::new();

    let flush_output =
        |pending: &mut Vec<u8>, tx: &Sender<PtyEvent>, log: &LogHandle| -> bool {
            if pending.is_empty() {
                return true;
            }
            let bytes = std::mem::take(pending);
            log.push(EventKind::Output, bytes.clone());
            tx.send(PtyEvent::Data(bytes)).is_ok()
        };

    let emit_marker = |kind: EventKind, tx: &Sender<PtyEvent>, log: &LogHandle| -> bool {
        log.push(kind, Vec::new());
        let ev = match kind {
            EventKind::Prompt => PtyEvent::Prompt,
            EventKind::Exit => PtyEvent::Exit,
            // The reader only emits Prompt/Exit via this helper.
            // Output carries bytes so it goes through flush_output.
            // Stop is emitted by the daemon, never by the reader.
            EventKind::Output | EventKind::Stop => {
                unreachable!("emit_marker called with {kind:?}")
            }
        };
        tx.send(ev).is_ok()
    };

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        let borrowed = unsafe { BorrowedFd::borrow_raw(master_fd) };
        let pollfd = PollFd::new(borrowed, PollFlags::POLLIN);
        match poll(&mut [pollfd], 100u16) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => {
                let _ = flush_output(&mut pending, &tx, &log);
                let _ = emit_marker(EventKind::Exit, &tx, &log);
                return;
            }
        }

        let n = match nix::unistd::read(master_fd, &mut buf) {
            Ok(0) => {
                let _ = flush_output(&mut pending, &tx, &log);
                let _ = emit_marker(EventKind::Exit, &tx, &log);
                return;
            }
            Ok(n) => n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => {
                let _ = flush_output(&mut pending, &tx, &log);
                let _ = emit_marker(EventKind::Exit, &tx, &log);
                return;
            }
        };

        pending.extend_from_slice(&buf[..n]);

        // Prompt detection operates on the ANSI-stripped view of the
        // entire pending buffer. Cheap enough since pending is capped.
        let pending_str = String::from_utf8_lossy(&pending);
        let cleaned = strip_ansi(&pending_str);
        if prompt_re.is_match(&cleaned) {
            if !flush_output(&mut pending, &tx, &log) {
                return;
            }
            if !emit_marker(EventKind::Prompt, &tx, &log) {
                return;
            }
        } else if pending.len() > 64 * 1024 {
            // Safety valve: stream large outputs to the log without
            // waiting for a prompt. Agents tailing via `dbg events`
            // still see progress on long-running commands.
            if !flush_output(&mut pending, &tx, &log) {
                return;
            }
        }
    }
}

fn strip_ansi(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_string();
    }
    ANSI_RE.replace_all(s, "").to_string()
}

impl Drop for DebuggerProcess {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = nix::sys::signal::kill(self.child_pid, Signal::SIGTERM);
        if let Some(h) = self.reader.take() {
            // Best-effort: reader polls shutdown flag every 100ms.
            let _ = h.join();
        }
    }
}
