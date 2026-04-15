//! C/C++/Rust/Zig/D/Nim symbol canonicalization.
//!
//! Strategy for Phase 1:
//!   * If the raw symbol starts with `_Z`/`_R` (Itanium / Rust v0 mangling),
//!     shell out to `c++filt --no-params`-style demangling when available.
//!     If the tool isn't installed we fall back to the raw string so
//!     callers still get *something* usable.
//!   * For Rust, strip the trailing `::h[0-9a-f]{16}` hash that rustc
//!     appends to prevent accidental collisions — it's noise for
//!     cross-session joins.
//!   * Collapse libc++ / libstdc++ inline-namespace markers
//!     (`std::__1::`, `std::__cxx11::`) so symbols from different stdlibs
//!     line up.
//!   * KEEP template parameters and parenthesized parameter lists —
//!     they disambiguate overloads and template instantiations. Losing
//!     them would merge `sgemm<float>` and `sgemm<half>` into one row.
//!   * Detect Rust closure syntax (`{{closure}}`, `{closure#N}`) and
//!     mark the symbol `is_synthetic = true`.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

use regex::Regex;

use super::{CanonicalSymbol, Canonicalizer};

pub struct CxxCanonicalizer {
    lang: &'static str,
}

impl CxxCanonicalizer {
    pub fn new(lang: &str) -> Self {
        let lang: &'static str = match lang {
            "c" => "c",
            "cpp" => "cpp",
            "rust" => "rust",
            "zig" => "zig",
            "d" => "d",
            "nim" => "nim",
            _ => "cpp",
        };
        Self { lang }
    }
}

impl Canonicalizer for CxxCanonicalizer {
    fn lang(&self) -> &'static str {
        self.lang
    }

    fn canonicalize(&self, raw: &str) -> CanonicalSymbol {
        let (demangled_out, used_demangler) = maybe_demangle(raw);
        let mut fqn = normalize(&demangled_out);
        let synthetic = looks_synthetic(&fqn);

        // Rust hash suffix: "core::fmt::Write::write_fmt::h1234567890abcdef"
        //                 →  "core::fmt::Write::write_fmt"
        if self.lang == "rust" {
            fqn = strip_rust_hash(&fqn);
        }

        CanonicalSymbol {
            lang: self.lang,
            fqn,
            file: None,
            line: None,
            demangled: if used_demangler { Some(demangled_out) } else { None },
            raw: raw.to_string(),
            is_synthetic: synthetic,
        }
    }
}

/// If `raw` looks like a mangled symbol, pipe it through the system
/// `c++filt`. Best-effort: returns `(raw, false)` on any failure.
fn maybe_demangle(raw: &str) -> (String, bool) {
    if !(raw.starts_with("_Z") || raw.starts_with("_R")) {
        return (raw.to_string(), false);
    }
    // Cache the "is c++filt on this system" decision once per process.
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    let available = *AVAILABLE.get_or_init(|| which::which("c++filt").is_ok());
    if !available {
        return (raw.to_string(), false);
    }

    let out = Command::new("c++filt")
        .arg(raw)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() || s == raw {
                (raw.to_string(), false)
            } else {
                (s, true)
            }
        }
        _ => (raw.to_string(), false),
    }
}

fn normalize(s: &str) -> String {
    // Collapse libc++/libstdc++ inline namespaces.
    let mut out = s.replace("std::__1::", "std::");
    out = out.replace("std::__cxx11::", "std::");
    out = out.replace("__gnu_cxx::", "std::");
    out
}

fn strip_rust_hash(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"::h[0-9a-f]{16}$").unwrap());
    re.replace(s, "").to_string()
}

fn looks_synthetic(s: &str) -> bool {
    s.contains("{{closure}}")
        || s.contains("{closure#")
        || s.contains("<lambda")   // clang lambdas: "<lambda(...)>"
        || s.contains("::$_")      // libc++ anonymous thunk prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpp() -> CxxCanonicalizer { CxxCanonicalizer::new("cpp") }
    fn rust() -> CxxCanonicalizer { CxxCanonicalizer::new("rust") }

    #[test]
    fn already_demangled_cpp_passes_through() {
        let c = cpp();
        let s = c.canonicalize("foo::bar::baz(int, double) const");
        assert_eq!(s.fqn, "foo::bar::baz(int, double) const");
        assert_eq!(s.lang, "cpp");
        assert!(!s.is_synthetic);
    }

    #[test]
    fn rust_hash_suffix_stripped() {
        let r = rust();
        let s = r.canonicalize("core::fmt::Write::write_fmt::h0123456789abcdef");
        assert_eq!(s.fqn, "core::fmt::Write::write_fmt");
    }

    #[test]
    fn rust_no_hash_left_alone() {
        let r = rust();
        let s = r.canonicalize("core::fmt::Write::write_fmt");
        assert_eq!(s.fqn, "core::fmt::Write::write_fmt");
    }

    #[test]
    fn rust_partial_hash_not_stripped() {
        // Only the 16-hex-char form is a real rustc suffix.
        let r = rust();
        let s = r.canonicalize("core::fmt::Write::write_fmt::habc");
        assert_eq!(s.fqn, "core::fmt::Write::write_fmt::habc");
    }

    #[test]
    fn stdlib_inline_namespaces_collapsed() {
        let c = cpp();
        let s = c.canonicalize("std::__1::vector<int>::push_back(int&&)");
        assert_eq!(s.fqn, "std::vector<int>::push_back(int&&)");
    }

    #[test]
    fn cxx11_inline_collapsed() {
        let c = cpp();
        let s = c.canonicalize("std::__cxx11::basic_string<char>::size() const");
        assert_eq!(s.fqn, "std::basic_string<char>::size() const");
    }

    #[test]
    fn template_params_preserved() {
        let c = cpp();
        let s = c.canonicalize("sgemm<float>(float const*, int)");
        assert_eq!(s.fqn, "sgemm<float>(float const*, int)");
        let t = c.canonicalize("sgemm<half>(half const*, int)");
        assert_ne!(s.fqn, t.fqn, "template params must distinguish");
    }

    #[test]
    fn rust_closure_marked_synthetic() {
        let r = rust();
        let s = r.canonicalize("my_app::run::{{closure}}::h0123456789abcdef");
        assert!(s.is_synthetic, "{:?}", s);
        assert_eq!(s.fqn, "my_app::run::{{closure}}");
    }

    #[test]
    fn rust_numbered_closure_synthetic() {
        let r = rust();
        let s = r.canonicalize("my_app::run::{closure#2}::h0123456789abcdef");
        assert!(s.is_synthetic);
        assert_eq!(s.fqn, "my_app::run::{closure#2}");
    }

    #[test]
    fn clang_lambda_synthetic() {
        let c = cpp();
        let s = c.canonicalize("foo::<lambda(int)>::operator()(int) const");
        assert!(s.is_synthetic);
    }

    #[test]
    fn raw_field_is_preserved() {
        let c = cpp();
        let s = c.canonicalize("std::__1::vector<int>::push_back(int&&)");
        assert_eq!(s.raw, "std::__1::vector<int>::push_back(int&&)");
    }

    #[test]
    fn mangled_symbol_without_cxxfilt_passes_through() {
        // We can't guarantee c++filt is present on the test runner; if it
        // is, we should still get something non-empty. Either way the
        // canonicalizer must not panic and must preserve `raw`.
        let c = cpp();
        let s = c.canonicalize("_ZN3foo3bar3bazEi");
        assert!(!s.fqn.is_empty());
        assert_eq!(s.raw, "_ZN3foo3bar3bazEi");
    }

    #[test]
    fn key_is_lang_plus_fqn() {
        let c = cpp();
        let s = c.canonicalize("foo::bar()");
        assert_eq!(s.key(), ("cpp", "foo::bar()"));
    }
}
