//! V8 Inspector transport for node-proto.
//!
//! Implements `DebuggerIo` by speaking the V8 Inspector WebSocket
//! protocol directly, instead of driving `node inspect`'s PTY REPL.
//! Two structural wins over the PTY approach:
//!
//! * Program output (`console.log`, etc.) is delivered as
//!   `Runtime.consoleAPICalled` events — completely separate from
//!   debugger chatter. The transport emits these as
//!   `EventKind::Stdout`, fulfilling Phase 4c.
//! * Stop events are delivered as structured `Debugger.paused`
//!   messages with frame data inline. No text-banner parsing, no
//!   async banner race windows. The transport surfaces a
//!   `pending_hit()` so the daemon skips `parse_hit` entirely.
//!
//! The session spawns `node --inspect-brk=127.0.0.1:0 <target>`,
//! scrapes the `ws://…` URL from node's stderr, connects a
//! tungstenite WebSocket, and runs a single driver thread that
//! multiplexes the socket and a command channel.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
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
use tungstenite::{Message, WebSocket};
use tungstenite::stream::MaybeTlsStream;

use crate::backend::canonical::HitEvent;
use crate::pty::{DebuggerIo, EventKind, LogHandle};

/// Tracks conversation state on the inspector socket. Protected by a
/// mutex + condvar so `send_and_wait` callers on the daemon side can
/// block until the driver thread records a state transition.
struct State {
    paused: bool,
    /// Frames from the most recent `Debugger.paused` event. Top-most
    /// frame first. Used to answer `backtrace`, `frame`, `print`.
    call_frames: Vec<Value>,
    /// Populated when a new paused event arrives. Daemon drains via
    /// `pending_hit()` before the next command runs.
    pending_hit: Option<HitEvent>,
    /// scriptId → source URL. Tracks `Debugger.scriptParsed` so we can
    /// map breakpoint paths and format call frames in terms of files.
    script_urls: HashMap<String, String>,
    /// scriptId → source text. Populated lazily on the first `list`
    /// that asks for a given script; `Debugger.getScriptSource` is
    /// the slow roundtrip we want to avoid doing per `list` call.
    script_sources: HashMap<String, String>,
    /// Our tracked breakpoints: "file:line" key → inspector bp id.
    breakpoints: HashMap<String, String>,
    /// Set by the driver when the WS closes or node exits.
    alive: bool,
    /// Main V8 execution context id. Captured on the first
    /// `Runtime.executionContextCreated` and used to scope
    /// `executionContextDestroyed` — worker_threads tear down their
    /// own contexts during a healthy run, and we must not mark the
    /// whole session dead in that case.
    main_context_id: Option<i64>,
}

impl State {
    fn new() -> Self {
        Self {
            paused: false,
            call_frames: Vec::new(),
            pending_hit: None,
            script_urls: HashMap::new(),
            script_sources: HashMap::new(),
            breakpoints: HashMap::new(),
            alive: true,
            main_context_id: None,
        }
    }
}

impl crate::transport_common::StopState for State {
    fn clear_pending(&mut self) {
        self.pending_hit = None;
        self.paused = false;
    }
    fn has_pending_hit(&self) -> bool {
        self.pending_hit.is_some()
    }
    fn alive(&self) -> bool {
        self.alive
    }
}

/// Driver-thread command. Anything the daemon wants the driver to do
/// with the WebSocket funnels through this channel; the driver has
/// sole ownership of the socket.
enum DriverCmd {
    /// Make an Inspector JSON-RPC call and send the response back on
    /// `resp`. `Err(String)` for protocol-level error objects.
    Call {
        method: String,
        params: Value,
        resp: Sender<Result<Value, String>>,
    },
    /// Stop the driver and close the socket.
    Shutdown,
}

pub struct InspectorTransport {
    child_pid: Pid,
    /// Kept so the child doesn't become a zombie; reaping happens in
    /// `quit()` / `Drop`.
    child: Mutex<Option<Child>>,
    driver_tx: Sender<DriverCmd>,
    log: LogHandle,
    state: Arc<(Mutex<State>, Condvar)>,
    shutdown: Arc<AtomicBool>,
    driver: Mutex<Option<JoinHandle<()>>>,
}

impl InspectorTransport {
    /// Spawn `node --inspect-brk=127.0.0.1:0 <target> [args...]`,
    /// scrape the WS URL from stderr, connect, enable domains, and
    /// return a transport that is ready for commands. The session is
    /// paused at the very first statement (node's --inspect-brk
    /// semantics); callers drive forward with `cont` / breakpoints.
    pub fn spawn(target: &str, args: &[String]) -> Result<Self> {
        let mut cmd = Command::new("node");
        cmd.arg("--inspect-brk=127.0.0.1:0")
            .arg(target)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("failed to spawn node")?;
        let child_pid = Pid::from_raw(child.id() as i32);

        let stderr = child.stderr.take().context("missing node stderr")?;
        // Claim stdout from the Child so it closes cleanly on Drop.
        // Not piped into the event log — see comment below.
        let _stdout = child.stdout.take().context("missing node stdout")?;

        // Scrape the ws URL from stderr. Node emits
        //   "Debugger listening on ws://127.0.0.1:PORT/UUID"
        // on the first line after `--inspect-brk` binds.
        let ws_url = scrape_ws_url(stderr, Duration::from_secs(10))
            .context("failed to read Debugger listening line from node stderr")?;

        // Connect the WebSocket. tungstenite hands back a
        // MaybeTlsStream even for plain ws:// — we only care about the
        // inner TcpStream to flip it to non-blocking.
        let (mut ws, _resp) = tungstenite::client::connect(&ws_url)
            .with_context(|| format!("failed to connect to {ws_url}"))?;
        set_nonblocking(&mut ws)?;

        let log = LogHandle::new();
        let state = Arc::new((Mutex::new(State::new()), Condvar::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (driver_tx, driver_rx) = mpsc::channel::<DriverCmd>();

        // B1 deliberately does NOT forward the node subprocess's
        // stdout/stderr into the event log. `console.log` is already
        // delivered structurally via `Runtime.consoleAPICalled` and
        // forwarding stdout in parallel produces duplicate entries.
        // Direct `process.stdout.write` bypasses consoleAPICalled and
        // is therefore invisible in B1 — a known B2 follow-up.
        // Node's stderr after the "Debugger listening" line is pure
        // attach-handshake noise ("Debugger attached.", "For help…")
        // with no diagnostic value, so we drop it too.

        // Spawn the driver thread. It owns the WebSocket and is the
        // single place reads + writes happen. Uses a 5ms poll cadence
        // to interleave WS reads with command-channel pulls.
        let driver_state = state.clone();
        let driver_log = log.clone();
        let driver_shutdown = shutdown.clone();
        let driver = std::thread::Builder::new()
            .name("dbg-inspector-driver".into())
            .spawn(move || {
                driver_loop(ws, driver_rx, driver_state, driver_log, driver_shutdown);
            })
            .context("failed to spawn inspector driver thread")?;

        let transport = Self {
            child_pid,
            child: Mutex::new(Some(child)),
            driver_tx,
            log,
            state,
            shutdown,
            driver: Mutex::new(Some(driver)),
        };

        // Enable domains. Do these on the caller thread via the
        // driver channel — the driver handles request/response
        // correlation.
        transport.call_blocking("Runtime.enable", json!({}), Duration::from_secs(5))?;
        transport.call_blocking("Debugger.enable", json!({}), Duration::from_secs(5))?;
        // Track console.* calls so we can route them to EventKind::Stdout.
        let _ = transport.call_blocking("Console.enable", json!({}), Duration::from_secs(5));
        // With --inspect-brk, V8 holds the initial event loop until
        // we acknowledge we're attached via runIfWaitingForDebugger.
        // The VM then executes the first statement and immediately
        // pauses, emitting `Debugger.paused` (reason "Break on
        // start") — that's what `wait_for_prompt` blocks on.
        transport.call_blocking(
            "Runtime.runIfWaitingForDebugger",
            json!({}),
            Duration::from_secs(5),
        )?;
        Ok(transport)
    }

    /// Low-level call into the driver. Blocks up to `timeout` waiting
    /// for the driver to ack the response.
    fn call_blocking(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let (resp_tx, resp_rx) = mpsc::channel();
        self.driver_tx
            .send(DriverCmd::Call {
                method: method.to_string(),
                params,
                resp: resp_tx,
            })
            .map_err(|_| anyhow!("driver thread gone"))?;
        match resp_rx.recv_timeout(timeout) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!("inspector error: {e}")),
            Err(_) => Err(anyhow!("inspector call timeout ({method})")),
        }
    }

    /// Parse and execute a command string received through
    /// `send_and_wait`. The daemon's canonical dispatcher hands us
    /// either the `NodeProtoBackend` op string (e.g. "cont",
    /// "sb('f.js',10)") or a raw user command. Anything we can't map
    /// is reported back as an error string — the daemon surfaces it
    /// verbatim to the agent.
    fn run_command(&self, cmd: &str, timeout: Duration) -> Result<String> {
        let trimmed = cmd.trim();

        // Resume / step family.
        if matches!(trimmed, "cont" | "c" | "continue") {
            return self.exec(|s| s.call_blocking("Debugger.resume", json!({}), timeout), timeout);
        }
        if matches!(trimmed, "step" | "s" | "stepi") {
            return self.exec(|s| s.call_blocking("Debugger.stepInto", json!({}), timeout), timeout);
        }
        if matches!(trimmed, "next" | "n") {
            return self.exec(|s| s.call_blocking("Debugger.stepOver", json!({}), timeout), timeout);
        }
        if matches!(trimmed, "out" | "finish") {
            return self.exec(|s| s.call_blocking("Debugger.stepOut", json!({}), timeout), timeout);
        }
        if trimmed == "catch" || trimmed == "catch off" {
            self.call_blocking(
                "Debugger.setPauseOnExceptions",
                json!({"state": "none"}),
                timeout,
            )?;
            return Ok("exception breakpoints cleared".into());
        }
        if let Some(rest) = trimmed.strip_prefix("catch ") {
            // V8 accepts exactly one of: "none" | "uncaught" | "all".
            // Map common DAP-style filter names onto these.
            let filters: Vec<&str> = rest
                .split(|c: char| c.is_ascii_whitespace() || c == ',')
                .filter(|s| !s.is_empty())
                .collect();
            let has_caught = filters.iter().any(|f| matches!(*f, "caught" | "all"));
            let has_uncaught = filters.iter().any(|f| matches!(*f, "uncaught"));
            let state = if has_caught {
                "all"
            } else if has_uncaught {
                "uncaught"
            } else {
                "none"
            };
            self.call_blocking(
                "Debugger.setPauseOnExceptions",
                json!({"state": state}),
                timeout,
            )?;
            return Ok(format!("exception breakpoints: state={state}"));
        }
        if trimmed == "pause" {
            // Inspector's pause doesn't emit `Debugger.paused` on its own
            // reply — the async notification arrives shortly after. Use
            // the same exec() helper so we wait for the stopped event.
            return self.exec(|s| s.call_blocking("Debugger.pause", json!({}), timeout), timeout);
        }

        // backtrace — from cached call_frames, no round trip.
        if trimmed == "backtrace" || trimmed == "bt" || trimmed == "where" {
            return Ok(self.format_backtrace());
        }

        // breakpoints listing
        if trimmed == "breakpoints" {
            return Ok(self.format_breakpoints());
        }

        // locals — walk scope chain of top frame and collect
        // properties into a JSON object. Skips the global scope so
        // the result doesn't drown in node's module-wrapping clutter.
        if trimmed == "locals" {
            return self.collect_locals(timeout);
        }

        // list             — top-frame current line ±10
        // list file:line   — centred on file:line, ±10
        if trimmed == "list" {
            return self.list_source(None, timeout);
        }
        if let Some(loc) = trimmed.strip_prefix("list ") {
            return self.list_source(Some(loc.trim()), timeout);
        }

        // sb('file.js', 10)  → Debugger.setBreakpointByUrl
        // sb('name')         → function breakpoint (Debugger.setBreakpointByUrl on regex
        //                      over function name — best-effort)
        // sb(10)             → breakpoint on current script at line 10
        if let Some((bp, cond)) = parse_sb(trimmed) {
            return self.set_breakpoint(bp, cond.as_deref(), timeout);
        }

        // exec <expr>  / print <expr>  → evaluateOnCallFrame
        if let Some(expr) = trimmed
            .strip_prefix("exec ")
            .or_else(|| trimmed.strip_prefix("print "))
            .or_else(|| trimmed.strip_prefix("p "))
        {
            return self.evaluate(expr, timeout);
        }
        if let Some(rest) = trimmed.strip_prefix("set ") {
            // V8 evaluates `x = 5` as a regular expression on the top
            // frame — no dedicated setVariableValue call needed for the
            // common case. Agents wanting scope-targeted assignment can
            // still drop to `dbg raw` with Debugger.setVariableValue.
            let (lhs, rhs) = match rest.find('=') {
                Some(i) => (rest[..i].trim(), rest[i + 1..].trim()),
                None => return Err(anyhow!("usage: set <lhs> = <expr>")),
            };
            if lhs.is_empty() || rhs.is_empty() {
                return Err(anyhow!("usage: set <lhs> = <expr>"));
            }
            return self.evaluate(&format!("{lhs} = {rhs}"), timeout);
        }

        // .exit / quit  — shutdown
        if trimmed == ".exit" || trimmed == "quit" {
            self.shutdown.store(true, Ordering::Relaxed);
            let _ = self.driver_tx.send(DriverCmd::Shutdown);
            return Ok(String::new());
        }

        Err(anyhow!(
            "node-proto: unsupported command `{trimmed}` (B1 supports: cont/step/next/out, backtrace, breakpoints, sb(...), print/exec <expr>)"
        ))
    }

    /// Shared resume/step machinery: fire the Inspector method, wait
    /// for either the next `Debugger.paused` or an end-of-session
    /// signal, return a short status string. The structured hit, if
    /// any, lands in `state.pending_hit` for the daemon to drain via
    /// `pending_hit()`.
    fn exec<F: FnOnce(&Self) -> Result<Value>>(&self, f: F, timeout: Duration) -> Result<String> {
        crate::transport_common::wait_for_stop(
            &self.state,
            || f(self).map(|_| ()),
            timeout,
        )
    }

    fn set_breakpoint(&self, bp: ParsedSb, cond: Option<&str>, timeout: Duration) -> Result<String> {
        match bp {
            ParsedSb::FileLine { file, line } => {
                // Inspector wants `url` or `urlRegex`. Node reports
                // script urls as absolute `file://` paths. Use a regex
                // match on the tail so user-supplied `foo.js:10`
                // matches a fully-qualified url.
                let escaped = regex_escape(&file);
                let mut params = serde_json::Map::new();
                params.insert("urlRegex".into(), Value::String(format!("{}$", escaped)));
                params.insert("lineNumber".into(), json!(line.saturating_sub(1)));
                if let Some(c) = cond {
                    params.insert("condition".into(), Value::String(c.to_string()));
                }
                let resp = self.call_blocking(
                    "Debugger.setBreakpointByUrl",
                    Value::Object(params),
                    timeout,
                )?;
                let bp_id = resp
                    .get("breakpointId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let key = format!("{file}:{line}");
                {
                    let (lock, _) = &*self.state;
                    lock.lock().unwrap().breakpoints.insert(key.clone(), bp_id.clone());
                }
                match cond {
                    Some(c) => Ok(format!("Breakpoint set at {key} if {c} (id={bp_id})")),
                    None => Ok(format!("Breakpoint set at {key} (id={bp_id})")),
                }
            }
            ParsedSb::Line(line) => {
                // Without a file we'd need the current script id; use
                // the top call-frame's scriptId.
                let (script_id, url) = {
                    let (lock, _) = &*self.state;
                    let s = lock.lock().unwrap();
                    match s.call_frames.first() {
                        Some(f) => (
                            f.get("location")
                                .and_then(|l| l.get("scriptId"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            f.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        ),
                        None => (String::new(), String::new()),
                    }
                };
                if script_id.is_empty() {
                    bail!("sb(line) without a file requires a current frame");
                }
                let mut params = serde_json::Map::new();
                params.insert(
                    "location".into(),
                    json!({ "scriptId": script_id, "lineNumber": line.saturating_sub(1) }),
                );
                if let Some(c) = cond {
                    params.insert("condition".into(), Value::String(c.to_string()));
                }
                let resp = self.call_blocking(
                    "Debugger.setBreakpoint",
                    Value::Object(params),
                    timeout,
                )?;
                let bp_id = resp
                    .get("breakpointId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let key = format!("{url}:{line}");
                {
                    let (lock, _) = &*self.state;
                    lock.lock().unwrap().breakpoints.insert(key.clone(), bp_id.clone());
                }
                Ok(format!("Breakpoint set at {key} (id={bp_id})"))
            }
            ParsedSb::Name(_) => {
                bail!("node-proto: function-name breakpoints unsupported — use `break file.js:line`");
            }
        }
    }

    fn evaluate(&self, expr: &str, timeout: Duration) -> Result<String> {
        let call_frame_id = {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            s.call_frames
                .first()
                .and_then(|f| f.get("callFrameId").and_then(|v| v.as_str()).map(String::from))
        };
        let (method, params) = match call_frame_id {
            Some(id) => (
                "Debugger.evaluateOnCallFrame",
                json!({ "callFrameId": id, "expression": expr, "returnByValue": true }),
            ),
            None => (
                "Runtime.evaluate",
                json!({ "expression": expr, "returnByValue": true }),
            ),
        };
        let resp = self.call_blocking(method, params, timeout)?;
        let result = resp.get("result").unwrap_or(&Value::Null);
        if let Some(exc) = resp.get("exceptionDetails") {
            return Ok(format!("[exception] {}", exc.get("text").and_then(|v| v.as_str()).unwrap_or("")));
        }
        let rendered = match result.get("value") {
            Some(v) if !v.is_null() => match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            },
            _ => result
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("undefined")
                .to_string(),
        };
        Ok(rendered)
    }

    /// Collect locals from the top call frame's scope chain. Walks
    /// each non-global scope's object (via `Runtime.getProperties`)
    /// and aggregates `{name: value}` pairs. Scope precedence is
    /// inner → outer; inner shadows outer for the same name.
    ///
    /// Values are simplified: primitives use their `value` payload,
    /// objects/functions use the Inspector `description` string
    /// (e.g. `"Array(3)"`, `"function fibonacci(n)"`) so the result
    /// stays human-readable. The output is a JSON object string —
    /// `parse_locals` round-trips it into the session DB's
    /// `locals_json` column.
    fn collect_locals(&self, timeout: Duration) -> Result<String> {
        let scopes: Vec<Value> = {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            s.call_frames
                .first()
                .and_then(|f| f.get("scopeChain").and_then(|v| v.as_array().cloned()))
                .unwrap_or_default()
        };
        if scopes.is_empty() {
            bail!("locals: not paused");
        }
        let mut out = serde_json::Map::new();
        // V8 scopeChain order is inner → outer:
        //   [block..., local, closure..., script, global]
        //
        // We want the user's "frame locals": parameters of the current
        // function, any `let`/`const` bindings in enclosing blocks, and
        // captured closure variables. We deliberately EXCLUDE `script`
        // and `global` scopes because those expose every top-level
        // function/class in the module (e.g. `ackermann`, `collatz`,
        // `factorial`, `fibonacci`) — surfacing those as "locals" on a
        // function-entry breakpoint was the original JS1 bug, where the
        // recorded locals were top-level function names instead of the
        // function's actual parameters.
        for scope in &scopes {
            let ty = scope.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if matches!(ty, "global" | "script" | "module") {
                continue;
            }
            let obj_id = scope
                .get("object")
                .and_then(|o| o.get("objectId"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if obj_id.is_empty() {
                continue;
            }
            let resp = self.call_blocking(
                "Runtime.getProperties",
                json!({
                    "objectId": obj_id,
                    "ownProperties": true,
                    "generatePreview": false,
                }),
                timeout,
            )?;
            let props = resp.get("result").and_then(|v| v.as_array());
            if let Some(props) = props {
                for p in props {
                    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if name.is_empty() {
                        continue;
                    }
                    // Drop function-typed properties: in a CommonJS
                    // module, V8 exposes every top-level `function` as
                    // a binding in the module-wrapper's closure scope,
                    // which would otherwise drown the real locals in
                    // `ackermann=[Function], collatz=[Function], ...`
                    // noise. Agents inspect functions via `dbg print`.
                    let prop_ty = p
                        .get("value")
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if prop_ty == "function" {
                        continue;
                    }
                    let raw = p.get("value").map(simplify_value).unwrap_or(Value::Null);
                    // Wrap as `{"value": "<rendered>"}` to match the
                    // canonical locals shape that PTY backends produce and
                    // that `crosstrack::locals_summary` reads via
                    // `v.get("value").as_str()`. Without this wrapper, the
                    // hit-record on the indexer side shows `name=` with
                    // empty values even though live `dbg locals` prints the
                    // numbers correctly.
                    let rendered = match &raw {
                        Value::String(s) => s.clone(),
                        Value::Null => "null".to_string(),
                        other => other.to_string(),
                    };
                    // Inner scope wins: if a name is already recorded
                    // from a more-nested scope, skip the outer one.
                    if out.contains_key(name) {
                        continue;
                    }
                    out.insert(
                        name.to_string(),
                        json!({ "value": rendered }),
                    );
                }
            }
        }
        Ok(Value::Object(out).to_string())
    }

    /// `list` — render a source window around a location.
    /// * `None`        → top-frame current line, ±10
    /// * `Some("f:N")` → line `N` of the script whose url tail matches
    ///                   `f`, ±10
    /// Source is fetched via `Debugger.getScriptSource` on first use
    /// per script and memoised thereafter.
    fn list_source(&self, loc: Option<&str>, timeout: Duration) -> Result<String> {
        const WINDOW: u32 = 10;
        let (script_id, centre_line) = match loc {
            None => {
                let (lock, _) = &*self.state;
                let s = lock.lock().unwrap();
                let top = s.call_frames.first().ok_or_else(|| anyhow!("list: not paused"))?;
                let location = top.get("location");
                let sid = location
                    .and_then(|l| l.get("scriptId"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let line = location
                    .and_then(|l| l.get("lineNumber"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32
                    + 1;
                (sid, line)
            }
            Some(spec) => {
                let (file, line_s) = spec
                    .rsplit_once(':')
                    .ok_or_else(|| anyhow!("list: expected `file:line`, got `{spec}`"))?;
                let line: u32 = line_s
                    .trim()
                    .parse()
                    .map_err(|_| anyhow!("list: invalid line `{line_s}`"))?;
                let sid = {
                    let (lock, _) = &*self.state;
                    let s = lock.lock().unwrap();
                    s.script_urls
                        .iter()
                        .find(|(_, url)| url.ends_with(file))
                        .map(|(id, _)| id.clone())
                };
                let sid = sid.ok_or_else(|| anyhow!("list: no script url matches `{file}`"))?;
                (sid, line)
            }
        };
        if script_id.is_empty() {
            bail!("list: unknown scriptId");
        }

        // Cache lookup; fetch via Debugger.getScriptSource on miss.
        let source: String = {
            let (lock, _) = &*self.state;
            let s = lock.lock().unwrap();
            s.script_sources.get(&script_id).cloned()
        }
        .map(Ok)
        .unwrap_or_else(|| -> Result<String> {
            let resp = self.call_blocking(
                "Debugger.getScriptSource",
                json!({ "scriptId": script_id }),
                timeout,
            )?;
            let src = resp
                .get("scriptSource")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let (lock, _) = &*self.state;
            lock.lock().unwrap().script_sources.insert(script_id.clone(), src.clone());
            Ok(src)
        })?;

        let lines: Vec<&str> = source.split('\n').collect();
        if lines.is_empty() {
            return Ok(String::new());
        }
        let centre = centre_line.max(1) as usize;
        let start = centre.saturating_sub(WINDOW as usize + 1);
        let end = (centre + WINDOW as usize).min(lines.len());
        let mut out = String::new();
        for (i, line) in lines.iter().enumerate().take(end).skip(start) {
            let n = i + 1;
            let marker = if n as u32 == centre_line { "->" } else { "  " };
            out.push_str(&format!("{marker} {n:4}  {line}\n"));
        }
        Ok(out.trim_end().to_string())
    }

    fn format_backtrace(&self) -> String {
        let (lock, _) = &*self.state;
        let s = lock.lock().unwrap();
        if s.call_frames.is_empty() {
            return "(no frames — program not paused)".to_string();
        }
        let mut out = String::new();
        for (i, f) in s.call_frames.iter().enumerate() {
            let func = f.get("functionName").and_then(|v| v.as_str()).filter(|x| !x.is_empty()).unwrap_or("(anonymous)");
            let loc = f.get("location");
            let line = loc.and_then(|l| l.get("lineNumber")).and_then(|v| v.as_u64()).unwrap_or(0) + 1;
            let script_id = loc.and_then(|l| l.get("scriptId")).and_then(|v| v.as_str()).unwrap_or("");
            let url = s.script_urls.get(script_id).cloned()
                .or_else(|| f.get("url").and_then(|v| v.as_str()).map(String::from))
                .unwrap_or_default();
            out.push_str(&format!("#{i} {func} at {url}:{line}\n"));
        }
        out.trim_end().to_string()
    }

    fn format_breakpoints(&self) -> String {
        let (lock, _) = &*self.state;
        let s = lock.lock().unwrap();
        if s.breakpoints.is_empty() {
            return "(no breakpoints set)".to_string();
        }
        let mut out = String::new();
        for (key, id) in &s.breakpoints {
            out.push_str(&format!("{key}  [{id}]\n"));
        }
        out.trim_end().to_string()
    }
}


/// Render a `Runtime.RemoteObject` as a JSON-friendly value for the
/// locals dump. Primitives come through with their real value;
/// everything else collapses to a short descriptive string so the
/// locals column stays compact and agent-readable.
fn simplify_value(v: &Value) -> Value {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "undefined" => Value::Null,
        "string" | "number" | "boolean" => v.get("value").cloned().unwrap_or(Value::Null),
        "bigint" => {
            // BigInts come over as {type:"bigint", unserializableValue:"42n"}
            v.get("unserializableValue")
                .cloned()
                .or_else(|| v.get("description").cloned())
                .unwrap_or(Value::Null)
        }
        "function" => {
            // Function refs (very common in closure scope) expose
            // their full source via `description`. That's agent noise
            // — collapse to a short tag so locals stays scannable.
            let name = v
                .get("className")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("function");
            Value::String(format!("[{name}]"))
        }
        _ => v
            .get("description")
            .cloned()
            .or_else(|| v.get("value").cloned())
            .unwrap_or(Value::Null),
    }
}

impl DebuggerIo for InspectorTransport {
    fn send_and_wait(&self, cmd: &str, timeout: Duration) -> Result<String> {
        self.run_command(cmd, timeout)
    }

    fn drain_pending(&self) -> Option<String> {
        // Inspector events go straight to the log and to state.
        // Nothing synchronous to drain at command boundaries.
        None
    }

    fn wait_for_prompt(&self, timeout: Duration) -> Result<String> {
        // After --inspect-brk we receive a `Debugger.paused` with
        // reason "Break on start" almost immediately after enabling
        // Debugger. Poll state until paused.
        let deadline = Instant::now() + timeout;
        loop {
            {
                let (lock, _) = &*self.state;
                let s = lock.lock().unwrap();
                if s.paused || !s.alive {
                    return Ok(String::new());
                }
            }
            if Instant::now() >= deadline {
                bail!("timeout waiting for initial paused state");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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
        // Wake any exec() parked waiting for a pending_hit that will
        // never arrive now that we're tearing down.
        {
            let (lock, cvar) = &*self.state;
            let mut s = lock.lock().unwrap();
            s.alive = false;
            cvar.notify_all();
        }
        // Best-effort SIGTERM to node, SIGKILL after 500ms.
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
            CanonicalReq::Break { loc, cond, log } => {
                // Inspector has no logpoint concept — fall back to the
                // native string so the node-proto backend can surface
                // `unsupported` via its own vocabulary.
                if log.is_some() {
                    return None;
                }
                let parsed = match loc {
                    BreakLoc::FileLine { file, line } => ParsedSb::FileLine {
                        file: file.clone(),
                        line: *line,
                    },
                    BreakLoc::Fqn(name) => ParsedSb::Name(name.clone()),
                    BreakLoc::ModuleMethod { .. } => return None,
                };
                Some(self.set_breakpoint(parsed, cond.as_deref(), timeout))
            }
        }
    }
}

impl Drop for InspectorTransport {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.driver_tx.send(DriverCmd::Shutdown);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Read node's stderr line-by-line until we find the "Debugger
/// listening on ws://…" announcement, then return that URL plus the
/// underlying reader (which will be handed to the pipe-forwarder for
/// any post-handshake stderr output).
fn scrape_ws_url(stderr: ChildStderr, timeout: Duration) -> Result<String> {
    let fd = stderr.as_raw_fd();
    nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK))?;
    let mut reader = BufReader::new(stderr);
    let deadline = Instant::now() + timeout;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => bail!("node exited before announcing debugger port"),
            Ok(_) => {
                if let Some(url) = extract_ws_url(&line) {
                    return Ok(url);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for node's Debugger-listening line");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn extract_ws_url(line: &str) -> Option<String> {
    let start = line.find("ws://")?;
    let tail = &line[start..];
    let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
    Some(tail[..end].to_string())
}

fn set_nonblocking(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>) -> Result<()> {
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => s.set_nonblocking(true)?,
        _ => bail!("unexpected TLS stream for ws://"),
    }
    Ok(())
}

/// Driver loop — sole owner of the WebSocket. Multiplexes:
///   * outbound: pulls `DriverCmd::Call` off the channel, writes
///     JSON-RPC frames with monotonic `id`, parks a resp sender in
///     `pending`.
///   * inbound: reads WS messages, routes responses to the matching
///     `pending[id]`, dispatches async events (Debugger.paused,
///     Runtime.consoleAPICalled, Debugger.scriptParsed) into the
///     state + event log.
fn driver_loop(
    mut ws: WebSocket<MaybeTlsStream<TcpStream>>,
    rx: Receiver<DriverCmd>,
    state: Arc<(Mutex<State>, Condvar)>,
    log: LogHandle,
    shutdown: Arc<AtomicBool>,
) {
    let mut next_id: u64 = 1;
    let mut pending: HashMap<u64, Sender<Result<Value, String>>> = HashMap::new();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Drain inbound WS messages.
        loop {
            match ws.read() {
                Ok(Message::Text(t)) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        dispatch_incoming(v, &mut pending, &state, &log);
                    }
                }
                Ok(Message::Binary(_)) => {}
                Ok(Message::Close(_)) => {
                    drain_pending(&mut pending);
                    mark_dead(&state);
                    return;
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(ref e))
                    if e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    break;
                }
                Err(_) => {
                    drain_pending(&mut pending);
                    mark_dead(&state);
                    return;
                }
            }
        }

        // Drain outbound commands (non-blocking).
        loop {
            match rx.try_recv() {
                Ok(DriverCmd::Shutdown) => {
                    let _ = ws.close(None);
                    drain_pending(&mut pending);
                    mark_dead(&state);
                    return;
                }
                Ok(DriverCmd::Call { method, params, resp }) => {
                    let id = next_id;
                    next_id += 1;
                    let frame = json!({ "id": id, "method": method, "params": params });
                    if let Err(e) = ws.send(Message::Text(frame.to_string().into())) {
                        let _ = resp.send(Err(format!("ws send failed: {e}")));
                        continue;
                    }
                    pending.insert(id, resp);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let _ = ws.close(None);
                    drain_pending(&mut pending);
                    mark_dead(&state);
                    return;
                }
            }
        }

        std::thread::sleep(Duration::from_millis(5));
    }
}

fn drain_pending(pending: &mut HashMap<u64, Sender<Result<Value, String>>>) {
    for (_id, tx) in pending.drain() {
        let _ = tx.send(Err("Inspector driver thread exited".into()));
    }
}

fn mark_dead(state: &Arc<(Mutex<State>, Condvar)>) {
    let (lock, cvar) = &**state;
    let mut s = lock.lock().unwrap();
    s.alive = false;
    cvar.notify_all();
}

/// Handle a single inbound Inspector message. Responses land in
/// `pending[id]`. Events (no `id`, has `method`) update the state and
/// emit log entries.
fn dispatch_incoming(
    v: Value,
    pending: &mut HashMap<u64, Sender<Result<Value, String>>>,
    state: &Arc<(Mutex<State>, Condvar)>,
    log: &LogHandle,
) {
    // Response to a prior request.
    if let Some(id) = v.get("id").and_then(|v| v.as_u64()) {
        if let Some(tx) = pending.remove(&id) {
            if let Some(err) = v.get("error") {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string();
                let _ = tx.send(Err(msg));
            } else {
                let result = v.get("result").cloned().unwrap_or(Value::Null);
                let _ = tx.send(Ok(result));
            }
        }
        return;
    }

    // Async event.
    let method = match v.get("method").and_then(|m| m.as_str()) {
        Some(m) => m,
        None => return,
    };
    let params = v.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "Debugger.scriptParsed" => {
            let script_id = params.get("scriptId").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let url = params.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if !script_id.is_empty() {
                let (lock, _) = &**state;
                lock.lock().unwrap().script_urls.insert(script_id, url);
            }
        }
        "Debugger.paused" => {
            // Node is single-threaded: a `Debugger.paused` can only
            // arrive as the response to an execution command
            // (resume/step), so the daemon's capture path is always
            // in-band. We stash the structured hit for `pending_hit()`
            // and leave the timeline Stop event to the daemon — that
            // one carries hit_seq, ours wouldn't.
            let call_frames = params
                .get("callFrames")
                .cloned()
                .and_then(|v| v.as_array().cloned().map(|a| a.iter().cloned().collect::<Vec<_>>()))
                .unwrap_or_default();
            let hit = build_hit_event(&call_frames, state);
            let (lock, cvar) = &**state;
            let mut s = lock.lock().unwrap();
            s.paused = true;
            s.call_frames = call_frames;
            s.pending_hit = Some(hit);
            cvar.notify_all();
        }
        "Debugger.resumed" => {
            let (lock, _) = &**state;
            let mut s = lock.lock().unwrap();
            s.paused = false;
            s.call_frames.clear();
        }
        "Runtime.consoleAPICalled" => {
            // Concatenate arg values into a single line. This is the
            // Phase 4c payoff: program stdout comes out as a distinct
            // EventKind::Stdout stream.
            let mut line = String::new();
            if let Some(args) = params.get("args").and_then(|v| v.as_array()) {
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        line.push(' ');
                    }
                    let piece = a
                        .get("value")
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .or_else(|| {
                            a.get("description")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                        })
                        .unwrap_or_default();
                    line.push_str(&piece);
                }
            }
            line.push('\n');
            log.push(EventKind::Stdout, line.into_bytes());
        }
        "Runtime.executionContextCreated" => {
            // First context is the main realm; subsequent ones are
            // worker_threads, vm modules, etc. — ignore those.
            if let Some(id) = params.get("context").and_then(|c| c.get("id")).and_then(|v| v.as_i64()) {
                let (lock, _) = &**state;
                let mut s = lock.lock().unwrap();
                if s.main_context_id.is_none() {
                    s.main_context_id = Some(id);
                }
            }
        }
        "Runtime.executionContextDestroyed" => {
            // Only tear down when the *main* context dies. A
            // worker_thread finishing should not kill the session.
            let destroyed_id = params
                .get("executionContextId")
                .and_then(|v| v.as_i64());
            let is_main = {
                let (lock, _) = &**state;
                let s = lock.lock().unwrap();
                s.main_context_id.is_some() && s.main_context_id == destroyed_id
            };
            if is_main {
                mark_dead(state);
            }
        }
        _ => {}
    }
}

fn build_hit_event(frames: &[Value], state: &Arc<(Mutex<State>, Condvar)>) -> HitEvent {
    let top = frames.first();
    let function_name = top
        .and_then(|f| f.get("functionName").and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .unwrap_or("(anonymous)")
        .to_string();
    let (script_id, line_no) = top
        .and_then(|f| f.get("location"))
        .map(|l| {
            (
                l.get("scriptId").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                l.get("lineNumber").and_then(|v| v.as_u64()).unwrap_or(0) as u32 + 1,
            )
        })
        .unwrap_or_default();
    let url = {
        let (lock, _) = &**state;
        let s = lock.lock().unwrap();
        s.script_urls.get(&script_id).cloned().unwrap_or_default()
    };
    // Pull file path from the url — strip `file://` if present.
    let file = url
        .strip_prefix("file://")
        .unwrap_or(&url)
        .to_string();
    HitEvent {
        location_key: if file.is_empty() {
            format!("scriptId={script_id}:{line_no}")
        } else {
            format!("{file}:{line_no}")
        },
        thread: None,
        frame_symbol: Some(function_name),
        file: if file.is_empty() { None } else { Some(file) },
        line: Some(line_no),
    }
}

#[derive(Debug)]
enum ParsedSb {
    FileLine { file: String, line: u32 },
    Line(u32),
    Name(String),
}

fn parse_sb(cmd: &str) -> Option<(ParsedSb, Option<String>)> {
    // sb('file.js', 10)  |  sb(10)  |  sb('funcName')
    // Optionally followed by ` if <expr>`.
    let rest = cmd.strip_prefix("sb(")?;
    let (inner, cond) = match rest.find(") if ") {
        Some(i) => (&rest[..i], Some(rest[i + 5..].trim().to_string())),
        None => (rest.strip_suffix(')')?, None),
    };
    let inner = inner.trim();
    if inner.is_empty() {
        return None;
    }
    let parsed = if let Some((a, b)) = inner.split_once(',') {
        let file = a.trim().trim_matches('\'').trim_matches('"').to_string();
        let line: u32 = b.trim().parse().ok()?;
        ParsedSb::FileLine { file, line }
    } else if let Ok(line) = inner.parse::<u32>() {
        ParsedSb::Line(line)
    } else {
        let name = inner.trim_matches('\'').trim_matches('"').to_string();
        ParsedSb::Name(name)
    };
    Some((parsed, cond))
}

fn regex_escape(s: &str) -> String {
    let specials = r".\+*?()[]{}|^$";
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if specials.contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sb_file_line() {
        match parse_sb("sb('app.js', 10)") {
            Some((ParsedSb::FileLine { file, line }, None)) => {
                assert_eq!(file, "app.js");
                assert_eq!(line, 10);
            }
            other => panic!("expected FileLine, got {other:?}"),
        }
    }

    #[test]
    fn parse_sb_line_only() {
        assert!(matches!(parse_sb("sb(42)"), Some((ParsedSb::Line(42), None))));
    }

    #[test]
    fn parse_sb_name() {
        match parse_sb("sb('handleRequest')") {
            Some((ParsedSb::Name(n), None)) => assert_eq!(n, "handleRequest"),
            other => panic!("expected Name, got {other:?}"),
        }
    }

    #[test]
    fn parse_sb_conditional() {
        match parse_sb("sb('app.js', 10) if x > 5") {
            Some((ParsedSb::FileLine { file, line }, Some(cond))) => {
                assert_eq!(file, "app.js");
                assert_eq!(line, 10);
                assert_eq!(cond, "x > 5");
            }
            other => panic!("expected FileLine with cond, got {other:?}"),
        }
    }

    #[test]
    fn extract_ws_url_works() {
        let line = "Debugger listening on ws://127.0.0.1:12345/abcd-uuid\n";
        assert_eq!(
            extract_ws_url(line).as_deref(),
            Some("ws://127.0.0.1:12345/abcd-uuid"),
        );
    }

    #[test]
    fn regex_escape_specials() {
        assert_eq!(regex_escape("/tmp/foo.js"), "/tmp/foo\\.js");
    }
}
