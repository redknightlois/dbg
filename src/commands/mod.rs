//! Canonical command dispatchers.
//!
//! Two layers:
//!   * `debug` — canonical debug ops (`break`, `step`, `locals`, ...)
//!     that translate to a native debugger command via CanonicalOps.
//!   * `crosstrack` — cross-track queries (`hits`, `hit-diff`, `cross`,
//!     `disasm`, `source`, ...) that operate on the SessionDb and
//!     optionally reach back to the live debugger for `at-hit`.
//!
//! The top-level dispatcher first routes lifecycle, instruction-hit, and
//! cross-track commands. It then routes commands that require a live backend.

pub mod crosstrack;
pub mod debug;
pub mod insnhits;
pub mod lifecycle;

use crate::backend::{Backend, CanonicalReq};

/// Unified dispatch outcome. Daemon consumes one of these per command.
pub enum Dispatched {
    /// Send a native debugger command to the PTY. Canonical verbs
    /// set `decorate=true` so the daemon prepends `[via <tool>]`;
    /// `raw` passthrough sets `decorate=false`.
    Native {
        canonical_op: &'static str,
        native_cmd: String,
        decorate: bool,
        /// Structured form of the request for transports that can
        /// consume it directly (DAP, Inspector). PTY backends ignore
        /// this and rely on `native_cmd`. `None` for ops that have no
        /// parse-back problem (step/continue/…) or for `raw`
        /// passthrough.
        structured: Option<CanonicalReq>,
    },
    /// Pre-computed response — no PTY roundtrip.
    Immediate(String),
    /// Cross-track query — daemon runs it against the SessionDb.
    Query(crosstrack::Query),
    /// Session-lifecycle command (sessions/save/prune/diff) — daemon
    /// resolves the `.dbg/sessions/` path and optional ATTACH-for-diff.
    Lifecycle(lifecycle::Lifecycle),
    /// `insn-hits <target>` request — planner picks a backend and the
    /// daemon hands the SessionDb to the collector.
    InsnHits(insnhits::Request),
    /// Not a canonical verb — daemon runs the legacy passthrough path.
    Fallthrough,
}

/// Top-level dispatcher. Vocab resolution order:
///   1. lifecycle verbs (sessions / save / prune / diff)
///   2. crosstrack verbs (hits / hit-diff / cross / disasm / …)
///   3. canonical debug verbs (break / step / continue / …)
///   4. Fallthrough → daemon runs the legacy passthrough path.
pub fn dispatch(input: &str, backend: &dyn Backend) -> Dispatched {
    dispatch_no_backend(input).unwrap_or_else(|| debug::dispatch_to(input, backend))
}

/// Dispatch commands that do not require a live backend. This is also the
/// common first stage for live dispatch, so live and replay sessions use the
/// same command precedence.
pub fn dispatch_no_backend(input: &str) -> Option<Dispatched> {
    lifecycle::try_dispatch(input)
        .or_else(|| insnhits::try_dispatch(input))
        .or_else(|| crosstrack::try_dispatch(input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::lldb::LldbBackend;

    #[test]
    fn live_dispatch_routes_each_command_family() {
        let backend = LldbBackend;

        assert!(matches!(
            dispatch("sessions", &backend),
            Dispatched::Lifecycle(_)
        ));
        assert!(matches!(
            dispatch("insn-hits main", &backend),
            Dispatched::InsnHits(_)
        ));
        assert!(matches!(
            dispatch("hits main.rs:1", &backend),
            Dispatched::Query(_)
        ));
        assert!(matches!(
            dispatch("continue", &backend),
            Dispatched::Native {
                canonical_op: "continue",
                ..
            }
        ));
        assert!(matches!(
            dispatch("an-unknown-command", &backend),
            Dispatched::Fallthrough
        ));
    }

    #[test]
    fn backend_free_dispatch_matches_live_precedence() {
        let backend = LldbBackend;
        let inputs = ["sessions", "insn-hits main", "hits main.rs:1"];

        for input in inputs {
            let live = dispatch(input, &backend);
            let replay = dispatch_no_backend(input).expect("backend-free command");
            assert_eq!(variant_name(&live), variant_name(&replay), "input: {input}");
        }
        assert!(dispatch_no_backend("continue").is_none());
        assert!(dispatch_no_backend("an-unknown-command").is_none());
    }

    fn variant_name(dispatched: &Dispatched) -> &'static str {
        match dispatched {
            Dispatched::Native { .. } => "native",
            Dispatched::Immediate(_) => "immediate",
            Dispatched::Query(_) => "query",
            Dispatched::Lifecycle(_) => "lifecycle",
            Dispatched::InsnHits(_) => "insn-hits",
            Dispatched::Fallthrough => "fallthrough",
        }
    }
}
