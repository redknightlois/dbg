//! .NET (CoreCLR / Mono) symbol canonicalization.
//!
//! Canonical form: `Namespace.Class.Method` with:
//!   * Parameter lists dropped (CLR metadata tokens disambiguate overloads;
//!     cross-session joins don't need the signature).
//!   * Nested-class `+` separators normalized to `.`
//!     (`Outer+Inner.Method` → `Outer.Inner.Method`).
//!   * Module prefix `asm!Class.Method` stripped when present.
//!   * Async state machines `<Method>d__N.MoveNext` unwrapped to `Method`.
//!     The `MoveNext` entry from a state machine is what samplers see; the
//!     user wrote `async Method()`, so that's what the agent should see.
//!   * Generic backtick notation (`List`1`) preserved — it's part of the
//!     CLR symbol and cross-session stable.
//!   * Compiler-generated display classes (`<>c`, `<>c__DisplayClass`)
//!     marked `is_synthetic = true`.

use std::sync::OnceLock;

use regex::Regex;

use super::{CanonicalSymbol, Canonicalizer};

pub struct DotnetCanonicalizer;

impl Canonicalizer for DotnetCanonicalizer {
    fn lang(&self) -> &'static str {
        "dotnet"
    }

    fn canonicalize(&self, raw: &str) -> CanonicalSymbol {
        let stripped = strip_module_prefix(raw);
        let stripped = strip_param_list(stripped);
        let stripped = stripped.replace('+', ".");

        let (fqn, synthetic) = if let Some(unwrapped) = unwrap_async_state_machine(&stripped) {
            (unwrapped, false)
        } else if looks_synthetic(&stripped) {
            (stripped.clone(), true)
        } else {
            (stripped.clone(), false)
        };

        CanonicalSymbol {
            lang: "dotnet",
            fqn,
            file: None,
            line: None,
            demangled: None,
            raw: raw.to_string(),
            is_synthetic: synthetic,
        }
    }

    fn canonicalize_structured(
        &self,
        module: &str,
        class: &str,
        method: &str,
        _sig: &str,
    ) -> CanonicalSymbol {
        let _ = module; // CLR module is noise for canonical identity
        let joined = if class.is_empty() {
            method.to_string()
        } else {
            format!("{class}.{method}")
        };
        self.canonicalize(&joined)
    }

    fn resolve_async_frame(&self, raw: &str) -> Option<String> {
        unwrap_async_state_machine(raw)
    }
}

fn strip_module_prefix(s: &str) -> &str {
    // Forms like `System.Private.CoreLib!System.String.Concat` — keep
    // everything after the first `!`.
    match s.find('!') {
        Some(i) => &s[i + 1..],
        None => s,
    }
}

fn strip_param_list(s: &str) -> String {
    // Find an unbalanced '(' at top level (ignoring the ones inside
    // generic args `<...>`) and drop from there.
    let mut depth_angle: i32 = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '<' => depth_angle += 1,
            '>' => depth_angle -= 1,
            '(' if depth_angle <= 0 => return s[..i].to_string(),
            _ => {}
        }
    }
    s.to_string()
}

/// `<MethodAsync>d__7.MoveNext` → `MethodAsync`.
/// Also handles the variant `<>c__DisplayClass0_0.<Method>b__0` (anonymous
/// local inside async method) — those we treat as synthetic rather than
/// unwrapping.
fn unwrap_async_state_machine(s: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?x)
            ^(?P<prefix>.*?)               # namespace / class prefix (non-greedy)
            <(?P<method>[A-Za-z_][A-Za-z0-9_]*)>   # <Method>
            d__\d+                         # d__N  (state machine discriminator)
            \.MoveNext$").unwrap()
    });
    re.captures(s).map(|c| {
        let prefix = c.name("prefix").unwrap().as_str();
        let method = c.name("method").unwrap().as_str();
        if prefix.is_empty() {
            method.to_string()
        } else {
            // prefix already ends with '.' if there was a namespace
            format!("{prefix}{method}")
        }
    })
}

fn looks_synthetic(s: &str) -> bool {
    s.contains("<>c__DisplayClass")
        || s.contains("<>c.")
        || s.contains("<>c<>")
        || s.contains("__AnonymousType")
        || (s.contains(".<") && s.contains(">b__"))    // local func / lambda
        || (s.contains(".<") && s.contains(">g__"))    // local static func
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n() -> DotnetCanonicalizer { DotnetCanonicalizer }

    #[test]
    fn simple_fqn_preserved() {
        let s = n().canonicalize("MyApp.Services.OrderService.ProcessOrder");
        assert_eq!(s.fqn, "MyApp.Services.OrderService.ProcessOrder");
        assert_eq!(s.lang, "dotnet");
        assert!(!s.is_synthetic);
    }

    #[test]
    fn param_list_dropped() {
        let s = n().canonicalize("MyApp.Foo.Bar(Int32, String)");
        assert_eq!(s.fqn, "MyApp.Foo.Bar");
    }

    #[test]
    fn param_list_with_generic_method_still_dropped() {
        let s = n().canonicalize("MyApp.Foo.Bar<T>(T, Int32)");
        assert_eq!(s.fqn, "MyApp.Foo.Bar<T>");
    }

    #[test]
    fn nested_plus_normalized_to_dot() {
        let s = n().canonicalize("MyApp.Outer+Inner.Method");
        assert_eq!(s.fqn, "MyApp.Outer.Inner.Method");
    }

    #[test]
    fn module_prefix_stripped() {
        let s = n().canonicalize("System.Private.CoreLib!System.String.Concat");
        assert_eq!(s.fqn, "System.String.Concat");
    }

    #[test]
    fn async_state_machine_unwrapped() {
        let s = n().canonicalize("MyApp.Foo.<ProcessOrderAsync>d__7.MoveNext");
        assert_eq!(s.fqn, "MyApp.Foo.ProcessOrderAsync");
    }

    #[test]
    fn async_at_module_root_unwrapped() {
        let s = n().canonicalize("<MainAsync>d__0.MoveNext");
        assert_eq!(s.fqn, "MainAsync");
    }

    #[test]
    fn display_class_marked_synthetic() {
        let s = n().canonicalize("MyApp.Foo.<>c__DisplayClass5_0.<Bar>b__0");
        assert!(s.is_synthetic, "{s:?}");
    }

    #[test]
    fn closure_sentinel_marked_synthetic() {
        let s = n().canonicalize("MyApp.Foo.<>c.<<Bar>b__0_0>");
        assert!(s.is_synthetic);
    }

    #[test]
    fn resolve_async_frame_returns_method() {
        let got = n().resolve_async_frame("A.B.<DoWorkAsync>d__3.MoveNext");
        assert_eq!(got, Some("A.B.DoWorkAsync".into()));
    }

    #[test]
    fn resolve_async_frame_none_for_plain_method() {
        assert!(n().resolve_async_frame("A.B.C").is_none());
    }

    #[test]
    fn generic_backtick_notation_preserved() {
        let s = n().canonicalize("System.Collections.Generic.List`1.Add");
        assert_eq!(s.fqn, "System.Collections.Generic.List`1.Add");
    }

    #[test]
    fn structured_ignores_module_and_sig() {
        let s = n().canonicalize_structured("MyAsm.dll", "MyNs.MyClass", "Foo", "(I)V");
        assert_eq!(s.fqn, "MyNs.MyClass.Foo");
    }

    #[test]
    fn key_is_lang_plus_fqn() {
        let s = n().canonicalize("A.B.C");
        assert_eq!(s.key(), ("dotnet", "A.B.C"));
    }
}
