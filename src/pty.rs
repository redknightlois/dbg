use std::collections::VecDeque;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
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

/// Maximum number of debugger response bytes retained for one command.
/// Reading continues after this limit so a debugger cannot block on a full
/// PTY while the caller waits for the protocol prompt.
pub const MAX_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;
const COMMAND_OUTPUT_HALF: usize = MAX_COMMAND_OUTPUT_BYTES / 2;

/// How long a resync waits in silence for the reply of a command that was
/// written while the stream was unsynchronized. The debugger runs such a
/// command right after the stale prompt, or it already read the command as
/// input; a buffered command slower than this window is misattributed.
const BUFFERED_REPLY_QUIET: Duration = Duration::from_millis(200);

/// Bounded response capture. Keep both ends of a large response: debugger
/// stop records normally occur at the end, while the beginning still
/// contains the command's useful context. The discarded middle is explicit
/// in the returned text.
struct BoundedOutput {
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
}

impl BoundedOutput {
    fn new() -> Self {
        Self {
            head: Vec::with_capacity(COMMAND_OUTPUT_HALF),
            tail: Vec::with_capacity(COMMAND_OUTPUT_HALF),
            total: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len());
        let head_missing = COMMAND_OUTPUT_HALF.saturating_sub(self.head.len());
        let head_bytes = head_missing.min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_bytes]);
        self.push_tail(&bytes[head_bytes..]);
    }

    fn push_tail(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if bytes.len() >= COMMAND_OUTPUT_HALF {
            self.tail.clear();
            self.tail
                .extend_from_slice(&bytes[bytes.len() - COMMAND_OUTPUT_HALF..]);
            return;
        }
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > COMMAND_OUTPUT_HALF {
            let discard = self.tail.len() - COMMAND_OUTPUT_HALF;
            self.tail.drain(..discard);
        }
    }

    fn is_truncated(&self) -> bool {
        self.total > self.head.len() + self.tail.len()
    }

    fn into_string(self) -> String {
        if !self.is_truncated() {
            let mut bytes = self.head;
            bytes.extend_from_slice(&self.tail);
            return String::from_utf8_lossy(&bytes).into_owned();
        }
        let marker = format!(
            "\n[debugger output truncated: retained bytes from a {} byte response]\n",
            self.total
        );
        let budget = MAX_COMMAND_OUTPUT_BYTES.saturating_sub(marker.len());
        let head_len = self.head.len().min(budget / 2);
        let tail_len = self.tail.len().min(budget - head_len);
        let tail_start = self.tail.len() - tail_len;
        let mut bytes = Vec::with_capacity(head_len + marker.len() + tail_len);
        bytes.extend_from_slice(&self.head[..head_len]);
        bytes.extend_from_slice(marker.as_bytes());
        bytes.extend_from_slice(&self.tail[tail_start..]);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

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
/// `Stdout` is emitted by transports that can distinguish inferior
/// program output from debugger chatter (protocol backends: V8
/// Inspector, DAP). The PTY transport never emits `Stdout` because a
/// TTY mixes both streams at the OS level; everything goes into
/// `Output`. Agents filter with `--kind=stdout` to see only the
/// program's own writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Output,
    Prompt,
    Exit,
    Stop,
    Stdout,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Output => "output",
            EventKind::Prompt => "prompt",
            EventKind::Exit => "exit",
            EventKind::Stop => "stop",
            EventKind::Stdout => "stdout",
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
            "stdout" => Some(EventKind::Stdout),
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
const MAX_EVENT_LOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_PENDING_PTY_EVENTS: usize = 256;
const MAX_PENDING_PTY_BYTES: usize = 4 * 1024 * 1024;

/// Reader-to-consumer queue. `send` never blocks: the reader must keep
/// draining the pty while no command runs, so the queue evicts its oldest
/// `Data` event (the `EventLog` keeps a copy) once it holds more than
/// `MAX_PENDING_PTY_EVENTS` events or `MAX_PENDING_PTY_BYTES` bytes.
/// Markers are evicted only when no `Data` event remains.
struct PendingEvents {
    state: Mutex<PendingState>,
    ready: Condvar,
}

struct PendingState {
    events: VecDeque<PtyEvent>,
    bytes: usize,
    closed: bool,
}

impl PendingEvents {
    fn new() -> Self {
        Self {
            state: Mutex::new(PendingState {
                events: VecDeque::new(),
                bytes: 0,
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    fn send(&self, event: PtyEvent) {
        let mut state = self.state.lock().unwrap();
        if let PtyEvent::Data(bytes) = &event {
            state.bytes += bytes.len();
        }
        state.events.push_back(event);
        while state.events.len() > MAX_PENDING_PTY_EVENTS || state.bytes > MAX_PENDING_PTY_BYTES {
            let index = state
                .events
                .iter()
                .position(|event| matches!(event, PtyEvent::Data(_)))
                .unwrap_or(0);
            if let Some(PtyEvent::Data(bytes)) = state.events.remove(index) {
                state.bytes -= bytes.len();
            }
        }
        self.ready.notify_all();
    }

    /// Mark the producer gone; consumers see `Disconnected` once the queue is empty.
    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.ready.notify_all();
    }

    fn pop(state: &mut PendingState) -> Option<PtyEvent> {
        let event = state.events.pop_front()?;
        if let PtyEvent::Data(bytes) = &event {
            state.bytes -= bytes.len();
        }
        Some(event)
    }

    fn try_recv(&self) -> Option<PtyEvent> {
        Self::pop(&mut self.state.lock().unwrap())
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<PtyEvent, RecvTimeoutError> {
        let guard = self.state.lock().unwrap();
        let (mut state, _) = self
            .ready
            .wait_timeout_while(guard, timeout, |state| {
                state.events.is_empty() && !state.closed
            })
            .unwrap();
        match Self::pop(&mut state) {
            Some(event) => Ok(event),
            None if state.closed => Err(RecvTimeoutError::Disconnected),
            None => Err(RecvTimeoutError::Timeout),
        }
    }
}

struct EventLog {
    entries: VecDeque<EventEntry>,
    last_seq: u64,
    started: Instant,
    bytes: usize,
}

impl EventLog {
    fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(MAX_EVENTS),
            last_seq: 0,
            started: Instant::now(),
            bytes: 0,
        }
    }

    fn push(&mut self, kind: EventKind, mut bytes: Vec<u8>) {
        self.last_seq += 1;
        let ts_ms = self.started.elapsed().as_millis() as u64;
        if bytes.len() > MAX_EVENT_LOG_BYTES {
            bytes.truncate(MAX_EVENT_LOG_BYTES);
        }
        while self.entries.len() == MAX_EVENTS
            || self.bytes.saturating_add(bytes.len()) > MAX_EVENT_LOG_BYTES
        {
            let Some(removed) = self.entries.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(removed.bytes.len());
        }
        self.bytes += bytes.len();
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
    pub fn new() -> Self {
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

/// Transport-agnostic debugger I/O. The daemon holds a
/// `Box<dyn DebuggerIo>` and talks to the debugger through this
/// interface regardless of whether the underlying transport is a PTY,
/// a V8 Inspector WebSocket, or a DAP JSON-RPC subprocess.
///
/// All implementations must be `Send + Sync` because the daemon's
/// connection-handling threads call into them from inside a mutex.
///
/// Implementations:
///   * `DebuggerProcess` — PTY transport, default for line-oriented
///     debuggers (pdb, jdb, lldb, gdb, …).
///   * Protocol transports (coming in later steps) — Inspector, DAP.
pub trait DebuggerIo: Send + Sync {
    /// Send a command and wait for the prompt / ready state. Returns
    /// the debugger's response between command echo and the next
    /// prompt.
    fn send_and_wait(&self, cmd: &str, timeout: Duration) -> Result<String>;

    /// Drain any events that arrived asynchronously (e.g. a deferred
    /// stop banner from a prior `continue`). Non-blocking.
    fn drain_pending(&self) -> Option<String>;

    /// Wait for the initial prompt / first-ready signal after spawn.
    fn wait_for_prompt(&self, timeout: Duration) -> Result<String>;

    /// Clone the shared event-log handle. Callers drop the session
    /// mutex before waiting on the log's condvar.
    fn log(&self) -> LogHandle;

    /// PID of the process to SIGINT for `cancel` / `quit`. Protocol
    /// attach-mode transports may not have one; those use a
    /// protocol-level interrupt request instead and should override.
    fn child_pid(&self) -> Pid;

    /// Is the debugger still alive?
    fn is_alive(&self) -> bool;

    /// Graceful shutdown — send quit command then SIGKILL on timeout.
    fn quit(&self, quit_cmd: &str);

    /// Structured hit event produced by the transport's async channel
    /// (e.g. a V8 Inspector `Debugger.paused`). When `Some`, the
    /// daemon uses it directly and skips the text-based `parse_hit`
    /// pipeline — no regex banner scraping, no async banner races.
    /// Returns `None` on text/PTY transports; the `parse_hit` path
    /// handles them.
    ///
    /// Contract: each call takes the pending event out. Callers drain
    /// once per execution command, immediately after the command
    /// returns. Returning `Some` more than once per actual pause is a
    /// transport bug.
    fn pending_hit(&self) -> Option<crate::backend::canonical::HitEvent> {
        None
    }

    /// Transport-direct dispatch for structured canonical requests.
    /// Transports that can consume the structured form (DAP, Inspector)
    /// return `Some(Ok/Err)` after servicing the request; returning
    /// `None` signals "not handled — fall back to `send_and_wait` on the
    /// formatted native command." PTY backends always return `None`.
    fn dispatch_structured(
        &self,
        _req: &crate::backend::canonical::CanonicalReq,
        _timeout: Duration,
    ) -> Option<Result<String>> {
        None
    }
}

/// A debugger process running in a PTY. The reader thread owns the
/// read side of the master fd; the daemon holds this struct and writes
/// commands + consumes events from the channel.
pub struct DebuggerProcess {
    master: OwnedFd,
    child_pid: Pid,
    child_reaped: Arc<AtomicBool>,
    /// Consumers hold this lock for a whole receive so one command's
    /// events are never split between two consumers.
    rx: Mutex<Arc<PendingEvents>>,
    /// Shared handle to the reader's event log. Clonable — daemon
    /// handlers grab their own clone so they can wait on the condvar
    /// without pinning the session mutex.
    log: LogHandle,
    shutdown: Arc<AtomicBool>,
    synchronized: AtomicBool,
    /// Commands written while unsynchronized whose prompt may still arrive.
    buffered: AtomicUsize,
    /// Output that a resync consumed before a stale prompt; `drain_pending`
    /// returns it.
    stale: Mutex<BoundedOutput>,
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
        // Validate all fallible configuration before creating a child.
        let prompt_re = Regex::new(prompt_pattern).context("invalid prompt pattern")?;
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

                let c_bin = std::ffi::CString::new(bin).unwrap_or_else(|_| std::process::exit(127));
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

                let reader_prompt_re = prompt_re.clone();
                let master_fd = master.as_raw_fd();
                let rx = Arc::new(PendingEvents::new());
                let tx = rx.clone();
                let shutdown = Arc::new(AtomicBool::new(false));
                let child_reaped = Arc::new(AtomicBool::new(false));
                let reader_shutdown = shutdown.clone();
                let log = LogHandle::new();
                let reader_log = log.clone();

                let reader = match std::thread::Builder::new()
                    .name("dbg-pty-reader".into())
                    .spawn(move || {
                        reader_loop(
                            master_fd,
                            reader_prompt_re,
                            &tx,
                            reader_shutdown,
                            reader_log,
                        );
                        tx.close();
                    }) {
                    Ok(reader) => reader,
                    Err(error) => {
                        let _ = nix::sys::signal::kill(child, Signal::SIGKILL);
                        let _ = nix::sys::wait::waitpid(child, None);
                        return Err(error).context("failed to spawn reader thread");
                    }
                };

                Ok(Self {
                    master,
                    child_pid: child,
                    child_reaped,
                    rx: Mutex::new(rx),
                    log,
                    shutdown,
                    synchronized: AtomicBool::new(true),
                    buffered: AtomicUsize::new(0),
                    stale: Mutex::new(BoundedOutput::new()),
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
            match nix::unistd::write(unsafe { BorrowedFd::borrow_raw(fd) }, &data[written..]) {
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
        let mut accumulated =
            std::mem::replace(&mut *self.stale.lock().unwrap(), BoundedOutput::new());
        loop {
            match rx.try_recv() {
                Some(PtyEvent::Data(bytes)) => accumulated.push(&bytes),
                Some(PtyEvent::Prompt) => self.saw_prompt(),
                Some(PtyEvent::Exit) | None => break,
            }
        }
        if accumulated.total == 0 {
            return None;
        }
        Some(strip_ansi(&accumulated.into_string()))
    }

    /// Account for one prompt: the first one ends an unsynchronized stream,
    /// and each later one answers one buffered command.
    fn saw_prompt(&self) {
        if self.synchronized.swap(true, Ordering::AcqRel) {
            let _ = self
                .buffered
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
        }
    }

    /// Wait for the initial prompt after spawn.
    pub fn wait_for_prompt(&self, timeout: Duration) -> Result<String> {
        let rx = self.rx.lock().unwrap();
        let mut collected = BoundedOutput::new();
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("timeout waiting for initial prompt");
            }
            match rx.recv_timeout(remaining) {
                Ok(PtyEvent::Data(bytes)) => collected.push(&bytes),
                Ok(PtyEvent::Prompt) => {
                    self.saw_prompt();
                    return Ok(strip_ansi(&collected.into_string()));
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
    ///
    /// `timeout` bounds the whole call. After a timeout, the stream is
    /// unsynchronized until a prompt arrives. The call first waits for that
    /// prompt, and for the prompts of commands written while unsynchronized,
    /// then sends `cmd` as usual. The output before those prompts goes to
    /// `drain_pending`. When the stale prompt does not arrive, `cmd` is
    /// written as input to the timed-out command (for example the answer to
    /// a `(y or n)` question) and the call returns an error without waiting
    /// for a reply. When the deadline cuts the quiet window of a buffered
    /// command short, the call returns an error and does not write `cmd`.
    pub fn send_and_wait(&self, cmd: &str, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        {
            let rx = self.rx.lock().unwrap();
            let mut stale = self.stale.lock().unwrap();
            loop {
                let synchronized = self.synchronized.load(Ordering::Acquire);
                if synchronized && self.buffered.load(Ordering::Acquire) == 0 {
                    break;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                let wait = if synchronized {
                    remaining.min(BUFFERED_REPLY_QUIET)
                } else {
                    remaining
                };
                match rx.recv_timeout(wait) {
                    Ok(PtyEvent::Data(bytes)) => stale.push(&bytes),
                    Ok(PtyEvent::Prompt) => self.saw_prompt(),
                    // A quiet wait cut short does not prove the buffered command was read as input.
                    Err(RecvTimeoutError::Timeout)
                        if synchronized && wait < BUFFERED_REPLY_QUIET =>
                    {
                        bail!(
                            "timeout waiting for the prompt of a buffered command; `{cmd}` was not sent"
                        );
                    }
                    Ok(PtyEvent::Exit) | Err(_) => {
                        // A buffered command that stays silent for the full window was read as input.
                        if synchronized {
                            self.buffered.store(0, Ordering::Release);
                        }
                        break;
                    }
                }
            }
        }
        let resync = !self.synchronized.load(Ordering::Acquire);
        // Sticky "session has exited" guard. Once the child is gone,
        // the reader-thread channel is drained/closed and the loop
        // below would bail with "reader thread disconnected" — loudly
        // and for every subsequent verb. Return a clean, recognizable
        // status instead so agents can distinguish a dead session
        // (typical after the debuggee runs to completion) from a
        // genuine protocol error.
        if !self.is_alive() {
            return Ok(
                "(debuggee has exited — live inspection is over, but captured state is \
still available: `dbg hits <loc>`, `dbg stack`, `dbg locals`, `dbg cross <sym>`, \
`dbg sessions`. Start a fresh session with `dbg start` when ready.)"
                    .to_string(),
            );
        }
        if let Err(e) = self.write_master(format!("{cmd}\n").as_bytes()) {
            // EIO / EPIPE on write almost always means the PTY master
            // closed under us because the debugger exited between the
            // alive-check above and the write. Surface the same clean
            // sticky message rather than the raw errno.
            if !self.is_alive() {
                return Ok(
                    "(debuggee has exited — live inspection is over, but captured state is \
still available: `dbg hits <loc>`, `dbg stack`, `dbg locals`, `dbg cross <sym>`, \
`dbg sessions`. Start a fresh session with `dbg start` when ready.)"
                        .to_string(),
                );
            }
            return Err(e);
        }
        if resync {
            self.buffered.fetch_add(1, Ordering::AcqRel);
            bail!(
                "PTY input `{cmd}` went to a timed-out command, which has not returned to the prompt; the debugger either read it as input or runs it once the prompt returns, so check the state before sending it again"
            );
        }

        let rx = self.rx.lock().unwrap();
        let mut collected = BoundedOutput::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.synchronized.store(false, Ordering::Release);
                bail!("timeout waiting for prompt");
            }
            match rx.recv_timeout(remaining) {
                Ok(PtyEvent::Data(bytes)) => collected.push(&bytes),
                Ok(PtyEvent::Prompt) => {
                    self.synchronized.store(true, Ordering::Release);
                    break;
                }
                Ok(PtyEvent::Exit) => {
                    bail!("debugger exited while running `{cmd}`")
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    // Reader thread exited — child is gone. Return the
                    // sticky status so the agent sees a consistent
                    // message regardless of which verb first noticed.
                    return Ok(
                        "(debuggee has exited — live inspection is over, but captured state is \
still available: `dbg hits <loc>`, `dbg stack`, `dbg locals`, `dbg cross <sym>`, \
`dbg sessions`. Start a fresh session with `dbg start` when ready.)"
                            .to_string(),
                    );
                }
            }
        }

        // Do not sleep and then drain here. Output that arrives after this
        // prompt belongs to the next asynchronous event and remains in the
        // receiver for `drain_pending`. Output that arrived before the
        // prompt is already in `collected` and the daemon parses it for
        // stop banners even when this command is an inspection request.
        let raw = collected.into_string();
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
        if self.child_reaped.load(Ordering::Acquire) {
            return false;
        }
        match nix::sys::wait::waitpid(self.child_pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
            Ok(nix::sys::wait::WaitStatus::StillAlive) => true,
            Ok(
                nix::sys::wait::WaitStatus::Exited(_, _)
                | nix::sys::wait::WaitStatus::Signaled(_, _, _),
            ) => {
                self.child_reaped.store(true, Ordering::Release);
                false
            }
            Ok(_) => true,
            Err(nix::errno::Errno::ECHILD) => {
                self.child_reaped.store(true, Ordering::Release);
                false
            }
            Err(_) => false,
        }
    }

    /// Send quit command and wait for exit.
    pub fn quit(&self, quit_cmd: &str) {
        if self.is_alive() {
            let _ = self.write_master(format!("{quit_cmd}\n").as_bytes());
            std::thread::sleep(Duration::from_millis(500));
            if self.is_alive() {
                let _ = nix::sys::signal::kill(self.child_pid, Signal::SIGKILL);
            }
            self.wait_for_child_exit(Duration::from_secs(1));
        }
    }

    fn wait_for_child_exit(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.is_alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Trait-object forwarder so the daemon can hold `Box<dyn DebuggerIo>`
/// and route the same interface to future transports (Inspector, DAP)
/// without touching call sites.
impl DebuggerIo for DebuggerProcess {
    fn send_and_wait(&self, cmd: &str, timeout: Duration) -> Result<String> {
        DebuggerProcess::send_and_wait(self, cmd, timeout)
    }
    fn drain_pending(&self) -> Option<String> {
        DebuggerProcess::drain_pending(self)
    }
    fn wait_for_prompt(&self, timeout: Duration) -> Result<String> {
        DebuggerProcess::wait_for_prompt(self, timeout)
    }
    fn log(&self) -> LogHandle {
        DebuggerProcess::log(self)
    }
    fn child_pid(&self) -> Pid {
        DebuggerProcess::child_pid(self)
    }
    fn is_alive(&self) -> bool {
        DebuggerProcess::is_alive(self)
    }
    fn quit(&self, quit_cmd: &str) {
        DebuggerProcess::quit(self, quit_cmd)
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
    tx: &PendingEvents,
    shutdown: Arc<AtomicBool>,
    log: LogHandle,
) {
    let mut buf = [0u8; 4096];
    // Pending output bytes not yet emitted. Flushed to a single Output
    // event when a prompt is detected, when it grows past 64KB, when the
    // pty is idle for one poll interval, or on exit.
    let mut pending: Vec<u8> = Vec::new();
    // Keep a bounded copy separate from `pending`. The latter is flushed
    // during very large responses; without this probe, a prompt split
    // across two flushes could never match and the caller would wait until
    // its timeout even though the debugger is ready.
    const PROMPT_PROBE_BYTES: usize = 64 * 1024;
    let mut prompt_probe: Vec<u8> = Vec::new();

    let flush_output = |pending: &mut Vec<u8>, tx: &PendingEvents, log: &LogHandle| {
        if pending.is_empty() {
            return;
        }
        let bytes = std::mem::take(pending);
        log.push(EventKind::Output, bytes.clone());
        tx.send(PtyEvent::Data(bytes));
    };

    let emit_marker = |kind: EventKind, tx: &PendingEvents, log: &LogHandle| {
        log.push(kind, Vec::new());
        let ev = match kind {
            EventKind::Prompt => PtyEvent::Prompt,
            EventKind::Exit => PtyEvent::Exit,
            // The reader only emits Prompt/Exit via this helper.
            // Output carries bytes so it goes through flush_output.
            // Stop is emitted by the daemon, never by the reader.
            EventKind::Output | EventKind::Stop | EventKind::Stdout => {
                unreachable!("emit_marker called with {kind:?}")
            }
        };
        tx.send(ev);
    };

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        let borrowed = unsafe { BorrowedFd::borrow_raw(master_fd) };
        let pollfd = PollFd::new(borrowed, PollFlags::POLLIN);
        match poll(&mut [pollfd], 100u16) {
            // Idle output with no prompt, such as a `(y or n)` question.
            Ok(0) => {
                flush_output(&mut pending, tx, &log);
                continue;
            }
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => {
                flush_output(&mut pending, tx, &log);
                emit_marker(EventKind::Exit, tx, &log);
                return;
            }
        }

        let n = match nix::unistd::read(master_fd, &mut buf) {
            Ok(0) => {
                flush_output(&mut pending, tx, &log);
                emit_marker(EventKind::Exit, tx, &log);
                return;
            }
            Ok(n) => n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => {
                flush_output(&mut pending, tx, &log);
                emit_marker(EventKind::Exit, tx, &log);
                return;
            }
        };

        pending.extend_from_slice(&buf[..n]);
        prompt_probe.extend_from_slice(&buf[..n]);
        if prompt_probe.len() > PROMPT_PROBE_BYTES {
            let discard = prompt_probe.len() - PROMPT_PROBE_BYTES;
            prompt_probe.drain(..discard);
        }

        // Prompt detection operates on the bounded probe, not only on the
        // bytes waiting to be emitted. This preserves a match when the
        // prompt starts in a chunk that was already flushed.
        let probe_str = String::from_utf8_lossy(&prompt_probe);
        let cleaned = strip_ansi(&probe_str);
        if prompt_re.is_match(&cleaned) {
            flush_output(&mut pending, tx, &log);
            emit_marker(EventKind::Prompt, tx, &log);
            prompt_probe.clear();
        } else if pending.len() > 64 * 1024 {
            // Safety valve: stream large outputs to the log without
            // waiting for a prompt. Agents tailing via `dbg events`
            // still see progress on long-running commands.
            flush_output(&mut pending, tx, &log);
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
        if !self.child_reaped.load(Ordering::Acquire) && self.is_alive() {
            let _ = nix::sys::signal::kill(self.child_pid, Signal::SIGTERM);
            self.wait_for_child_exit(Duration::from_millis(250));
            if self.is_alive() {
                let _ = nix::sys::signal::kill(self.child_pid, Signal::SIGKILL);
                self.wait_for_child_exit(Duration::from_secs(1));
            }
        }
        if let Some(h) = self.reader.take() {
            // Best-effort: reader polls shutdown flag every 100ms.
            let _ = h.join();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Instant;

    #[test]
    fn event_log_is_bounded_by_bytes_as_well_as_entries() {
        let mut log = EventLog::new();
        log.push(EventKind::Output, vec![b'a'; 5 * 1024 * 1024]);
        log.push(EventKind::Output, vec![b'b'; 5 * 1024 * 1024]);
        assert!(log.bytes <= MAX_EVENT_LOG_BYTES);
        assert_eq!(log.entries.len(), 1);
        assert_eq!(log.entries[0].bytes[0], b'b');
    }

    #[test]
    fn invalid_prompt_is_rejected_before_a_child_is_created() {
        let error = match DebuggerProcess::spawn("/bin/sh", &[], &[], "(") {
            Ok(_) => panic!("invalid prompt unexpectedly created a child"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("invalid prompt pattern"));
    }

    #[test]
    fn timed_out_command_blocks_new_commands_until_its_prompt_is_drained() {
        let script = "printf 'dbg> '; while IFS= read -r line; do if [ \"$line\" = slow ]; then sleep 0.15; fi; printf 'ok\\ndbg> '; done";
        let process =
            DebuggerProcess::spawn("/bin/sh", &["-c".into(), script.into()], &[], r"dbg> ")
                .unwrap();
        process.wait_for_prompt(Duration::from_secs(2)).unwrap();
        assert!(
            process
                .send_and_wait("slow", Duration::from_millis(10))
                .unwrap_err()
                .to_string()
                .contains("timeout")
        );
        assert!(
            process
                .send_and_wait("next", Duration::from_millis(10))
                .unwrap_err()
                .to_string()
                .contains("timed-out command")
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            process
                .drain_pending()
                .is_some_and(|output| output.contains("ok"))
        );
        assert_eq!(
            process
                .send_and_wait("next", Duration::from_secs(1))
                .unwrap(),
            "ok"
        );
    }

    #[test]
    fn reader_keeps_logging_without_a_consumer() {
        let process = DebuggerProcess::spawn(
            "/bin/sh",
            &["-c".into(), "printf 'dbg> '; yes".into()],
            &[],
            r"dbg> ",
        )
        .unwrap();
        process.wait_for_prompt(Duration::from_secs(2)).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let a = process.log().last_seq();
        std::thread::sleep(Duration::from_millis(500));
        let b = process.log().last_seq();
        assert!(b > a, "reader stalled at seq {a}");
        let rx = process.rx.lock().unwrap();
        let state = rx.state.lock().unwrap();
        assert!(state.events.len() <= MAX_PENDING_PTY_EVENTS);
        assert!(state.bytes <= MAX_PENDING_PTY_BYTES);
    }

    #[test]
    fn pending_events_evict_data_before_markers() {
        let queue = PendingEvents::new();
        queue.send(PtyEvent::Prompt);
        for _ in 0..MAX_PENDING_PTY_EVENTS {
            queue.send(PtyEvent::Data(vec![b'x']));
        }
        assert!(matches!(queue.try_recv(), Some(PtyEvent::Prompt)));
        let mut data = 0;
        while let Some(event) = queue.try_recv() {
            assert!(matches!(event, PtyEvent::Data(_)));
            data += 1;
        }
        assert_eq!(data, MAX_PENDING_PTY_EVENTS - 1);
        queue.close();
        assert!(matches!(
            queue.recv_timeout(Duration::from_millis(10)),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn timed_out_command_without_a_prompt_recovers_through_input() {
        let script = "printf 'dbg> '; while IFS= read -r line; do if [ \"$line\" = slow ]; then sleep 0.15; printf 'Continue? (y or n) '; else printf 'ok\\ndbg> '; fi; done";
        let process =
            DebuggerProcess::spawn("/bin/sh", &["-c".into(), script.into()], &[], r"dbg> ")
                .unwrap();
        process.wait_for_prompt(Duration::from_secs(2)).unwrap();
        assert!(
            process
                .send_and_wait("slow", Duration::from_millis(10))
                .unwrap_err()
                .to_string()
                .contains("timeout")
        );
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            process
                .drain_pending()
                .is_some_and(|output| output.contains("Continue?"))
        );
        let recovered = (0..5).any(|_| {
            let ok = process
                .send_and_wait("next", Duration::from_secs(1))
                .is_ok_and(|output| output == "ok");
            process.drain_pending();
            ok
        });
        assert!(recovered);
    }

    #[test]
    fn command_after_a_timeout_runs_once_and_reports_its_reply() {
        let script = "printf 'dbg> '; while IFS= read -r line; do if [ \"$line\" = slow ]; then sleep 0.3; printf 'slow-done\\ndbg> '; else printf \"ran:$line\\ndbg> \"; fi; done";
        let process =
            DebuggerProcess::spawn("/bin/sh", &["-c".into(), script.into()], &[], r"dbg> ")
                .unwrap();
        process.wait_for_prompt(Duration::from_secs(2)).unwrap();
        assert!(
            process
                .send_and_wait("slow", Duration::from_millis(10))
                .unwrap_err()
                .to_string()
                .contains("timeout")
        );
        let reply = process
            .send_and_wait("next", Duration::from_secs(2))
            .unwrap();
        assert!(reply.contains("ran:next"), "{reply}");
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !process
                .drain_pending()
                .is_some_and(|output| output.contains("ran:next"))
        );
    }

    fn spawn_script(script: &str) -> DebuggerProcess {
        let process =
            DebuggerProcess::spawn("/bin/sh", &["-c".into(), script.into()], &[], r"dbg> ")
                .unwrap();
        process.wait_for_prompt(Duration::from_secs(2)).unwrap();
        process
    }

    fn assert_times_out(process: &DebuggerProcess, cmd: &str) {
        let error = process
            .send_and_wait(cmd, Duration::from_millis(10))
            .unwrap_err()
            .to_string();
        assert!(error.contains("timeout"), "{error}");
    }

    #[test]
    fn one_timeout_bounds_the_resync_and_the_reply() {
        let process = spawn_script(
            "printf 'dbg> '; while IFS= read -r line; do if [ \"$line\" = slow ]; then sleep 0.4; else sleep 2; fi; printf 'done\\ndbg> '; done",
        );
        assert_times_out(&process, "slow");
        let started = Instant::now();
        assert!(
            process
                .send_and_wait("slower", Duration::from_millis(500))
                .is_err()
        );
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(800), "{elapsed:?}");
    }

    #[test]
    fn stop_banner_of_a_timed_out_command_reaches_drain_pending() {
        let process = spawn_script(
            "printf 'dbg> '; while IFS= read -r line; do if [ \"$line\" = slow ]; then sleep 0.3; printf 'Breakpoint 1, main () at a.c:3\\ndbg> '; else printf \"ran:$line\\ndbg> \"; fi; done",
        );
        assert_times_out(&process, "slow");
        let reply = process
            .send_and_wait("next", Duration::from_secs(2))
            .unwrap();
        let drained = process.drain_pending().unwrap_or_default();
        assert!(
            reply.contains("Breakpoint 1") || drained.contains("Breakpoint 1"),
            "reply: {reply}; drained: {drained}"
        );
        assert!(reply.contains("ran:next"), "{reply}");
    }

    #[test]
    fn discarded_resync_output_is_bounded_and_keeps_its_end() {
        let bytes = 4 * MAX_COMMAND_OUTPUT_BYTES;
        let process = spawn_script(&format!(
            "printf 'dbg> '; while IFS= read -r line; do if [ \"$line\" = slow ]; then sleep 0.3; head -c {bytes} /dev/zero | tr '\\0' x; printf '\\nBreakpoint 1\\ndbg> '; else printf \"ran:$line\\ndbg> \"; fi; done"
        ));
        assert_times_out(&process, "slow");
        process
            .send_and_wait("next", Duration::from_secs(5))
            .unwrap();
        let drained = process.drain_pending().unwrap();
        assert!(
            drained.len() <= MAX_COMMAND_OUTPUT_BYTES,
            "{}",
            drained.len()
        );
        assert!(drained.contains("Breakpoint 1"));
    }

    #[test]
    fn reply_after_a_resync_bail_belongs_to_the_next_command() {
        let process = spawn_script(
            "printf 'dbg> '; while IFS= read -r line; do if [ \"$line\" = slow ]; then sleep 0.5; printf 'slow-done\\ndbg> '; else printf \"ran:$line\\ndbg> \"; fi; done",
        );
        assert_times_out(&process, "slow");
        let error = process
            .send_and_wait("a", Duration::from_millis(10))
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed-out command"), "{error}");
        let reply = process.send_and_wait("b", Duration::from_secs(2)).unwrap();
        assert!(
            reply.contains("ran:b") && !reply.contains("ran:a"),
            "{reply}"
        );
        let drained = process.drain_pending().unwrap_or_default();
        assert_eq!(drained.matches("ran:a").count(), 1, "{drained}");
    }

    #[test]
    fn reply_after_a_truncated_resync_window_belongs_to_the_next_command() {
        let temp = tempfile::tempdir().unwrap();
        let [ready, release_slow, release_a] =
            ["ready", "release-slow", "release-a"].map(|name| temp.path().join(name));
        let process = spawn_script(&format!(
            r#"
gate() {{ while [ ! -f "$1" ]; do sleep 0.001; done; }}
printf 'dbg> '
while IFS= read -r line; do
    if [ "$line" = slow ]; then
        gate '{release_slow}'
        printf 'slow-done\ndbg> '
        : > '{ready}'
    elif [ "$line" = a ]; then
        gate '{release_a}'
        printf 'ran:a\ndbg> '
    else
        printf "ran:$line\ndbg> "
    fi
done
"#,
            ready = ready.display(),
            release_slow = release_slow.display(),
            release_a = release_a.display(),
        ));
        assert_times_out(&process, "slow");
        let error = process
            .send_and_wait("a", Duration::from_millis(10))
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed-out command"), "{error}");
        std::fs::write(&release_slow, b"release").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "`slow` must print its prompt");
            thread::sleep(Duration::from_millis(1));
        }
        // The stale prompt is already written and `a` is gated, so a timeout
        // below the quiet window always cuts that window short.
        let error = process
            .send_and_wait("b", BUFFERED_REPLY_QUIET * 3 / 4)
            .unwrap_err()
            .to_string();
        assert!(error.contains("`b` was not sent"), "{error}");
        std::fs::write(&release_a, b"release").unwrap();
        let reply = process.send_and_wait("c", Duration::from_secs(2)).unwrap();
        assert!(
            reply.contains("ran:c") && !reply.contains("ran:b"),
            "{reply}"
        );
    }

    #[test]
    fn drop_kills_and_reaps_a_child_that_ignores_term() {
        let process = DebuggerProcess::spawn(
            "/bin/sh",
            &[
                "-c".into(),
                "trap '' TERM; printf 'dbg> '; while :; do sleep 1; done".into(),
            ],
            &[],
            r"dbg> ",
        )
        .unwrap();
        process.wait_for_prompt(Duration::from_secs(2)).unwrap();
        let pid = process.child_pid();

        drop(process);

        assert_eq!(
            nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)),
            Err(nix::errno::Errno::ECHILD)
        );
    }

    fn large_output_process(ready: &std::path::Path, release: &std::path::Path) -> DebuggerProcess {
        let script = r#"
ready=$1
release=$2
printf 'dbg> '
while IFS= read -r line; do
    if [ "$line" = large ]; then
        : > "$ready"
        while [ ! -f "$release" ]; do
            sleep 0.001
        done
        head -c 400000 /dev/zero | tr '\0' x
        printf '\nSTOPPED\n'
    else
        printf 'ok\n'
    fi
    printf 'dbg> '
done
"#;
        let process = DebuggerProcess::spawn(
            "/bin/sh",
            &[
                "-c".to_string(),
                script.to_string(),
                "large-output-test".to_string(),
                ready.display().to_string(),
                release.display().to_string(),
            ],
            &[],
            r"dbg> ",
        )
        .unwrap();
        process.wait_for_prompt(Duration::from_secs(10)).unwrap();
        process
    }

    #[test]
    fn large_output_truncation() {
        // The child-side markers prove that both producers reached their
        // release gate. The Rust barrier makes both producer threads submit
        // their commands before the controller inspects those markers and
        // releases either stream. This is a real rendezvous, not a timing
        // assumption about two threads starting close together.
        let temp = tempfile::tempdir().unwrap();
        let release = temp.path().join("release");
        let ready = [temp.path().join("ready-0"), temp.path().join("ready-1")];
        let processes = ready
            .iter()
            .map(|path| large_output_process(path, &release))
            .collect::<Vec<_>>();
        let barrier = Arc::new(Barrier::new(3));
        let workers: Vec<_> = processes
            .into_iter()
            .enumerate()
            .map(|(_, process)| {
                let barrier = barrier.clone();
                thread::spawn(move || {
                    let write_result = process.write_master(b"large\n");
                    barrier.wait();
                    write_result.unwrap();
                    let response = process.wait_for_prompt(Duration::from_secs(5)).unwrap();
                    assert!(
                        response.contains("[debugger output truncated:"),
                        "missing truncation indication"
                    );
                    assert!(response.contains("STOPPED"), "stop marker was discarded");
                    assert!(
                        response.len() < MAX_COMMAND_OUTPUT_BYTES + 1024,
                        "response exceeded hard capture bound: {}",
                        response.len()
                    );
                    let follow_up = process
                        .send_and_wait("small", Duration::from_secs(2))
                        .unwrap();
                    assert_eq!(follow_up, "ok");
                })
            })
            .collect();

        barrier.wait();
        let deadline = Instant::now() + Duration::from_secs(5);
        let both_ready = loop {
            if ready.iter().all(|path| path.exists()) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(1));
        };
        if !both_ready {
            // Release the children before joining so a failed readiness
            // check cannot leave a worker blocked during test cleanup.
            std::fs::write(&release, b"release").unwrap();
            for worker in workers {
                worker.join().unwrap();
            }
            panic!("both PTY producers must reach the release gate");
        }
        std::fs::write(&release, b"release").unwrap();
        for worker in workers {
            worker.join().unwrap();
        }
    }
}
