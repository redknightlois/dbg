//! Debug Adapter Protocol (DAP) transport.
//!
//! Generic transport that speaks DAP over a TCP socket to any
//! DAP-capable backend (delve/dlv dap, lldb-dap, debugpy, netcoredbg
//! --interpreter=vscode, js-debug). The transport itself is
//! backend-agnostic — it owns the framing, request/response
//! correlation, event dispatch, and state machine. Per-language
//! backends (delve-proto, debugpy-proto, …) layer on top by
//! providing:
//!
//!   * how to spawn the adapter subprocess (binary + args + stderr
//!     scrape pattern for the listen address)
//!   * how to build the `launch` request payload
//!
//! Structural parity with `InspectorTransport`:
//!   * structured stop events via DAP `stopped` → `pending_hit()`,
//!     so the daemon skips `parse_hit` text scraping;
//!   * program output routed through `EventKind::Stdout` via DAP
//!     `output` events with category=stdout;
//!   * no PTY, no banner timing races.
//!
//! Framing follows the DAP spec: each message is
//!   `Content-Length: N\r\n\r\n<N bytes of JSON>`
//! Messages have `type` ∈ {request, response, event}, monotonic
//! `seq`, and (for responses) `request_seq` referencing the original.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use nix::unistd::Pid;
use serde_json::{Value, json};

use crate::backend::canonical::HitEvent;
use crate::pty::{DebuggerIo, EventKind, LogHandle};

const MAX_DAP_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_DAP_HEADER_BYTES: usize = 4096;
const DAP_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

struct SpawnedChildGuard(Option<Child>);

impl Drop for SpawnedChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessIdentity {
    start_time: u64,
    exe_device: u64,
    exe_inode: u64,
}

fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = stat
        .rsplit_once(") ")?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let start_time = fields.get(19)?.parse().ok()?;
    let metadata = std::fs::metadata(format!("/proc/{pid}/exe")).ok()?;
    use std::os::unix::fs::MetadataExt;
    Some(ProcessIdentity {
        start_time,
        exe_device: metadata.dev(),
        exe_inode: metadata.ino(),
    })
}

fn process_children(pid: u32) -> Vec<u32> {
    process_children_from_root(Path::new("/proc"), pid)
}

fn process_children_from_root(proc_root: &Path, pid: u32) -> Vec<u32> {
    let task_dir = proc_root.join(format!("{pid}/task"));
    let mut children = Vec::new();
    let Ok(tasks) = std::fs::read_dir(task_dir) else {
        return children;
    };
    for task in tasks.flatten() {
        let path = task.path().join("children");
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        children.extend(
            content
                .split_whitespace()
                .filter_map(|child| child.parse::<u32>().ok()),
        );
    }
    children.sort_unstable();
    children.dedup();
    children
}

fn capture_owned_descendants(pid: u32) -> Vec<(u32, ProcessIdentity)> {
    let mut tree = Vec::new();
    let mut pending = process_children(pid);
    while let Some(current) = pending.pop() {
        if tree.iter().any(|(seen, _)| *seen == current) {
            continue;
        }
        if let Some(current_identity) = process_identity(current) {
            tree.push((current, current_identity));
        }
        pending.extend(process_children(current));
    }
    tree
}

fn find_new_descendant(
    before: &[(u32, ProcessIdentity)],
    after: Vec<(u32, ProcessIdentity)>,
) -> Option<(u32, ProcessIdentity)> {
    after
        .into_iter()
        .find(|candidate| !before.iter().any(|old| old == candidate))
}

fn kill_captured_processes(tree: Vec<(u32, ProcessIdentity)>) {
    for (current, expected_identity) in tree.into_iter().rev() {
        kill_process_instance(current, expected_identity);
    }
}

fn kill_process_instance(pid: u32, expected_identity: ProcessIdentity) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        // Bind the signal to this process instance. A check followed by
        // kill(pid) still has a PID-reuse window.
        let raw = unsafe { nix::libc::syscall(nix::libc::SYS_pidfd_open, pid, 0) } as i32;
        if raw < 0 {
            return;
        }
        // SAFETY: pidfd_open returned an owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // Recheck the instance bound by this descriptor. A check before
        // pidfd_open alone leaves a PID-reuse window.
        if process_identity(pid) != Some(expected_identity) {
            return;
        }
        let _ = unsafe {
            nix::libc::syscall(
                nix::libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                nix::libc::SIGKILL,
                std::ptr::null::<nix::libc::siginfo_t>(),
                0,
            )
        };
    }
    #[cfg(not(target_os = "linux"))]
    {
        if process_identity(pid) != Some(expected_identity) {
            return;
        }
        let _ =
            nix::sys::signal::kill(Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL);
    }
}

fn kill_owned_process_tree(pid: u32, identity: ProcessIdentity) {
    if process_identity(pid) != Some(identity) {
        return;
    }
    let mut tree = capture_owned_descendants(pid);
    tree.push((pid, identity));
    kill_captured_processes(tree);
}

/// Launch-time configuration supplied by the backend. Everything the
/// transport needs to spawn the adapter and drive the DAP handshake
/// through to the first `stopped` event.
pub struct DapLaunchConfig {
    /// Adapter binary (e.g. "dlv", "lldb-dap", "python").
    pub bin: String,
    /// Adapter args — should include the DAP-mode flag plus a
    /// listen-on-random-port flag the transport can scrape from
    /// stderr. For dlv: `["dap", "-l", "127.0.0.1:0"]`.
    pub args: Vec<String>,
    /// Regex substring that flags the stderr line announcing the
    /// listen address. Transport scrapes the first `host:port` match
    /// from any line containing this marker. For dlv:
    /// `"DAP server listening at:"`.
    pub listen_marker: String,
    /// The `launch` request payload (or `attach`, depending on
    /// backend choice). Transport sends this verbatim after the
    /// `initialize` response arrives.
    pub launch_args: Value,
    /// Launch verb — almost always "launch"; some adapters support
    /// "attach".
    pub launch_verb: String,
    /// Skip the stdout/stderr scrape and connect to this address
    /// directly. For adapters that don't announce their listen port
    /// (netcoredbg). Backends should pick a free port via
    /// `DapLaunchConfig::pick_free_port` and pass it to the adapter
    /// through `args`.
    pub preassigned_addr: Option<String>,
    /// Debuggee process id when this DAP session attaches to an
    /// already-running process. Used only for diagnostics/fallbacks;
    /// protocol requests still go through adapter-reported thread ids.
    pub debuggee_pid: Option<u32>,
}

impl DapLaunchConfig {
    /// Bind and immediately release a TCP port so the caller can pass
    /// it to an adapter that doesn't support `:0`. There is a small
    /// race window before the adapter reclaims the port; in practice
    /// the `connect_with_retry` loop absorbs it.
    pub fn pick_free_port() -> Result<u16> {
        let l = std::net::TcpListener::bind("127.0.0.1:0")
            .context("bind 127.0.0.1:0 to pick a free port")?;
        let port = l.local_addr()?.port();
        drop(l);
        Ok(port)
    }
}

struct State {
    /// Highest threadId seen from a stopped event — used as the
    /// default for stack / continue requests.
    current_thread: Option<i64>,
    /// Top frame's id for the current stop, set by the stopped-event
    /// handler after auto-fetching stackTrace.
    top_frame: Option<Value>,
    /// Full call-frame vec from the last stopped event.
    call_frames: Vec<Value>,
    /// Set by the driver when a DAP `stopped` event lands.
    pending_hit: Option<HitEvent>,
    pending_is_unscoped: bool,
    pending_action_generation: u64,
    /// Tracked user breakpoints: "file:line" → nothing (DAP
    /// setBreakpoints is path-keyed, not id-keyed).
    breakpoints: HashMap<String, Vec<u32>>,
    line_breakpoint_ids: HashMap<String, u32>,
    /// Accumulated function-breakpoint names. DAP `setFunctionBreakpoints`
    /// replaces the whole set on each call, so we replay them all.
    function_breakpoints: Vec<String>,
    function_breakpoint_ids: HashMap<String, u32>,
    /// "absolute-path:line" → condition expression, for replaying
    /// conditional line breakpoints across the full-set setBreakpoints call.
    breakpoint_conditions: HashMap<String, String>,
    /// Function-name → condition expression, same idea for setFunctionBreakpoints.
    function_breakpoint_conditions: HashMap<String, String>,
    function_breakpoint_log_messages: HashMap<String, String>,
    /// "absolute-path:line" → logMessage template. Logpoints emit
    /// formatted output without stopping the debuggee.
    breakpoint_log_messages: HashMap<String, String>,
    next_breakpoint_id: u32,
    /// True between `stopped` and the next `continue`/step.
    paused: bool,
    /// Flipped when the adapter disconnects or terminates.
    alive: bool,
    /// Set when the `initialized` event arrives. The transport blocks
    /// on this before sending `configurationDone`.
    initialized: bool,
    /// Flag set when a `terminated` or `exited` event arrives.
    terminated: bool,
    /// OS pid the adapter reported via the DAP `process` event for a
    /// launched session. None for attach (where we already know the
    /// pid via cfg.debuggee_pid). Used as a SIGKILL fallback during
    /// shutdown when the adapter doesn't terminate the debuggee
    /// cleanly — netcoredbg leaks the dotnet host to systemd-user
    /// otherwise.
    launched_pid: Option<u32>,
    launched_identity: Option<ProcessIdentity>,
    /// Generation of the current execution request. Deferred stack
    /// helpers carry this value so a late response from an earlier stop
    /// cannot populate a later continue's state.
    action_generation: u64,
    /// Generation which has reached the adapter request queue. Inbound
    /// stopped frames are dispatched before a new action is armed.
    armed_action_generation: u64,
    stop_generation: u64,
    enriched_stop_generation: u64,
}

impl State {
    fn new() -> Self {
        Self {
            current_thread: None,
            top_frame: None,
            call_frames: Vec::new(),
            pending_hit: None,
            pending_is_unscoped: false,
            pending_action_generation: 0,
            breakpoints: HashMap::new(),
            line_breakpoint_ids: HashMap::new(),
            function_breakpoints: Vec::new(),
            function_breakpoint_ids: HashMap::new(),
            breakpoint_conditions: HashMap::new(),
            function_breakpoint_conditions: HashMap::new(),
            function_breakpoint_log_messages: HashMap::new(),
            breakpoint_log_messages: HashMap::new(),
            next_breakpoint_id: 1,
            paused: false,
            alive: true,
            initialized: false,
            terminated: false,
            launched_pid: None,
            launched_identity: None,
            action_generation: 0,
            armed_action_generation: 0,
            stop_generation: 0,
            enriched_stop_generation: 0,
        }
    }
}

impl crate::transport_common::StopState for State {
    fn clear_pending(&mut self) {
        self.pending_hit = None;
        self.pending_is_unscoped = false;
        self.paused = false;
    }
    fn has_pending_hit(&self) -> bool {
        self.pending_hit.is_some()
    }
    fn stop_generation(&self) -> u64 {
        self.stop_generation
    }
    fn action_generation(&self) -> u64 {
        self.action_generation
    }
    fn pending_action_generation(&self) -> u64 {
        self.pending_action_generation
    }
    fn pending_is_unscoped(&self) -> bool {
        self.pending_is_unscoped
    }
    fn alive(&self) -> bool {
        self.alive
    }
    fn terminated(&self) -> bool {
        self.terminated
    }
}

enum DriverCmd {
    /// Send a DAP request with the supplied command + arguments.
    /// Reply is either the `body` of a successful response, or an
    /// `Err` with the adapter's message on failure.
    Call {
        command: String,
        arguments: Value,
        resp: Sender<Result<Value, String>>,
        arm_action: bool,
    },
    Shutdown,
}

pub struct DapTransport {
    child_pid: Pid,
    child: Mutex<Option<Child>>,
    driver_tx: Sender<DriverCmd>,
    log: LogHandle,
    state: Arc<(Mutex<State>, Condvar)>,
    /// Serializes breakpoint state transactions through the adapter.
    /// Without this guard, a late rollback can overwrite a concurrent
    /// breakpoint update that has already committed.
    breakpoint_transaction: Mutex<()>,
    shutdown: Arc<AtomicBool>,
    driver: Mutex<Option<JoinHandle<()>>>,
    debuggee_pid: Option<u32>,
    /// True when this session attached to an existing process. Attach
    /// sessions must default to `terminateDebuggee=false` on disconnect
    /// so we don't kill a process the user only wanted to observe;
    /// launch sessions default to true so we don't leak children.
    is_attach: bool,
}

impl DapTransport {
    /// Spawn the DAP adapter, connect, drive the full DAP handshake
    /// (initialize → launch → configurationDone), and return the
    /// transport positioned just before the first `stopped` event.
    /// Callers that need `stopOnEntry=true` behaviour bake it into
    /// `launch_args`; the transport doesn't assume either way.
    pub fn spawn(cfg: DapLaunchConfig) -> Result<Self> {
        // Yama-restricted ptrace will silently swallow the attach and
        // surface only as a `configurationDone` timeout 10s later.
        // Catch it up front when we can prove the attach won't work.
        if cfg.launch_verb == "attach" {
            if let Some(pid) = cfg.debuggee_pid {
                preflight_attach(pid)?;
            }
        }

        let mut cmd = Command::new(&cfg.bin);
        cmd.args(&cfg.args)
            .stdin(Stdio::null())
            // Adapters differ on where they announce their listen
            // address: dlv prints to stdout, lldb-dap & debugpy to
            // stderr. Pipe both and let the scraper search either.
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child_guard = SpawnedChildGuard(Some(
            cmd.spawn()
                .with_context(|| format!("failed to spawn {}", cfg.bin))?,
        ));
        let child = child_guard.0.as_mut().expect("child guard is populated");
        let child_pid = Pid::from_raw(child.id() as i32);
        let stderr = child.stderr.take().context("missing adapter stderr")?;
        let stdout = child.stdout.take().context("missing adapter stdout")?;

        // Race stdout vs stderr for the listen-address announcement.
        // Whichever stream produces the marker first wins; the other
        // stays drained by a background forwarder so its buffer
        // doesn't block the adapter later. `leftover_stdout` carries
        // the adapter's stdout after scrape so spawn_drain can route
        // it to EventKind::Stdout (needed for delve, which inherits
        // stdio to the target and doesn't route program output
        // through DAP `output` events).
        let log = LogHandle::new();
        let addr = if let Some(ref a) = cfg.preassigned_addr {
            // Adapter is silent about its listen port (netcoredbg);
            // the backend already picked a free port and told the
            // adapter to bind it. Drain both streams in case the
            // adapter does chatter later.
            spawn_drain(stdout, Some(log.clone()));
            spawn_drain(stderr, None);
            a.clone()
        } else {
            let (addr, leftover_stdout, leftover_stderr) = match scrape_listen_addr_either(
                stdout,
                stderr,
                &cfg.listen_marker,
                Duration::from_secs(10),
            ) {
                Ok(r) => r,
                Err(e) => {
                    if let Ok(Some(status)) = child.try_wait() {
                        bail!("adapter exited before announcing (status={status:?}): {e:#}");
                    }
                    return Err(e).context("failed to read listen address");
                }
            };
            if let Some(so) = leftover_stdout {
                spawn_drain(so, Some(log.clone()));
            }
            if let Some(se) = leftover_stderr {
                spawn_drain(se, None);
            }
            addr
        };

        // Retry TCP connect a few times — some adapters announce the
        // listen port just before bind() completes.
        let require_listener_owner =
            cfg.preassigned_addr.is_some() && cfg.bin.to_ascii_lowercase().contains("netcoredbg");
        let stream = connect_with_retry_owned(
            &addr,
            Duration::from_secs(5),
            require_listener_owner,
            child_pid.as_raw() as u32,
        )
        .with_context(|| format!("failed to connect to adapter at {addr}"))?;
        stream.set_nonblocking(true)?;

        let state = Arc::new((Mutex::new(State::new()), Condvar::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (driver_tx, driver_rx) = mpsc::channel::<DriverCmd>();

        let driver_state = state.clone();
        let driver_log = log.clone();
        let driver_shutdown = shutdown.clone();
        let driver = std::thread::Builder::new()
            .name("dbg-dap-driver".into())
            .spawn(move || {
                driver_loop(stream, driver_rx, driver_state, driver_log, driver_shutdown);
            })
            .context("failed to spawn DAP driver thread")?;

        let is_attach = cfg.launch_verb == "attach";
        let transport = Self {
            child_pid,
            child: Mutex::new(Some(
                child_guard.0.take().expect("child guard is populated"),
            )),
            driver_tx,
            log,
            state,
            breakpoint_transaction: Mutex::new(()),
            shutdown,
            driver: Mutex::new(Some(driver)),
            debuggee_pid: cfg.debuggee_pid,
            is_attach,
        };

        // DAP handshake.
        transport.call_blocking(
            "initialize",
            json!({
                "clientID": "dbg-cli",
                "clientName": "dbg",
                "adapterID": cfg.bin,
                "pathFormat": "path",
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "supportsVariableType": true,
                "supportsRunInTerminalRequest": false,
            }),
            Duration::from_secs(10),
        )?;
        // DAP handshake after initialize:
        //   1. Fire `launch` async — lldb-dap delays its launch response
        //      until after configurationDone, so a blocking send would
        //      deadlock. Delve responds to launch immediately; both
        //      flows work under the async pattern.
        //   2. Wait for the `initialized` event.
        //   3. Send configurationDone (blocking).
        //   4. Drain the launch response before returning.
        let launch_rx = transport.call_async(&cfg.launch_verb, cfg.launch_args)?;
        transport.wait_for_initialized(Duration::from_secs(15))?;
        if let Err(e) =
            transport.call_blocking("configurationDone", json!({}), Duration::from_secs(10))
        {
            if cfg.launch_verb == "attach" {
                if let Some(pid) = cfg.debuggee_pid {
                    bail!(
                        "DAP configurationDone failed while attaching to pid {pid}: {e:#}. Verify that the PID is the managed/debuggable child process, not a shell or launcher parent."
                    );
                }
                bail!("DAP configurationDone failed during attach: {e:#}");
            }
            return Err(e);
        }
        match launch_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => bail!("DAP launch: {e}"),
            Err(_) => bail!("DAP launch: timeout waiting for response"),
        }
        Ok(transport)
    }

    fn wait_for_initialized(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let (lock, cvar) = &*self.state;
        let mut guard = lock.lock().unwrap();
        while guard.alive && !guard.initialized && !guard.terminated {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("timeout waiting for DAP initialized event");
            }
            let r = cvar.wait_timeout(guard, remaining).unwrap();
            guard = r.0;
        }
        if guard.terminated && !guard.initialized {
            bail!("adapter terminated before initialized event");
        }
        Ok(())
    }

    fn call_async(
        &self,
        command: &str,
        arguments: Value,
    ) -> Result<mpsc::Receiver<std::result::Result<Value, String>>> {
        let (tx, rx) = mpsc::channel();
        self.driver_tx
            .send(DriverCmd::Call {
                command: command.to_string(),
                arguments,
                resp: tx,
                arm_action: false,
            })
            .map_err(|_| anyhow!("DAP driver thread gone"))?;
        Ok(rx)
    }

    fn call_blocking(&self, command: &str, arguments: Value, timeout: Duration) -> Result<Value> {
        self.call_blocking_inner(command, arguments, timeout, false)
    }

    fn call_blocking_action(
        &self,
        command: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<Value> {
        self.call_blocking_inner(command, arguments, timeout, true)
    }

    fn call_blocking_inner(
        &self,
        command: &str,
        arguments: Value,
        timeout: Duration,
        arm_action: bool,
    ) -> Result<Value> {
        let (tx, rx) = mpsc::channel();
        self.driver_tx
            .send(DriverCmd::Call {
                command: command.to_string(),
                arguments,
                resp: tx,
                arm_action,
            })
            .map_err(|_| anyhow!("DAP driver thread gone"))?;
        match rx.recv_timeout(timeout) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!("DAP {command}: {e}")),
            Err(_) => Err(anyhow!("DAP {command}: timeout")),
        }
    }

    fn run_command(&self, cmd: &str, timeout: Duration) -> Result<String> {
        let trimmed = cmd.trim();
        if matches!(trimmed, "cont" | "c" | "continue") {
            let tid = self.current_thread().unwrap_or(1);
            return self.exec(
                |s| s.call_blocking_action("continue", json!({"threadId": tid}), timeout),
                timeout,
            );
        }
        if matches!(trimmed, "step" | "s" | "stepi") {
            let tid = self.current_thread().unwrap_or(1);
            return self.exec(
                |s| s.call_blocking_action("stepIn", json!({"threadId": tid}), timeout),
                timeout,
            );
        }
        if matches!(trimmed, "next" | "n") {
            let tid = self.current_thread().unwrap_or(1);
            return self.exec(
                |s| s.call_blocking_action("next", json!({"threadId": tid}), timeout),
                timeout,
            );
        }
        if matches!(trimmed, "out" | "finish") {
            let tid = self.current_thread().unwrap_or(1);
            return self.exec(
                |s| s.call_blocking_action("stepOut", json!({"threadId": tid}), timeout),
                timeout,
            );
        }
        if trimmed == "pause" {
            return self.pause(timeout);
        }
        if trimmed == "restart" {
            // Adapter behavior on restart varies: some relaunch with
            // stopOnEntry (emits a new stopped event), others resume the
            // process as if continue was pressed (no stop event). We
            // just fire the request and return — callers can query
            // state afterward. Wrapping in exec() would hang waiting
            // for a stop that may never arrive.
            //
            // Clear per-session frame state *before* the restart so the
            // post-restart `stopped` event (if any) repopulates from
            // scratch instead of returning stale frameIds to
            // `locals`/`print`.
            let descendants_before = capture_owned_descendants(self.child_pid.as_raw() as u32);
            {
                let (lock, _) = &*self.state;
                let mut s = lock.lock().unwrap();
                s.action_generation = s.action_generation.wrapping_add(1);
                s.paused = false;
                s.top_frame = None;
                s.call_frames.clear();
                s.pending_hit = None;
                s.pending_action_generation = s.action_generation;
                s.armed_action_generation = s.action_generation;
                // A restart replaces the launched process. Do not retain
                // the old PID as a shutdown fallback while the replacement
                // process event is still in flight.
                s.launched_pid = None;
                s.launched_identity = None;
            }
            self.call_blocking("restart", json!({}), timeout)?;
            self.track_restarted_debuggee(&descendants_before, timeout);
            return Ok("restart requested".into());
        }
        if trimmed == "catch" || trimmed == "catch off" {
            self.call_blocking(
                "setExceptionBreakpoints",
                json!({"filters": Vec::<String>::new()}),
                timeout,
            )?;
            return Ok("exception breakpoints cleared".into());
        }
        if let Some(rest) = trimmed.strip_prefix("catch ") {
            let filters: Vec<&str> = rest
                .split(|c: char| c.is_ascii_whitespace() || c == ',')
                .filter(|s| !s.is_empty())
                .collect();
            self.call_blocking(
                "setExceptionBreakpoints",
                json!({"filters": filters}),
                timeout,
            )?;
            return Ok(format!("exception breakpoints: {}", filters.join(", ")));
        }
        if trimmed == "backtrace" || trimmed == "bt" || trimmed == "where" {
            return Ok(self.format_backtrace());
        }
        if trimmed == "breakpoints" {
            return Ok(self.format_breakpoints());
        }
        if let Some(rest) = trimmed
            .strip_prefix("breakpoint delete ")
            .or_else(|| trimmed.strip_prefix("delete "))
        {
            let id: u32 = rest
                .trim()
                .parse()
                .context("delete: invalid breakpoint id")?;
            return self.delete_breakpoint(id, timeout);
        }
        if trimmed == "locals" {
            return self.collect_locals(timeout);
        }
        if trimmed == "threads" || trimmed == "thread list" {
            return self.list_threads(timeout);
        }
        if let Some(rest) = trimmed.strip_prefix("thread ") {
            if let Ok(n) = rest.trim().parse::<i64>() {
                return self.set_thread(n);
            }
        }
        if trimmed == "list" {
            return self.list_source(None);
        }
        if let Some(loc) = trimmed.strip_prefix("list ") {
            return self.list_source(Some(loc.trim()));
        }
        if let Some(rest) = trimmed.strip_prefix("bfn ") {
            let (name, cond) = match rest.find(" if ") {
                Some(i) => (rest[..i].trim(), Some(rest[i + 4..].trim())),
                None => (rest.trim(), None),
            };
            return self.set_function_breakpoint(name, cond, None, timeout);
        }
        if let Some(spec) = parse_break(trimmed) {
            return self.set_breakpoint(&spec, timeout);
        }
        if let Some(expr) = trimmed
            .strip_prefix("print ")
            .or_else(|| trimmed.strip_prefix("p "))
        {
            return self.evaluate(expr, timeout);
        }
        if let Some(rest) = trimmed.strip_prefix("set ") {
            return self.set_expression(rest, timeout);
        }
        if trimmed == ".exit" || trimmed == "quit" {
            self.shutdown.store(true, Ordering::Relaxed);
            let _ = self.driver_tx.send(DriverCmd::Shutdown);
            return Ok(String::new());
        }
        Err(anyhow!("dap: unsupported command `{trimmed}`"))
    }

    fn exec<F: FnOnce(&Self) -> Result<Value>>(&self, f: F, timeout: Duration) -> Result<String> {
        {
            let (lock, _) = &*self.state;
            let mut s = lock.lock().unwrap();
            s.action_generation = s.action_generation.wrapping_add(1);
        }
        crate::transport_common::wait_for_stop(&self.state, || f(self).map(|_| ()), timeout)
    }

    fn current_thread(&self) -> Option<i64> {
        let (lock, _) = &*self.state;
        lock.lock().unwrap().current_thread
    }

    /// Some adapters restart the inferior without sending a DAP `process`
    /// event. Keep quit cleanup correct by finding a new adapter descendant
    /// after the restart request. The search is bounded because a restart
    /// request must not turn into an unbounded process wait.
    fn track_restarted_debuggee(
        &self,
        descendants_before: &[(u32, ProcessIdentity)],
        timeout: Duration,
    ) {
        if self.is_attach {
            return;
        }
        let deadline = Instant::now() + timeout.min(Duration::from_secs(2));
        loop {
            let candidate = find_new_descendant(
                descendants_before,
                capture_owned_descendants(self.child_pid.as_raw() as u32),
            );
            if let Some((pid, identity)) = candidate {
                let (lock, _) = &*self.state;
                let mut s = lock.lock().unwrap();
                s.launched_pid = Some(pid);
                s.launched_identity = Some(identity);
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn list_threads(&self, timeout: Duration) -> Result<String> {
        let resp = self.call_blocking("threads", json!({}), timeout)?;
        let arr = resp
            .get("threads")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let os_threads = self.proc_task_threads();
        if arr.is_empty() {
            if os_threads.is_empty() {
                return Ok("(no threads)".into());
            }
            let mut out = String::from(
                "(adapter reported no DAP threads; OS threads from /proc, not switchable by dbg thread)\n",
            );
            for tid in os_threads {
                out.push_str(&format!("  {tid}  <os thread>\n"));
            }
            return Ok(out.trim_end().to_string());
        }
        let current = self.current_thread();
        let mut out = String::new();
        for t in &arr {
            let id = t.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let marker = if Some(id) == current { "*" } else { " " };
            out.push_str(&format!("{marker} {id}  {name}\n"));
        }
        if let Some(pid) = self.debuggee_pid {
            if os_threads.len() > arr.len() {
                out.push_str(&format!(
                    "(note: adapter exposed {} DAP thread(s), but /proc/{pid}/task has {} OS thread(s); only DAP ids are switchable)\n",
                    arr.len(),
                    os_threads.len()
                ));
            }
        }
        Ok(out.trim_end().to_string())
    }

    fn proc_task_threads(&self) -> Vec<i64> {
        let Some(pid) = self.debuggee_pid else {
            return Vec::new();
        };
        let mut tids = Vec::new();
        if let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/task")) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if let Ok(tid) = name.parse::<i64>() {
                        tids.push(tid);
                    }
                }
            }
        }
        tids.sort_unstable();
        tids
    }

    fn dap_thread_ids(&self, timeout: Duration) -> Result<Vec<i64>> {
        let resp = self.call_blocking("threads", json!({}), timeout)?;
        Ok(resp
            .get("threads")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("id").and_then(|v| v.as_i64()))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn pause(&self, timeout: Duration) -> Result<String> {
        let tids = match self.current_thread() {
            Some(tid) => vec![tid],
            None => self.dap_thread_ids(Duration::from_secs(5))?,
        };
        if tids.is_empty() {
            bail!("pause: adapter reported no DAP threads to pause");
        }
        {
            let (lock, _) = &*self.state;
            let mut s = lock.lock().unwrap();
            s.action_generation = s.action_generation.wrapping_add(1);
        }
        match crate::transport_common::wait_for_stop(
            &self.state,
            || {
                let mut last_err = None;
                for tid in &tids {
                    if let Err(e) =
                        self.call_blocking_action("pause", json!({"threadId": tid}), timeout)
                    {
                        last_err = Some(e);
                    }
                }
                if tids.len() == 1 {
                    if let Some(e) = last_err {
                        return Err(e);
                    }
                }
                Ok(())
            },
            timeout,
        ) {
            Ok(_) => Ok("paused".into()),
            Err(e) => {
                if e.to_string().contains("timeout waiting for stopped event") {
                    if let Some(pid) = self.debuggee_pid {
                        bail!(
                            "timeout waiting for stopped event after pause requested for DAP thread(s) {tids:?} in attached pid {pid}; the adapter may not be able to interrupt this runtime state"
                        );
                    }
                    bail!(
                        "timeout waiting for stopped event after pause requested for DAP thread(s) {tids:?}; the adapter may not be able to interrupt this runtime state"
                    );
                }
                Err(e)
            }
        }
    }

    fn set_thread(&self, id: i64) -> Result<String> {
        // DAP has no explicit "switch thread"; the threadId we pass to
        // subsequent continue/next/step decides. Record it as current
        // and refresh the stack view so backtrace/locals operate on the
        // newly-selected thread.
        {
            let (lock, _) = &*self.state;
            lock.lock().unwrap().current_thread = Some(id);
        }
        // Re-fetch stackTrace for the new thread so `where`/`locals` reflect it.
        if let Ok(resp) = self.call_blocking(
            "stackTrace",
            json!({ "threadId": id, "startFrame": 0, "levels": 20 }),
            Duration::from_secs(5),
        ) {
            if let Some(frames) = resp.get("stackFrames").and_then(|v| v.as_array()).cloned() {
                let (lock, cvar) = &*self.state;
                let mut s = lock.lock().unwrap();
                s.top_frame = frames.first().cloned();
                s.call_frames = frames;
                cvar.notify_all();
            }
        }
        Ok(format!("switched to thread {id}"))
    }

    fn list_source(&self, loc: Option<&str>) -> Result<String> {
        // Resolve (path, line) — either from the argument or the top
        // frame. DAP adapters do expose a `source` request, but for
        // on-disk files it's strictly slower than reading the path
        // directly. We only fall back to the adapter when no path is
        // available (inline scripts, virtual sources).
        let (path, line) = match loc {
            Some(s) => {
                let (p, l) = s
                    .rsplit_once(':')
                    .ok_or_else(|| anyhow!("list: expected file:line"))?;
                let line: u32 = l.trim().parse().context("list: invalid line number")?;
                (p.trim().to_string(), line)
            }
            None => {
                let (lock, _) = &*self.state;
                let s = lock.lock().unwrap();
                let f = s
                    .call_frames
                    .first()
                    .ok_or_else(|| anyhow!("list: no current frame"))?;
                let path = f
                    .get("source")
                    .and_then(|src| src.get("path"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("list: current frame has no source path"))?
                    .to_string();
                let line = f.get("line").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                (path, line)
            }
        };
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("list: reading {path}"))?;
        let lines: Vec<&str> = text.lines().collect();
        let center = line as usize;
        let start = center.saturating_sub(10).max(1);
        let end = (center + 10).min(lines.len());
        let mut out = String::new();
        for (i, l) in lines.iter().enumerate().take(end).skip(start - 1) {
            let n = i + 1;
            let marker = if n == center { "->" } else { "  " };
            out.push_str(&format!("{marker} {n:>5}  {l}\n"));
        }
        Ok(out)
    }

    fn set_function_breakpoint(
        &self,
        name: &str,
        cond: Option<&str>,
        log_message: Option<&str>,
        timeout: Duration,
    ) -> Result<String> {
        let _transaction = self.breakpoint_transaction.lock().unwrap();
        // DAP `setFunctionBreakpoints` replaces the whole set per call,
        // same semantics as `setBreakpoints`. Accumulate in state so
        // adding a second fn bp doesn't remove the first.
        let snapshot = {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            (
                s.function_breakpoints.clone(),
                s.function_breakpoint_ids.clone(),
                s.function_breakpoint_conditions.clone(),
                s.function_breakpoint_log_messages.clone(),
                s.next_breakpoint_id,
            )
        };
        let all: Vec<String> = {
            let (lock, _) = &*self.state;
            let mut s = lock.lock().unwrap();
            let already_present = s
                .function_breakpoints
                .iter()
                .any(|candidate| candidate == name);
            if !already_present {
                // Reserve the public id before changing any function
                // breakpoint state. In particular, an exhausted allocator
                // must not leave a condition or log message behind.
                let id = allocate_breakpoint_id(&mut s)?;
                s.function_breakpoints.push(name.to_string());
                s.function_breakpoint_ids.insert(name.to_string(), id);
            }
            if let Some(c) = cond {
                s.function_breakpoint_conditions
                    .insert(name.to_string(), c.to_string());
            } else {
                s.function_breakpoint_conditions.remove(name);
            }
            if let Some(message) = log_message {
                s.function_breakpoint_log_messages
                    .insert(name.to_string(), message.to_string());
            } else {
                s.function_breakpoint_log_messages.remove(name);
            }
            s.function_breakpoints.clone()
        };
        let fns: Vec<Value> = {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            all.iter()
                .map(|n| {
                    function_breakpoint_value(
                        n,
                        s.function_breakpoint_conditions.get(n).map(String::as_str),
                        s.function_breakpoint_log_messages
                            .get(n)
                            .map(String::as_str),
                    )
                })
                .collect()
        };
        let response = match self.call_blocking(
            "setFunctionBreakpoints",
            json!({ "breakpoints": fns }),
            timeout,
        ) {
            Ok(response) => response,
            Err(error) => {
                self.restore_function_breakpoint_state(snapshot);
                return Err(error);
            }
        };
        if !function_breakpoints_verified(&response, fns.len()) {
            self.restore_function_breakpoint_state(snapshot);
            bail!("function breakpoint `{name}` was not verified by the adapter");
        }
        match (cond, log_message) {
            (Some(c), Some(message)) => {
                Ok(format!("Function logpoint set: {name} if {c}: {message}"))
            }
            (None, Some(message)) => Ok(format!("Function logpoint set: {name}: {message}")),
            (Some(c), None) => Ok(format!("Function breakpoint set: {name} if {c}")),
            (None, None) => Ok(format!("Function breakpoint set: {name}")),
        }
    }

    fn restore_function_breakpoint_state(
        &self,
        snapshot: (
            Vec<String>,
            HashMap<String, u32>,
            HashMap<String, String>,
            HashMap<String, String>,
            u32,
        ),
    ) {
        let (lock, _) = &*self.state;
        let mut s = lock.lock().unwrap();
        s.function_breakpoints = snapshot.0;
        s.function_breakpoint_ids = snapshot.1;
        s.function_breakpoint_conditions = snapshot.2;
        s.function_breakpoint_log_messages = snapshot.3;
        s.next_breakpoint_id = snapshot.4;
    }

    fn set_breakpoint(&self, spec: &BreakSpec, timeout: Duration) -> Result<String> {
        let _transaction = self.breakpoint_transaction.lock().unwrap();
        let BreakSpec {
            file,
            line,
            condition,
            log_message,
        } = spec;
        let snapshot = {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            (
                s.breakpoints.clone(),
                s.line_breakpoint_ids.clone(),
                s.breakpoint_conditions.clone(),
                s.breakpoint_log_messages.clone(),
                s.next_breakpoint_id,
            )
        };
        // DAP requires the full set of breakpoints for a source each
        // call — it doesn't merge. Accumulate in state.breakpoints
        // and replay the full list per source on each add.
        let resolved_path = resolve_breakpoint_path(file);
        let lines: Vec<u32> = {
            let (lock, _) = &*self.state;
            let mut s = lock.lock().unwrap();
            let already_present = s
                .breakpoints
                .get(&resolved_path)
                .is_some_and(|lines| lines.contains(line));
            if !already_present {
                // Reserve the public id before changing any line
                // breakpoint state. This keeps exhaustion a true
                // transaction boundary.
                let id = allocate_breakpoint_id(&mut s)?;
                s.breakpoints
                    .entry(resolved_path.clone())
                    .or_default()
                    .push(*line);
                let key = format!("{resolved_path}:{line}");
                s.line_breakpoint_ids.insert(key, id);
            }
            let lines_snapshot = s.breakpoints[&resolved_path].clone();
            let key = format!("{resolved_path}:{line}");
            if let Some(c) = condition {
                s.breakpoint_conditions.insert(key.clone(), c.clone());
            } else {
                s.breakpoint_conditions.remove(&key);
            }
            if let Some(m) = log_message {
                s.breakpoint_log_messages.insert(key, m.clone());
            } else {
                s.breakpoint_log_messages.remove(&key);
            }
            lines_snapshot
        };
        let breakpoints: Vec<Value> = {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            lines
                .iter()
                .map(|l| {
                    let mut b = serde_json::Map::new();
                    b.insert("line".into(), json!(l));
                    let key = format!("{resolved_path}:{l}");
                    if let Some(c) = s.breakpoint_conditions.get(&key) {
                        b.insert("condition".into(), Value::String(c.clone()));
                    }
                    if let Some(m) = s.breakpoint_log_messages.get(&key) {
                        b.insert("logMessage".into(), Value::String(m.clone()));
                    }
                    Value::Object(b)
                })
                .collect()
        };
        let response = match self.call_blocking(
            "setBreakpoints",
            json!({
                "source": { "path": resolved_path },
                "breakpoints": breakpoints,
                "sourceModified": false,
            }),
            timeout,
        ) {
            Ok(response) => response,
            Err(e) => {
                let (lock, _) = &*self.state;
                let mut s = lock.lock().unwrap();
                s.breakpoints = snapshot.0;
                s.line_breakpoint_ids = snapshot.1;
                s.breakpoint_conditions = snapshot.2;
                s.breakpoint_log_messages = snapshot.3;
                s.next_breakpoint_id = snapshot.4;
                return Err(e);
            }
        };
        let requested_index = lines.iter().position(|candidate| candidate == line);
        let verified =
            breakpoint_response_verified(&response, lines.len()) && requested_index.is_some();
        if !verified {
            let (lock, _) = &*self.state;
            let mut s = lock.lock().unwrap();
            s.breakpoints = snapshot.0;
            s.line_breakpoint_ids = snapshot.1;
            s.breakpoint_conditions = snapshot.2;
            s.breakpoint_log_messages = snapshot.3;
            s.next_breakpoint_id = snapshot.4;
            bail!("breakpoint at {file}:{line} was not verified by the adapter")
        }
        match (condition, log_message) {
            (Some(c), Some(m)) => Ok(format!("Logpoint set at {file}:{line} if {c}: {m}")),
            (None, Some(m)) => Ok(format!("Logpoint set at {file}:{line}: {m}")),
            (Some(c), None) => Ok(format!("Breakpoint set at {file}:{line} if {c}")),
            (None, None) => Ok(format!("Breakpoint set at {file}:{line}")),
        }
    }

    fn set_expression(&self, rest: &str, timeout: Duration) -> Result<String> {
        // `set <lhs> = <rhs>`. Split on the first `=` so LHS may contain
        // dots, indexing, etc.
        let (lhs, rhs) = match rest.find('=') {
            Some(i) => (
                rest[..i].trim().to_string(),
                rest[i + 1..].trim().to_string(),
            ),
            None => bail!("usage: dbg set <lhs> = <expr>"),
        };
        if lhs.is_empty() || rhs.is_empty() {
            bail!("usage: dbg set <lhs> = <expr>");
        }
        let frame_id = {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            s.top_frame
                .as_ref()
                .and_then(|f| f.get("id").and_then(|v| v.as_i64()))
        };
        let mut args = json!({
            "expression": lhs,
            "value": rhs,
        });
        if let Some(id) = frame_id {
            args["frameId"] = json!(id);
        }
        // Try setExpression first; fall back to scope-walking
        // setVariable only when the adapter reports the request as
        // unsupported (e.g. delve: "Not yet implemented"). Any other
        // error is a genuine evaluation failure — surface it.
        match self.call_blocking("setExpression", args, timeout) {
            Ok(resp) => Ok(resp
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()),
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                let looks_unsupported = msg.contains("not yet implemented")
                    || msg.contains("unsupported")
                    || msg.contains("not supported")
                    || msg.contains("unknown command")
                    // lldb-dap returns the opaque "request failed" with
                    // no body for setExpression — also treat that as
                    // a signal to try the setVariable path. Real
                    // evaluation failures on lldb-dap come back with
                    // a more specific message.
                    || msg.contains("request failed");
                if looks_unsupported {
                    self.set_variable_fallback(&lhs, &rhs, timeout)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// When an adapter doesn't support `setExpression`, walk the top
    /// frame's scopes, find the variable by name, and send
    /// `setVariable` against its containing scope's variablesReference.
    /// Only handles plain names (no dotted LHS) — complex lvalues
    /// should use `dbg raw` with the adapter's native syntax.
    fn set_variable_fallback(&self, lhs: &str, rhs: &str, timeout: Duration) -> Result<String> {
        let frame_id = {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            s.top_frame
                .as_ref()
                .and_then(|f| f.get("id").and_then(|v| v.as_i64()))
                .ok_or_else(|| anyhow!("no active frame"))?
        };
        let scopes = self.call_blocking("scopes", json!({"frameId": frame_id}), timeout)?;
        let arr = scopes
            .get("scopes")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for scope in arr {
            let Some(vref) = scope.get("variablesReference").and_then(|v| v.as_i64()) else {
                continue;
            };
            if vref == 0 {
                continue;
            }
            let vars =
                self.call_blocking("variables", json!({"variablesReference": vref}), timeout)?;
            let found = vars
                .get("variables")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .any(|v| v.get("name").and_then(|n| n.as_str()) == Some(lhs))
                })
                .unwrap_or(false);
            if found {
                let resp = self.call_blocking(
                    "setVariable",
                    json!({"variablesReference": vref, "name": lhs, "value": rhs}),
                    timeout,
                )?;
                return Ok(resp
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string());
            }
        }
        bail!("variable `{lhs}` not found in any frame scope")
    }

    fn evaluate(&self, expr: &str, timeout: Duration) -> Result<String> {
        let frame_id = {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            s.top_frame
                .as_ref()
                .and_then(|f| f.get("id").and_then(|v| v.as_i64()))
        };
        let mut args = json!({
            "expression": expr,
            "context": "repl",
        });
        if let Some(id) = frame_id {
            args["frameId"] = json!(id);
        }
        let resp = self.call_blocking("evaluate", args, timeout)?;
        Ok(resp
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    fn collect_locals(&self, timeout: Duration) -> Result<String> {
        let frame_id = {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            s.top_frame
                .as_ref()
                .and_then(|f| f.get("id").and_then(|v| v.as_i64()))
                .ok_or_else(|| anyhow!("locals: not paused"))?
        };
        let scopes_resp = self.call_blocking("scopes", json!({ "frameId": frame_id }), timeout)?;
        let scopes = scopes_resp
            .get("scopes")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = serde_json::Map::new();
        for scope in scopes {
            let name = scope.get("name").and_then(|v| v.as_str()).unwrap_or("");
            // Skip globals/built-ins; agents want frame-local state.
            if name.eq_ignore_ascii_case("globals") || name.eq_ignore_ascii_case("global") {
                continue;
            }
            // Skip register scopes (lldb-dap exposes "General Purpose
            // Registers", "Floating Point Registers", etc. as top-level
            // scopes). `presentationHint == "registers"` is the stable
            // way to detect them; fall back to a name heuristic for
            // adapters that don't set the hint.
            let hint = scope
                .get("presentationHint")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if hint.eq_ignore_ascii_case("registers") || name.to_lowercase().contains("register") {
                continue;
            }
            let var_ref = match scope.get("variablesReference").and_then(|v| v.as_i64()) {
                Some(v) if v != 0 => v,
                _ => continue,
            };
            let vars_resp = self.call_blocking(
                "variables",
                json!({ "variablesReference": var_ref }),
                timeout,
            )?;
            if let Some(arr) = vars_resp.get("variables").and_then(|v| v.as_array()) {
                for var in arr {
                    let n = var.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if n.is_empty() || out.contains_key(n) {
                        continue;
                    }
                    let value = var
                        .get("value")
                        .and_then(|v| v.as_str())
                        .map(|s| Value::String(s.to_string()))
                        .unwrap_or(Value::Null);
                    out.insert(n.to_string(), value);
                }
            }
        }
        Ok(Value::Object(out).to_string())
    }

    fn format_backtrace(&self) -> String {
        let (lock, _) = &*self.state;
        let s = lock.lock().unwrap();
        if s.call_frames.is_empty() {
            return "(no frames — program not paused)".to_string();
        }
        let mut out = String::new();
        for (i, f) in s.call_frames.iter().enumerate() {
            let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let path = f
                .get("source")
                .and_then(|src| src.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let line = f.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
            out.push_str(&format!("#{i} {name} at {path}:{line}\n"));
        }
        out.trim_end().to_string()
    }

    fn format_breakpoints(&self) -> String {
        let (lock, _) = &*self.state;
        let s = lock.lock().unwrap();
        let mut entries = Vec::new();
        for (file, lines) in &s.breakpoints {
            for line in lines {
                let key = format!("{file}:{line}");
                if let Some(id) = s.line_breakpoint_ids.get(&key) {
                    entries.push((*id, format!("{file}:{line}")));
                }
            }
        }
        for name in &s.function_breakpoints {
            if let Some(id) = s.function_breakpoint_ids.get(name) {
                entries.push((*id, format!("function {name}")));
            }
        }
        if entries.is_empty() {
            return "(no breakpoints set)".into();
        }
        entries.sort_by_key(|(id, _)| *id);
        let out = entries
            .into_iter()
            .map(|(id, location)| format!("{id}: {location}"))
            .collect::<Vec<_>>()
            .join("\n");
        out.trim_end().to_string()
    }

    fn delete_breakpoint(&self, id: u32, timeout: Duration) -> Result<String> {
        let _transaction = self.breakpoint_transaction.lock().unwrap();
        if id == 0 {
            bail!("delete: breakpoint ids start at 1");
        }
        enum Target {
            Line(String, u32),
            Function(String),
        }
        let mut target = None;
        {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            for (file, lines) in &s.breakpoints {
                for line in lines {
                    if s.line_breakpoint_ids.get(&format!("{file}:{line}")) == Some(&id) {
                        target = Some(Target::Line(file.clone(), *line));
                    }
                }
            }
            for name in &s.function_breakpoints {
                if s.function_breakpoint_ids.get(name) == Some(&id) {
                    target = Some(Target::Function(name.clone()));
                }
            }
        }
        let Some(target) = target else {
            bail!("delete: no breakpoint id {id}; run `dbg breaks` to list ids");
        };
        let snapshot = {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            (
                s.breakpoints.clone(),
                s.line_breakpoint_ids.clone(),
                s.function_breakpoints.clone(),
                s.function_breakpoint_ids.clone(),
                s.breakpoint_conditions.clone(),
                s.function_breakpoint_conditions.clone(),
                s.function_breakpoint_log_messages.clone(),
                s.breakpoint_log_messages.clone(),
                s.next_breakpoint_id,
            )
        };
        if let Target::Function(ref name) = target {
            let fns = {
                let (lock, _) = &*self.state;
                let mut s = lock.lock().unwrap();
                s.function_breakpoints.retain(|candidate| candidate != name);
                s.function_breakpoint_ids.remove(name);
                s.function_breakpoint_conditions.remove(name);
                s.function_breakpoint_log_messages.remove(name);
                s.function_breakpoints
                    .iter()
                    .map(|candidate| {
                        function_breakpoint_value(
                            candidate,
                            s.function_breakpoint_conditions
                                .get(candidate)
                                .map(String::as_str),
                            s.function_breakpoint_log_messages
                                .get(candidate)
                                .map(String::as_str),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let response = self.call_blocking(
                "setFunctionBreakpoints",
                json!({"breakpoints": fns}),
                timeout,
            );
            if response.as_ref().is_err()
                || !response
                    .as_ref()
                    .ok()
                    .is_some_and(|body| function_breakpoints_verified(body, fns.len()))
            {
                let (lock, _) = &*self.state;
                let mut s = lock.lock().unwrap();
                s.breakpoints = snapshot.0;
                s.line_breakpoint_ids = snapshot.1;
                s.function_breakpoints = snapshot.2;
                s.function_breakpoint_ids = snapshot.3;
                s.breakpoint_conditions = snapshot.4;
                s.function_breakpoint_conditions = snapshot.5;
                s.function_breakpoint_log_messages = snapshot.6;
                s.breakpoint_log_messages = snapshot.7;
                s.next_breakpoint_id = snapshot.8;
                if let Err(error) = response {
                    return Err(error);
                }
                bail!("function breakpoint `{name}` was not verified by the adapter");
            }
            return Ok(format!("Breakpoint {id} cleared (function {name})"));
        }
        let Target::Line(file, line) = target else {
            unreachable!()
        };
        let source_path = file.clone();
        let remaining: Vec<u32> = {
            let (lock, _) = &*self.state;
            let mut s = lock.lock().unwrap();
            let Some(lines) = s.breakpoints.get_mut(&file) else {
                bail!("delete: no breakpoint id {id}; run `dbg breaks` to list ids");
            };
            lines.retain(|l| *l != line);
            let remaining = lines.clone();
            if remaining.is_empty() {
                s.breakpoints.remove(&file);
            }
            let key = format!("{file}:{line}");
            s.breakpoint_conditions.remove(&key);
            s.breakpoint_log_messages.remove(&key);
            remaining
        };
        let breakpoints: Vec<Value> = {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            remaining
                .iter()
                .map(|l| {
                    let mut b = serde_json::Map::new();
                    b.insert("line".into(), json!(l));
                    let key = format!("{file}:{l}");
                    if let Some(c) = s.breakpoint_conditions.get(&key) {
                        b.insert("condition".into(), Value::String(c.clone()));
                    }
                    if let Some(m) = s.breakpoint_log_messages.get(&key) {
                        b.insert("logMessage".into(), Value::String(m.clone()));
                    }
                    Value::Object(b)
                })
                .collect()
        };
        let response = match self.call_blocking(
            "setBreakpoints",
            json!({
                "source": { "path": source_path },
                "breakpoints": breakpoints,
                "sourceModified": false,
            }),
            timeout,
        ) {
            Ok(response) => response,
            Err(error) => {
                let (lock, _) = &*self.state;
                let mut s = lock.lock().unwrap();
                s.breakpoints = snapshot.0;
                s.line_breakpoint_ids = snapshot.1;
                s.function_breakpoints = snapshot.2;
                s.function_breakpoint_ids = snapshot.3;
                s.breakpoint_conditions = snapshot.4;
                s.function_breakpoint_conditions = snapshot.5;
                s.function_breakpoint_log_messages = snapshot.6;
                s.breakpoint_log_messages = snapshot.7;
                s.next_breakpoint_id = snapshot.8;
                return Err(error);
            }
        };
        if !breakpoint_response_verified(&response, remaining.len()) {
            let (lock, _) = &*self.state;
            let mut s = lock.lock().unwrap();
            s.breakpoints = snapshot.0;
            s.line_breakpoint_ids = snapshot.1;
            s.function_breakpoints = snapshot.2;
            s.function_breakpoint_ids = snapshot.3;
            s.breakpoint_conditions = snapshot.4;
            s.function_breakpoint_conditions = snapshot.5;
            s.function_breakpoint_log_messages = snapshot.6;
            s.breakpoint_log_messages = snapshot.7;
            s.next_breakpoint_id = snapshot.8;
            bail!("breakpoint {id} was not verified by the adapter")
        }
        let (lock, _) = &*self.state;
        lock.lock()
            .unwrap()
            .line_breakpoint_ids
            .remove(&format!("{file}:{line}"));
        Ok(format!("Breakpoint {id} cleared ({line})"))
    }
}

impl DebuggerIo for DapTransport {
    fn send_and_wait(&self, cmd: &str, timeout: Duration) -> Result<String> {
        self.run_command(cmd, timeout)
    }
    fn drain_pending(&self) -> Option<String> {
        None
    }
    fn wait_for_prompt(&self, timeout: Duration) -> Result<String> {
        // If the backend's launch config specified stopOnEntry, the
        // first `stopped` event arrives soon after configurationDone.
        // For backends that don't, we return immediately and the
        // first user-issued execution command does the waiting.
        let deadline = Instant::now() + timeout;
        let (lock, cvar) = &*self.state;
        let mut guard = lock.lock().unwrap();
        while guard.alive && !guard.paused && !guard.terminated {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // Not-stopped yet: programs without stopOnEntry are
                // already running — that's fine, caller will drive.
                return Ok(String::new());
            }
            let r = cvar
                .wait_timeout(guard, Duration::from_millis(250))
                .unwrap();
            guard = r.0;
            if r.1.timed_out() && guard.call_frames.is_empty() {
                // No stop yet; treat "running without stopOnEntry" as
                // an acceptable state and return.
                return Ok(String::new());
            }
        }
        Ok(String::new())
    }
    fn log(&self) -> LogHandle {
        self.log.clone()
    }
    fn child_pid(&self) -> Pid {
        self.child_pid
    }
    fn is_alive(&self) -> bool {
        let (lock, _) = &*self.state;
        lock.lock().unwrap().alive
    }
    fn quit(&self, _quit_cmd: &str) {
        // Capture descendants before disconnecting or killing the adapter.
        // This is the fallback for adapters which launch the debuggee but
        // never send a DAP `process` event, and it also covers a replacement
        // debuggee created by `restart` before its event is dispatched.
        let adapter_descendants = capture_owned_descendants(self.child_pid.as_raw() as u32);
        // 1. Politely ask the adapter to disconnect. Spec-compliant
        //    adapters (delve, lldb-dap, debugpy) honour
        //    `terminateDebuggee=true` by killing the inferior here.
        //    netcoredbg accepts and ACKs the request but does NOT
        //    take the dotnet host down: its TerminateProcess() only
        //    calls ICorDebug::Terminate, which can be refused by the
        //    CLR (native frame on top, hung finalizer, etc.) and has
        //    no OS-level fallback. The SIGKILL below covers that.
        //    See vscodeprotocol.cpp + manageddebugger.cpp upstream.
        let _ = self.call_blocking(
            "disconnect",
            json!({ "terminateDebuggee": !self.is_attach }),
            Duration::from_millis(1500),
        );

        // 2. Snapshot the launched pid (set by the DAP `process` event)
        //    *before* tearing down state — we may need to SIGKILL it
        //    later if the disconnect didn't take.
        let launched_pid = {
            let (lock, _) = &*self.state;
            lock.lock().unwrap().launched_pid
        };

        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.driver_tx.send(DriverCmd::Shutdown);
        // Wake anybody parked in exec() waiting on pending_hit —
        // without this, kill_daemon blocks until their timeout fires.
        {
            let (lock, cvar) = &*self.state;
            let mut s = lock.lock().unwrap();
            s.alive = false;
            s.terminated = true;
            cvar.notify_all();
        }
        let _ = nix::sys::signal::kill(self.child_pid, nix::sys::signal::Signal::SIGTERM);
        std::thread::sleep(Duration::from_millis(500));
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        kill_captured_processes(adapter_descendants);
        if let Some(h) = self.driver.lock().unwrap().take() {
            let _ = h.join();
        }

        // 3. Belt-and-braces cleanup: SIGKILL the tracked debuggee
        //    pid if it's still alive. Empirically, netcoredbg ACKs
        //    `disconnect terminateDebuggee=true` but does not take
        //    down the dotnet host — the host gets reparented to
        //    systemd-user and keeps running with its socket bound.
        //    We need this fallback for any DAP adapter that doesn't
        //    follow through. Skip in attach mode — the user explicitly
        //    opted into not owning the lifecycle.
        if !self.is_attach {
            let identity = {
                let (lock, _) = &*self.state;
                lock.lock().unwrap().launched_identity
            };
            if let (Some(pid), Some(identity)) = (launched_pid, identity) {
                kill_owned_process_tree(pid, identity);
            }
        }
    }
    fn pending_hit(&self) -> Option<HitEvent> {
        let (lock, _) = &*self.state;
        lock.lock().unwrap().pending_hit.take()
    }
    fn dispatch_structured(
        &self,
        req: &crate::backend::canonical::CanonicalReq,
        timeout: Duration,
    ) -> Option<Result<String>> {
        use crate::backend::canonical::{BreakLoc, CanonicalReq};
        match req {
            CanonicalReq::Break { loc, cond, log } => match loc {
                BreakLoc::FileLine { file, line } => {
                    let spec = BreakSpec {
                        file: file.clone(),
                        line: *line,
                        condition: cond.clone(),
                        log_message: log.clone(),
                    };
                    Some(self.set_breakpoint(&spec, timeout))
                }
                BreakLoc::Fqn(name) => Some(self.set_function_breakpoint(
                    name,
                    cond.as_deref(),
                    log.as_deref(),
                    timeout,
                )),
                BreakLoc::ModuleMethod { .. } => None,
            },
        }
    }
}

impl Drop for DapTransport {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.driver_tx.send(DriverCmd::Shutdown);
        // Capture adapter descendants before killing the adapter. A launch
        // can have started its debuggee while the handshake is still in
        // progress, before the DAP `process` event has been dispatched.
        let adapter_descendants = capture_owned_descendants(self.child_pid.as_raw() as u32);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        kill_captured_processes(adapter_descendants);
        if !self.is_attach {
            let launched = {
                let (lock, _) = &*self.state;
                let state = lock.lock().unwrap();
                state.launched_pid.zip(state.launched_identity)
            };
            if let Some((pid, identity)) = launched {
                kill_owned_process_tree(pid, identity);
            }
        }
    }
}

/// Read from both stdout and stderr concurrently until one of them
/// produces a line containing `marker`. Returns the scraped
/// host:port. After returning, the caller takes ownership of both
/// streams (we hand them back via an extra return) so it can drain
/// them in background — full pipe buffers will otherwise SIGPIPE the
/// adapter once it starts chattering under load.
type ScrapeResult = (
    String,
    Option<std::process::ChildStdout>,
    Option<ChildStderr>,
);

fn scrape_listen_addr_either(
    stdout: std::process::ChildStdout,
    stderr: ChildStderr,
    marker: &str,
    timeout: Duration,
) -> Result<ScrapeResult> {
    use std::os::fd::AsRawFd;
    let fd_o = stdout.as_raw_fd();
    let fd_e = stderr.as_raw_fd();
    nix::fcntl::fcntl(
        fd_o,
        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
    )?;
    nix::fcntl::fcntl(
        fd_e,
        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
    )?;

    let mut out_buf: Vec<u8> = Vec::with_capacity(512);
    let mut err_buf: Vec<u8> = Vec::with_capacity(512);
    let mut tmp = [0u8; 256];
    let deadline = Instant::now() + timeout;
    let mut so = stdout;
    let mut se = stderr;
    let mut so_open = true;
    let mut se_open = true;
    loop {
        if Instant::now() >= deadline {
            bail!("timed out scraping for `{marker}`");
        }
        // Read from stdout.
        if so_open {
            match so.read(&mut tmp) {
                Ok(0) => so_open = false,
                Ok(n) => {
                    out_buf.extend_from_slice(&tmp[..n]);
                    if let Some(addr) = scan_for_marker(&mut out_buf, marker) {
                        // Drain the rest in background so the pipe
                        // doesn't fill and block the adapter.
                        // stderr stays a diagnostic-only stream
                        // (adapters put their `--log` trace there) —
                        // discard, don't clutter the event log.
                        // stdout carries program output for adapters
                        // that inherit (delve) so route it to the
                        // caller via a log handle they pass later.
                        return Ok((addr, Some(so), Some(se)));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => so_open = false,
            }
        }
        // Read from stderr.
        if se_open {
            match se.read(&mut tmp) {
                Ok(0) => se_open = false,
                Ok(n) => {
                    err_buf.extend_from_slice(&tmp[..n]);
                    if let Some(addr) = scan_for_marker(&mut err_buf, marker) {
                        // stderr stays a diagnostic-only stream
                        // (adapters put their `--log` trace there) —
                        // discard, don't clutter the event log.
                        // stdout carries program output for adapters
                        // that inherit (delve) so route it to the
                        // caller via a log handle they pass later.
                        return Ok((addr, Some(so), Some(se)));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => se_open = false,
            }
        }
        if !so_open && !se_open {
            bail!("adapter closed both stdout and stderr before announcing");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn scan_for_marker(buf: &mut Vec<u8>, marker: &str) -> Option<String> {
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let line = String::from_utf8_lossy(&buf[..pos]).to_string();
        buf.drain(..=pos);
        if line.contains(marker) {
            if let Some(addr) = extract_host_port(&line) {
                return Some(addr);
            }
        }
    }
    None
}

fn spawn_drain<R: std::io::Read + Send + 'static>(mut r: R, log: Option<LogHandle>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        loop {
            match r.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    // When `log` is Some, forward as program stdout —
                    // some adapters (notably delve) inherit stdio to
                    // the target process, so anything the program
                    // writes to stdout arrives here rather than as a
                    // DAP `output` event. Forwarding preserves that
                    // output for `dbg events --kind=stdout`.
                    if let Some(ref log) = log {
                        log.push(EventKind::Stdout, buf[..n].to_vec());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => return,
            }
        }
    });
}

fn extract_host_port(line: &str) -> Option<String> {
    // Adapters vary on surrounding decoration:
    //   delve:    "DAP server listening at: 127.0.0.1:34407"
    //   lldb-dap: "Listening for: connection://[127.0.0.1]:38191"
    //   debugpy:  "… 127.0.0.1:5678"
    // Strip scheme prefix (`scheme://`), surrounding brackets, and
    // trailing punctuation; pull `<host>:<port>` out of whatever
    // token contains it.
    for tok in line.split_whitespace() {
        // Strip a leading `scheme://`.
        let mut t = tok;
        if let Some(idx) = t.find("://") {
            t = &t[idx + 3..];
        }
        // Strip square brackets around an IP literal: `[127.0.0.1]:X`.
        if t.starts_with('[') {
            if let Some(close) = t.find(']') {
                let host = &t[1..close];
                let after = &t[close + 1..];
                if let Some(port) = after.strip_prefix(':') {
                    let digits: String = port.chars().take_while(|c| c.is_ascii_digit()).collect();
                    if !digits.is_empty() && digits.parse::<u16>().is_ok() {
                        return Some(format!("{host}:{digits}"));
                    }
                }
                continue;
            }
        }
        // Plain host:port.
        if let Some(colon) = t.rfind(':') {
            let (host, port) = (&t[..colon], &t[colon + 1..]);
            let digits: String = port.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !host.is_empty() && !digits.is_empty() && digits.parse::<u16>().is_ok() {
                return Some(format!("{host}:{digits}"));
            }
        }
    }
    None
}

fn connect_with_retry(addr: &str, timeout: Duration) -> Result<TcpStream> {
    connect_with_retry_owned(addr, timeout, false, 0)
}

fn connect_with_retry_owned(
    addr: &str,
    timeout: Duration,
    require_owner: bool,
    adapter_pid: u32,
) -> Result<TcpStream> {
    let deadline = Instant::now() + timeout;
    loop {
        if require_owner && !listener_owned_by(addr, adapter_pid) {
            if Instant::now() >= deadline {
                bail!("listener at {addr} is not owned by spawned adapter PID {adapter_pid}");
            }
            std::thread::sleep(Duration::from_millis(25));
            continue;
        }
        match TcpStream::connect(addr) {
            Ok(s) => return Ok(s),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(target_os = "linux")]
fn listener_owned_by(addr: &str, adapter_pid: u32) -> bool {
    let Some(port) = addr
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
    else {
        return false;
    };
    let wanted = format!("{port:04X}");
    let Some(inode) = ["/proc/net/tcp", "/proc/net/tcp6"].iter().find_map(|path| {
        let text = std::fs::read_to_string(path).ok()?;
        text.lines().skip(1).find_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            (fields.get(1)?.rsplit_once(':')?.1 == wanted && *fields.get(3)? == "0A")
                .then(|| fields.get(9)?.parse::<u64>().ok())
                .flatten()
        })
    }) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(format!("/proc/{adapter_pid}/fd")) else {
        return false;
    };
    entries.flatten().any(|entry| {
        std::fs::read_link(entry.path())
            .is_ok_and(|link| link.to_string_lossy() == format!("socket:[{inode}]"))
    })
}

#[cfg(not(target_os = "linux"))]
fn listener_owned_by(_addr: &str, _adapter_pid: u32) -> bool {
    false
}

/// Driver: reads DAP frames from the TCP stream, writes outbound
/// requests with monotonic seq, dispatches responses + events.
fn driver_loop(
    mut stream: TcpStream,
    rx: Receiver<DriverCmd>,
    state: Arc<(Mutex<State>, Condvar)>,
    log: LogHandle,
    shutdown: Arc<AtomicBool>,
) {
    // Inbound buffer holds bytes that arrived but didn't yet form a
    // complete Content-Length-framed message.
    let mut decoder = DapFrameDecoder::new();
    let mut next_seq: i64 = 1;
    let mut pending: HashMap<i64, (String, Sender<Result<Value, String>>)> = HashMap::new();
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Drain inbound bytes.
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => {
                    drain_pending(&mut pending);
                    mark_dead(&state);
                    return;
                }
                Ok(n) => decoder.inbox.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    drain_pending(&mut pending);
                    mark_dead(&state);
                    return;
                }
            }
        }

        // Parse as many complete messages as are in the inbox.
        loop {
            match decoder.next_frame() {
                Ok(Some(bytes)) => {
                    if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                        dispatch_incoming(
                            v,
                            &mut pending,
                            &state,
                            &log,
                            &mut stream,
                            &mut next_seq,
                        );
                    }
                }
                Ok(None) => break,
                Err(_) => continue,
            }
        }

        // Drain outbound command channel.
        loop {
            match rx.try_recv() {
                Ok(DriverCmd::Shutdown) => {
                    drain_pending(&mut pending);
                    mark_dead(&state);
                    return;
                }
                Ok(DriverCmd::Call {
                    command,
                    arguments,
                    resp,
                    arm_action,
                }) => {
                    if arm_action {
                        let (lock, _) = &*state;
                        let mut s = lock.lock().unwrap();
                        s.armed_action_generation = s.action_generation;
                    }
                    let seq = next_seq;
                    next_seq += 1;
                    let frame = json!({
                        "seq": seq,
                        "type": "request",
                        "command": command,
                        "arguments": arguments,
                    });
                    if let Err(e) = write_frame(&mut stream, &frame) {
                        let _ = resp.send(Err(format!("write failed: {e}")));
                        continue;
                    }
                    pending.insert(seq, (command, resp));
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    drain_pending(&mut pending);
                    mark_dead(&state);
                    return;
                }
            }
        }

        std::thread::sleep(Duration::from_millis(5));
    }
}

/// When the driver thread exits, any `call_blocking` senders still
/// parked in `pending` would time out with the generic "timeout" error.
/// Drain them with a clearer "driver dead" message so callers can
/// distinguish a dead adapter from a slow one.
fn drain_pending(pending: &mut HashMap<i64, (String, Sender<Result<Value, String>>)>) {
    for (_seq, (_cmd, tx)) in pending.drain() {
        let _ = tx.send(Err("DAP driver thread exited".into()));
    }
}

fn mark_dead(state: &Arc<(Mutex<State>, Condvar)>) {
    let (lock, cvar) = &**state;
    let mut s = lock.lock().unwrap();
    s.alive = false;
    cvar.notify_all();
}

struct DapFrameDecoder {
    inbox: Vec<u8>,
    /// An oversized frame has no safe, bounded body buffer. Once its
    /// header is rejected, scan for the next header marker instead of
    /// trusting its declared length, which could be `usize::MAX`.
    resync_oversized: bool,
}

impl DapFrameDecoder {
    fn new() -> Self {
        Self {
            inbox: Vec::with_capacity(16 * 1024),
            resync_oversized: false,
        }
    }

    fn next_frame(&mut self) -> Result<Option<Vec<u8>>, &'static str> {
        // Do not discard an attacker-controlled number of bytes. A hostile
        // oversized length can be usize::MAX, and waiting for that declared
        // boundary would consume every later valid frame. Resynchronize at
        // the next plausible DAP header while retaining only a bounded
        // suffix when the marker has not arrived yet.
        if self.resync_oversized {
            const CONTENT_LENGTH: &[u8] = b"Content-Length:";
            if let Some(pos) = self
                .inbox
                .windows(CONTENT_LENGTH.len())
                .position(|window| window == CONTENT_LENGTH)
            {
                self.inbox.drain(..pos);
                self.resync_oversized = false;
            } else {
                let keep = CONTENT_LENGTH.len().saturating_sub(1);
                let discard = self.inbox.len().saturating_sub(keep);
                if discard > 0 {
                    self.inbox.drain(..discard);
                }
                return Ok(None);
            }
        }

        let Some(hdr_end) = self.inbox.windows(4).position(|w| w == b"\r\n\r\n") else {
            if self.inbox.len() > MAX_DAP_HEADER_BYTES {
                let keep = self
                    .inbox
                    .windows(b"Content-Length:".len())
                    .rposition(|w| w == b"Content-Length:")
                    .unwrap_or(self.inbox.len().saturating_sub(b"Content-Length:".len()));
                self.inbox.drain(..keep);
                return Err("DAP header exceeded maximum size");
            }
            return Ok(None);
        };
        if hdr_end > MAX_DAP_HEADER_BYTES {
            self.inbox.drain(..hdr_end + 4);
            return Err("DAP header exceeded maximum size");
        }
        let header_s = match std::str::from_utf8(&self.inbox[..hdr_end]) {
            Ok(s) => s,
            Err(_) => {
                self.inbox.drain(..hdr_end + 4);
                return Err("DAP header is not valid UTF-8");
            }
        };
        let mut content_length = None;
        for line in header_s.split("\r\n") {
            if let Some(rest) = line.strip_prefix("Content-Length:") {
                if content_length.is_some() {
                    self.inbox.drain(..hdr_end + 4);
                    return Err("duplicate DAP Content-Length header");
                }
                let parsed = match rest.trim().parse::<usize>() {
                    Ok(length) => length,
                    Err(_) => {
                        self.inbox.drain(..hdr_end + 4);
                        return Err("invalid DAP Content-Length");
                    }
                };
                content_length = Some(parsed);
            }
        }
        let Some(content_length) = content_length else {
            self.inbox.drain(..hdr_end + 4);
            return Err("missing DAP Content-Length header");
        };
        if content_length > MAX_DAP_FRAME_BYTES {
            self.inbox.drain(..hdr_end + 4);
            self.resync_oversized = true;
            return Err("DAP frame exceeds maximum size");
        }
        let total = hdr_end + 4 + content_length;
        if self.inbox.len() < total {
            return Ok(None);
        }
        let body = self.inbox[hdr_end + 4..total].to_vec();
        self.inbox.drain(..total);
        Ok(Some(body))
    }
}

fn take_frame(inbox: &mut Vec<u8>) -> Option<Vec<u8>> {
    let mut decoder = DapFrameDecoder {
        inbox: std::mem::take(inbox),
        resync_oversized: false,
    };
    let frame = decoder.next_frame().ok().flatten();
    *inbox = decoder.inbox;
    frame
}

fn write_frame(stream: &mut TcpStream, frame: &Value) -> std::io::Result<()> {
    let body = frame.to_string();
    let bytes = body.as_bytes();
    let header = format!("Content-Length: {}\r\n\r\n", bytes.len());
    // With non-blocking, we may get WouldBlock mid-write. Loop.
    let mut to_write: Vec<u8> = Vec::with_capacity(header.len() + bytes.len());
    to_write.extend_from_slice(header.as_bytes());
    to_write.extend_from_slice(bytes);
    let mut written = 0;
    let deadline = Instant::now() + DAP_WRITE_TIMEOUT;
    while written < to_write.len() {
        match stream.write(&to_write[written..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "write returned 0",
                ));
            }
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "DAP write remained blocked past its deadline",
                    ));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn dispatch_incoming(
    v: Value,
    pending: &mut HashMap<i64, (String, Sender<Result<Value, String>>)>,
    state: &Arc<(Mutex<State>, Condvar)>,
    log: &LogHandle,
    stream: &mut TcpStream,
    next_seq: &mut i64,
) {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "response" => {
            let req_seq = v.get("request_seq").and_then(|v| v.as_i64()).unwrap_or(0);
            if let Some((_cmd, tx)) = pending.remove(&req_seq) {
                if v.get("success").and_then(|s| s.as_bool()) == Some(true) {
                    let body = v.get("body").cloned().unwrap_or(Value::Null);
                    let _ = tx.send(Ok(body));
                } else {
                    let msg = v
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("request failed")
                        .to_string();
                    let _ = tx.send(Err(msg));
                }
            }
        }
        "event" => {
            let name = v.get("event").and_then(|e| e.as_str()).unwrap_or("");
            let body = v.get("body").cloned().unwrap_or(Value::Null);
            match name {
                "initialized" => {
                    let (lock, cvar) = &**state;
                    let mut s = lock.lock().unwrap();
                    s.initialized = true;
                    cvar.notify_all();
                }
                "stopped" => {
                    let thread_id = body.get("threadId").and_then(|v| v.as_i64());
                    let stop_generation = {
                        let (lock, _) = &**state;
                        let mut s = lock.lock().unwrap();
                        s.paused = true;
                        s.current_thread = thread_id;
                        s.stop_generation = s.stop_generation.wrapping_add(1);
                        // Publish ownership of the stop before the
                        // asynchronous stackTrace helper returns. A late
                        // stop must remain available to the next action,
                        // but its delayed helper must not create a hit for
                        // a later continue.
                        s.pending_hit = None;
                        s.stop_generation
                    };
                    if let Some(tid) = thread_id {
                        {
                            let (lock, _) = &**state;
                            let mut s = lock.lock().unwrap();
                            s.pending_action_generation = s.armed_action_generation;
                            s.pending_is_unscoped = false;
                        }
                        // Fire an out-of-band stackTrace request so the
                        // handler can build a structured HitEvent. We
                        // bypass the call_blocking path (driver can't
                        // block on itself) and write directly.
                        let seq = *next_seq;
                        *next_seq += 1;
                        let frame = json!({
                            "seq": seq,
                            "type": "request",
                            "command": "stackTrace",
                            "arguments": { "threadId": tid, "startFrame": 0, "levels": 20 },
                        });
                        let (tx, rx) = mpsc::channel::<Result<Value, String>>();
                        pending.insert(seq, ("stackTrace".into(), tx));
                        let _ = write_frame(stream, &frame);
                        // Defer the response-waiting onto a short-lived
                        // helper thread so we don't block the driver.
                        let state2 = state.clone();
                        std::thread::spawn(move || {
                            if let Ok(Ok(body)) = rx.recv_timeout(Duration::from_secs(5)) {
                                handle_stack_response(body, &state2, stop_generation);
                            }
                        });
                    } else {
                        let (lock, cvar) = &**state;
                        let mut s = lock.lock().unwrap();
                        s.pending_hit = Some(HitEvent::default());
                        s.pending_is_unscoped = true;
                        s.pending_action_generation = s.armed_action_generation;
                        cvar.notify_all();
                    }
                }
                "continued" => {
                    let (lock, _) = &**state;
                    let mut s = lock.lock().unwrap();
                    s.paused = false;
                    s.call_frames.clear();
                    s.top_frame = None;
                }
                "output" => {
                    // DAP: {category: "stdout"|"stderr"|"console"|"important"|..., output: "..."}
                    // Spec default when category is absent is "console".
                    // Adapters vary wildly: delve marks program
                    // output as "stdout", lldb-dap as "console" with
                    // an "output" group, debugpy uses "stdout". We
                    // treat stdout/stderr/console all as program
                    // output and route to EventKind::Stdout; truly
                    // adapter-internal messages go in "important" or
                    // "telemetry", which we drop.
                    let category = body
                        .get("category")
                        .and_then(|v| v.as_str())
                        .unwrap_or("console");
                    let text = body
                        .get("output")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if matches!(category, "stdout" | "stderr" | "console") {
                        log.push(EventKind::Stdout, text.into_bytes());
                    }
                }
                "terminated" | "exited" => {
                    let (lock, cvar) = &**state;
                    let mut s = lock.lock().unwrap();
                    s.terminated = true;
                    s.alive = false;
                    cvar.notify_all();
                }
                "process" => {
                    // DAP body: { name, systemProcessId, isLocalProcess,
                    // startMethod, ... }. Recording the OS pid lets the
                    // shutdown path SIGKILL the debuggee if a graceful
                    // disconnect+terminate round-trip fails to take it
                    // down (observed with netcoredbg + dotnet host).
                    if let Some(spid) = extract_system_process_id(&body) {
                        let (lock, _) = &**state;
                        let mut s = lock.lock().unwrap();
                        s.launched_pid = Some(spid);
                        s.launched_identity = process_identity(spid);
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn handle_stack_response(body: Value, state: &Arc<(Mutex<State>, Condvar)>, stop_generation: u64) {
    let frames = body
        .get("stackFrames")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let top = frames.first().cloned();
    let hit = top.as_ref().map(|f| {
        let name = f
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let path = f
            .get("source")
            .and_then(|s| s.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let line = f.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        HitEvent {
            location_key: if path.is_empty() {
                format!("?:{line}")
            } else {
                format!("{path}:{line}")
            },
            thread: None,
            frame_symbol: Some(name),
            file: if path.is_empty() { None } else { Some(path) },
            line: Some(line),
        }
    });
    let (lock, cvar) = &**state;
    let mut s = lock.lock().unwrap();
    // A stack helper owns exactly the stop event which created it. If the
    // daemon already consumed that stop, or another stop arrived, this
    // delayed response must not create a hit for a later continue.
    if s.stop_generation != stop_generation
        || s.pending_action_generation != s.action_generation
        || s.enriched_stop_generation == stop_generation
    {
        return;
    }
    s.call_frames = frames;
    s.top_frame = top;
    if let Some(hit) = hit {
        s.pending_hit = Some(hit);
    } else {
        // The adapter answered the enrichment request but supplied no
        // frames. Publish a structured placeholder only after that answer;
        // this keeps the wait ordering deterministic without inventing a
        // hit before enrichment completes.
        s.pending_hit = Some(HitEvent::default());
    }
    s.pending_action_generation = s.action_generation;
    s.enriched_stop_generation = stop_generation;
    cvar.notify_all();
}

#[derive(Debug)]
struct BreakSpec {
    file: String,
    line: u32,
    condition: Option<String>,
    log_message: Option<String>,
}

fn function_breakpoints_verified(response: &Value, expected: usize) -> bool {
    let Some(items) = response.get("breakpoints").and_then(Value::as_array) else {
        return expected == 0;
    };
    items.len() == expected
        && items
            .iter()
            .all(|bp| bp.get("verified").and_then(Value::as_bool) == Some(true))
}

fn breakpoint_response_verified(response: &Value, expected: usize) -> bool {
    let Some(items) = response.get("breakpoints").and_then(Value::as_array) else {
        return expected == 0;
    };
    items.len() == expected
        && items
            .iter()
            .all(|bp| bp.get("verified").and_then(Value::as_bool) == Some(true))
}

fn function_breakpoint_value(
    name: &str,
    condition: Option<&str>,
    log_message: Option<&str>,
) -> Value {
    let mut breakpoint = serde_json::Map::new();
    breakpoint.insert("name".into(), Value::String(name.to_string()));
    if let Some(condition) = condition {
        breakpoint.insert("condition".into(), Value::String(condition.to_string()));
    }
    if let Some(log_message) = log_message {
        breakpoint.insert("logMessage".into(), Value::String(log_message.to_string()));
    }
    Value::Object(breakpoint)
}

fn allocate_breakpoint_id(state: &mut State) -> Result<u32> {
    // Zero is reserved as the post-u32::MAX exhausted sentinel. Keep this
    // sentinel separate from the maps so deleting the maximum ID cannot
    // make it available for reuse.
    if state.next_breakpoint_id == 0 {
        bail!("breakpoint id space exhausted");
    }

    let mut candidate = state.next_breakpoint_id;
    loop {
        let already_used = state
            .line_breakpoint_ids
            .values()
            .any(|id| *id == candidate)
            || state
                .function_breakpoint_ids
                .values()
                .any(|id| *id == candidate);
        if !already_used {
            state.next_breakpoint_id = candidate.checked_add(1).unwrap_or(0);
            return Ok(candidate);
        }
        if candidate == u32::MAX {
            // Do not change the cursor on failure. The caller can therefore
            // retry without changing committed allocation state.
            bail!("breakpoint id space exhausted");
        }
        candidate += 1;
    }
}

fn parse_break(cmd: &str) -> Option<BreakSpec> {
    // Accepts `break file:line` or `b file:line`, optionally followed by
    // ` if <expr>` and/or ` log <template>`. Peel the log suffix first
    // because log templates can contain ` if ` literally; conditions
    // cannot embed ` log ` without confusing the parser, so that
    // trade-off matches DAP's own field separation.
    let rest = cmd
        .strip_prefix("break ")
        .or_else(|| cmd.strip_prefix("b "))?;
    let (head, log_message) = match rest.find(" log ") {
        Some(i) => (&rest[..i], Some(rest[i + 5..].trim().to_string())),
        None => (rest, None),
    };
    let (locspec, condition) = match head.find(" if ") {
        Some(i) => (&head[..i], Some(head[i + 4..].trim().to_string())),
        None => (head, None),
    };
    let (file, line_s) = locspec.rsplit_once(':')?;
    let line: u32 = line_s.trim().parse().ok()?;
    Some(BreakSpec {
        file: file.trim().to_string(),
        line,
        condition,
        log_message,
    })
}

fn resolve_breakpoint_path(file: &str) -> String {
    let path = std::path::Path::new(file);
    if path.is_absolute() {
        return file.to_string();
    }
    resolve_breakpoint_path_from(std::env::current_dir().ok().as_deref(), file)
}

fn resolve_breakpoint_path_from(cwd: Option<&std::path::Path>, file: &str) -> String {
    let path = std::path::Path::new(file);
    if path.is_absolute() {
        return file.to_string();
    }
    cwd.map(|base| base.join(path).display().to_string())
        .unwrap_or_else(|| file.to_string())
}

/// Decide whether dbg can attach to `pid` before we ask the adapter to
/// try. On Linux, Yama-restricted ptrace (`/proc/sys/kernel/yama/ptrace_scope`
/// >= 1) only permits a process's parent to ptrace it. dbg's daemon
/// is never the parent of an unrelated PID, so the attach silently
/// hangs inside the adapter and surfaces 10s later as a `configurationDone`
/// timeout. Reading two procfs files lets us turn that into an
/// actionable error before we even spawn the adapter.
///
/// We deliberately don't fail when the proc files aren't readable —
/// non-Linux hosts and weird mount setups should fall through to the
/// existing timeout path rather than break valid attach flows.
#[cfg(target_os = "linux")]
fn preflight_attach(pid: u32) -> Result<()> {
    let pid_alive = std::path::Path::new(&format!("/proc/{pid}")).exists();
    let scope: i32 = std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    preflight_attach_decide(pid, pid_alive, scope)
}

#[cfg(not(target_os = "linux"))]
fn preflight_attach(_pid: u32) -> Result<()> {
    Ok(())
}

/// Extract `body.systemProcessId` from a DAP `process` event body.
/// Returns None if the field is absent, non-numeric, or out of range.
/// Some adapters emit it as JSON number, some as string — both are
/// accepted (debugpy uses string for large pids on 32-bit platforms).
fn extract_system_process_id(body: &Value) -> Option<u32> {
    let raw = body.get("systemProcessId")?;
    if let Some(n) = raw.as_u64() {
        return u32::try_from(n).ok();
    }
    if let Some(s) = raw.as_str() {
        return s.parse().ok();
    }
    None
}

/// Pure decision logic for [`preflight_attach`]. Split out so the
/// regression tests don't need to mutate /proc.
fn preflight_attach_decide(pid: u32, pid_alive: bool, ptrace_scope: i32) -> Result<()> {
    if !pid_alive {
        bail!(
            "no process with pid {pid} (or /proc not mounted). Verify the PID is correct and still running."
        );
    }
    if ptrace_scope == 0 {
        return Ok(());
    }
    bail!(
        "cannot attach to pid {pid}: kernel.yama.ptrace_scope = {ptrace_scope} restricts ptrace to the target's parent process, and dbg's daemon is not the parent. \
Fix one of:\n  \
  - relaunch the target through `dbg start <type> <target> --args ...` (preferred — daemon becomes parent, no kernel changes)\n  \
  - `sudo sysctl kernel.yama.ptrace_scope=0` for the duration of this session\n  \
  - have the target call prctl(PR_SET_PTRACER, <dbg-daemon-pid>) before you attach"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_transport(
        responses: Vec<std::result::Result<Value, String>>,
    ) -> (DapTransport, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::AtomicUsize;

        let state = Arc::new((Mutex::new(State::new()), Condvar::new()));
        let (driver_tx, driver_rx) = mpsc::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_driver = calls.clone();
        let driver = std::thread::spawn(move || {
            let mut responses = responses.into_iter();
            while let Ok(command) = driver_rx.recv() {
                match command {
                    DriverCmd::Call { resp, .. } => {
                        calls_for_driver.fetch_add(1, Ordering::Relaxed);
                        let response = responses
                            .next()
                            .unwrap_or_else(|| Ok(json!({ "breakpoints": [] })));
                        let _ = resp.send(response);
                    }
                    DriverCmd::Shutdown => break,
                }
            }
        });
        let transport = DapTransport {
            child_pid: Pid::from_raw(-1),
            child: Mutex::new(None),
            driver_tx,
            log: LogHandle::new(),
            state,
            breakpoint_transaction: Mutex::new(()),
            shutdown: Arc::new(AtomicBool::new(false)),
            driver: Mutex::new(Some(driver)),
            debuggee_pid: None,
            is_attach: true,
        };
        (transport, calls)
    }

    fn verified_breakpoint_response(count: usize) -> Value {
        json!({
            "breakpoints": (0..count)
                .map(|_| json!({ "verified": true }))
                .collect::<Vec<_>>()
        })
    }

    fn assert_no_breakpoint_state(transport: &DapTransport, next_id: u32) {
        let (lock, _) = &*transport.state;
        let state = lock.lock().unwrap();
        assert!(state.breakpoints.is_empty());
        assert!(state.line_breakpoint_ids.is_empty());
        assert!(state.function_breakpoints.is_empty());
        assert!(state.function_breakpoint_ids.is_empty());
        assert!(state.breakpoint_conditions.is_empty());
        assert!(state.function_breakpoint_conditions.is_empty());
        assert!(state.breakpoint_log_messages.is_empty());
        assert!(state.function_breakpoint_log_messages.is_empty());
        assert_eq!(state.next_breakpoint_id, next_id);
    }

    #[test]
    fn line_breakpoint_id_exhaustion() {
        let (transport, calls) = test_transport(Vec::new());
        {
            let (lock, _) = &*transport.state;
            // Zero is the exhausted sentinel reached after u32::MAX was
            // already allocated.
            lock.lock().unwrap().next_breakpoint_id = 0;
        }
        let error = transport
            .set_breakpoint(
                &BreakSpec {
                    file: "main.rs".into(),
                    line: 10,
                    condition: Some("ready".into()),
                    log_message: Some("hit".into()),
                },
                Duration::from_millis(100),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("id space exhausted"),
            "unexpected error: {error:#}"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_no_breakpoint_state(&transport, 0);
    }

    #[test]
    fn function_breakpoint_id_exhaustion() {
        let (transport, calls) = test_transport(Vec::new());
        {
            let (lock, _) = &*transport.state;
            lock.lock().unwrap().next_breakpoint_id = 0;
        }
        let error = transport
            .set_function_breakpoint(
                "worker",
                Some("ready"),
                Some("hit"),
                Duration::from_millis(100),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("id space exhausted"),
            "unexpected error: {error:#}"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_no_breakpoint_state(&transport, 0);
    }

    #[test]
    fn breakpoint_id_exhaustion_does_not_reuse_ids() {
        let mut occupied = State::new();
        occupied.next_breakpoint_id = 42;
        occupied
            .line_breakpoint_ids
            .insert("occupied.rs:1".into(), 42);
        occupied
            .function_breakpoint_ids
            .insert("occupied".into(), 43);
        assert_eq!(allocate_breakpoint_id(&mut occupied).unwrap(), 44);
        assert_eq!(occupied.line_breakpoint_ids["occupied.rs:1"], 42);
        assert_eq!(occupied.function_breakpoint_ids["occupied"], 43);

        occupied.next_breakpoint_id = u32::MAX;
        occupied
            .function_breakpoint_ids
            .insert("maximum".into(), u32::MAX);
        assert!(allocate_breakpoint_id(&mut occupied).is_err());
        assert_eq!(occupied.next_breakpoint_id, u32::MAX);
        assert_eq!(occupied.function_breakpoint_ids["maximum"], u32::MAX);

        let (transport, calls) = test_transport(vec![Ok(verified_breakpoint_response(1))]);
        {
            let (lock, _) = &*transport.state;
            lock.lock().unwrap().next_breakpoint_id = u32::MAX;
        }
        transport
            .set_breakpoint(
                &BreakSpec {
                    file: "first.rs".into(),
                    line: 1,
                    condition: None,
                    log_message: None,
                },
                Duration::from_millis(100),
            )
            .unwrap();
        let error = transport
            .set_breakpoint(
                &BreakSpec {
                    file: "second.rs".into(),
                    line: 2,
                    condition: Some("retry".into()),
                    log_message: Some("retry".into()),
                },
                Duration::from_millis(100),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("id space exhausted"),
            "unexpected error: {error:#}"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let listing = transport
            .run_command("breakpoints", Duration::from_millis(100))
            .unwrap();
        assert!(listing.contains(&u32::MAX.to_string()));
        assert!(!listing.contains("second.rs"));
        let (lock, _) = &*transport.state;
        let state = lock.lock().unwrap();
        assert_eq!(state.next_breakpoint_id, 0);
        assert_eq!(state.line_breakpoint_ids.len(), 1);
    }

    #[test]
    fn breakpoint_adapter_rejection_rolls_back_and_retry_succeeds() {
        let (transport, calls) = test_transport(vec![
            Err("adapter rejected breakpoint".into()),
            Ok(verified_breakpoint_response(1)),
            Ok(verified_breakpoint_response(0)),
            Ok(verified_breakpoint_response(1)),
        ]);
        let spec = BreakSpec {
            file: "retry.rs".into(),
            line: 7,
            condition: Some("ready".into()),
            log_message: Some("hit".into()),
        };
        assert!(
            transport
                .set_breakpoint(&spec, Duration::from_millis(100))
                .is_err()
        );
        assert_no_breakpoint_state(&transport, 1);

        transport
            .set_breakpoint(&spec, Duration::from_millis(100))
            .unwrap();
        let listing = transport
            .run_command("breakpoints", Duration::from_millis(100))
            .unwrap();
        assert!(listing.contains("retry.rs:7"));
        assert!(listing.starts_with("1:"));

        transport
            .run_command("breakpoint delete 1", Duration::from_millis(100))
            .unwrap();
        assert_eq!(
            transport
                .run_command("breakpoints", Duration::from_millis(100))
                .unwrap(),
            "(no breakpoints set)"
        );

        transport
            .set_breakpoint(
                &BreakSpec {
                    file: "after.rs".into(),
                    line: 3,
                    condition: None,
                    log_message: None,
                },
                Duration::from_millis(100),
            )
            .unwrap();
        let listing = transport
            .run_command("breakpoints", Duration::from_millis(100))
            .unwrap();
        assert!(listing.starts_with("2:"));
        assert!(listing.contains("after.rs:3"));
        assert_eq!(calls.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn function_breakpoint_adapter_rejection_rolls_back_and_retry_succeeds() {
        let (transport, calls) = test_transport(vec![
            Ok(json!({"breakpoints": [{"verified": false}]})),
            Ok(verified_breakpoint_response(1)),
        ]);

        assert!(
            transport
                .set_function_breakpoint(
                    "worker",
                    Some("ready"),
                    Some("hit"),
                    Duration::from_millis(100),
                )
                .is_err()
        );
        assert_no_breakpoint_state(&transport, 1);

        transport
            .set_function_breakpoint(
                "worker",
                Some("ready"),
                Some("hit"),
                Duration::from_millis(100),
            )
            .unwrap();
        assert_eq!(
            transport
                .run_command("breakpoints", Duration::from_millis(100))
                .unwrap(),
            "1: function worker"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn line_breakpoint_partial_adapter_rejection_rolls_back_and_retry_succeeds() {
        let (transport, calls) = test_transport(vec![
            Ok(verified_breakpoint_response(1)),
            Ok(json!({
                "breakpoints": [
                    {"verified": false},
                    {"verified": true}
                ]
            })),
            Ok(verified_breakpoint_response(2)),
        ]);
        let first = BreakSpec {
            file: "partial.rs".into(),
            line: 3,
            condition: None,
            log_message: None,
        };
        let second = BreakSpec {
            file: "partial.rs".into(),
            line: 7,
            condition: Some("ready".into()),
            log_message: Some("hit".into()),
        };

        transport
            .set_breakpoint(&first, Duration::from_millis(100))
            .unwrap();
        assert!(
            transport
                .set_breakpoint(&second, Duration::from_millis(100))
                .is_err()
        );
        let listing = transport
            .run_command("breakpoints", Duration::from_millis(100))
            .unwrap();
        assert!(listing.starts_with("1: "));
        assert!(listing.ends_with("partial.rs:3"));
        {
            let (lock, _) = &*transport.state;
            let state = lock.lock().unwrap();
            assert_eq!(state.next_breakpoint_id, 2);
            assert!(!state.breakpoint_conditions.contains_key("partial.rs:7"));
            assert!(!state.breakpoint_log_messages.contains_key("partial.rs:7"));
        }

        transport
            .set_breakpoint(&second, Duration::from_millis(100))
            .unwrap();
        let listing = transport
            .run_command("breakpoints", Duration::from_millis(100))
            .unwrap();
        let mut entries = listing.lines();
        assert!(entries.next().unwrap().ends_with("partial.rs:3"));
        assert!(entries.next().unwrap().ends_with("partial.rs:7"));
        assert!(entries.next().is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn breakpoint_transactions_do_not_overlap_adapter_requests() {
        let state = Arc::new((Mutex::new(State::new()), Condvar::new()));
        let (driver_tx, driver_rx) = mpsc::channel();
        let (first_seen_tx, first_seen_rx) = mpsc::channel();
        let (probe_tx, probe_rx) = mpsc::channel();
        let (overlap_tx, overlap_rx) = mpsc::channel();
        let driver = std::thread::spawn(move || {
            let DriverCmd::Call {
                resp: first_resp, ..
            } = driver_rx.recv().unwrap()
            else {
                panic!("expected first breakpoint request")
            };
            first_seen_tx.send(()).unwrap();
            probe_rx.recv().unwrap();
            let overlapping = driver_rx.recv_timeout(Duration::from_millis(200)).ok();
            overlap_tx.send(overlapping.is_some()).unwrap();
            first_resp
                .send(Err("adapter rejected first request".into()))
                .unwrap();

            let second = overlapping.unwrap_or_else(|| driver_rx.recv().unwrap());
            let DriverCmd::Call {
                resp: second_resp, ..
            } = second
            else {
                panic!("expected second breakpoint request")
            };
            second_resp
                .send(Ok(verified_breakpoint_response(1)))
                .unwrap();
            while let Ok(command) = driver_rx.recv() {
                if matches!(command, DriverCmd::Shutdown) {
                    break;
                }
            }
        });
        let transport = Arc::new(DapTransport {
            child_pid: Pid::from_raw(-1),
            child: Mutex::new(None),
            driver_tx,
            log: LogHandle::new(),
            state,
            breakpoint_transaction: Mutex::new(()),
            shutdown: Arc::new(AtomicBool::new(false)),
            driver: Mutex::new(Some(driver)),
            debuggee_pid: None,
            is_attach: true,
        });

        let first_transport = transport.clone();
        let first = std::thread::spawn(move || {
            first_transport.set_breakpoint(
                &BreakSpec {
                    file: "first.rs".into(),
                    line: 1,
                    condition: None,
                    log_message: None,
                },
                Duration::from_secs(1),
            )
        });
        first_seen_rx.recv().unwrap();

        let (second_started_tx, second_started_rx) = mpsc::channel();
        let second_transport = transport.clone();
        let second = std::thread::spawn(move || {
            second_started_tx.send(()).unwrap();
            second_transport.set_breakpoint(
                &BreakSpec {
                    file: "second.rs".into(),
                    line: 2,
                    condition: None,
                    log_message: None,
                },
                Duration::from_secs(1),
            )
        });
        second_started_rx.recv().unwrap();
        probe_tx.send(()).unwrap();

        assert!(!overlap_rx.recv().unwrap());
        assert!(first.join().unwrap().is_err());
        second.join().unwrap().unwrap();
        let listing = transport
            .run_command("breakpoints", Duration::from_millis(100))
            .unwrap();
        assert!(listing.starts_with("1: "));
        assert!(listing.ends_with("second.rs:2"));
    }

    #[test]
    fn breakpoint_failure_keeps_break_unbreak_listing_consistent() {
        let (transport, _) = test_transport(vec![
            Ok(verified_breakpoint_response(1)),
            Ok(json!({"breakpoints": [{"verified": false}]})),
            Ok(verified_breakpoint_response(0)),
        ]);
        transport
            .set_breakpoint(
                &BreakSpec {
                    file: "consistent.rs".into(),
                    line: 4,
                    condition: None,
                    log_message: None,
                },
                Duration::from_millis(100),
            )
            .unwrap();
        assert!(
            transport
                .run_command("breakpoint delete 1", Duration::from_millis(100))
                .is_err()
        );
        assert!(
            transport
                .run_command("breakpoints", Duration::from_millis(100))
                .unwrap()
                .contains("consistent.rs:4")
        );

        transport
            .run_command("breakpoint delete 1", Duration::from_millis(100))
            .unwrap();
        assert_eq!(
            transport
                .run_command("breakpoints", Duration::from_millis(100))
                .unwrap(),
            "(no breakpoints set)"
        );
        assert!(
            transport
                .run_command("breakpoint delete 1", Duration::from_millis(100))
                .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn netcoredbg_listener_ownership() {
        use std::io::BufRead;
        use std::process::Command;

        // A different process owns the selected port. A connect-only check
        // would accept this listener; netcoredbg launch must reject it
        // unless the spawned adapter (or one of its descendants) owns it.
        let mut unrelated = Command::new("python3")
            .args([
                "-c",
                "import socket,time; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(1); print(s.getsockname()[1], flush=True); time.sleep(30)",
            ])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("python3 is required for the listener ownership regression");
        let mut line = String::new();
        std::io::BufReader::new(unrelated.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let addr = format!("127.0.0.1:{}", line.trim());
        let error =
            connect_with_retry_owned(&addr, Duration::from_millis(100), true, std::process::id())
                .unwrap_err();
        assert!(error.to_string().contains("not owned by spawned adapter"));
        let _ = unrelated.kill();
        let _ = unrelated.wait();
    }

    #[test]
    fn parse_break_file_line() {
        let b = parse_break("break app.go:10").unwrap();
        assert_eq!(b.file, "app.go");
        assert_eq!(b.line, 10);
    }

    #[test]
    fn dap_capture_includes_thread_descendants() {
        let tmp = tempfile::tempdir().unwrap();
        let task_dir = tmp.path().join("42/task");
        std::fs::create_dir_all(task_dir.join("42")).unwrap();
        std::fs::create_dir(task_dir.join("43")).unwrap();
        std::fs::write(task_dir.join("42/children"), "100 101\n").unwrap();
        std::fs::write(task_dir.join("43/children"), "101 102\n").unwrap();

        assert_eq!(
            process_children_from_root(tmp.path(), 42),
            vec![100, 101, 102]
        );
    }

    #[test]
    fn dap_stopped_without_thread_id_wakes_waiters() {
        let state = Arc::new((Mutex::new(State::new()), Condvar::new()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let mut pending = HashMap::new();
        let log = LogHandle::new();
        let mut next_seq = 1;
        {
            let (lock, _) = &*state;
            let mut guard = lock.lock().unwrap();
            // Model the race where exec advances its generation before the
            // driver has armed the request which will follow.
            guard.action_generation = 1;
            guard.armed_action_generation = 0;
        }
        dispatch_incoming(
            json!({"type":"event", "event":"stopped", "body":{"reason":"pause"}}),
            &mut pending,
            &state,
            &log,
            &mut stream,
            &mut next_seq,
        );
        drop(peer);
        crate::transport_common::wait_for_stop(&state, || Ok(()), Duration::from_millis(20))
            .unwrap();
        let (lock, _) = &*state;
        assert!(lock.lock().unwrap().pending_is_unscoped);
    }

    #[test]
    fn dap_function_logpoint_uses_log_message() {
        let value = function_breakpoint_value("worker", None, Some("hit {x}"));
        assert_eq!(value.get("name").and_then(Value::as_str), Some("worker"));
        assert_eq!(
            value.get("logMessage").and_then(Value::as_str),
            Some("hit {x}")
        );
        assert!(value.get("name").unwrap().as_str().unwrap() != "hit {x}");
    }

    #[test]
    fn dap_rejects_unverified_function_breakpoints() {
        assert!(!function_breakpoints_verified(
            &json!({"breakpoints":[{"verified":false}]}),
            1
        ));
        assert!(function_breakpoints_verified(
            &json!({"breakpoints":[{"verified":true}]}),
            1
        ));
    }

    #[test]
    fn dap_breakpoint_ids_cover_line_and_function_breakpoints() {
        let mut state = State::new();
        state
            .line_breakpoint_ids
            .insert("/tmp/main.rs:10".into(), 1);
        state.function_breakpoint_ids.insert("worker".into(), 2);
        state.breakpoints.insert("/tmp/main.rs".into(), vec![10]);
        state.function_breakpoints.push("worker".into());
        let ids: Vec<u32> = state
            .line_breakpoint_ids
            .values()
            .chain(state.function_breakpoint_ids.values())
            .copied()
            .collect();
        assert_eq!(ids, vec![1, 2]);
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn resolve_breakpoint_path_preserves_absolute_path() {
        let path = "/mnt/storage/ravendb/v72-experimental/src/Voron/Data/Graphs/Hnsw.Parallel.cs";
        assert_eq!(resolve_breakpoint_path_from(None, path), path);
    }

    #[test]
    fn resolve_breakpoint_path_expands_relative_path_from_start_cwd() {
        let cwd = std::path::Path::new("/mnt/storage/ravendb/v72-experimental");
        let resolved =
            resolve_breakpoint_path_from(Some(cwd), "src/Voron/Data/Graphs/Hnsw.Parallel.cs");
        assert_eq!(
            resolved,
            "/mnt/storage/ravendb/v72-experimental/src/Voron/Data/Graphs/Hnsw.Parallel.cs"
        );
    }

    #[test]
    fn resolve_breakpoint_path_keeps_relative_when_cwd_unavailable() {
        let path = "src/Voron/Data/Graphs/Hnsw.Parallel.cs";
        assert_eq!(resolve_breakpoint_path_from(None, path), path);
    }

    #[test]
    fn extract_host_port_delve() {
        let line = "DAP server listening at: 127.0.0.1:34407\n";
        assert_eq!(extract_host_port(line).as_deref(), Some("127.0.0.1:34407"));
    }

    #[test]
    fn extract_host_port_lldb_dap_bracketed() {
        let line = "Listening for: connection://[127.0.0.1]:38191\n";
        assert_eq!(extract_host_port(line).as_deref(), Some("127.0.0.1:38191"));
    }

    #[test]
    fn take_frame_parses_content_length_body() {
        let mut inbox = b"Content-Length: 2\r\n\r\n{}leftover".to_vec();
        let frame = take_frame(&mut inbox).unwrap();
        assert_eq!(frame, b"{}");
        assert_eq!(inbox, b"leftover");
    }

    #[test]
    fn take_frame_returns_none_when_incomplete() {
        let mut inbox = b"Content-Length: 10\r\n\r\n{}".to_vec();
        assert!(take_frame(&mut inbox).is_none());
    }

    #[test]
    fn framing_rejects_bad_headers_and_keeps_the_next_frame() {
        let valid = b"Content-Length: 2\r\n\r\n{}";
        for bad in [
            b"Content-Length: nope\r\n\r\n".as_slice(),
            b"Content-Length: 2\r\nX: \xff\r\n\r\n".as_slice(),
        ] {
            let mut decoder = DapFrameDecoder::new();
            decoder.inbox.extend_from_slice(bad);
            decoder.inbox.extend_from_slice(valid);
            assert!(decoder.next_frame().is_err());
            assert_eq!(decoder.next_frame().unwrap().as_deref(), Some(&b"{}"[..]));
        }
    }

    #[test]
    fn framing_discards_oversized_body_before_resynchronizing() {
        let mut decoder = DapFrameDecoder::new();
        let header = format!("Content-Length: {}\r\n\r\n", MAX_DAP_FRAME_BYTES + 1);
        decoder.inbox.extend_from_slice(header.as_bytes());
        decoder
            .inbox
            .extend(std::iter::repeat(b'x').take(MAX_DAP_FRAME_BYTES + 1));
        decoder
            .inbox
            .extend_from_slice(b"Content-Length: 2\r\n\r\n{}");
        assert!(decoder.next_frame().is_err());
        assert_eq!(decoder.next_frame().unwrap().as_deref(), Some(&b"{}"[..]));
    }

    #[test]
    fn framing_resynchronizes_after_an_oversized_length_without_waiting_for_it() {
        let mut decoder = DapFrameDecoder::new();
        decoder
            .inbox
            .extend_from_slice(b"Content-Length: 18446744073709551615\r\n\r\n");
        decoder
            .inbox
            .extend_from_slice(b"Content-Length: 2\r\n\r\n{}");

        assert!(decoder.next_frame().is_err());
        assert_eq!(decoder.next_frame().unwrap().as_deref(), Some(&b"{}"[..]));
    }

    #[test]
    fn oversized_resynchronization_keeps_memory_bounded_between_chunks() {
        let mut decoder = DapFrameDecoder::new();
        decoder
            .inbox
            .extend_from_slice(b"Content-Length: 999999999999999999\r\n\r\n");
        assert!(decoder.next_frame().is_err());

        decoder
            .inbox
            .extend(std::iter::repeat(b'x').take(1_000_000));
        assert!(decoder.next_frame().unwrap().is_none());
        assert!(decoder.inbox.len() < b"Content-Length:".len());

        decoder
            .inbox
            .extend_from_slice(b"Content-Length: 2\r\n\r\n{}");
        assert_eq!(decoder.next_frame().unwrap().as_deref(), Some(&b"{}"[..]));
    }

    #[test]
    fn a_consumed_stop_cannot_be_recreated_by_a_late_stack_helper() {
        let state = Arc::new((Mutex::new(State::new()), Condvar::new()));
        {
            let (lock, _) = &*state;
            let mut guard = lock.lock().unwrap();
            guard.stop_generation = 7;
            guard.pending_hit = Some(HitEvent::default());
        }
        let body = json!({
            "stackFrames": [{
                "name": "old_stop",
                "line": 10,
                "source": { "path": "old.rs" }
            }]
        });
        handle_stack_response(body.clone(), &state, 7);
        {
            let (lock, _) = &*state;
            assert_eq!(
                lock.lock()
                    .unwrap()
                    .pending_hit
                    .take()
                    .unwrap()
                    .file
                    .as_deref(),
                Some("old.rs")
            );
        }

        // The daemon consumed the stop and began a later action. A delayed
        // helper from the earlier stop must not repopulate pending state.
        handle_stack_response(body, &state, 7);
        let (lock, _) = &*state;
        assert!(lock.lock().unwrap().pending_hit.is_none());
    }

    #[test]
    fn stopped_frame_queued_before_action_keeps_the_previous_generation() {
        let state = Arc::new((Mutex::new(State::new()), Condvar::new()));
        {
            let (lock, _) = &*state;
            let mut guard = lock.lock().unwrap();
            guard.action_generation = 1;
            guard.armed_action_generation = 0;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let _peer = peer;
        let mut pending = HashMap::new();
        let log = LogHandle::new();
        let mut next_seq = 1;
        dispatch_incoming(
            json!({
                "type": "event",
                "event": "stopped",
                "body": { "reason": "breakpoint" }
            }),
            &mut pending,
            &state,
            &log,
            &mut stream,
            &mut next_seq,
        );
        let (lock, _) = &*state;
        assert_eq!(lock.lock().unwrap().pending_action_generation, 0);

        {
            let mut guard = lock.lock().unwrap();
            guard.armed_action_generation = 1;
        }
        dispatch_incoming(
            json!({
                "type": "event",
                "event": "stopped",
                "body": { "reason": "breakpoint" }
            }),
            &mut pending,
            &state,
            &log,
            &mut stream,
            &mut next_seq,
        );
        assert_eq!(lock.lock().unwrap().pending_action_generation, 1);
    }

    #[test]
    fn restart_tracking_accepts_a_reused_pid_only_with_new_identity() {
        let old = ProcessIdentity {
            start_time: 1,
            exe_device: 2,
            exe_inode: 3,
        };
        let replacement = ProcessIdentity {
            start_time: 4,
            exe_device: 2,
            exe_inode: 3,
        };
        assert_eq!(find_new_descendant(&[(10, old)], vec![(10, old)]), None);
        assert_eq!(
            find_new_descendant(&[(10, old)], vec![(10, replacement)]),
            Some((10, replacement))
        );
    }

    #[test]
    fn preflight_attach_passes_when_ptrace_unrestricted() {
        // ptrace_scope=0 → kernel allows any user to ptrace own
        // processes regardless of parentage; preflight has nothing to
        // catch and must let the attach proceed.
        assert!(preflight_attach_decide(1234, true, 0).is_ok());
    }

    #[test]
    fn preflight_attach_blocks_when_ptrace_restricted() {
        // Regression for the netcoredbg "DAP configurationDone: timeout"
        // we used to surface 10s after attach. With ptrace_scope >= 1
        // and dbg's daemon not the parent of the target, we now bail
        // up front with the actionable message.
        for scope in [1, 2, 3] {
            let err = preflight_attach_decide(1234, true, scope).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains(&format!("kernel.yama.ptrace_scope = {scope}")),
                "missing scope value in error: {msg}"
            );
            assert!(
                msg.contains("relaunch the target through `dbg start"),
                "missing primary fix instruction: {msg}"
            );
            assert!(
                msg.contains("sysctl kernel.yama.ptrace_scope=0"),
                "missing sysctl fix instruction: {msg}"
            );
        }
    }

    #[test]
    fn preflight_attach_rejects_dead_pid() {
        let err = preflight_attach_decide(99999, false, 0).unwrap_err();
        assert!(
            format!("{err:#}").contains("no process with pid 99999"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn extract_system_process_id_accepts_number() {
        let body = serde_json::json!({ "systemProcessId": 12345, "name": "dotnet" });
        assert_eq!(extract_system_process_id(&body), Some(12345));
    }

    #[test]
    fn extract_system_process_id_accepts_string() {
        // debugpy emits the pid as a JSON string in some configs.
        let body = serde_json::json!({ "systemProcessId": "67890" });
        assert_eq!(extract_system_process_id(&body), Some(67890));
    }

    #[test]
    fn extract_system_process_id_rejects_garbage() {
        let body = serde_json::json!({ "systemProcessId": null });
        assert_eq!(extract_system_process_id(&body), None);

        let body = serde_json::json!({ "systemProcessId": "not-a-pid" });
        assert_eq!(extract_system_process_id(&body), None);

        // u32::MAX + 1 — rejected to avoid silently truncating.
        let body = serde_json::json!({ "systemProcessId": 4_294_967_296_u64 });
        assert_eq!(extract_system_process_id(&body), None);

        let body = serde_json::json!({ "name": "missing field" });
        assert_eq!(extract_system_process_id(&body), None);
    }
}
