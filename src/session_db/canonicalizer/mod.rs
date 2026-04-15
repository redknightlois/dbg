//! Per-language symbol canonicalization.
//!
//! Each language adapter produces a `CanonicalSymbol` from whatever raw
//! form the underlying tool emitted (demangled C++, .NET FQN, Python
//! `module.qualname`, etc.). The cross-language join key is
//! `(lang, fqn)` — that's the column agents join on for cross-session
//! diffs and cross-track correlation.

pub mod cuda;
pub mod cxx;
pub mod dotnet;
pub mod go;
pub mod python;

/// A language-agnostic symbol identity. Two sessions' `CanonicalSymbol`
/// values compare equal iff `(lang, fqn)` match — `file`/`line`/etc. are
/// metadata, not part of identity.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CanonicalSymbol {
    pub lang: &'static str,
    pub fqn: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub demangled: Option<String>,
    pub raw: String,
    pub is_synthetic: bool,
}

impl CanonicalSymbol {
    /// The (lang, fqn) join key used for cross-session correlation.
    pub fn key(&self) -> (&'static str, &str) {
        (self.lang, &self.fqn)
    }
}

/// A per-language canonicalizer. Impls are in the submodules.
pub trait Canonicalizer: Send + Sync {
    fn lang(&self) -> &'static str;

    /// Convert a raw (possibly mangled) symbol name into a canonical form.
    fn canonicalize(&self, raw: &str) -> CanonicalSymbol;

    /// Convert a structured symbol emitted by the tool (e.g., dotnet-trace
    /// yields separate module/class/method/sig). Default: join with `.`
    /// and fall through to `canonicalize`. Adapters override when they can
    /// do something smarter than the default join.
    fn canonicalize_structured(
        &self,
        module: &str,
        class: &str,
        method: &str,
        _sig: &str,
    ) -> CanonicalSymbol {
        let parts: Vec<&str> = [module, class, method]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        self.canonicalize(&parts.join("."))
    }

    /// Best-effort unwrap of async/state-machine frames back to the
    /// user-visible method name. Returns `Some(user_method)` only when
    /// the raw frame is recognized as a compiler-generated wrapper.
    fn resolve_async_frame(&self, _raw: &str) -> Option<String> {
        None
    }
}

/// Return the canonicalizer for a language tag or `None` if unknown.
pub fn for_lang(lang: &str) -> Option<Box<dyn Canonicalizer>> {
    Some(match lang {
        "cpp" | "rust" | "c" | "zig" | "d" | "nim" => Box::new(cxx::CxxCanonicalizer::new(lang)),
        "dotnet" => Box::new(dotnet::DotnetCanonicalizer),
        "python" => Box::new(python::PythonCanonicalizer),
        "go" => Box::new(go::GoCanonicalizer),
        "cuda" => Box::new(cuda::CudaCanonicalizer),
        _ => return None,
    })
}
