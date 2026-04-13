//! Shared dependency-checking infrastructure.
//!
//! Used by both `dbg` and `gdbg` to verify tool availability.

use std::process::Command;

/// How to verify a dependency is installed.
#[allow(dead_code)]
pub enum DependencyCheck {
    /// Check that a binary exists on PATH (optionally with minimum version).
    Binary {
        name: &'static str,
        /// Alternative names to try (e.g., "lldb-20", "lldb-18", "lldb").
        alternatives: &'static [&'static str],
        /// Command + args to get version string, e.g., ("lldb-20", &["--version"]).
        /// If None, just checks existence.
        version_cmd: Option<(&'static str, &'static [&'static str])>,
    },
    /// Check that a Python module can be imported.
    PythonImport {
        module: &'static str,
    },
    /// Run an arbitrary command; exit code 0 means installed.
    Command {
        program: &'static str,
        args: &'static [&'static str],
    },
}

/// A single dependency with its check and install instructions.
pub struct Dependency {
    pub name: &'static str,
    pub check: DependencyCheck,
    pub install: &'static str,
}

/// Result of checking a single dependency.
pub struct DepStatus {
    pub name: &'static str,
    pub ok: bool,
    /// The resolved path or version if found.
    pub detail: String,
    /// Install instructions if not found.
    pub install: &'static str,
    /// Optional warning (tool found but degraded).
    pub warning: Option<String>,
}

/// Check a single dependency.
pub fn check_dep(dep: Dependency) -> DepStatus {
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
                        warning: None,
                    };
                }
                for dir in extra_tool_dirs() {
                    let path = dir.join(name);
                    if path.is_file() {
                        return DepStatus {
                            name: dep.name,
                            ok: true,
                            detail: path.display().to_string(),
                            install: dep.install,
                            warning: None,
                        };
                    }
                }
            }
            DepStatus {
                name: dep.name,
                ok: false,
                detail: "not found".into(),
                install: dep.install,
                warning: None,
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
                warning: None,
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
                warning: None,
            }
        }
    }
}

/// Resolve a binary name to its full path, checking PATH and extra tool dirs.
/// Returns the full path if found, or the original name as fallback.
pub fn find_bin(name: &str) -> String {
    if let Ok(path) = which::which(name) {
        return path.display().to_string();
    }
    for dir in extra_tool_dirs() {
        let path = dir.join(name);
        if path.is_file() {
            return path.display().to_string();
        }
    }
    name.to_string()
}

/// Extra directories to search for tool binaries not on PATH.
pub fn extra_tool_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        let home = std::path::PathBuf::from(&home);
        dirs.push(home.join(".dotnet/tools"));
        dirs.push(home.join(".ghcup/bin"));
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join(".local/bin"));
    }
    dirs
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
            if let Some(warn) = &s.warning {
                out.push_str(&format!("    WARNING: {warn}\n"));
            }
        }
    }
    out
}
