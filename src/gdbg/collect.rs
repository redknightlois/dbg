use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use dbg_cli::deps::{BundledToolkit, ToolkitAnchor, ToolkitRoot, find_bundled_tool};

use super::db::GpuDb;
use super::parsers;

/// NVIDIA Nsight Systems install layout.
///
/// Helpers like `QdstrmImporter` live in `host-linux-x64/`, which is a
/// sibling of the `target-linux-x64/` directory that holds the `nsys`
/// binary on `$PATH`.  Debian's multiarch split puts those two dirs
/// under *different* prefixes, so the static-root list is load-bearing.
pub(super) const NSIGHT_SYSTEMS: BundledToolkit = BundledToolkit {
    name: "nsight-systems",
    bin_subdir: "host-linux-x64",
    roots: &[
        // Debian/Ubuntu apt (arch-independent helper dir).
        ToolkitRoot {
            path: "/usr/lib/nsight-systems",
            max_depth: 0,
            dir_filter: &[],
        },
        // Debian/Ubuntu apt (multiarch).
        ToolkitRoot {
            path: "/usr/lib/x86_64-linux-gnu/nsight-systems",
            max_depth: 0,
            dir_filter: &[],
        },
        // Standalone tarball / /opt install, possibly version-nested.
        ToolkitRoot {
            path: "/opt/nvidia/nsight-systems",
            max_depth: 1,
            dir_filter: &[],
        },
        // CUDA toolkit: /usr/local/cuda-<ver>/nsight-systems-<ver>/...
        ToolkitRoot {
            path: "/usr/local",
            max_depth: 2,
            dir_filter: &["cuda", "nsight-systems"],
        },
    ],
    anchor: Some(ToolkitAnchor {
        bin: "nsys",
        walk_up: 3,
    }),
};

/// Run a Command, check for success, bail with stderr on failure.
fn run_cmd(cmd: &mut Command, context: &str) -> Result<Output> {
    let output = cmd.output().with_context(|| context.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{context}:\n{stderr}");
    }
    Ok(output)
}

/// Compute a simple hash of a file for consistency checking.
/// Uses the file's size + first/last 4KB to avoid hashing multi-GB binaries.
fn hash_target(path: &str) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let meta = std::fs::metadata(path).ok()?;
    let size = meta.len();
    let mut file = std::fs::File::open(path).ok()?;

    // Read first 4KB
    let head_len = 4096.min(size as usize);
    let mut head = vec![0u8; head_len];
    file.read_exact(&mut head).ok()?;

    // Read last 4KB (if file is large enough that tail differs from head)
    let mut tail_sum: u64 = 0;
    if size > 8192 {
        let tail_len = 4096.min(size as usize);
        let mut tail = vec![0u8; tail_len];
        file.seek(SeekFrom::End(-(tail_len as i64))).ok()?;
        file.read_exact(&mut tail).ok()?;
        tail_sum = tail.iter().map(|&b| b as u64).sum();
    }

    // Simple fingerprint: size + head sum + tail sum (not cryptographic)
    let head_sum: u64 = head.iter().map(|&b| b as u64).sum();
    Some(format!("{size:x}:{head_sum:x}:{tail_sum:x}"))
}

// ---------------------------------------------------------------------------
// Target detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Binary,
    CudaSource,
    Python,
    PythonTorch,
    PythonTriton,
}

pub fn detect_target(target: &str) -> TargetKind {
    if target.ends_with(".cu") {
        return TargetKind::CudaSource;
    }
    if !target.ends_with(".py") {
        return TargetKind::Binary;
    }
    let content = std::fs::read_to_string(target).unwrap_or_default();
    if content.contains("import triton") || content.contains("from triton") {
        TargetKind::PythonTriton
    } else if content.contains("import torch") || content.contains("from torch") {
        TargetKind::PythonTorch
    } else {
        TargetKind::Python
    }
}

// ---------------------------------------------------------------------------
// Session temp directory
// ---------------------------------------------------------------------------

fn session_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gdbg-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

// ---------------------------------------------------------------------------
// Full collection pipeline
// ---------------------------------------------------------------------------

pub fn collect_all(db: &GpuDb, target: &str, args: &[String]) -> Result<()> {
    let kind = detect_target(target);
    let session = session_dir();

    // Compile .cu sources first
    let effective_target = if kind == TargetKind::CudaSource {
        eprintln!("--- compiling {target} ---");
        compile_cuda(target)?
    } else {
        target.to_string()
    };
    let target = effective_target.as_str();
    let target_hash = hash_target(target);

    // Runs a collection phase, recording failures without aborting.
    let run_phase = |phase: &str, f: &dyn Fn() -> Result<()>| {
        if let Err(e) = f() {
            eprintln!("{phase} collection failed: {e}");
            let _ = db.add_failure(phase, &e.to_string());
        }
    };

    // Phase 1: nsys timeline
    eprintln!("--- phase 1: timeline capture (nsys) ---");
    run_phase("nsys", &|| collect_nsys(db, target, args, kind, &session, target_hash.as_deref()));

    // Identify top kernels for ncu
    let top_names = top_kernel_names(db, 5);

    // Phase 2: ncu deep metrics (on top kernels only)
    if !top_names.is_empty() {
        eprintln!("--- phase 2: deep kernel metrics (ncu) ---");
        eprintln!("  profiling {} kernels: {}", top_names.len(), top_names.join(", "));
        run_phase("ncu", &|| collect_ncu(db, target, args, &top_names, &session, target_hash.as_deref()));
    } else {
        eprintln!("--- phase 2: skipped (no kernels found in phase 1) ---");
    }

    // Phase 3: op mapping (PyTorch/Triton only)
    match kind {
        TargetKind::PythonTorch => {
            eprintln!("--- phase 3: op mapping (torch.profiler) ---");
            run_phase("torch", &|| collect_torch(db, target, args, &session, target_hash.as_deref(), "torch"));
        }
        TargetKind::PythonTriton => {
            eprintln!("--- phase 3: op mapping (proton) ---");
            run_phase("proton", &|| collect_torch(db, target, args, &session, target_hash.as_deref(), "proton"));
        }
        _ => {
            eprintln!("--- phase 3: skipped (not a Python target) ---");
        }
    }

    // Re-compute op GPU times against the best timeline layer.
    // During phase 3 import, ops.gpu_time_us is computed from torch/proton
    // layer launches.  If nsys is also present (phase 1), its kernel
    // durations are more complete.  This ensures top-ops, compare-ops, and
    // hotpath stay consistent with breakdown and kernels.
    db.recompute_op_gpu_times();

    Ok(())
}

// ---------------------------------------------------------------------------
// nsys collection
// ---------------------------------------------------------------------------


fn collect_nsys(
    db: &GpuDb,
    target: &str,
    args: &[String],
    kind: TargetKind,
    session: &Path,
    target_hash: Option<&str>,
) -> Result<()> {
    let trace_base = session.join("trace");
    let trace_rep = session.join("trace.nsys-rep");
    let start = Instant::now();

    let mut cmd = Command::new("nsys");
    cmd.args(["profile", "-o"]);
    cmd.arg(&trace_base);
    cmd.arg("--force-overwrite=true");
    // Enable GPU memory allocation tracking — needed for the `memory` command.
    cmd.arg("--cuda-memory-usage=true");

    match kind {
        TargetKind::Python | TargetKind::PythonTorch | TargetKind::PythonTriton => {
            cmd.arg("python3");
        }
        _ => {}
    }
    cmd.arg(target);
    for a in args {
        cmd.arg(a);
    }

    let profile_output = run_cmd(&mut cmd, "nsys profile failed")?;
    let elapsed = start.elapsed().as_secs_f64();

    // nsys 2023.x on Debian/Ubuntu has a silent-importer bug: it writes
    // `trace.qdstrm` during profiling but fails to invoke QdstrmImporter
    // at the end (missing Qt runtime deps), yet still exits 0.  Detect
    // that case and run the importer ourselves.  nsys 2024+ folded the
    // importer into the main binary, so this branch becomes a no-op.
    if !trace_rep.exists() {
        let qdstrm = session.join("trace.qdstrm");
        if qdstrm.exists() {
            let importer = find_bundled_tool(&NSIGHT_SYSTEMS, "QdstrmImporter").ok_or_else(|| {
                let stderr = String::from_utf8_lossy(&profile_output.stderr);
                anyhow::anyhow!(
                    "nsys produced {} but no trace.nsys-rep (silent QdstrmImporter failure).\n\
                     Could not locate QdstrmImporter binary on this system.\n\
                     Install nsight-systems with its runtime deps, or upgrade nsys to 2024+.\n\
                     nsys stderr:\n{}",
                    qdstrm.display(),
                    stderr
                )
            })?;
            run_cmd(
                Command::new(&importer).arg("-i").arg(&qdstrm),
                "QdstrmImporter fallback failed",
            )?;
        }
        if !trace_rep.exists() {
            bail!("nsys did not produce {}", trace_rep.display());
        }
    }

    // nsys-rep is a proprietary container, not plain SQLite.
    // Export to SQLite first.
    let sqlite_path = session.join("trace.sqlite");
    run_cmd(
        Command::new("nsys")
            .args(["export", "--type", "sqlite", "--output"])
            .arg(&sqlite_path)
            .arg("--force-overwrite=true")
            .arg(&trace_rep),
        "nsys export to sqlite failed",
    )?;

    if !sqlite_path.exists() {
        bail!("nsys export did not produce {}", sqlite_path.display());
    }

    let layer_id = db.add_layer(
        "nsys",
        &trace_rep.display().to_string(),
        Some(&format!("nsys profile {target}")),
        Some(elapsed),
        target_hash,
    )?;

    parsers::nsys::import_nsys_rep(&db.conn, &sqlite_path, layer_id)?;

    eprintln!("  nsys done in {elapsed:.1}s ({} kernels, {} launches)",
        db.unique_kernel_count(), db.total_launch_count());
    Ok(())
}

// ---------------------------------------------------------------------------
// ncu collection
// ---------------------------------------------------------------------------

fn collect_ncu(
    db: &GpuDb,
    target: &str,
    args: &[String],
    kernel_names: &[String],
    session: &Path,
    target_hash: Option<&str>,
) -> Result<()> {
    let csv_path = session.join("ncu_metrics.csv");
    let start = Instant::now();

    let regex = kernel_names.join("|");

    let mut cmd = Command::new("ncu");
    cmd.args(["--set", "full", "--csv"]);
    cmd.args(["--kernel-name", &format!("regex:{regex}")]);
    cmd.arg(target);
    for a in args {
        cmd.arg(a);
    }

    let output = run_cmd(&mut cmd, "ncu failed")?;
    std::fs::write(&csv_path, &output.stdout)?;
    let elapsed = start.elapsed().as_secs_f64();

    let layer_id = db.add_layer(
        "ncu",
        &csv_path.display().to_string(),
        Some(&format!("ncu --set full --kernel-name regex:{regex} {target}")),
        Some(elapsed),
        target_hash,
    )?;

    parsers::ncu::import_ncu_csv(&db.conn, &csv_path, layer_id)?;

    eprintln!("  ncu done in {elapsed:.1}s ({} kernels with metrics)", db.kernels_with_metrics());
    Ok(())
}

// ---------------------------------------------------------------------------
// torch.profiler collection
// ---------------------------------------------------------------------------

fn collect_torch(
    db: &GpuDb,
    target: &str,
    args: &[String],
    session: &Path,
    target_hash: Option<&str>,
    layer_name: &str,
) -> Result<()> {
    let trace_json = session.join("torch_trace.json");
    let start = Instant::now();

    // Write a wrapper script to a temp file instead of using -c,
    // to avoid shell/Python injection via target or args.
    let wrapper_path = session.join("_torch_wrapper.py");
    let mut wrapper = String::new();
    wrapper.push_str("import sys, runpy\n");
    wrapper.push_str(&format!("sys.argv = [{}]\n", {
        let mut parts = vec![escape_python_str(target)];
        for a in args {
            parts.push(escape_python_str(a));
        }
        parts.join(", ")
    }));
    wrapper.push_str("import torch\n");
    wrapper.push_str("from torch.profiler import profile, ProfilerActivity\n");
    wrapper.push_str("with profile(\n");
    wrapper.push_str("    activities=[ProfilerActivity.CPU, ProfilerActivity.CUDA],\n");
    wrapper.push_str("    record_shapes=True,\n");
    wrapper.push_str("    with_stack=True,\n");
    wrapper.push_str(") as prof:\n");
    wrapper.push_str(&format!("    runpy.run_path({}, run_name='__main__')\n", escape_python_str(target)));
    wrapper.push_str(&format!("prof.export_chrome_trace({})\n", escape_python_str(trace_json.display().to_string().as_str())));
    std::fs::write(&wrapper_path, &wrapper)?;

    run_cmd(Command::new("python3").arg(&wrapper_path), "torch.profiler wrapper failed")?;
    let elapsed = start.elapsed().as_secs_f64();

    if !trace_json.exists() {
        bail!("torch.profiler did not produce {}", trace_json.display());
    }

    let layer_id = db.add_layer(
        layer_name,
        &trace_json.display().to_string(),
        Some(&format!("torch.profiler on {target}")),
        Some(elapsed),
        target_hash,
    )?;

    parsers::chrome_trace::import_chrome_trace(&db.conn, &trace_json, layer_id)?;

    eprintln!("  torch.profiler done in {elapsed:.1}s");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn top_kernel_names(db: &GpuDb, n: usize) -> Vec<String> {
    let mut stmt = db
        .conn
        .prepare(
            "SELECT kernel_name, SUM(duration_us) as total
             FROM launches GROUP BY kernel_name
             ORDER BY total DESC LIMIT ?1",
        )
        .unwrap();
    stmt.query_map([n as i64], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
}

/// Escape a string for safe use as a Python string literal.
/// Returns a single-quoted representation with backslash escaping.
fn escape_python_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

fn compile_cuda(source: &str) -> Result<String> {
    let path = std::path::Path::new(source);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("a");
    let output = path.parent().unwrap_or(Path::new(".")).join(stem);
    let output_str = output.display().to_string();

    let status = Command::new("nvcc")
        .args(cuda_compile_flags(&output_str, source))
        .status()
        .context("nvcc not found — install CUDA toolkit")?;

    if !status.success() {
        bail!("nvcc compilation failed for {source}");
    }
    Ok(output_str)
}

/// Flags for the `nvcc` invocation. `-G` (full device debug) already
/// implies line-number info, and passing it alongside `-lineinfo`
/// makes nvcc+ptxas print "Conflicting options" warnings on every
/// build. Keep `-G` — gdbg needs device-side symbols for kernel
/// attribution — and drop the redundant `-lineinfo`.
pub(crate) fn cuda_compile_flags<'a>(output: &'a str, source: &'a str) -> [&'a str; 5] {
    ["-g", "-G", "-o", output, source]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: nvcc+ptxas printed "Conflicting options specified:
    /// --device-debug --generate-line-info" on every build because
    /// both `-G` and `-lineinfo` were passed. `-G` already implies
    /// line info, so drop `-lineinfo` to silence the warning without
    /// losing debug information.
    #[test]
    fn cuda_compile_flags_does_not_pass_lineinfo_with_capital_g() {
        let flags = cuda_compile_flags("out", "in.cu");
        assert!(
            flags.contains(&"-G"),
            "-G must stay — gdbg needs device symbols: {flags:?}"
        );
        assert!(
            !flags.contains(&"-lineinfo"),
            "-lineinfo must be dropped when -G is present: {flags:?}"
        );
    }
}
