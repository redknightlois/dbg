//! Shared dependency-checking infrastructure.
//!
//! Used by both `dbg` and `gdbg` to verify tool availability.

use std::path::{Path, PathBuf};
use std::process::Command;

/// How to verify a dependency is installed.
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
            version_cmd,
            ..
        } => {
            for name in *alternatives {
                let found_path = which::which(name)
                    .map(|p| p.display().to_string())
                    .or_else(|_| {
                        for dir in extra_tool_dirs() {
                            let path = dir.join(name);
                            if path.is_file() {
                                return Ok(path.display().to_string());
                            }
                        }
                        Err(())
                    });
                if let Ok(path) = found_path {
                    // Binary exists on disk. If a version_cmd is
                    // provided, run it to verify the toolchain
                    // actually works (catches broken installs like a
                    // Homebrew GHC that can't find libc).
                    if let Some((probe_bin, probe_args)) = version_cmd {
                        let runnable = Command::new(probe_bin)
                            .args(*probe_args)
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .is_ok_and(|s| s.success());
                        if !runnable {
                            return DepStatus {
                                name: dep.name,
                                ok: false,
                                detail: format!("{path} (found but broken — `{probe_bin}` failed to run)"),
                                install: dep.install,
                                warning: None,
                            };
                        }
                    }
                    return DepStatus {
                        name: dep.name,
                        ok: true,
                        detail: path,
                        install: dep.install,
                        warning: None,
                    };
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

// ---------------------------------------------------------------------------
// Bundled-toolkit finder
// ---------------------------------------------------------------------------
//
// Some NVIDIA toolkits (Nsight Systems, Nsight Compute, the CUDA toolkit
// itself) ship helper binaries in an install-local subdirectory that is
// *not* on $PATH.  Example: NVIDIA Nsight Systems places the `nsys`
// CLI in `<prefix>/target-linux-x64/nsys` but its `QdstrmImporter`
// helper in the sibling `<prefix>/host-linux-x64/QdstrmImporter`.
//
// Finding those helpers is awkward because `<prefix>` varies:
//   * `/usr/lib/nsight-systems`                    (Debian/Ubuntu apt)
//   * `/usr/lib/x86_64-linux-gnu/nsight-systems`   (apt multiarch layout)
//   * `/opt/nvidia/nsight-systems/<ver>`           (tarball / standalone)
//   * `/usr/local/cuda-<ver>/nsight-systems-<ver>` (CUDA toolkit)
//
// `find_bundled_tool` takes a declarative description of where a toolkit
// can live and resolves a named helper binary.

/// A directory to probe for a bundled toolkit.
pub struct ToolkitRoot {
    /// Absolute path to probe.
    pub path: &'static str,
    /// How many levels to descend below `path` looking for the toolkit's
    /// `bin_subdir`.  `0` means `path` itself IS the toolkit root, so the
    /// tool is looked up at `<path>/<bin_subdir>/<tool>`.
    pub max_depth: usize,
    /// If non-empty, only descend into subdirectories whose names start
    /// with one of these prefixes.  Used to prune wide roots like
    /// `/usr/local` where only `cuda*` and `nsight-systems*` are relevant.
    pub dir_filter: &'static [&'static str],
}

/// Anchor a toolkit lookup to a binary that IS on `$PATH`.  When set,
/// `find_bundled_tool` canonicalizes the anchor binary and walks up the
/// directory tree looking for a sibling `<bin_subdir>/<tool>`.
pub struct ToolkitAnchor {
    /// Name of the binary (e.g. `"nsys"`).
    pub bin: &'static str,
    /// How many parent levels to walk above the resolved anchor before
    /// giving up.  Typical nsys-style layouts require 1 (grandparent).
    pub walk_up: usize,
}

/// Declarative description of a toolkit that bundles helpers in a
/// known subdirectory (e.g. `host-linux-x64/`).
pub struct BundledToolkit {
    /// Human-readable name, used for diagnostics.
    pub name: &'static str,
    /// Subdirectory within each install prefix that holds the helpers.
    pub bin_subdir: &'static str,
    /// Static roots to probe, ordered by preference.
    pub roots: &'static [ToolkitRoot],
    /// Optional `$PATH` anchor for non-standard installs.
    pub anchor: Option<ToolkitAnchor>,
}

/// Locate a helper binary inside a bundled toolkit.  Returns the full
/// path to the binary if found, otherwise `None`.
///
/// Resolution order:
///   1. Each `ToolkitRoot` in declaration order (with bounded descent).
///   2. The `ToolkitAnchor`, if set: `which <bin>` → canonicalize → walk up.
pub fn find_bundled_tool(toolkit: &BundledToolkit, tool: &str) -> Option<PathBuf> {
    for root in toolkit.roots {
        if let Some(p) = probe_root(
            Path::new(root.path),
            root.max_depth,
            root.dir_filter,
            toolkit.bin_subdir,
            tool,
        ) {
            return Some(p);
        }
    }
    if let Some(anchor) = &toolkit.anchor
        && let Some(p) = probe_anchor(anchor, toolkit.bin_subdir, tool)
    {
        return Some(p);
    }
    None
}

/// Recursive bounded-depth probe of a single toolkit root.
fn probe_root(
    root: &Path,
    max_depth: usize,
    dir_filter: &[&str],
    bin_subdir: &str,
    tool: &str,
) -> Option<PathBuf> {
    let candidate = root.join(bin_subdir).join(tool);
    if candidate.is_file() {
        return Some(candidate);
    }
    if max_depth == 0 {
        return None;
    }
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if !dir_filter.is_empty() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !dir_filter.iter().any(|p| name.starts_with(p)) {
                continue;
            }
        }
        if let Some(found) = probe_root(&path, max_depth - 1, dir_filter, bin_subdir, tool) {
            return Some(found);
        }
    }
    None
}

/// Resolve a tool like `dotnet` to its installation root.  Canonicalizes
/// `anchor_bin` from `$PATH` (following Homebrew-style shims), walks up
/// to `walk_up` levels looking for a `preferred_sibling` directory, and
/// falls back to the directory that directly contains the anchor binary.
///
/// If `sibling_marker` is `Some`, only `preferred_sibling` directories
/// that contain the given relative path (e.g. `"shared"`) are accepted —
/// used to gate on a shape-specific file that proves this is the right
/// root, not just a directory that happens to be named the same.
pub fn find_tool_root(
    anchor_bin: &str,
    preferred_sibling: Option<&str>,
    sibling_marker: Option<&str>,
    walk_up: usize,
) -> Option<PathBuf> {
    let path = which::which(anchor_bin).ok()?;
    let real = std::fs::canonicalize(&path).ok()?;

    if let Some(sibling) = preferred_sibling {
        let mut cur: &Path = real.as_path();
        for _ in 0..=walk_up {
            let candidate = cur.join(sibling);
            if candidate.is_dir() {
                let accepted = match sibling_marker {
                    Some(m) => candidate.join(m).exists(),
                    None => true,
                };
                if accepted {
                    return Some(candidate);
                }
            }
            match cur.parent() {
                Some(p) => cur = p,
                None => break,
            }
        }
    }

    // Fallback: directory containing the canonicalized anchor binary.
    real.parent().map(|p| p.to_path_buf())
}

/// Walk up from a `$PATH`-resolvable anchor binary, probing at each level
/// for `<cur>/<bin_subdir>/<tool>`.
fn probe_anchor(anchor: &ToolkitAnchor, bin_subdir: &str, tool: &str) -> Option<PathBuf> {
    let path = which::which(anchor.bin).ok()?;
    let real = std::fs::canonicalize(&path).ok()?;
    let mut cur: &Path = real.as_path();
    for _ in 0..=anchor.walk_up {
        let candidate = cur.join(bin_subdir).join(tool);
        if candidate.is_file() {
            return Some(candidate);
        }
        match cur.parent() {
            Some(parent) => cur = parent,
            None => return None,
        }
    }
    None
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
