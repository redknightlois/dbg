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
use std::os::fd::AsRawFd;
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
    /// Tracked user breakpoints: "file:line" → nothing (DAP
    /// setBreakpoints is path-keyed, not id-keyed).
    breakpoints: HashMap<String, Vec<u32>>,
    /// Accumulated function-breakpoint names. DAP `setFunctionBreakpoints`
    /// replaces the whole set on each call, so we replay them all.
    function_breakpoints: Vec<String>,
    /// "absolute-path:line" → condition expression, for replaying
    /// conditional line breakpoints across the full-set setBreakpoints call.
    breakpoint_conditions: HashMap<String, String>,
    /// Function-name → condition expression, same idea for setFunctionBreakpoints.
    function_breakpoint_conditions: HashMap<String, String>,
    /// "absolute-path:line" → logMessage template. Logpoints emit
    /// formatted output without stopping the debuggee.
    breakpoint_log_messages: HashMap<String, String>,
    /// True between `stopped` and the next `continue`/step.
    paused: bool,
    /// Flipped when the adapter disconnects or terminates.
    alive: bool,
    /// Set when the `initialized` event arrives. The transport blocks
    /// on this before sending `configurationDone`.
    initialized: bool,
    /// Flag set when a `terminated` or `exited` event arrives.
    terminated: bool,
}

impl State {
    fn new() -> Self {
        Self {
            current_thread: None,
            top_frame: None,
            call_frames: Vec::new(),
            pending_hit: None,
            breakpoints: HashMap::new(),
            function_breakpoints: Vec::new(),
            breakpoint_conditions: HashMap::new(),
            function_breakpoint_conditions: HashMap::new(),
            breakpoint_log_messages: HashMap::new(),
            paused: false,
            alive: true,
            initialized: false,
            terminated: false,
        }
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
    },
    Shutdown,
}

pub struct DapTransport {
    child_pid: Pid,
    child: Mutex<Option<Child>>,
    driver_tx: Sender<DriverCmd>,
    log: LogHandle,
    state: Arc<(Mutex<State>, Condvar)>,
    shutdown: Arc<AtomicBool>,
    driver: Mutex<Option<JoinHandle<()>>>,
    /// Absolute path of the program being debugged, for
    /// `setBreakpoints` path resolution.
    target_path: String,
}

impl DapTransport {
    /// Spawn the DAP adapter, connect, drive the full DAP handshake
    /// (initialize → launch → configurationDone), and return the
    /// transport positioned just before the first `stopped` event.
    /// Callers that need `stopOnEntry=true` behaviour bake it into
    /// `launch_args`; the transport doesn't assume either way.
    pub fn spawn(target: &str, cfg: DapLaunchConfig) -> Result<Self> {
        let mut cmd = Command::new(&cfg.bin);
        cmd.args(&cfg.args)
            .stdin(Stdio::null())
            // Adapters differ on where they announce their listen
            // address: dlv prints to stdout, lldb-dap & debugpy to
            // stderr. Pipe both and let the scraper search either.
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().with_context(|| format!("failed to spawn {}", cfg.bin))?;
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
        let stream = connect_with_retry(&addr, Duration::from_secs(5))
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

        let transport = Self {
            child_pid,
            child: Mutex::new(Some(child)),
            driver_tx,
            log,
            state,
            shutdown,
            driver: Mutex::new(Some(driver)),
            target_path: std::fs::canonicalize(target)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| target.to_string()),
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
        transport.call_blocking("configurationDone", json!({}), Duration::from_secs(10))?;
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
            })
            .map_err(|_| anyhow!("DAP driver thread gone"))?;
        Ok(rx)
    }

    fn call_blocking(&self, command: &str, arguments: Value, timeout: Duration) -> Result<Value> {
        let (tx, rx) = mpsc::channel();
        self.driver_tx
            .send(DriverCmd::Call {
                command: command.to_string(),
                arguments,
                resp: tx,
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
            return self.exec(|s| s.call_blocking("continue", json!({"threadId": tid}), timeout), timeout);
        }
        if matches!(trimmed, "step" | "s" | "stepi") {
            let tid = self.current_thread().unwrap_or(1);
            return self.exec(|s| s.call_blocking("stepIn", json!({"threadId": tid}), timeout), timeout);
        }
        if matches!(trimmed, "next" | "n") {
            let tid = self.current_thread().unwrap_or(1);
            return self.exec(|s| s.call_blocking("next", json!({"threadId": tid}), timeout), timeout);
        }
        if matches!(trimmed, "out" | "finish") {
            let tid = self.current_thread().unwrap_or(1);
            return self.exec(|s| s.call_blocking("stepOut", json!({"threadId": tid}), timeout), timeout);
        }
        if trimmed == "pause" {
            let tid = self.current_thread().unwrap_or(1);
            return self.exec(|s| s.call_blocking("pause", json!({"threadId": tid}), timeout), timeout);
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
            {
                let (lock, _) = &*self.state;
                let mut s = lock.lock().unwrap();
                s.paused = false;
                s.top_frame = None;
                s.call_frames.clear();
                s.pending_hit = None;
            }
            self.call_blocking("restart", json!({}), timeout)?;
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
            return self.set_function_breakpoint(name, cond, timeout);
        }
        if let Some(spec) = parse_break(trimmed) {
            return self.set_breakpoint(&spec, timeout);
        }
        if let Some(expr) = trimmed.strip_prefix("print ").or_else(|| trimmed.strip_prefix("p ")) {
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
            s.pending_hit = None;
            s.paused = false;
        }
        f(self)?;
        let deadline = Instant::now() + timeout;
        let (lock, cvar) = &*self.state;
        let mut guard = lock.lock().unwrap();
        while guard.alive && guard.pending_hit.is_none() && !guard.terminated {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("dap: timeout waiting for stopped event");
            }
            let r = cvar.wait_timeout(guard, remaining).unwrap();
            guard = r.0;
        }
        Ok(String::new())
    }

    fn current_thread(&self) -> Option<i64> {
        let (lock, _) = &*self.state;
        lock.lock().unwrap().current_thread
    }

    fn list_threads(&self, timeout: Duration) -> Result<String> {
        let resp = self.call_blocking("threads", json!({}), timeout)?;
        let arr = resp.get("threads").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if arr.is_empty() {
            return Ok("(no threads)".into());
        }
        let current = self.current_thread();
        let mut out = String::new();
        for t in arr {
            let id = t.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let marker = if Some(id) == current { "*" } else { " " };
            out.push_str(&format!("{marker} {id}  {name}\n"));
        }
        Ok(out)
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
        if let Ok(resp) = self.call_blocking("stackTrace", json!({ "threadId": id, "startFrame": 0, "levels": 20 }), Duration::from_secs(5)) {
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
                let (p, l) = s.rsplit_once(':').ok_or_else(|| anyhow!("list: expected file:line"))?;
                let line: u32 = l.trim().parse().context("list: invalid line number")?;
                (p.trim().to_string(), line)
            }
            None => {
                let (lock, _) = &*self.state;
                let s = lock.lock().unwrap();
                let f = s.call_frames.first().ok_or_else(|| anyhow!("list: no current frame"))?;
                let path = f.get("source").and_then(|src| src.get("path")).and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("list: current frame has no source path"))?.to_string();
                let line = f.get("line").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                (path, line)
            }
        };
        let text = std::fs::read_to_string(&path).with_context(|| format!("list: reading {path}"))?;
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
        timeout: Duration,
    ) -> Result<String> {
        // DAP `setFunctionBreakpoints` replaces the whole set per call,
        // same semantics as `setBreakpoints`. Accumulate in state so
        // adding a second fn bp doesn't remove the first.
        let all: Vec<String> = {
            let (lock, _) = &*self.state;
            let mut s = lock.lock().unwrap();
            if !s.function_breakpoints.contains(&name.to_string()) {
                s.function_breakpoints.push(name.to_string());
            }
            if let Some(c) = cond {
                s.function_breakpoint_conditions.insert(name.to_string(), c.to_string());
            } else {
                s.function_breakpoint_conditions.remove(name);
            }
            s.function_breakpoints.clone()
        };
        let fns: Vec<Value> = {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            all.iter()
                .map(|n| {
                    let mut b = serde_json::Map::new();
                    b.insert("name".into(), Value::String(n.clone()));
                    if let Some(c) = s.function_breakpoint_conditions.get(n) {
                        b.insert("condition".into(), Value::String(c.clone()));
                    }
                    Value::Object(b)
                })
                .collect()
        };
        self.call_blocking("setFunctionBreakpoints", json!({ "breakpoints": fns }), timeout)?;
        match cond {
            Some(c) => Ok(format!("Function breakpoint set: {name} if {c}")),
            None => Ok(format!("Function breakpoint set: {name}")),
        }
    }

    fn set_breakpoint(&self, spec: &BreakSpec, timeout: Duration) -> Result<String> {
        let BreakSpec { file, line, condition, log_message } = spec;
        // DAP requires the full set of breakpoints for a source each
        // call — it doesn't merge. Accumulate in state.breakpoints
        // and replay the full list per source on each add.
        let resolved_path = if std::path::Path::new(file).is_absolute() {
            file.clone()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(file).display().to_string())
                .unwrap_or_else(|_| file.clone())
        };
        let lines: Vec<u32> = {
            let (lock, _) = &*self.state;
            let mut s = lock.lock().unwrap();
            let lines_snapshot = {
                let entry = s.breakpoints.entry(resolved_path.clone()).or_default();
                if !entry.contains(line) {
                    entry.push(*line);
                }
                entry.clone()
            };
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
        self.call_blocking(
            "setBreakpoints",
            json!({
                "source": { "path": resolved_path },
                "breakpoints": breakpoints,
                "sourceModified": false,
            }),
            timeout,
        )?;
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
            Some(i) => (rest[..i].trim().to_string(), rest[i + 1..].trim().to_string()),
            None => bail!("usage: dbg set <lhs> = <expr>"),
        };
        if lhs.is_empty() || rhs.is_empty() {
            bail!("usage: dbg set <lhs> = <expr>");
        }
        let frame_id = {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            s.top_frame.as_ref().and_then(|f| f.get("id").and_then(|v| v.as_i64()))
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
            let vars = self.call_blocking("variables", json!({"variablesReference": vref}), timeout)?;
            let found = vars
                .get("variables")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().any(|v| v.get("name").and_then(|n| n.as_str()) == Some(lhs)))
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
            s.top_frame.as_ref().and_then(|f| f.get("id").and_then(|v| v.as_i64()))
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
        let scopes_resp =
            self.call_blocking("scopes", json!({ "frameId": frame_id }), timeout)?;
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
            let hint = scope.get("presentationHint").and_then(|v| v.as_str()).unwrap_or("");
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
        if s.breakpoints.is_empty() {
            return "(no breakpoints set)".into();
        }
        let mut out = String::new();
        for (file, lines) in &s.breakpoints {
            for line in lines {
                out.push_str(&format!("{file}:{line}\n"));
            }
        }
        out.trim_end().to_string()
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
            let r = cvar.wait_timeout(guard, Duration::from_millis(250)).unwrap();
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
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.driver_tx.send(DriverCmd::Shutdown);
        let _ = nix::sys::signal::kill(self.child_pid, nix::sys::signal::Signal::SIGTERM);
        std::thread::sleep(Duration::from_millis(500));
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(h) = self.driver.lock().unwrap().take() {
            let _ = h.join();
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
                BreakLoc::Fqn(name) => {
                    // Function breakpoints don't carry a log template
                    // in the current DAP path; logpoints on a symbol
                    // fall back to the native string (`bfn` has no
                    // log field) so PTY-style callers still work.
                    if log.is_some() {
                        return None;
                    }
                    Some(self.set_function_breakpoint(name, cond.as_deref(), timeout))
                }
                BreakLoc::ModuleMethod { .. } => None,
            },
        }
    }
}

impl Drop for DapTransport {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.driver_tx.send(DriverCmd::Shutdown);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Read from both stdout and stderr concurrently until one of them
/// produces a line containing `marker`. Returns the scraped
/// host:port. After returning, the caller takes ownership of both
/// streams (we hand them back via an extra return) so it can drain
/// them in background — full pipe buffers will otherwise SIGPIPE the
/// adapter once it starts chattering under load.
type ScrapeResult = (String, Option<std::process::ChildStdout>, Option<ChildStderr>);

fn scrape_listen_addr_either(
    stdout: std::process::ChildStdout,
    stderr: ChildStderr,
    marker: &str,
    timeout: Duration,
) -> Result<ScrapeResult> {
    use std::os::fd::AsRawFd;
    let fd_o = stdout.as_raw_fd();
    let fd_e = stderr.as_raw_fd();
    nix::fcntl::fcntl(fd_o, nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK))?;
    nix::fcntl::fcntl(fd_e, nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK))?;

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
                    let digits: String =
                        port.chars().take_while(|c| c.is_ascii_digit()).collect();
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
    let deadline = Instant::now() + timeout;
    loop {
        match TcpStream::connect(addr) {
            Ok(s) => return Ok(s),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.into()),
        }
    }
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
    let mut inbox: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut next_seq: i64 = 1;
    let mut pending: HashMap<i64, (String, Sender<Result<Value, String>>)> = HashMap::new();
    // Temporary storage for auto-fetched stackTrace responses keyed
    // by the originating request seq (so the event-side auto-fetch
    // flow can await the response without blocking the driver).
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Drain inbound bytes.
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => {
                    mark_dead(&state);
                    return;
                }
                Ok(n) => inbox.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    mark_dead(&state);
                    return;
                }
            }
        }

        // Parse as many complete messages as are in the inbox.
        loop {
            match take_frame(&mut inbox) {
                Some(bytes) => {
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
                None => break,
            }
        }

        // Drain outbound command channel.
        loop {
            match rx.try_recv() {
                Ok(DriverCmd::Shutdown) => {
                    mark_dead(&state);
                    return;
                }
                Ok(DriverCmd::Call { command, arguments, resp }) => {
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
                    mark_dead(&state);
                    return;
                }
            }
        }

        std::thread::sleep(Duration::from_millis(5));
    }
}

fn mark_dead(state: &Arc<(Mutex<State>, Condvar)>) {
    let (lock, cvar) = &**state;
    let mut s = lock.lock().unwrap();
    s.alive = false;
    cvar.notify_all();
}

fn take_frame(inbox: &mut Vec<u8>) -> Option<Vec<u8>> {
    // Find header/body boundary.
    let hdr_end = inbox.windows(4).position(|w| w == b"\r\n\r\n")?;
    let header_s = std::str::from_utf8(&inbox[..hdr_end]).ok()?;
    let mut content_length = 0usize;
    for line in header_s.split("\r\n") {
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse().ok()?;
        }
    }
    let total = hdr_end + 4 + content_length;
    if inbox.len() < total {
        return None;
    }
    let body = inbox[hdr_end + 4..total].to_vec();
    inbox.drain(..total);
    Some(body)
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
                    {
                        let (lock, _) = &**state;
                        let mut s = lock.lock().unwrap();
                        s.paused = true;
                        s.current_thread = thread_id;
                    }
                    if let Some(tid) = thread_id {
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
                                handle_stack_response(body, &state2);
                            }
                        });
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
                _ => {}
            }
        }
        _ => {}
    }
}

fn handle_stack_response(body: Value, state: &Arc<(Mutex<State>, Condvar)>) {
    let frames = body
        .get("stackFrames")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let top = frames.first().cloned();
    let hit = top.as_ref().map(|f| {
        let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("?").to_string();
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
    s.call_frames = frames;
    s.top_frame = top;
    s.pending_hit = hit;
    cvar.notify_all();
}

#[derive(Debug)]
struct BreakSpec {
    file: String,
    line: u32,
    condition: Option<String>,
    log_message: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_break_file_line() {
        let b = parse_break("break app.go:10").unwrap();
        assert_eq!(b.file, "app.go");
        assert_eq!(b.line, 10);
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
}
