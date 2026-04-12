mod check;
mod collect;
mod commands;
mod db;
mod parsers;
mod repl;
#[cfg(test)]
mod tests;

use anyhow::{Result, bail};
use clap::Parser;

use db::GpuDb;

#[derive(Parser)]
#[command(
    name = "gdbg",
    version,
    about = "GPU profiler — collect, correlate, and query CUDA/PyTorch/Triton performance data"
)]
struct Cli {
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
        "list" | "ls" => { commands::cmd_list(); Ok(()) }
        "diff" => cmd_diff(&cli.args[1..]),
        "check" => { print!("{}", check::format_report()); Ok(()) }
        "--from" => cmd_from(&cli.args[1..]),
        "help" | "--help" | "-h" => { print_usage(); Ok(()) }
        _ => cmd_profile(&cli.args),
    }
}

fn print_usage() {
    println!("gdbg — GPU profiler for CUDA, PyTorch, and Triton\n");
    println!("  gdbg <target> [args...]         Profile a binary or script");
    println!("  gdbg --from <name>              Reload a saved session");
    println!("  gdbg list                       List saved sessions");
    println!("  gdbg diff <a> <b>               Diff two saved sessions");
    println!("  gdbg check                      Check tool dependencies\n");
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
    eprintln!("restored: {} kernels, {} launches", db.unique_kernel_count(), db.total_launch_count());
    repl::run(&mut db)
}

fn cmd_profile(args: &[String]) -> Result<()> {
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

    // Pre-flight check
    if let Some(msg) = check::check_minimum() {
        bail!("{msg}");
    }

    // Create session DB in temp dir
    let db_path = std::env::temp_dir()
        .join(format!("gdbg-{}", std::process::id()))
        .join("session.gpu.db");
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

    collect::collect_all(&db, target, extra_args)?;

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
