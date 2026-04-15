//! Canonical command dispatchers.
//!
//! Two layers:
//!   * `debug` — canonical debug ops (`break`, `step`, `locals`, ...)
//!     that translate to a native debugger command via CanonicalOps.
//!   * `crosstrack` — cross-track queries (`hits`, `hit-diff`, `cross`,
//!     `disasm`, `source`, ...) that operate on the SessionDb and
//!     optionally reach back to the live debugger for `at-hit`.
//!
//! The top-level `dispatch` tries `debug` first, then `crosstrack`,
//! and hands back a unified `Dispatched` verdict.

pub mod crosstrack;
pub mod debug;
pub mod lifecycle;

use crate::backend::Backend;

/// Unified dispatch outcome. Daemon consumes one of these per command.
pub enum Dispatched {
    /// Send a native debugger command to the PTY. Canonical verbs
    /// set `decorate=true` so the daemon prepends `[via <tool>]`;
    /// `raw` passthrough sets `decorate=false`.
    Native {
        canonical_op: &'static str,
        native_cmd: String,
        decorate: bool,
    },
    /// Pre-computed response — no PTY roundtrip.
    Immediate(String),
    /// Cross-track query — daemon runs it against the SessionDb.
    Query(crosstrack::Query),
    /// Session-lifecycle command (sessions/save/prune/diff) — daemon
    /// resolves the `.dbg/sessions/` path and optional ATTACH-for-diff.
    Lifecycle(lifecycle::Lifecycle),
    /// Not a canonical verb — daemon runs the legacy passthrough path.
    Fallthrough,
}

/// Top-level dispatcher. Vocab resolution order:
///   1. lifecycle verbs (sessions / save / prune / diff)
///   2. crosstrack verbs (hits / hit-diff / cross / disasm / …)
///   3. canonical debug verbs (break / step / continue / …)
///   4. Fallthrough → daemon runs the legacy passthrough path.
pub fn dispatch(input: &str, backend: &dyn Backend) -> Dispatched {
    if let Some(d) = lifecycle::try_dispatch(input) {
        return d;
    }
    if let Some(d) = crosstrack::try_dispatch(input) {
        return d;
    }
    debug::dispatch_to(input, backend)
}

/// Dispatch variant for contexts without a live backend (e.g.
/// `dbg replay`). Returns `None` for debug verbs (step/continue/…)
/// since those require a live debugger.
pub fn dispatch_no_backend(input: &str) -> Option<Dispatched> {
    if let Some(d) = lifecycle::try_dispatch(input) {
        return Some(d);
    }
    if let Some(d) = crosstrack::try_dispatch(input) {
        return Some(d);
    }
    None
}
