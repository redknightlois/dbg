use dbg_cli::deps::{self, Dependency, DependencyCheck, DepStatus};

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
    let statuses: Vec<DepStatus> = dependencies().into_iter().map(deps::check_dep).collect();
    vec![("gdbg", statuses)]
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
