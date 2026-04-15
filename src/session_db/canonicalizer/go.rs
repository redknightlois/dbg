//! Go symbol canonicalization.
//!
//! Go emits frames in forms like:
//!   * `main.handleRequest`
//!   * `main.(*Server).handleRequest`              — pointer receiver
//!   * `main.Server.handleRequest`                 — value receiver
//!   * `main.handleRequest.func1`                  — closure inside func
//!   * `github.com/foo/bar.(*Client).Do`           — full import path
//!   * `runtime.goexit`                            — runtime built-ins
//!   * `type:.eq.[...]`                            — auto-generated
//!
//! Canonical form preserves pointer-vs-value receiver distinction
//! (they're different methods in Go). Closures (`.func1`, `.func2`) are
//! flagged synthetic — their numeric suffix isn't stable across edits.

use super::{CanonicalSymbol, Canonicalizer};

pub struct GoCanonicalizer;

impl Canonicalizer for GoCanonicalizer {
    fn lang(&self) -> &'static str {
        "go"
    }

    fn canonicalize(&self, raw: &str) -> CanonicalSymbol {
        let synthetic = is_synthetic(raw);
        CanonicalSymbol {
            lang: "go",
            fqn: raw.to_string(),
            file: None,
            line: None,
            demangled: None,
            raw: raw.to_string(),
            is_synthetic: synthetic,
        }
    }
}

fn is_synthetic(s: &str) -> bool {
    // `.func1`, `.func2`, ... — compiler-generated closure names.
    // They're the only common synthetic form; everything else is the
    // user's symbol verbatim.
    if let Some(tail) = s.rsplit('.').next() {
        if let Some(rest) = tail.strip_prefix("func") {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    // Auto-generated type equality / hash helpers.
    s.starts_with("type:.eq.") || s.starts_with("type:.hash.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g() -> GoCanonicalizer { GoCanonicalizer }

    #[test]
    fn plain_function_preserved() {
        let s = g().canonicalize("main.handleRequest");
        assert_eq!(s.fqn, "main.handleRequest");
        assert_eq!(s.lang, "go");
        assert!(!s.is_synthetic);
    }

    #[test]
    fn pointer_and_value_receivers_distinct() {
        let ptr = g().canonicalize("main.(*Server).handleRequest");
        let val = g().canonicalize("main.Server.handleRequest");
        assert_ne!(ptr.fqn, val.fqn);
    }

    #[test]
    fn full_import_path_preserved() {
        let s = g().canonicalize("github.com/foo/bar.(*Client).Do");
        assert_eq!(s.fqn, "github.com/foo/bar.(*Client).Do");
    }

    #[test]
    fn numeric_closure_is_synthetic() {
        let s = g().canonicalize("main.handleRequest.func1");
        assert!(s.is_synthetic, "{s:?}");
    }

    #[test]
    fn multidigit_closure_is_synthetic() {
        let s = g().canonicalize("main.handleRequest.func42");
        assert!(s.is_synthetic);
    }

    #[test]
    fn func_in_name_is_not_synthetic() {
        // `.func` alone (no digit) should NOT be considered synthetic —
        // a user could legitimately name a method `func`.
        let s = g().canonicalize("pkg.Service.func");
        assert!(!s.is_synthetic);
    }

    #[test]
    fn runtime_symbol_not_synthetic() {
        let s = g().canonicalize("runtime.goexit");
        assert!(!s.is_synthetic);
    }

    #[test]
    fn type_eq_synthetic() {
        let s = g().canonicalize("type:.eq.[1024]uint8");
        assert!(s.is_synthetic);
    }

    #[test]
    fn key_is_lang_plus_fqn() {
        let s = g().canonicalize("main.f");
        assert_eq!(s.key(), ("go", "main.f"));
    }
}
