//! Python (CPython / PyPy) symbol canonicalization.
//!
//! Raw forms seen in the wild:
//!   * cProfile pstats: `/path/to/app.py:42(my_func)`
//!     → fqn=`app.my_func`, file=`/path/to/app.py`, line=42
//!   * py-spy / austin:   `my_func (app.py)`   → fqn=`app.my_func`, file=`app.py`
//!   * dotted already:    `app.submodule.my_func` → fqn as-is
//!   * Built-ins:         `<built-in method builtins.print>` → fqn=`builtins.print`
//!
//! Lambdas and comprehensions — `<lambda>`, `<listcomp>`, `<genexpr>` —
//! get `is_synthetic = true` because their line-number-derived identity
//! is not stable across edits.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use super::{CanonicalSymbol, Canonicalizer};

pub struct PythonCanonicalizer;

impl Canonicalizer for PythonCanonicalizer {
    fn lang(&self) -> &'static str {
        "python"
    }

    fn canonicalize(&self, raw: &str) -> CanonicalSymbol {
        let parsed = parse(raw);
        CanonicalSymbol {
            lang: "python",
            fqn: parsed.fqn,
            file: parsed.file,
            line: parsed.line,
            demangled: None,
            raw: raw.to_string(),
            is_synthetic: parsed.synthetic,
        }
    }
}

struct Parsed {
    fqn: String,
    file: Option<String>,
    line: Option<u32>,
    synthetic: bool,
}

fn parse(raw: &str) -> Parsed {
    // `<built-in method builtins.print>`  → `builtins.print`
    if let Some(inner) = raw
        .strip_prefix("<built-in method ")
        .and_then(|s| s.strip_suffix('>'))
    {
        return Parsed {
            fqn: inner.to_string(),
            file: None,
            line: None,
            synthetic: false,
        };
    }
    // `<method 'write' of 'BufferedWriter' objects>` → `BufferedWriter.write`
    static METHOD_OF: OnceLock<Regex> = OnceLock::new();
    let re_method_of = METHOD_OF
        .get_or_init(|| Regex::new(r"^<method '(?P<m>[^']+)' of '(?P<t>[^']+)' objects>$").unwrap());
    if let Some(c) = re_method_of.captures(raw) {
        return Parsed {
            fqn: format!("{}.{}", &c["t"], &c["m"]),
            file: None,
            line: None,
            synthetic: false,
        };
    }

    // pstats form: `<file>:<line>(<func>)`
    static PSTATS: OnceLock<Regex> = OnceLock::new();
    let re_pstats = PSTATS.get_or_init(|| {
        Regex::new(r"^(?P<file>[^\s\(]+):(?P<line>\d+)\((?P<func>[^)]+)\)$").unwrap()
    });
    if let Some(c) = re_pstats.captures(raw) {
        let file = c["file"].to_string();
        let line: u32 = c["line"].parse().ok().unwrap_or(0);
        let func = c["func"].to_string();
        let module = module_from_file(&file);
        let fqn = if module.is_empty() {
            func.clone()
        } else {
            format!("{module}.{func}")
        };
        let synthetic = is_synthetic_func(&func);
        return Parsed { fqn, file: Some(file), line: Some(line), synthetic };
    }

    // py-spy form: `my_func (app.py)` / `my_func (app.py:42)`
    static PYSPY: OnceLock<Regex> = OnceLock::new();
    let re_pyspy = PYSPY.get_or_init(|| {
        Regex::new(r"^(?P<func>[A-Za-z_<][\w<>]*)\s+\((?P<file>[^:)]+)(?::(?P<line>\d+))?\)$").unwrap()
    });
    if let Some(c) = re_pyspy.captures(raw) {
        let func = c["func"].to_string();
        let file = c["file"].to_string();
        let line: Option<u32> = c.name("line").and_then(|m| m.as_str().parse().ok());
        let module = module_from_file(&file);
        let fqn = if module.is_empty() {
            func.clone()
        } else {
            format!("{module}.{func}")
        };
        let synthetic = is_synthetic_func(&func);
        return Parsed { fqn, file: Some(file), line, synthetic };
    }

    // Bare dotted or bare function name.
    let synthetic = is_synthetic_func(raw);
    Parsed {
        fqn: raw.to_string(),
        file: None,
        line: None,
        synthetic,
    }
}

fn module_from_file(file: &str) -> String {
    // Drop directories, drop `.py` suffix. Preserve package-ish dots only
    // if the filename already contains them (rare).
    Path::new(file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

fn is_synthetic_func(f: &str) -> bool {
    matches!(f, "<lambda>" | "<listcomp>" | "<dictcomp>" | "<setcomp>" | "<genexpr>" | "<module>")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn py() -> PythonCanonicalizer { PythonCanonicalizer }

    #[test]
    fn pstats_form_parsed() {
        let s = py().canonicalize("/opt/myapp/api.py:42(handle_request)");
        assert_eq!(s.fqn, "api.handle_request");
        assert_eq!(s.file.as_deref(), Some("/opt/myapp/api.py"));
        assert_eq!(s.line, Some(42));
        assert!(!s.is_synthetic);
    }

    #[test]
    fn pstats_lambda_is_synthetic() {
        let s = py().canonicalize("/opt/myapp/api.py:42(<lambda>)");
        assert!(s.is_synthetic);
        assert_eq!(s.fqn, "api.<lambda>");
    }

    #[test]
    fn pyspy_form_with_line_parsed() {
        let s = py().canonicalize("handle_request (api.py:42)");
        assert_eq!(s.fqn, "api.handle_request");
        assert_eq!(s.file.as_deref(), Some("api.py"));
        assert_eq!(s.line, Some(42));
    }

    #[test]
    fn pyspy_form_without_line_parsed() {
        let s = py().canonicalize("handle_request (api.py)");
        assert_eq!(s.fqn, "api.handle_request");
        assert_eq!(s.line, None);
    }

    #[test]
    fn bare_dotted_is_passed_through() {
        let s = py().canonicalize("myapp.services.users.login");
        assert_eq!(s.fqn, "myapp.services.users.login");
        assert_eq!(s.file, None);
    }

    #[test]
    fn builtin_method_form() {
        let s = py().canonicalize("<built-in method builtins.print>");
        assert_eq!(s.fqn, "builtins.print");
    }

    #[test]
    fn method_of_form() {
        let s = py().canonicalize("<method 'write' of 'BufferedWriter' objects>");
        assert_eq!(s.fqn, "BufferedWriter.write");
    }

    #[test]
    fn listcomp_synthetic() {
        let s = py().canonicalize("/app/main.py:10(<listcomp>)");
        assert!(s.is_synthetic);
    }

    #[test]
    fn module_level_synthetic() {
        let s = py().canonicalize("/app/main.py:1(<module>)");
        assert!(s.is_synthetic);
    }

    #[test]
    fn structured_default_joins_with_dot() {
        let s = py().canonicalize_structured("", "UserService", "login", "");
        assert_eq!(s.fqn, "UserService.login");
    }

    #[test]
    fn key_is_lang_plus_fqn() {
        let s = py().canonicalize("app.main");
        assert_eq!(s.key(), ("python", "app.main"));
    }
}
