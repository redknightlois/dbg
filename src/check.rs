use crate::backend::{DepStatus, Registry};

// Re-export shared functions
pub use dbg_cli::deps::{check_dep, find_bin, format_results};

/// Check all dependencies for the given backend types.
/// Returns a list of (backend_name, Vec<DepStatus>).
pub fn check_backends<'a>(
    registry: &Registry,
    types: &[&'a str],
) -> (Vec<(&'static str, Vec<DepStatus>)>, Vec<&'a str>) {
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut unknown = Vec::new();

    for t in types {
        if let Some(backend) = registry.get(t) {
            if seen.insert(backend.name()) {
                let statuses = backend.dependencies().into_iter().map(check_dep).collect();
                results.push((backend.name(), statuses));
            }
        } else {
            unknown.push(*t);
        }
    }
    (results, unknown)
}
