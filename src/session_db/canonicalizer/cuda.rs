//! CUDA kernel symbol canonicalization.
//!
//! Kernels come from nsys / ncu in two shapes:
//!   * Demangled C++:
//!     `void sgemm_128x128<float>(float const*, float const*, float*, int, int, int)`
//!     →  canonical `sgemm_128x128<float>`
//!     (drop leading return type, drop parenthesized parameter list,
//!      KEEP template parameters — they distinguish instantiations).
//!   * Raw mangled (`_Z...`) — delegate to `c++filt` like cxx does, then
//!     apply the same normalization.
//!
//! Template parameters are MANDATORY to preserve: agents correlate
//! `sgemm<float>` and `sgemm<half>` as different rows and a canonical
//! form that drops them would merge and lose the distinction.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

use super::{CanonicalSymbol, Canonicalizer};

pub struct CudaCanonicalizer;

impl Canonicalizer for CudaCanonicalizer {
    fn lang(&self) -> &'static str {
        "cuda"
    }

    fn canonicalize(&self, raw: &str) -> CanonicalSymbol {
        let (demangled_s, used_demangler) = maybe_demangle(raw);
        let fqn = normalize(&demangled_s);

        CanonicalSymbol {
            lang: "cuda",
            fqn,
            file: None,
            line: None,
            demangled: if used_demangler {
                Some(demangled_s.clone())
            } else {
                None
            },
            raw: raw.to_string(),
            is_synthetic: false,
        }
    }
}

fn maybe_demangle(raw: &str) -> (String, bool) {
    if !raw.starts_with("_Z") {
        return (raw.to_string(), false);
    }
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

/// Drop the leading return type and the trailing parenthesized parameter
/// list while preserving template parameters.
///
/// Input shapes we handle:
///   * `T fn<Args>(Params)`          — typed demangled
///   * `fn<Args>(Params)`            — no return type
///   * `fn<Args>`                    — neither
fn normalize(s: &str) -> String {
    let s = s.trim();

    // 1. Drop a leading return-type token if one is present.
    //    Heuristic: the first angle bracket or paren determines where
    //    the symbol body begins; if there's a space before that which
    //    isn't inside brackets, everything before it was the return type.
    let body_start = {
        let (mut depth_angle, mut depth_paren): (i32, i32) = (0, 0);
        let mut first_space: Option<usize> = None;
        for (i, ch) in s.char_indices() {
            match ch {
                '<' => depth_angle += 1,
                '>' => depth_angle -= 1,
                '(' => depth_paren += 1,
                ')' => depth_paren -= 1,
                ' ' if depth_angle <= 0 && depth_paren <= 0 => {
                    first_space = Some(i);
                    break;
                }
                _ => {}
            }
        }
        // We only treat the prefix as a return type if a `<` or `(`
        // appears AFTER that space — otherwise the space was part of an
        // unusual symbol name and we leave it alone.
        match first_space {
            Some(i) => {
                let after = &s[i + 1..];
                if after.contains('<') || after.contains('(') {
                    i + 1
                } else {
                    0
                }
            }
            None => 0,
        }
    };
    let s = &s[body_start..];

    // 2. Drop a trailing parenthesized parameter list at top level.
    let mut depth_angle = 0i32;
    let mut paren_start: Option<usize> = None;
    for (i, ch) in s.char_indices() {
        match ch {
            '<' => depth_angle += 1,
            '>' => depth_angle -= 1,
            '(' if depth_angle <= 0 => {
                paren_start = Some(i);
                break;
            }
            _ => {}
        }
    }
    match paren_start {
        Some(i) => s[..i].trim().to_string(),
        None => s.trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c() -> CudaCanonicalizer { CudaCanonicalizer }

    #[test]
    fn simple_kernel_preserved() {
        let s = c().canonicalize("vector_add");
        assert_eq!(s.fqn, "vector_add");
        assert_eq!(s.lang, "cuda");
    }

    #[test]
    fn return_type_stripped() {
        let s = c().canonicalize("void sgemm<float>(float const*, int)");
        assert_eq!(s.fqn, "sgemm<float>");
    }

    #[test]
    fn param_list_stripped() {
        let s = c().canonicalize("sgemm<float>(float const*, int)");
        assert_eq!(s.fqn, "sgemm<float>");
    }

    #[test]
    fn template_params_preserved_and_distinguishing() {
        let f = c().canonicalize("void sgemm<float>(float const*, int)");
        let h = c().canonicalize("void sgemm<half>(half const*, int)");
        assert_ne!(f.fqn, h.fqn);
        assert_eq!(f.fqn, "sgemm<float>");
        assert_eq!(h.fqn, "sgemm<half>");
    }

    #[test]
    fn multi_template_params_preserved() {
        let s = c().canonicalize("void gemm<float, 128, 128, 16>(float const*, int)");
        assert_eq!(s.fqn, "gemm<float, 128, 128, 16>");
    }

    #[test]
    fn qualified_name_preserved() {
        let s = c().canonicalize("void ns::kernel<int>(int*)");
        assert_eq!(s.fqn, "ns::kernel<int>");
    }

    #[test]
    fn no_parens_no_return_left_alone() {
        let s = c().canonicalize("sgemm<float>");
        assert_eq!(s.fqn, "sgemm<float>");
    }

    #[test]
    fn key_is_lang_plus_fqn() {
        let s = c().canonicalize("sgemm<float>");
        assert_eq!(s.key(), ("cuda", "sgemm<float>"));
    }

    #[test]
    fn raw_is_preserved_verbatim() {
        let input = "void sgemm<float>(float const*, int)";
        let s = c().canonicalize(input);
        assert_eq!(s.raw, input);
    }
}
