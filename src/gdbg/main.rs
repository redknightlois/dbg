mod check;
mod collect;
mod commands;
mod db;
mod kernel;
mod parsers;
mod repl;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result, bail};
use clap::Parser;

use db::GpuDb;

#[derive(Parser)]
#[command(
    name = "gdbg",
    version,
    about = "GPU profiler — collect, correlate, and query CUDA/PyTorch/Triton performance data"
)]
struct Cli {
    #[command(flatten)]
    collection: collect::CollectionOptions,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.args.is_empty() {
        print_usage();
        return Ok(());
    }

    match cli.args[0].as_str() {
        "list" | "ls" => {
            commands::cmd_list();
            Ok(())
        }
        "diff" => cmd_diff(&cli.args[1..]),
        "check" => {
            print!("{}", check::format_report());
            Ok(())
        }
        "--from" => cmd_from(&cli.args[1..]),
        "codegen" => cmd_codegen(&cli.args[1..]),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        _ => cmd_profile(&cli.args, &cli.collection),
    }
}

fn print_usage() {
    println!("gdbg — GPU profiler for CUDA, PyTorch, and Triton\n");
    println!("  gdbg <target> [args...]         Profile a binary or script");
    println!("  gdbg --all-kernels --all-launches <target>  Collect every kernel instance");
    println!("  gdbg --timeline-only <target>   Capture a timeline before selecting counters");
    println!("  gdbg --counters-only --kernel-name NAME --section ID <target>  Selected counters");
    println!("  gdbg --from <name>              Reload a saved session");
    println!("  gdbg list                       List saved sessions");
    println!("  gdbg diff <a> <b>               Diff two saved sessions");
    println!("  gdbg check                      Check tool dependencies");
    println!("  gdbg codegen <action> ...       Build, capture and diff CUDA PTX/SASS\n");
    println!("Targets:");
    println!("  ./cuda_app                      CUDA binary (nsys + ncu)");
    println!("  train.py                        PyTorch script (nsys + ncu + torch.profiler)");
    println!("  kernel.py                       Triton script (nsys + ncu + proton)");
    println!("  kernel.cu                       CUDA source (compile + profile)\n");
    println!("The profiler runs in 3 phases:");
    println!("  1. Timeline capture (nsys)      — kernel durations, transfers, gaps");
    println!("  2. Deep kernel metrics (ncu)    — boundedness, occupancy, bandwidth");
    println!("  3. Op mapping (torch/proton)    — which Python op launched which kernel\n");
    println!("Session data is stored in a SQLite database — no reparsing needed.");
    println!("Type 'help' in the REPL for full command list.");
}

/// Runs the embedded stdlib-only Python helper; it owns argument parsing and exit codes.
fn cmd_codegen(args: &[String]) -> Result<()> {
    let python = std::env::var("GDBG_PYTHON").unwrap_or_else(|_| "python3".into());
    let status = std::process::Command::new(&python)
        .args(["-I", "-c", include_str!("codegen.py")])
        .args(args)
        .env("GDBG_CODEGEN_CAPTURE_C", include_str!("codegen_capture.c"))
        .status()
        .with_context(|| {
            format!("cannot run {python}; set GDBG_PYTHON to a Python 3 interpreter")
        })?;
    std::process::exit(status.code().unwrap_or(1));
}

fn cmd_diff(args: &[String]) -> Result<()> {
    if args.len() < 2 {
        bail!("usage: gdbg diff <session_a> <session_b>");
    }
    let db = GpuDb::load(&args[0])?;
    commands::cmd_diff(&db, &[&args[1]]);
    Ok(())
}

fn cmd_from(args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!("usage: gdbg --from <session_name>");
    }
    eprintln!("loading session '{}'...", args[0]);
    let mut db = GpuDb::load(&args[0])?;
    if db.timeline_layer_id().is_none() && db.has_layer("ncu") {
        let count = db
            .ncu_instance_counts()
            .as_ref()
            .map(|counts| counts.values().sum::<usize>());
        eprintln!(
            "restored: {} counter-only kernel names; NCU instances: {}",
            db.kernels_with_metrics(),
            commands::ncu_instance_count_label(&db, count)
        );
    } else {
        eprintln!(
            "restored: {} kernels, {} launches",
            db.unique_kernel_count(),
            db.total_launch_count()
        );
    }
    repl::run(&mut db)
}

fn cmd_profile(args: &[String], options: &collect::CollectionOptions) -> Result<()> {
    let target = &args[0];
    let extra_args = &args[1..];

    if !std::path::Path::new(target).exists() {
        // Try loading as saved session
        if let Ok(mut db) = GpuDb::load(target) {
            eprintln!("loaded saved session '{target}'");
            return repl::run(&mut db);
        }
        bail!("target not found: {target}");
    }

    // Counter-only collection does not require Nsight Systems.
    if options.counters_only {
        which::which("ncu").context("ncu not found — install Nsight Compute")?;
    } else if let Some(msg) = check::check_minimum() {
        bail!("{msg}");
    }

    // Create session DB in temp dir
    let db_path = collect::session_dir().join("session.gpu.db");
    let mut db = GpuDb::create(&db_path)?;

    db.set_meta("target", target)?;
    let now = {
        use std::time::{SystemTime, UNIX_EPOCH};
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => {
                let secs = d.as_secs();
                // Format as basic ISO-8601 timestamp (UTC)
                let s = secs % 60;
                let m = (secs / 60) % 60;
                let h = (secs / 3600) % 24;
                let days = secs / 86400;
                // Days since epoch to Y-M-D (simplified: no leap second, Gregorian)
                let (y, mo, day) = epoch_days_to_ymd(days);
                format!("{y:04}-{mo:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
            }
            Err(_) => String::new(),
        }
    };
    db.set_meta("created", &now)?;

    eprintln!("gdbg — GPU profiler");
    eprintln!("target: {target}");
    eprintln!("session: {}", db_path.display());
    eprintln!("collecting profile data (this may take several minutes)...\n");

    collect::collect_all(&db, target, extra_args, options)?;

    // Consistency checks
    if let Some(warning) = db.check_target_consistency() {
        eprintln!("\nWARNING: {warning}");
    }
    for warning in db.check_kernel_consistency() {
        eprintln!("WARNING: {warning}");
    }

    eprintln!("\ncollection complete — entering REPL\n");
    repl::run(&mut db)
}

/// Convert days since Unix epoch to (year, month, day).
fn epoch_days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn selected_counter_flags_validate_scope_and_forward_target_args() {
        let selected = Cli::try_parse_from([
            "gdbg",
            "--counters-only",
            "--kernel-name",
            "Marlin<(int)4>",
            "--section",
            "WarpStateStats",
            "--section",
            "SpeedOfLight",
            "--launch-skip",
            "1",
            "--launch-count",
            "1",
            "worker.py",
            "--section",
            "target-arg",
        ])
        .unwrap();
        assert_eq!(selected.collection.kernel_name, ["Marlin<(int)4>"]);
        assert_eq!(
            selected.collection.section,
            ["WarpStateStats", "SpeedOfLight"]
        );
        assert_eq!(selected.collection.launch_skip, 1);
        assert_eq!(selected.args, ["worker.py", "--section", "target-arg"]);
        let native = Cli::try_parse_from([
            "gdbg",
            "--gpu-lock",
            "/shared/gpu.lock",
            "--timeline-only",
            "./app",
        ])
        .unwrap();
        assert_eq!(
            native.collection.gpu_lock,
            Some(std::path::PathBuf::from("/shared/gpu.lock"))
        );
        for flags in [
            vec!["--counters-only"],
            vec!["--launch-count", "0"],
            vec!["--section", "SpeedOfLight", "--ncu-set", "full"],
            vec!["--timeline-only", "--kernel-name", "kernel"],
            vec![
                "--timeline-only",
                "--counters-only",
                "--kernel-name",
                "kernel",
            ],
            vec!["--all-kernels", "--kernel-name", "kernel"],
            vec!["--all-launches", "--launch-count", "1"],
        ] {
            let mut args = vec!["gdbg"];
            args.extend(flags);
            args.push("./app");
            assert!(Cli::try_parse_from(args).is_err());
        }
        assert!(Cli::try_parse_from(["gdbg", "--timeline-only", "./app"]).is_ok());
    }

    #[test]
    fn counter_flags_apply_only_before_the_target() {
        let default = Cli::try_parse_from(["gdbg", "./app"]).unwrap();
        assert!(!default.collection.all_kernels && !default.collection.all_launches);
        let all = Cli::try_parse_from([
            "gdbg",
            "--all-kernels",
            "--all-launches",
            "./app",
            "--all-kernels",
        ])
        .unwrap();
        assert!(all.collection.all_kernels && all.collection.all_launches);
        assert_eq!(all.args, ["./app", "--all-kernels"]);
    }
}
