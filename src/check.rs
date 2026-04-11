use std::process::Command;

use crate::backend::{DepStatus, Dependency, DependencyCheck, Registry};

/// Check all dependencies for the given backend types.
/// Returns a list of (backend_name, Vec<DepStatus>).
pub fn check_backends(
    registry: &Registry,
    types: &[&str],
) -> Vec<(&'static str, Vec<DepStatus>)> {
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for t in types {
        if let Some(backend) = registry.get(t) {
            if seen.insert(backend.name()) {
                let statuses = backend.dependencies().into_iter().map(check_dep).collect();
                results.push((backend.name(), statuses));
            }
        }
    }
    results
}

fn check_dep(dep: Dependency) -> DepStatus {
    match &dep.check {
        DependencyCheck::Binary {
            alternatives,
            ..
        } => {
            for name in *alternatives {
                if let Ok(path) = which::which(name) {
                    return DepStatus {
                        name: dep.name,
                        ok: true,
                        detail: path.display().to_string(),
                        install: dep.install,
                    };
                }
            }
            DepStatus {
                name: dep.name,
                ok: false,
                detail: "not found".into(),
                install: dep.install,
            }
        }
        DependencyCheck::PythonImport { module } => {
            let ok = Command::new("python3")
                .args(["-c", &format!("import {module}")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            DepStatus {
                name: dep.name,
                ok,
                detail: if ok {
                    format!("{module} importable")
                } else {
                    format!("{module} not found")
                },
                install: dep.install,
            }
        }
        DependencyCheck::Command { program, args } => {
            let ok = Command::new(program)
                .args(*args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            DepStatus {
                name: dep.name,
                ok,
                detail: if ok { "ok".into() } else { "failed".into() },
                install: dep.install,
            }
        }
    }
}

/// Format check results for display.
pub fn format_results(results: &[(&str, Vec<DepStatus>)]) -> String {
    let mut out = String::new();
    for (name, statuses) in results {
        out.push_str(&format!("{name}:\n"));
        for s in statuses {
            let icon = if s.ok { "ok" } else { "MISSING" };
            out.push_str(&format!("  {}: {} ({})\n", s.name, icon, s.detail));
            if !s.ok {
                out.push_str(&format!("    install: {}\n", s.install));
            }
        }
    }
    out
}
