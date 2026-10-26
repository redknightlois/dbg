use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
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

pub(crate) fn session_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for attempt in 0..100u32 {
            let dir =
                std::env::temp_dir().join(format!("gdbg-{}-{stamp}-{attempt}", std::process::id()));
            if std::fs::create_dir(&dir).is_ok() {
                return dir;
            }
        }
        // Keep the error at the first file creation instead of silently
        // opening a stale directory from a previous process.
        std::env::temp_dir().join(format!("gdbg-unusable-{}-{stamp}", std::process::id()))
    })
    .clone()
}

// ---------------------------------------------------------------------------
// Full collection pipeline
// ---------------------------------------------------------------------------

#[derive(clap::Args, Default)]
pub struct CollectionOptions {
    #[arg(
        long,
        conflicts_with = "kernel_name",
        help = "Collect counters for every timeline kernel instead of the top five"
    )]
    pub all_kernels: bool,
    #[arg(
        long,
        conflicts_with = "launch_count",
        help = "Collect every matching launch instead of the first per kernel"
    )]
    pub all_launches: bool,
    #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new(), help = "Native kernel name to profile (repeatable, regex-escaped literal)")]
    pub kernel_name: Vec<String>,
    #[arg(long, conflicts_with = "section", value_parser = clap::builder::NonEmptyStringValueParser::new(), help = "Nsight Compute metric set; defaults to full unless sections are selected")]
    pub ncu_set: Option<String>,
    #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new(), help = "Nsight Compute section ID (repeatable; replaces the default full set)")]
    pub section: Vec<String>,
    #[arg(
        long,
        default_value_t = 0,
        help = "Skip this many matching launches per selected kernel invocation"
    )]
    pub launch_skip: u32,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..), help = "Collect this many matching launches per selected kernel; defaults to 1")]
    pub launch_count: Option<u32>,
    #[arg(
        long,
        requires = "kernel_name",
        conflicts_with = "timeline_only",
        help = "Collect selected counters only, without another timeline or operator capture"
    )]
    pub counters_only: bool,
    #[arg(long, conflicts_with_all = ["kernel_name", "section", "ncu_set", "launch_skip", "launch_count", "all_kernels", "all_launches"], help = "Collect only the Nsight Systems timeline")]
    pub timeline_only: bool,
}

pub fn collect_all(
    db: &GpuDb,
    target: &str,
    args: &[String],
    options: &CollectionOptions,
) -> Result<()> {
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

    // Pre-flight: for Python targets, verify the required deps import
    // before handing the script to nsys. Without this, missing torch
    // or triton surfaces as nsys hanging on a Python child that
    // ModuleNotFoundError'd immediately — no timeout, no message.
    if let Some(module) = python_preflight_module(kind) {
        let out = Command::new("python3")
            .args(["-c", &format!("import {module}")])
            .output();
        match out {
            Ok(o) if !o.status.success() => {
                bail!(
                    "`python3 -c \"import {module}\"` failed — gdbg would have \
                     hung in nsys waiting on a crashing child. Install the \
                     dependency (e.g. `pip install {module}`) and retry.\n\
                     stderr: {}",
                    String::from_utf8_lossy(&o.stderr).trim(),
                );
            }
            Err(e) => {
                bail!("python3 not available for pre-flight check: {e}");
            }
            _ => {}
        }
    }

    // Runs a collection phase, recording failures without aborting.
    let run_phase = |phase: &str, f: &dyn Fn() -> Result<()>| {
        if let Err(e) = f() {
            eprintln!("{phase} collection failed: {e:#}");
            let _ = db.add_failure(phase, &format!("{e:#}"));
        }
    };

    db.set_meta(
        "collection_mode",
        if options.timeline_only {
            "timeline only"
        } else if options.counters_only {
            "counters only"
        } else {
            "full pipeline"
        },
    )?;
    if !options.counters_only {
        eprintln!("--- phase 1: timeline capture (nsys) ---");
        run_phase("nsys", &|| {
            collect_nsys(db, target, args, kind, &session, target_hash.as_deref())
        });
    }
    if options.timeline_only {
        return Ok(());
    }

    let names = if options.kernel_name.is_empty() {
        top_kernel_names(db, if options.all_kernels { 0 } else { 5 })
    } else {
        options.kernel_name.clone()
    };
    db.set_meta(
        "ncu_kernel_scope",
        if !options.kernel_name.is_empty() {
            "selected"
        } else if options.all_kernels {
            "all"
        } else {
            "top five"
        },
    )?;
    db.set_meta("ncu_selected_kernel_names", &serde_json::to_string(&names)?)?;
    db.set_meta("ncu_sections", &serde_json::to_string(&options.section)?)?;
    db.set_meta(
        "ncu_set",
        options
            .ncu_set
            .as_deref()
            .unwrap_or(if options.section.is_empty() {
                "full"
            } else {
                "sections only"
            }),
    )?;
    db.set_meta("ncu_launch_skip", &options.launch_skip.to_string())?;
    db.set_meta(
        "ncu_launch_count",
        &if options.all_launches {
            "all".into()
        } else {
            options.launch_count.unwrap_or(1).to_string()
        },
    )?;
    db.set_meta(
        "ncu_launch_scope",
        if options.all_launches && options.launch_skip == 0 {
            "all matching"
        } else if options.launch_skip == 0 && options.launch_count.unwrap_or(1) == 1 {
            "first per kernel"
        } else {
            "selected matching instances"
        },
    )?;

    if !names.is_empty() {
        eprintln!("--- phase 2: deep kernel metrics (ncu) ---");
        eprintln!(
            "  profiling {} kernel selections: {}",
            names.len(),
            names.join(", ")
        );
        run_phase("ncu", &|| {
            collect_ncu(
                db,
                target,
                args,
                &names,
                kind,
                &session,
                target_hash.as_deref(),
                options,
            )
        });
    } else {
        eprintln!("--- phase 2: skipped (no kernels found in phase 1) ---");
    }
    if options.counters_only {
        return Ok(());
    }

    // Phase 3: op mapping (PyTorch/Triton only)
    match kind {
        TargetKind::PythonTorch => {
            eprintln!("--- phase 3: op mapping (torch.profiler) ---");
            run_phase("torch", &|| {
                collect_torch(db, target, args, &session, target_hash.as_deref(), "torch")
            });
        }
        TargetKind::PythonTriton => {
            eprintln!("--- phase 3: op mapping (proton) ---");
            run_phase("proton", &|| {
                collect_proton(db, target, args, &session, target_hash.as_deref())
            });
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
    db.normalize_kernel_names()?;
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

    let mut cmd = nsys_command(target, args, kind, &trace_base);

    let command = format!("{cmd:?}");
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
            let importer =
                find_bundled_tool(&NSIGHT_SYSTEMS, "QdstrmImporter").ok_or_else(|| {
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

    let _layer_id = db.import_layer(
        "nsys",
        &trace_rep.display().to_string(),
        Some(&command),
        Some(elapsed),
        target_hash,
        |conn, layer_id| parsers::nsys::import_nsys_rep(conn, &sqlite_path, layer_id),
    )?;

    eprintln!(
        "  nsys done in {elapsed:.1}s ({} kernels, {} launches)",
        db.unique_kernel_count(),
        db.total_launch_count()
    );
    Ok(())
}

fn nsys_command(target: &str, args: &[String], kind: TargetKind, trace: &Path) -> Command {
    let mut cmd = Command::new("nsys");
    cmd.args(["profile", "-o"]);
    cmd.arg(trace);
    cmd.arg("--force-overwrite=true");
    cmd.arg("--cuda-memory-usage=true");
    // Whole-graph tracing omits the kernel launches inside graph replays.
    cmd.arg("--cuda-graph-trace=node");
    cmd.arg("--cuda-um-gpu-page-faults=true");
    if matches!(
        kind,
        TargetKind::Python | TargetKind::PythonTorch | TargetKind::PythonTriton
    ) {
        cmd.arg("python3");
    }
    cmd.arg(target).args(args);
    cmd
}

// ---------------------------------------------------------------------------
// ncu collection
// ---------------------------------------------------------------------------

fn collect_ncu(
    db: &GpuDb,
    target: &str,
    args: &[String],
    kernel_names: &[String],
    kind: TargetKind,
    session: &Path,
    target_hash: Option<&str>,
    options: &CollectionOptions,
) -> Result<()> {
    let start = Instant::now();
    let unfiltered = options.all_kernels && options.all_launches;
    let labels = if unfiltered {
        vec!["all kernels".to_string()]
    } else {
        kernel_names.to_vec()
    };
    let description = format!(
        "ncu set={:?} sections={:?} kernels={:?} skip={} count={:?} all_launches={} target={target} args={args:?}",
        options.ncu_set,
        options.section,
        labels,
        options.launch_skip,
        options.launch_count,
        options.all_launches
    );
    collect_ncu_samples(db, session, &labels, &description, target_hash, |label| {
        let filter = if unfiltered { None } else { Some(label) };
        let mut cmd = ncu_command(target, args, filter, kind, options);
        cmd.output()
            .with_context(|| format!("cannot start ncu for `{label}`"))
    })?;
    let elapsed = start.elapsed().as_secs_f64();

    eprintln!(
        "  ncu done in {elapsed:.1}s ({} kernels with metrics)",
        db.kernels_with_metrics()
    );
    Ok(())
}

fn collect_ncu_samples(
    db: &GpuDb,
    session: &Path,
    kernel_names: &[String],
    command: &str,
    target_hash: Option<&str>,
    mut run: impl FnMut(&str) -> Result<Output>,
) -> Result<PathBuf> {
    let csv_path = session.join("ncu_metrics.csv");
    let mut combined_csv = Vec::new();
    let mut imported = 0;
    let mut partial = false;
    let mut layer_id = None;
    let collection_started = Instant::now();
    db.set_meta("ncu_collection_status", "collecting")?;
    db.set_meta("ncu_requested_captures", &kernel_names.len().to_string())?;
    db.set_meta("ncu_imported_captures", "0")?;
    for (index, kernel_name) in kernel_names.iter().enumerate() {
        let stdout_path = session.join(format!("ncu-{index}.stdout.csv"));
        let stderr_path = session.join(format!("ncu-{index}.stderr.log"));
        let failure = match run(kernel_name) {
            Ok(output) => {
                std::fs::write(&stdout_path, &output.stdout)?;
                std::fs::write(&stderr_path, &output.stderr)?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                let process_failure = (!output.status.success())
                    .then(|| format!("ncu exited with {}", output.status));
                if !stdout.contains("\"Kernel Name\"")
                    || !stdout.contains("\"Metric Name\"")
                    || !stdout.contains("\"Metric Value\"")
                {
                    Some(
                        process_failure
                            .unwrap_or_else(|| "ncu returned no metric table".to_string()),
                    )
                } else {
                    let imported_sample = (|| -> Result<i64> {
                        let transaction = db.conn.unchecked_transaction()?;
                        let layer = match layer_id {
                            Some(id) => id,
                            None => db.add_layer(
                                "ncu",
                                &csv_path.display().to_string(),
                                Some(command),
                                None,
                                target_hash,
                            )?,
                        };
                        parsers::ncu::import_ncu_csv(&transaction, &stdout_path, layer)?;
                        db.set_meta("ncu_imported_captures", &(imported + 1).to_string())?;
                        transaction.execute(
                            "UPDATE layers SET collection_secs=?1 WHERE id=?2",
                            rusqlite::params![collection_started.elapsed().as_secs_f64(), layer],
                        )?;
                        transaction.commit()?;
                        Ok(layer)
                    })();
                    match imported_sample {
                        Err(error) => {
                            Some(format!("cannot import completed ncu capture: {error:#}"))
                        }
                        Ok(layer) => {
                            layer_id = Some(layer);
                            imported += 1;
                            let keep_header = combined_csv.is_empty();
                            for line in stdout.lines() {
                                if !keep_header
                                    && line.contains("\"Kernel Name\"")
                                    && line.contains("\"Metric Name\"")
                                    && line.contains("\"Metric Value\"")
                                {
                                    continue;
                                }
                                combined_csv.extend_from_slice(line.as_bytes());
                                combined_csv.push(b'\n');
                            }
                            if !combined_csv.ends_with(b"\n") {
                                combined_csv.push(b'\n');
                            }
                            std::fs::write(&csv_path, &combined_csv)?;
                            process_failure
                        }
                    }
                }
            }
            Err(error) => {
                std::fs::write(&stderr_path, format!("{error:#}"))?;
                Some(format!("{error:#}"))
            }
        };
        if let Some(reason) = failure {
            partial = true;
            let message = format!(
                "{reason} for `{kernel_name}`; raw output: {} and {}",
                stdout_path.display(),
                stderr_path.display()
            );
            eprintln!("  ncu partial collection: {message}");
            db.add_failure("ncu", &message)?;
        }
    }
    db.set_meta(
        "ncu_collection_status",
        if !partial && imported == kernel_names.len() {
            "complete"
        } else {
            "partial"
        },
    )?;
    if combined_csv.is_empty() {
        bail!(
            "ncu collected no metrics for any selected kernel; inspect the saved per-kernel logs"
        );
    }
    Ok(csv_path)
}

fn ncu_command(
    target: &str,
    args: &[String],
    kernel_name: Option<&str>,
    kind: TargetKind,
    options: &CollectionOptions,
) -> Command {
    let mut cmd = Command::new("ncu");
    if let Some(set) = &options.ncu_set {
        cmd.args(["--set", set]);
    } else if options.section.is_empty() {
        cmd.args(["--set", "full"]);
    }
    for section in &options.section {
        cmd.args(["--section", section]);
    }
    cmd.args(["--csv", "--print-units", "base"]);
    // The importer keys off Nsight Compute's stable metric identifiers (for
    // example `sm__throughput.avg.pct_of_peak_sustained_elapsed`).  CSV output
    // uses localized/display labels such as `Compute (SM) Throughput` unless
    // this is requested explicitly, which creates a metrics row containing
    // only NULL values and makes the REPL report `[no ncu]`.
    cmd.args(["--print-metric-name", "name"]);
    cmd.args(["--kernel-name-base", "demangled"]);
    if let Some(kernel_name) = kernel_name {
        let regex = regex::escape(kernel_name);
        cmd.args(["--kernel-name", &format!("regex:{regex}")]);
    }
    if options.launch_skip > 0 {
        cmd.args(["--launch-skip", &options.launch_skip.to_string()]);
    }
    if !options.all_launches {
        cmd.args([
            "--launch-count",
            &options.launch_count.unwrap_or(1).to_string(),
        ]);
    }
    if matches!(
        kind,
        TargetKind::Python | TargetKind::PythonTorch | TargetKind::PythonTriton
    ) {
        cmd.arg("python3").arg(target);
    } else {
        cmd.arg(target);
    }
    cmd.args(args);
    cmd
}

/// Collect Triton's operator mapping with Proton. Triton targets must not
/// use the torch.profiler wrapper: that records a torch layer even when the
/// requested collector is Proton and produces misleading cross-track data.
fn collect_proton(
    db: &GpuDb,
    target: &str,
    args: &[String],
    session: &Path,
    target_hash: Option<&str>,
) -> Result<()> {
    let trace_json = session.join("proton_trace.json");
    let start = Instant::now();
    let mut cmd = Command::new("proton");
    cmd.args(["profile", "-o"])
        .arg(&trace_json)
        .arg("--")
        .arg("python3")
        .arg(target)
        .args(args);
    run_cmd(&mut cmd, "proton failed")?;
    if !trace_json.exists() {
        bail!("proton did not produce {}", trace_json.display());
    }
    let elapsed = start.elapsed().as_secs_f64();
    let _layer_id = db.import_layer(
        "proton",
        &trace_json.display().to_string(),
        Some(&format!("proton profile {target}")),
        Some(elapsed),
        target_hash,
        |conn, layer_id| parsers::chrome_trace::import_chrome_trace(conn, &trace_json, layer_id),
    )?;
    eprintln!("  proton done in {elapsed:.1}s");
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
    wrapper.push_str(&format!(
        "    runpy.run_path({}, run_name='__main__')\n",
        escape_python_str(target)
    ));
    wrapper.push_str(&format!(
        "prof.export_chrome_trace({})\n",
        escape_python_str(trace_json.display().to_string().as_str())
    ));
    std::fs::write(&wrapper_path, &wrapper)?;

    run_cmd(
        Command::new("python3").arg(&wrapper_path),
        "torch.profiler wrapper failed",
    )?;
    let elapsed = start.elapsed().as_secs_f64();

    if !trace_json.exists() {
        bail!("torch.profiler did not produce {}", trace_json.display());
    }

    let _layer_id = db.import_layer(
        layer_name,
        &trace_json.display().to_string(),
        Some(&format!("torch.profiler on {target}")),
        Some(elapsed),
        target_hash,
        |conn, layer_id| parsers::chrome_trace::import_chrome_trace(conn, &trace_json, layer_id),
    )?;

    eprintln!("  torch.profiler done in {elapsed:.1}s");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn top_kernel_names(db: &GpuDb, n: usize) -> Vec<String> {
    let mut stmt = db.conn.prepare(&format!(
        "SELECT kernel_name, SUM(duration_us) as total FROM launches WHERE {} GROUP BY kernel_name ORDER BY total DESC LIMIT ?1",
        db.timeline_filter(),
    )).unwrap();
    stmt.query_map([if n == 0 { -1 } else { n as i64 }], |row| {
        row.get::<_, String>(0)
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .map(|name| db.native_kernel_name(&name))
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
pub(crate) fn cuda_compile_flags<'a>(output: &'a str, source: &'a str) -> [&'a str; 6] {
    ["-g", "-G", "-arch=native", "-o", output, source]
}

/// Which Python module, if any, we should verify importable before
/// handing a script to nsys. Returning None means no pre-flight check
/// (plain Python and non-Python targets). Keeping this as a pure
/// function makes it trivially unit-testable without touching
/// subprocess execution.
pub(crate) fn python_preflight_module(kind: TargetKind) -> Option<&'static str> {
    match kind {
        TargetKind::PythonTorch => Some("torch"),
        TargetKind::PythonTriton => Some("triton"),
        _ => None,
    }
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
    fn nsys_collects_cuda_graph_kernel_nodes() {
        let command = nsys_command(
            "train.py",
            &["--tokens".into(), "256".into()],
            TargetKind::PythonTorch,
            Path::new("trace"),
        );
        let args: Vec<_> = command
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|arg| arg == "--cuda-graph-trace=node"));
        assert!(args.ends_with(&[
            "python3".into(),
            "train.py".into(),
            "--tokens".into(),
            "256".into()
        ]));
    }

    #[cfg(unix)]
    #[test]
    fn ncu_keeps_metrics_before_and_after_unmatched_kernel() {
        use std::os::unix::process::ExitStatusExt;
        let directory = tempfile::tempdir().unwrap();
        let db = GpuDb::create(&directory.path().join("session.db")).unwrap();
        let names = vec![
            "first".to_string(),
            "missing".to_string(),
            "last".to_string(),
        ];
        let mut visited = Vec::new();
        let csv = collect_ncu_samples(&db, directory.path(), &names, "test ncu", Some("test-target"), |name| {
            visited.push(name.to_string());
            let stdout = if name == "missing" {
                "==WARNING== No kernels were profiled.\n".to_string()
            } else {
                format!("==PROF== Connected to process 123\n\"Kernel Name\",\"Metric Name\",\"Metric Value\"\n\"{name}\",\"launch__registers_per_thread\",\"32\"\n")
            };
            Ok(Output { status: std::process::ExitStatus::from_raw(if name == "last" { 1 << 8 } else { 0 }), stdout: stdout.into_bytes(), stderr: Vec::new() })
        }).unwrap();
        assert_eq!(visited, names);
        assert!(directory.path().join("ncu-1.stdout.csv").exists());
        assert_eq!(db.failures().len(), 2);
        assert!(csv.exists());
        let layers: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM layers WHERE source='ncu'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(layers, 1);
        let count: i64 = db
            .conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM metrics WHERE {}",
                    db.metric_filter_for("metrics")
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[cfg(unix)]
    #[test]
    fn ncu_commits_completed_capture_before_next_process_is_killed() {
        use std::os::unix::process::ExitStatusExt;
        const CHILD_DIR: &str = "GDBG_NCU_INTERRUPTION_TEST_DIR";
        if let Some(directory) = std::env::var_os(CHILD_DIR) {
            let directory = PathBuf::from(directory);
            let db = GpuDb::create(&directory.join("session.db")).unwrap();
            collect_ncu_samples(&db, &directory, &["first".into(), "blocked".into()],
                "ncu test command", Some("frozen-source"), |name| {
                    if name == "blocked" {
                        std::fs::write(directory.join("second-started"), "ready").unwrap();
                        loop { std::thread::sleep(std::time::Duration::from_millis(100)); }
                    }
                    Ok(Output {
                        status: std::process::ExitStatus::from_raw(0),
                        stdout: b"\"Kernel Name\",\"Metric Name\",\"Metric Value\"\n\"first\",\"launch__registers_per_thread\",\"32\"\n".to_vec(),
                        stderr: Vec::new(),
                    })
                }).unwrap();
            unreachable!();
        }
        let directory = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "collect::tests::ncu_commits_completed_capture_before_next_process_is_killed",
            ])
            .env(CHILD_DIR, directory.path())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !directory.path().join("second-started").exists() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child.kill().unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(9));
        assert!(
            directory.path().join("second-started").exists(),
            "collector never started the second capture"
        );
        let db = GpuDb::open(&directory.path().join("session.db")).unwrap();
        assert_eq!(db.kernels_with_metrics(), 1);
        let (file, command, hash): (String, String, String) = db
            .conn
            .query_row(
                "SELECT file,command_used,target_hash FROM layers WHERE source='ncu'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(file.ends_with("ncu_metrics.csv"));
        assert!(directory.path().join("ncu-0.stdout.csv").exists());
        assert_eq!(
            (command.as_str(), hash.as_str()),
            ("ncu test command", "frozen-source")
        );
        let state: String = db
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key='ncu_collection_status'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "collecting");
        let imported: String = db
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key='ncu_imported_captures'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(imported, "1");
        assert!(directory.path().join("ncu_metrics.csv").exists());
    }

    #[cfg(unix)]
    #[test]
    fn ncu_records_process_errors_and_rejects_empty_collection() {
        use std::os::unix::process::ExitStatusExt;
        let directory = tempfile::tempdir().unwrap();
        let db = GpuDb::create(&directory.path().join("session.db")).unwrap();
        let names = vec!["failed".to_string(), "unavailable".to_string()];
        let error = collect_ncu_samples(
            &db,
            directory.path(),
            &names,
            "test ncu",
            Some("test-target"),
            |name| {
                if name == "unavailable" {
                    bail!("cannot start collector");
                }
                Ok(Output {
                    status: std::process::ExitStatus::from_raw(1 << 8),
                    stdout: Vec::new(),
                    stderr: b"counter unavailable".to_vec(),
                })
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("no metrics"));
        assert_eq!(db.failures().len(), 2);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("ncu-0.stderr.log")).unwrap(),
            "counter unavailable"
        );
    }

    #[test]
    fn ncu_filter_preserves_typed_template_arguments() {
        let raw = "void at::native::add<(int)4, std::array<char *, (unsigned long)3>>(int, T2)";
        let command = ncu_command(
            "train.py",
            &[],
            Some(raw),
            TargetKind::PythonTorch,
            &CollectionOptions::default(),
        );
        let args: Vec<_> = command
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&format!("regex:{}", regex::escape(raw))));
    }

    #[test]
    fn cuda_compile_flags_does_not_pass_lineinfo_with_capital_g() {
        let flags = cuda_compile_flags("out", "in.cu");
        assert!(flags.contains(&"-arch=native"));
        assert!(
            flags.contains(&"-G"),
            "-G must stay — gdbg needs device symbols: {flags:?}"
        );
        assert!(
            !flags.contains(&"-lineinfo"),
            "-lineinfo must be dropped when -G is present: {flags:?}"
        );
    }

    /// Regression: when torch/triton weren't installed, gdbg hung in
    /// nsys waiting on a Python child that crashed immediately with
    /// ModuleNotFoundError. collect_all now pre-flights the required
    /// import; python_preflight_module declares what each TargetKind
    /// needs.
    #[test]
    fn python_preflight_module_matches_target_kind() {
        assert_eq!(
            python_preflight_module(TargetKind::PythonTorch),
            Some("torch")
        );
        assert_eq!(
            python_preflight_module(TargetKind::PythonTriton),
            Some("triton")
        );
        // Plain Python and non-Python targets: no pre-flight — the
        // script either runs under nsys directly or there's no Python
        // dep to verify.
        assert_eq!(python_preflight_module(TargetKind::Python), None);
        assert_eq!(python_preflight_module(TargetKind::Binary), None);
        assert_eq!(python_preflight_module(TargetKind::CudaSource), None);
    }

    #[test]
    fn ncu_command_matches_nsys_demangled_name_and_bounds_launches() {
        let cmd = ncu_command(
            "./app",
            &["--size".into(), "42".into()],
            Some("add_one(float *, int)"),
            TargetKind::Binary,
            &CollectionOptions::default(),
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|w| w == ["--kernel-name-base", "demangled"]),
            "ncu must match the demangled names imported from nsys: {args:?}"
        );
        assert!(
            args.windows(2).any(|w| w == ["--print-units", "base"]),
            "ncu must emit base units consumed by the parser: {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--print-metric-name", "name"]),
            "ncu must emit stable metric identifiers consumed by the parser: {args:?}"
        );
        assert!(
            args.windows(2).any(|w| w == ["--launch-count", "1"]),
            "each ncu subprocess must capture one representative launch: {args:?}"
        );
        assert!(
            args.iter()
                .any(|arg| arg == "regex:add_one\\(float \\*, int\\)"),
            "kernel signatures must be regex-escaped: {args:?}"
        );
        assert!(args.ends_with(&["./app".into(), "--size".into(), "42".into()]));
    }

    #[test]
    fn selected_sections_and_second_matching_instance_preserve_raw_units() {
        let mut options = CollectionOptions {
            section: vec!["WarpStateStats".into(), "SpeedOfLight".into()],
            launch_skip: 1,
            launch_count: Some(1),
            ..Default::default()
        };
        let command = ncu_command(
            "worker.py",
            &[],
            Some("Marlin<(int)4>"),
            TargetKind::PythonTorch,
            &options,
        );
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|arg| arg == "--set"));
        for pair in [
            ["--section", "WarpStateStats"],
            ["--section", "SpeedOfLight"],
            ["--launch-skip", "1"],
            ["--launch-count", "1"],
            ["--print-units", "base"],
            ["--print-metric-name", "name"],
        ] {
            assert!(
                args.windows(2).any(|window| window == pair),
                "missing {pair:?}: {args:?}"
            );
        }
        assert!(args.contains(&format!("regex:{}", regex::escape("Marlin<(int)4>"))));
        options.section.clear();
        options.ncu_set = Some("basic".into());
        let command = ncu_command("./app", &[], None, TargetKind::Binary, &options);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|window| window == ["--set", "basic"]));
        let command = ncu_command(
            "./app",
            &[],
            None,
            TargetKind::Binary,
            &CollectionOptions::default(),
        );
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|window| window == ["--set", "full"]));
        assert!(
            args.windows(2)
                .any(|window| window == ["--launch-count", "1"])
        );
    }

    #[test]
    fn ncu_command_wraps_python_targets_after_profiler_flags() {
        let cmd = ncu_command(
            "train.py",
            &["--steps".into(), "2".into()],
            Some("kernel()"),
            TargetKind::PythonTorch,
            &CollectionOptions::default(),
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.ends_with(&[
            "python3".into(),
            "train.py".into(),
            "--steps".into(),
            "2".into(),
        ]));
    }

    #[test]
    fn unlimited_counters_keep_all_kernel_instances_in_one_invocation() {
        let cmd = ncu_command(
            "driver.py",
            &["--replays".into(), "1".into()],
            None,
            TargetKind::PythonTorch,
            &CollectionOptions {
                all_launches: true,
                ..Default::default()
            },
        );
        let args: Vec<_> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--launch-count" || arg == "--kernel-name")
        );
        assert!(args.ends_with(&[
            "python3".into(),
            "driver.py".into(),
            "--replays".into(),
            "1".into()
        ]));
        let matched = ncu_command(
            "./app",
            &[],
            Some("kernel()"),
            TargetKind::Binary,
            &CollectionOptions {
                all_launches: true,
                ..Default::default()
            },
        );
        let args: Vec<_> = matched
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|arg| arg == "--kernel-name"));
        assert!(!args.iter().any(|arg| arg == "--launch-count"));
    }
    #[test]
    fn counter_collection_uses_native_names_while_queries_keep_canonical_keys() {
        let dir = tempfile::tempdir().unwrap();
        let db = GpuDb::create(&dir.path().join("native-names.db")).unwrap();
        let layer = db
            .add_layer("nsys", "trace.sqlite", None, None, None)
            .unwrap();
        let raw = "void <unnamed>::gemv_kernel<(unsigned int)8192>(float *)";
        let canonical = crate::kernel::normalize_kernel_name(raw);
        db.conn
            .execute(
                "INSERT INTO launches (kernel_name, duration_us, layer_id) VALUES (?1, 5, ?2)",
                rusqlite::params![canonical, layer],
            )
            .unwrap();
        db.set_meta(
            &format!("nsys_kernel_names_{layer}"),
            &serde_json::json!({canonical: raw}).to_string(),
        )
        .unwrap();
        assert_eq!(top_kernel_names(&db, 1), vec![raw]);
        assert_eq!(db.unique_kernel_count(), 1);
        for index in 0..6 {
            db.conn
                .execute(
                    "INSERT INTO launches (kernel_name, duration_us, layer_id) VALUES (?1, 1, ?2)",
                    rusqlite::params![format!("other{index}"), layer],
                )
                .unwrap();
        }
        assert_eq!(top_kernel_names(&db, 5).len(), 5);
        assert_eq!(top_kernel_names(&db, 0).len(), 7);
    }
}
