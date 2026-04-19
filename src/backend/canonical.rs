//! Canonical debug-operation vocabulary.
//!
//! A thin translation layer over the native debugger's own command
//! surface. Every backend in the debug family (lldb, pdb, delve,
//! netcoredbg, jdb, ...) implements `CanonicalOps` so the daemon can
//! route `dbg break / step / next / locals / print / ...` uniformly
//! regardless of the underlying tool.
//!
//! Principles (see `plans/sorted-nibbling-lovelace.md`):
//!
//!   * **Transparency.** Every op emitted to the debugger is labelled
//!     via `tool_name()` / `tool_version()` so the daemon can prefix
//!     output with `[via <tool> <version>]`. Agents MUST still be able
//!     to drop into the native vocabulary via `dbg raw <text>` — the
//!     canonical layer is additive, not a replacement.
//!   * **Unsupported is explicit.** Backends that lack an operation
//!     (phpdbg has no watchpoints; pdb has no threads) return
//!     `Err(unsupported(...))` with a clean hint pointing the agent at
//!     `dbg raw`. No silent no-ops.
//!   * **Parsing is the inverse of emission.** After issuing an op the
//!     daemon feeds the raw output back to `parse_hit` / `parse_locals`
//!     so breakpoint hits and frame locals get captured into the
//!     SessionDb uniformly across backends.

use serde_json::Value;

/// Where to stop. `dbg break` parses one of these out of the user's
/// argument (`file:line`, bare symbol name, or `module!method`) and
/// hands it to the backend's `op_break`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BreakLoc {
    FileLine { file: String, line: u32 },
    Fqn(String),
    ModuleMethod { module: String, method: String },
}

impl BreakLoc {
    /// Parse the `<loc>` argument to `dbg break`. Precedence:
    /// 1. `module!method` (explicit)
    /// 2. `file:line` (line is all digits)
    /// 3. anything else → `Fqn`
    pub fn parse(spec: &str) -> Self {
        if let Some((m, meth)) = spec.split_once('!') {
            if !m.is_empty() && !meth.is_empty() {
                return BreakLoc::ModuleMethod {
                    module: m.to_string(),
                    method: meth.to_string(),
                };
            }
        }
        if let Some((f, l)) = spec.rsplit_once(':') {
            if !l.is_empty() && l.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(line) = l.parse::<u32>() {
                    return BreakLoc::FileLine {
                        file: f.to_string(),
                        line,
                    };
                }
            }
        }
        BreakLoc::Fqn(spec.to_string())
    }

    /// The string we store under `breakpoint_hits.location_key`. Same
    /// key is produced whether a hit came from a `file:line` breakpoint
    /// or a symbol one — stable for cross-session diffing.
    pub fn location_key(&self) -> String {
        match self {
            BreakLoc::FileLine { file, line } => format!("{file}:{line}"),
            BreakLoc::Fqn(s) => s.clone(),
            BreakLoc::ModuleMethod { module, method } => format!("{module}!{method}"),
        }
    }
}

/// A breakpoint-id for `dbg unbreak`. Backends expose integer ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BreakId(pub u32);

/// Structured canonical request carried alongside the native-command
/// string. Transports that can consume the structured form directly
/// (DAP, Inspector) use it to skip the string → regex round-trip; PTY
/// backends ignore it and fall back to parsing the native command.
///
/// Only ops with a real parse-back problem live here. Simple verbs like
/// `continue` / `step` have no structured data to recover and stay
/// string-only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanonicalReq {
    Break {
        loc: BreakLoc,
        cond: Option<String>,
        log: Option<String>,
    },
}

/// Parsed out of the PTY read loop after an op returns. A `Some` means
/// "the debugger just stopped"; the daemon then issues follow-up
/// `op_locals` + `op_stack` and persists a `breakpoint_hits` row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HitEvent {
    /// Canonical location key. See `BreakLoc::location_key`.
    pub location_key: String,
    /// Thread or goroutine identifier, when the backend reports it.
    pub thread: Option<String>,
    /// The frame's function symbol as reported by the tool, if any.
    /// Not yet canonicalized — that happens in the daemon after
    /// piping through the appropriate `Canonicalizer`.
    pub frame_symbol: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
}

/// The canonical vocabulary. Each method returns the native-tool
/// command text that the daemon will send to the debugger PTY. The
/// daemon wraps the response in `[via <tool> <version>]` headers and,
/// for stopping ops, feeds the response through `parse_hit`.
pub trait CanonicalOps: Send + Sync {
    /// Stable name — goes into the `[via ...]` header on every output.
    fn tool_name(&self) -> &'static str;

    /// Best-effort version string (e.g., `"lldb-20 20.1.0"`). Called
    /// once per session; fine to shell out.
    fn tool_version(&self) -> Option<String> {
        None
    }

    // ------------------------------------------------------------
    // Breakpoint management
    // ------------------------------------------------------------

    /// Default emits DAP-style `break file:line` / `bfn name`. PTY
    /// backends with different breakpoint syntax override this.
    fn op_break(&self, loc: &BreakLoc) -> anyhow::Result<String> {
        Ok(match loc {
            BreakLoc::FileLine { file, line } => format!("break {file}:{line}"),
            BreakLoc::Fqn(name) => format!("bfn {name}"),
            BreakLoc::ModuleMethod { module, method } => format!("bfn {module}::{method}"),
        })
    }

    /// Conditional breakpoint. Default falls back to `op_break` when
    /// `cond` is empty; backends that can't express conditions return
    /// `unsupported(...)`. DAP and Inspector backends override this to
    /// pass the condition through the wire.
    fn op_break_conditional(&self, loc: &BreakLoc, cond: &str) -> anyhow::Result<String> {
        if cond.is_empty() {
            self.op_break(loc)
        } else {
            Err(unsupported(self.tool_name(), "conditional breakpoints"))
        }
    }
    /// Logpoint: a breakpoint that prints `msg` without stopping the
    /// debuggee. Templates typically interpolate `{expr}` — the
    /// underlying adapter defines the exact syntax. Backends that
    /// can't emit logpoints return `unsupported`.
    fn op_break_log(&self, _loc: &BreakLoc, _msg: &str) -> anyhow::Result<String> {
        Err(unsupported(self.tool_name(), "logpoints"))
    }
    fn op_unbreak(&self, id: BreakId) -> anyhow::Result<String> {
        Ok(format!("breakpoint delete {}", id.0))
    }
    fn op_breaks(&self) -> anyhow::Result<String> {
        Ok("breakpoint list".into())
    }

    // ------------------------------------------------------------
    // Execution control
    // ------------------------------------------------------------

    fn op_run(&self, _args: &[String]) -> anyhow::Result<String> {
        Ok("continue".into())
    }
    fn op_continue(&self) -> anyhow::Result<String> {
        Ok("continue".into())
    }
    fn op_step(&self) -> anyhow::Result<String> {
        Ok("step".into())
    }
    fn op_next(&self) -> anyhow::Result<String> {
        Ok("next".into())
    }
    fn op_finish(&self) -> anyhow::Result<String> {
        Ok("out".into())
    }

    /// Async interrupt — ask a running target to stop. Backends that
    /// can't interrupt (most PTY-based tools, which are pause-to-prompt
    /// anyway) return `unsupported`.
    fn op_pause(&self) -> anyhow::Result<String> {
        Err(unsupported(self.tool_name(), "pause"))
    }

    /// Restart the debuggee from scratch. DAP has a `restart` request;
    /// other backends typically need a session tear-down + relaunch.
    fn op_restart(&self) -> anyhow::Result<String> {
        Err(unsupported(self.tool_name(), "restart"))
    }

    // ------------------------------------------------------------
    // Inspection
    // ------------------------------------------------------------

    fn op_stack(&self, _n: Option<u32>) -> anyhow::Result<String> {
        Ok("backtrace".into())
    }

    /// Per-op output post-processor. Runs *after* the backend's
    /// generic `clean(...)` pass and *before* the `[via <tool>]`
    /// decoration. Defaults to the identity transform — backends
    /// override this to drop noise that is specific to a single op
    /// (e.g. the internal `exec(...)` frame that pdb's `where` leaks
    /// at the bottom of every stack walk).
    ///
    /// The `canonical_op` argument is the daemon's stamp (`"stack"`,
    /// `"locals"`, …); backends dispatch on it so one override can
    /// cover several ops. No wiring change is required at call
    /// sites — the default is a no-op.
    fn postprocess_output(&self, _canonical_op: &str, out: &str) -> String {
        out.to_string()
    }
    fn op_frame(&self, n: u32) -> anyhow::Result<String> {
        Ok(format!("frame {n}"))
    }
    fn op_locals(&self) -> anyhow::Result<String> {
        Ok("locals".into())
    }
    fn op_print(&self, expr: &str) -> anyhow::Result<String> {
        Ok(format!("print {expr}"))
    }

    /// Mutate a live variable or expression. `lhs` is the target
    /// (usually a name, member access, or indexing); `rhs` is any
    /// expression the adapter can evaluate. Backends without assignment
    /// support return `unsupported`.
    fn op_set(&self, _lhs: &str, _rhs: &str) -> anyhow::Result<String> {
        Err(unsupported(self.tool_name(), "variable assignment"))
    }

    // ------------------------------------------------------------
    // Optional ops — backends lacking them return `unsupported(...)`.
    // ------------------------------------------------------------

    fn op_watch(&self, _expr: &str) -> anyhow::Result<String> {
        Err(unsupported(self.tool_name(), "watchpoints"))
    }
    fn op_threads(&self) -> anyhow::Result<String> {
        Err(unsupported(self.tool_name(), "thread listing"))
    }
    fn op_thread(&self, _n: u32) -> anyhow::Result<String> {
        Err(unsupported(self.tool_name(), "thread switching"))
    }
    fn op_list(&self, _loc: Option<&str>) -> anyhow::Result<String> {
        Err(unsupported(self.tool_name(), "source listing"))
    }

    /// Configure exception breakpoints. `filters` names the classes of
    /// throws that should stop the debuggee; common values are
    /// `uncaught`, `caught`, `raised`, `userUnhandled` depending on the
    /// adapter. An empty slice clears all exception breakpoints.
    fn op_catch(&self, _filters: &[String]) -> anyhow::Result<String> {
        Err(unsupported(self.tool_name(), "exception breakpoints"))
    }

    // ------------------------------------------------------------
    // Event parsing — called by the daemon on every PTY response.
    // Default implementations return `None` / `None` so profiler-style
    // backends can inherit sanely.
    // ------------------------------------------------------------

    fn parse_hit(&self, _output: &str) -> Option<HitEvent> {
        None
    }
    fn parse_locals(&self, _output: &str) -> Option<Value> {
        None
    }

    /// Whether the daemon should auto-send `op_locals()` + `op_stack()`
    /// after a hit to populate `breakpoint_hits.locals_json`. Backends
    /// whose PTY state is fragile (jdb, ghci, netcoredbg CLI mode)
    /// return `false` — agents use `dbg locals` explicitly instead.
    fn auto_capture_locals(&self) -> bool {
        true
    }
}

/// Construct the standard "op X not supported by tool Y" error. The
/// message always points the agent at `dbg raw` so they can drop into
/// the native vocabulary without guessing.
pub fn unsupported(tool: &'static str, what: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{what} not supported by {tool}: use `dbg raw <native-command>` to send backend-specific commands"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_file_line() {
        assert_eq!(
            BreakLoc::parse("src/main.rs:42"),
            BreakLoc::FileLine { file: "src/main.rs".into(), line: 42 }
        );
    }

    #[test]
    fn parse_fqn() {
        assert_eq!(
            BreakLoc::parse("foo::bar::baz"),
            BreakLoc::Fqn("foo::bar::baz".into())
        );
    }

    #[test]
    fn parse_module_method() {
        assert_eq!(
            BreakLoc::parse("libfoo.so!bar"),
            BreakLoc::ModuleMethod {
                module: "libfoo.so".into(),
                method: "bar".into(),
            }
        );
    }

    #[test]
    fn parse_colon_in_fqn_not_mistaken_for_line() {
        assert_eq!(
            BreakLoc::parse("foo::bar"),
            BreakLoc::Fqn("foo::bar".into())
        );
    }

    #[test]
    fn parse_empty_module_falls_through_to_filename() {
        assert_eq!(
            BreakLoc::parse("!foo"),
            BreakLoc::Fqn("!foo".into())
        );
    }

    #[test]
    fn location_key_stable_across_forms() {
        let fl = BreakLoc::FileLine { file: "m.c".into(), line: 1 };
        assert_eq!(fl.location_key(), "m.c:1");
        assert_eq!(BreakLoc::Fqn("main".into()).location_key(), "main");
        assert_eq!(
            BreakLoc::ModuleMethod { module: "m".into(), method: "f".into() }.location_key(),
            "m!f"
        );
    }

    #[test]
    fn unsupported_mentions_raw_escape() {
        let e = unsupported("pdb", "watchpoints").to_string();
        assert!(e.contains("pdb"));
        assert!(e.contains("watchpoints"));
        assert!(e.contains("dbg raw"));
    }
}
