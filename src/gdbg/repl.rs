use std::io::{self, BufRead, Write};

use anyhow::Result;

use super::commands;
use super::db::GpuDb;

pub fn run(db: &mut GpuDb) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    commands::cmd_stats(db);
    println!();

    loop {
        print!("gpu> ");
        stdout.flush()?;

        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }

        let line = line.trim();
        if line.is_empty() { continue; }

        let parts: Vec<&str> = line.split_whitespace().collect();
        let cmd = parts[0];
        let args = &parts[1..];

        match cmd {
            "quit" | "exit" | "q" => break,
            "help" | "h" | "?" => cmd_help(args),
            "stats" => commands::cmd_stats(db),
            "kernels" | "k" => commands::cmd_kernels(db, args),
            "ops" => commands::cmd_ops(db, args),
            "inspect" | "i" => commands::cmd_inspect(db, args),
            "bound" => commands::cmd_bound(db, args),
            "roofline" | "roof" => commands::cmd_roofline(db, args),
            "occupancy" | "occ" => commands::cmd_occupancy(db, args),
            "transfers" | "xfer" => commands::cmd_transfers(db, args),
            "gaps" => commands::cmd_gaps(db, args),
            "overlap" => commands::cmd_overlap(db),
            "streams" => commands::cmd_streams(db),
            "timeline" | "tl" => commands::cmd_timeline(db, args),
            "trace" => commands::cmd_trace(db, args),
            "callers" => commands::cmd_callers(db, args),
            "layers" => commands::cmd_layers(db),
            "suggest" => commands::cmd_suggest(db),
            "save" => commands::cmd_save(db, args),
            "list" | "ls" => commands::cmd_list(),
            "diff" => commands::cmd_diff(db, args),
            "focus" => commands::cmd_focus(db, args),
            "ignore" => commands::cmd_ignore(db, args),
            "region" => commands::cmd_region(db, args),
            "reset" => commands::cmd_reset(db),
            "variance" | "var" => commands::cmd_variance(db, args),
            "warmup" => commands::cmd_warmup(db),
            "small" => commands::cmd_small(db, args),
            "fuse" => commands::cmd_fuse(db, args),
            "concurrency" | "conc" => commands::cmd_concurrency(db),
            "hotpath" | "hot" => commands::cmd_hotpath(db),
            "compare-ops" | "cmp" => commands::cmd_compare_ops(db, args),
            "top-ops" | "top" => commands::cmd_top_ops(db, args),
            "breakdown" | "br" => commands::cmd_breakdown(db, args),
            "idle-between" | "idle" => commands::cmd_idle_between(db, args),
            _ => {
                println!("unknown command: {cmd}");
                println!("type 'help' for available commands");
            }
        }
    }

    Ok(())
}

fn cmd_help(args: &[&str]) {
    if args.is_empty() {
        println!("GPU Profile REPL — Commands:\n");
        println!("  Hotspots");
        println!("    kernels [N] [pattern]   Top kernels by total GPU time");
        println!("    ops [N] [pattern]       Top operators (needs torch/proton layer)");
        println!("    stats                   Overall summary\n");
        println!("  Analysis");
        println!("    roofline [pattern]      Classify compute-bound vs memory-bound");
        println!("    bound <kernel>          Detailed boundedness diagnosis");
        println!("    occupancy [N]           SM occupancy ranking");
        println!("    variance <kernel>       Launch-to-launch timing variance");
        println!("    warmup                  Detect warmup launches before steady state");
        println!("    small [N]               Kernels where launch overhead > compute");
        println!("    fuse [N]                Sequential kernels that could be fused");
        println!("    concurrency             Stream utilization and parallelism gaps");
        println!("    hotpath                 Critical path through ops (CPU vs GPU bound)");
        println!("    compare-ops [N]         CPU vs GPU time ratio per operator");
        println!("    top-ops [N] [pattern]   Ops ranked by GPU time (not CPU)");
        println!("    breakdown <op>          Which kernels an op expands into");
        println!("    idle-between <a> <b>    GPU idle gap between two ops\n");
        println!("  Timeline");
        println!("    transfers [N]           Memory copies ranked by cost");
        println!("    gaps [N]                GPU idle periods");
        println!("    overlap                 Compute/transfer concurrency");
        println!("    streams                 Per-stream utilization");
        println!("    timeline [N]            Chronological kernel launches\n");
        println!("  Drill-down");
        println!("    inspect <kernel>        Full detail from all layers");
        println!("    trace <op>              Op -> kernel mapping");
        println!("    callers <kernel>        Which op launched this kernel\n");
        println!("  Data management");
        println!("    layers                  Show loaded data layers");
        println!("    suggest                 Suggest what data to collect next");
        println!("    save <name>             Save session for later");
        println!("    list                    List saved sessions");
        println!("    diff <name>             Compare against saved session\n");
        println!("  Filtering");
        println!("    focus <pattern>         Show only matching kernels");
        println!("    ignore <pattern>        Hide matching kernels");
        println!("    region <name>           Focus on NVTX / profiler step");
        println!("    reset                   Clear all filters\n");
        println!("  quit                      Exit REPL");
    } else {
        println!("no detailed help for '{}'", args.join(" "));
    }
}
