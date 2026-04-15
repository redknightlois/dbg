use std::process::Command;

use dbg_cli::deps::{self, Dependency, DependencyCheck, DepStatus, find_bundled_tool};

use super::collect::NSIGHT_SYSTEMS;

/// All dependencies gdbg can use.
fn dependencies() -> Vec<Dependency> {
    vec![
        Dependency {
            name: "nsys",
            check: DependencyCheck::Binary {
                name: "nsys",
                alternatives: &["nsys"],
                version_cmd: None,
            },
            install: "Install NVIDIA Nsight Systems: https://developer.nvidia.com/nsight-systems",
        },
        Dependency {
            name: "ncu",
            check: DependencyCheck::Binary {
                name: "ncu",
                alternatives: &["ncu"],
                version_cmd: None,
            },
            install: "Install NVIDIA Nsight Compute: https://developer.nvidia.com/nsight-compute",
        },
        Dependency {
            name: "python3",
            check: DependencyCheck::Binary {
                name: "python3",
                alternatives: &["python3"],
                version_cmd: Some(("python3", &["--version"])),
            },
            install: "https://python.org or: sudo apt install python3",
        },
        Dependency {
            name: "nvcc",
            check: DependencyCheck::Binary {
                name: "nvcc",
                alternatives: &["nvcc"],
                version_cmd: None,
            },
            install: "Install CUDA toolkit: https://developer.nvidia.com/cuda-downloads",
        },
    ]
}

/// Check all dependencies. Returns (name, Vec<DepStatus>) for consistent formatting.
pub fn check_all() -> Vec<(&'static str, Vec<DepStatus>)> {
    let mut statuses: Vec<DepStatus> = dependencies().into_iter().map(deps::check_dep).collect();

    // If ncu is installed, check whether GPU performance counters are accessible.
    if let Some(ncu) = statuses.iter_mut().find(|s| s.name == "ncu" && s.ok) {
        if gpu_profiling_restricted() {
            ncu.warning = Some(
                "GPU performance counters restricted to admin. ncu will fail.\n\
                 \x20   fix: sudo modprobe nvidia NVreg_RestrictProfilingToAdminUsers=0\n\
                 \x20   persist: echo 'options nvidia NVreg_RestrictProfilingToAdminUsers=0' \
                 | sudo tee /etc/modprobe.d/nvidia-perf.conf"
                    .into(),
            );
        }
    }

    // nsys 2023.x silent-importer bug: nsys profile exits 0 but fails to
    // convert .qdstrm → .nsys-rep because QdstrmImporter can't load.  If
    // this system has an affected nsys AND no locatable importer, warn —
    // our fallback path inside collect_nsys relies on finding it.
    if let Some(nsys) = statuses.iter_mut().find(|s| s.name == "nsys" && s.ok) {
        if let Some(ver) = nsys_major_version() {
            if ver == 2023 && find_bundled_tool(&NSIGHT_SYSTEMS, "QdstrmImporter").is_none() {
                nsys.warning = Some(
                    "nsys 2023.x has a silent QdstrmImporter bug and the helper \
                     is not locatable.\n\
                     \x20   GPU profiling will capture data but fail to produce \
                     trace.nsys-rep.\n\
                     \x20   fix: repair nsight-systems Qt runtime deps, or upgrade \
                     to nsys 2024+ (importer built in)."
                        .into(),
                );
            }
        }
    }

    vec![("gdbg", statuses)]
}

/// Parse `nsys --version` output for the major release year (e.g. 2023, 2024).
/// Returns `None` if nsys isn't callable or the output can't be parsed.
fn nsys_major_version() -> Option<u32> {
    let output = Command::new("nsys").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // Typical line: "NVIDIA (R) Nsight Systems ... Version 2023.4.4.54-234412822800v0"
    for tok in text.split(|c: char| c.is_whitespace() || c == ',') {
        let stripped = tok.trim_start_matches('v');
        let head = stripped.split('.').next()?;
        if let Ok(n) = head.parse::<u32>() {
            if (2019..=2099).contains(&n) {
                return Some(n);
            }
        }
    }
    None
}

/// Check whether the NVIDIA driver restricts GPU profiling to admin users.
/// Reads `/proc/driver/nvidia/params` looking for `RmProfilingAdminOnly: 1`.
fn gpu_profiling_restricted() -> bool {
    let Ok(params) = std::fs::read_to_string("/proc/driver/nvidia/params") else {
        return false; // Can't determine — assume ok
    };
    for line in params.lines() {
        if let Some(rest) = line.strip_prefix("RmProfilingAdminOnly:") {
            return rest.trim() == "1";
        }
    }
    false
}

/// Check that at least nsys is available (minimum for gdbg to be useful).
/// Returns a formatted error message if critical deps are missing.
pub fn check_minimum() -> Option<String> {
    let results = check_all();
    let statuses = &results[0].1;
    let nsys = statuses.iter().find(|d| d.name == "nsys").unwrap();

    if nsys.ok {
        return None;
    }

    Some(deps::format_results(&results))
}

/// Format a full dependency report.
pub fn format_report() -> String {
    deps::format_results(&check_all())
}
