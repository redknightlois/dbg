//! SessionDb — SQLite-backed session store shared by the debug and
//! profile tracks.
//!
//! Policy highlights (see `plans/sorted-nibbling-lovelace.md`):
//!
//! * **No backward compatibility.** Old DBs fail to load with a
//!   re-collect message; see `schema::SCHEMA_VERSION`.
//! * **Adaptation layer.** Raw native captures live under
//!   `.dbg/sessions/<label>/raw/` and `layers.file` points at them —
//!   SessionDb tables index, they do not replace.
//! * **Two tracks, one DB.** Debug and profile commands write into
//!   the same DB and join on canonical `(lang, fqn)` symbols.

pub mod canonicalizer;
pub mod collectors;
pub mod lifecycle;
pub mod schema;

pub use canonicalizer::{CanonicalSymbol, Canonicalizer, for_lang};
pub use collectors::{
    CollectCtx, CollectTrigger, DisasmOutput, LiveDebugger, OnDemandCollector, persist_disasm,
};

use std::fmt;
use std::str::FromStr;

pub use lifecycle::{
    CreateOptions, PrunePolicy, SessionDb, auto_label, compute_target_hash, group_key, prune,
    raw_dir, sessions_dir,
};
pub use schema::SCHEMA_VERSION;

/// The target classification that drives which domain tables exist
/// and which profile collectors apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TargetClass {
    /// CUDA / PyTorch / Triton — driven by `gdbg`.
    Gpu,
    /// Native compiled binaries: C, C++, Rust, Zig, D, Nim, Go.
    NativeCpu,
    /// .NET / CoreCLR / Mono — C#, F#, VB.
    ManagedDotnet,
    /// HotSpot / GraalVM — Java, Kotlin, Scala.
    Jvm,
    /// CPython / PyPy.
    Python,
    /// Node.js / V8 — JavaScript, TypeScript.
    JsNode,
    /// MRI / YJIT — Ruby.
    Ruby,
    /// Zend — PHP.
    Php,
}

impl TargetClass {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetClass::Gpu => "gpu",
            TargetClass::NativeCpu => "native-cpu",
            TargetClass::ManagedDotnet => "managed-dotnet",
            TargetClass::Jvm => "jvm",
            TargetClass::Python => "python",
            TargetClass::JsNode => "node",
            TargetClass::Ruby => "ruby",
            TargetClass::Php => "php",
        }
    }
}

impl fmt::Display for TargetClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TargetClass {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "gpu" => TargetClass::Gpu,
            "native-cpu" => TargetClass::NativeCpu,
            "managed-dotnet" => TargetClass::ManagedDotnet,
            "jvm" => TargetClass::Jvm,
            "python" => TargetClass::Python,
            // Accept both the legacy "js-node" tag and the current "node" one.
            "js-node" | "node" => TargetClass::JsNode,
            "ruby" => TargetClass::Ruby,
            "php" => TargetClass::Php,
            other => anyhow::bail!("unknown target class: {other}"),
        })
    }
}

/// The session kind — debug sessions drive an interactive debugger,
/// profile sessions collect and analyze samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    Debug,
    Profile,
}

impl SessionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionKind::Debug => "debug",
            SessionKind::Profile => "profile",
        }
    }
}
