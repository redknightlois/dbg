use dbg_cli::session_db::canonicalizer::{Canonicalizer, cuda::CudaCanonicalizer};
use regex::Regex;
use std::sync::OnceLock;

/// Return the stable identity used to correlate one kernel across profilers.
///
/// Nsight Systems usually stores a demangled C++ signature. Nsight Compute
/// and torch.profiler can omit the return type or the argument list. Keep the
/// qualified function name and remove only those presentation differences.
pub fn normalize_kernel_name(raw: &str) -> String {
    let prepared = raw
        .replace("::<unnamed>::", "::")
        .replace(" <unnamed>::", " ")
        .replace("(anonymous namespace)::", "")
        .replace("<unnamed>::", "<")
        .replace("unnamed>::", "");
    let mut name = CudaCanonicalizer.canonicalize(&prepared).fqn;
    if let Some((base, suffix)) = name.rsplit_once(" [")
        && suffix.ends_with(']')
        && suffix[1..suffix.len() - 1]
            .chars()
            .all(|ch| ch.is_ascii_digit())
    {
        name = base.trim_end().to_string();
    }

    static TYPED_INTEGER: OnceLock<Regex> = OnceLock::new();
    static TRUE: OnceLock<Regex> = OnceLock::new();
    static FALSE: OnceLock<Regex> = OnceLock::new();
    static INTEGER_SUFFIX: OnceLock<Regex> = OnceLock::new();
    static OPERATOR_CALL: OnceLock<Regex> = OnceLock::new();
    static NSYS_LAMBDA: OnceLock<Regex> = OnceLock::new();
    static TORCH_LAMBDA: OnceLock<Regex> = OnceLock::new();
    static NAMESPACE: OnceLock<Regex> = OnceLock::new();
    static PRESENTATION_TYPE: OnceLock<Regex> = OnceLock::new();

    name = TYPED_INTEGER
        .get_or_init(|| {
            Regex::new(r"\((?:bool|int|unsigned\s+long|unsigned\s+int|long)\)\s*(\d+)").unwrap()
        })
        .replace_all(&name, "$1")
        .into_owned();
    name = TRUE
        .get_or_init(|| Regex::new(r"\btrue\b").unwrap())
        .replace_all(&name, "1")
        .into_owned();
    name = FALSE
        .get_or_init(|| Regex::new(r"\bfalse\b").unwrap())
        .replace_all(&name, "0")
        .into_owned();
    name = INTEGER_SUFFIX
        .get_or_init(|| Regex::new(r"(\d+)(?:ull|ul|ll|u|l)\b").unwrap())
        .replace_all(&name, "$1")
        .into_owned();
    name = OPERATOR_CALL
        .get_or_init(|| Regex::new(r"operator\s*\(\s*\)").unwrap())
        .replace_all(&name, "operator_call")
        .into_owned();
    name = NSYS_LAMBDA
        .get_or_init(|| Regex::new(r"\[lambda[^\]]*\]").unwrap())
        .replace_all(&name, "lambda")
        .into_owned();
    name = TORCH_LAMBDA
        .get_or_init(|| Regex::new(r"\{lambda[^\}]*\}").unwrap())
        .replace_all(&name, "lambda")
        .into_owned();
    name = strip_parenthesized_groups(&name);

    if name.contains("multi_tensor_apply_kernel") {
        let namespace = NAMESPACE.get_or_init(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]*::").unwrap());
        loop {
            let reduced = namespace.replace_all(&name, "").into_owned();
            if reduced == name {
                break;
            }
            name = reduced;
        }
    } else {
        name = name.replace("cutlass::Kernel2<", "Kernel2<");
    }
    name = PRESENTATION_TYPE
        .get_or_init(|| Regex::new(r"\b(?:void|const)\b").unwrap())
        .replace_all(&name, "")
        .into_owned();
    name.retain(|ch| !ch.is_whitespace() && ch != '&');
    name
}

fn strip_parenthesized_groups(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    let mut depth = 0_u32;
    for ch in name.chars() {
        match ch {
            '(' => depth = depth.saturating_add(1),
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => result.push(ch),
            _ => {}
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::normalize_kernel_name;

    #[test]
    fn removes_profiler_specific_signature_and_prefix() {
        assert_eq!(
            normalize_kernel_name("void at::native::foo<float>(int, float)"),
            "at::native::foo<float>"
        );
        assert_eq!(
            normalize_kernel_name("at::native::foo<float>"),
            "at::native::foo<float>"
        );
    }

    #[test]
    fn removes_numeric_display_suffix() {
        assert_eq!(normalize_kernel_name("kernel [12]"), "kernel");
    }

    #[test]
    fn removes_any_demangled_return_type() {
        assert_eq!(normalize_kernel_name("int ns::kernel(float)"), "ns::kernel");
    }

    #[test]
    fn correlates_cutlass_names_with_elided_ncu_namespace() {
        let nsys = "void cutlass::Kernel2<cutlass_80_simt_sgemm_64x64_8x5_tn_align1>(T1::Params)";
        let ncu = "void Kernel2<cutlass_80_simt_sgemm_64x64_8x5_tn_align1>(Params)";
        assert_eq!(normalize_kernel_name(nsys), normalize_kernel_name(ncu));
    }

    #[test]
    fn correlates_typed_profiler_literals() {
        let nsys = "void sgemm_largek_lds64<(bool)1, (bool)0, (int)5>(float *)";
        let torch = "void sgemm_largek_lds64<true, false, 5>(float*)";
        let ncu = "void sgemm_largek_lds64<1, 0, 5>(float *)";
        assert_eq!(normalize_kernel_name(nsys), normalize_kernel_name(torch));
        assert_eq!(normalize_kernel_name(torch), normalize_kernel_name(ncu));
    }

    #[test]
    fn correlates_anonymous_namespace_spellings() {
        let nsys = "void at::native::<unnamed>::multi_tensor_apply_kernel<at::native::<unnamed>::TensorListMetadata<(int)2>>(T1)";
        let torch = "void at::native::(anonymous namespace)::multi_tensor_apply_kernel<at::native::(anonymous namespace)::TensorListMetadata<2>>(float)";
        let ncu = "void unnamed>::multi_tensor_apply_kernel<unnamed>::TensorListMetadata<2>>(T1)";
        assert_eq!(normalize_kernel_name(nsys), normalize_kernel_name(torch));
        assert_eq!(normalize_kernel_name(torch), normalize_kernel_name(ncu));
    }

    #[test]
    fn correlates_lambda_spellings() {
        let nsys = "void at::native::reduce_kernel<(int)128, at::native::sum_functor<float>::operator ()(T1)::[lambda(float) (instance 1)]>(T2)";
        let torch = "void at::native::reduce_kernel<128, at::native::sum_functor<float>::operator()(T1)::{lambda(float)#1}>(T2)";
        assert_eq!(normalize_kernel_name(nsys), normalize_kernel_name(torch));
    }
}
