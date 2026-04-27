use std::path::PathBuf;

use super::{GpuDb, escape_regex, fmt_us, trunc};

// ---------------------------------------------------------------------------
// layers
// ---------------------------------------------------------------------------

pub fn cmd_layers(db: &GpuDb) {
    let sql = "SELECT id, source, file, collected_at, collection_secs FROM layers ORDER BY id";
    let rows: Vec<(i64, String, String, String, Option<f64>)> = db.query_vec(
        sql, [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    );

    if rows.is_empty() { println!("no layers loaded"); return; }
    println!("  #  Source  File                                     Collected          Secs");
    println!("  ── ─────── ──────────────────────────────────────── ────────────────── ─────");
    for (id, source, file, at, secs) in &rows {
        println!("  {:<2} {:<7} {:<43} {:<18} {}",
            id, source, trunc(file, 43), &at[..at.len().min(18)],
            secs.map(|s| format!("{s:.1}")).unwrap_or_else(|| "?".into()));
    }

    let uk = db.unique_kernel_count();
    let wm = db.kernels_with_metrics();
    let wo = db.kernels_with_ops();
    println!("\n  Correlation: {uk} unique kernels");
    println!("    With metrics:  {wm}/{uk}");
    println!("    With op map:   {wo}/{uk}");
}

// ---------------------------------------------------------------------------
// suggest
// ---------------------------------------------------------------------------

pub fn cmd_suggest(db: &GpuDb) {
    let uk = db.unique_kernel_count();
    let failures = db.failures();
    let has_nsys = db.has_layer("nsys");
    let has_ncu = db.has_layer("ncu");
    let has_torch = db.has_layer("torch");
    let target = db.meta("target");

    if uk == 0 && failures.is_empty() {
        println!("no profile data");
        return;
    }

    let mut n = 1;

    if !failures.is_empty() {
        println!("  Collection failures:\n");
        for (phase, error) in &failures {
            println!("  {n}. {phase} failed: {error}");
            n += 1;
        }
        println!();
    }

    if uk == 0 { println!("  No kernel data collected."); return; }
    println!("  Suggestions:\n");

    if !has_nsys {
        println!("  {n}. No timeline data. Run gdbg with your target.");
        println!("     This gives: kernel timeline, memory transfers, GPU idle gaps\n");
        n += 1;
    }

    if !has_ncu {
        let tl = db.timeline_filter();
        let top_sql = format!("SELECT kernel_name, SUM(duration_us) as total
                       FROM launches WHERE {tl} GROUP BY kernel_name ORDER BY total DESC LIMIT 5");
        let top: Vec<(String, f64)> = db.query_vec(
            &top_sql, [], |row| Ok((row.get(0)?, row.get(1)?)),
        );

        if !top.is_empty() {
            let gpu_total = db.total_gpu_time_us();
            let pct: f64 = top.iter().map(|t| if gpu_total > 0.0 { t.1 / gpu_total * 100.0 } else { 0.0 }).sum();
            let regex = top.iter().map(|t| escape_regex(&t.0)).collect::<Vec<_>>().join("|");
            println!("  {n}. Top {} kernels ({pct:.0}% of GPU) lack hardware metrics.", top.len());
            println!("     Collect: ncu --set full --kernel-name \"regex:{regex}\" {target}\n");
            n += 1;
        }
    }

    if !has_torch && target.ends_with(".py") {
        println!("  {n}. No op->kernel mapping. Can't trace kernels back to Python.");
        println!("     Needed for: ops, callers, trace commands\n");
        n += 1;
    }

    let tl2 = db.timeline_filter();
    let var_sql = format!("SELECT kernel_name, COUNT(*) as cnt, AVG(duration_us) as avg,
                   AVG(duration_us * duration_us) - AVG(duration_us) * AVG(duration_us) as var
                   FROM launches WHERE {tl2} GROUP BY kernel_name
                   HAVING cnt > 5 AND var > 0
                   ORDER BY SUM(duration_us) DESC LIMIT 5");
    let vars: Vec<(String, f64, f64)> = db.query_vec(
        &var_sql, [],
        |row| Ok((row.get(0)?, row.get(2)?, row.get(3)?)),
    );

    for (name, avg, var) in &vars {
        let stddev = var.max(0.0).sqrt();
        let cv = if *avg > 0.0 { stddev / avg } else { 0.0 };
        if cv > 0.3 {
            println!("  {n}. '{}' has high variance (CV={cv:.2}).", name);
            println!("     May indicate: data-dependent paths, cache effects, or varying input sizes.\n");
            n += 1;
        }
    }

    if has_nsys {
        let gpu_us = db.total_gpu_time_us();
        let xfer_us: f64 = db.scalar_f64("SELECT COALESCE(SUM(duration_us),0) FROM transfers");

        if gpu_us > 0.0 && xfer_us > 0.0 {
            let ratio = xfer_us / gpu_us;
            if ratio > 5.0 {
                println!("  {n}. Transfer:compute ratio is {ratio:.1}:1 — PCIe dominates.");
                println!("     Try: cudaMallocHost (pinned memory), overlap via CUDA streams, or increase batch size.\n");
                n += 1;
            }
        }

        let tl = db.timeline_filter();
        let tiny_sql = format!(
            "SELECT COUNT(*) FROM (
                SELECT kernel_name FROM launches WHERE {tl}
                GROUP BY kernel_name HAVING AVG(duration_us) < 10.0
             )"
        );
        let tiny_count: i64 = db.scalar_f64(&tiny_sql) as i64;
        if tiny_count > 10 {
            println!("  {n}. {tiny_count} distinct kernels average under 10us — launch overhead likely dominates.");
            println!("     Try: torch.compile(), CUDA graphs, or manual kernel fusion.  See 'small' and 'fuse'.\n");
            n += 1;
        }

        let dom_sql = format!(
            "SELECT kernel_name, SUM(duration_us) as t FROM launches WHERE {tl}
             GROUP BY kernel_name ORDER BY t DESC LIMIT 1"
        );
        if let Ok((dom_name, dom_time)) = db.conn.query_row(
            &dom_sql, [], |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        ) {
            if gpu_us > 0.0 && dom_time / gpu_us > 0.5 {
                let pct = dom_time / gpu_us * 100.0;
                println!("  {n}. One kernel accounts for {pct:.0}% of GPU time: {}", trunc(&dom_name, 60));
                println!("     Try: bound '{}' — optimize the hotspot directly.\n", trunc(&dom_name, 40));
                n += 1;
            }
        }
    }

    if has_nsys && has_ncu && (has_torch || !target.ends_with(".py")) {
        println!("  All layers loaded — full analysis available.");
    }

    let _ = n;
}

// ---------------------------------------------------------------------------
// save / list / diff
// ---------------------------------------------------------------------------

pub fn cmd_save(db: &GpuDb, args: &[&str]) {
    let name = match args.first() {
        Some(n) => *n,
        None => { println!("usage: save <name>"); return; }
    };
    match db.save(name) {
        Ok(path) => println!("saved to {}", path.display()),
        Err(e) => println!("save failed: {e}"),
    }
}

pub fn cmd_list() {
    match GpuDb::list_saved() {
        Ok(sessions) => {
            if sessions.is_empty() {
                println!("no saved sessions");
                return;
            }
            println!("  Name                    Device          Kernels  Layers           Created");
            println!("  ─────────────────────── ─────────────── ──────── ──────────────── ────────────────");
            for s in &sessions {
                let dev = if s.device.is_empty() { "?" } else { &s.device };
                println!("  {:<23} {:<15} {:>7}  {:<16} {}",
                    trunc(&s.name, 23), trunc(dev, 15), s.kernel_count,
                    s.layers.join("+"), &s.created[..s.created.len().min(16)]);
            }
        }
        Err(e) => println!("list failed: {e}"),
    }
}

pub fn cmd_diff(db: &GpuDb, args: &[&str]) {
    let name = match args.first() {
        Some(n) => *n,
        None => { println!("usage: diff <saved_session>"); return; }
    };

    let other_path = if name.ends_with(".gpu.db") || name.contains('/') {
        PathBuf::from(name)
    } else {
        GpuDb::session_dir().join(format!("{name}.gpu.db"))
    };

    if !other_path.exists() {
        println!("cannot load '{name}': no such session at {}", other_path.display());
        return;
    }
    if let Err(e) = db.attach(other_path.to_str().unwrap_or(""), "other") {
        println!("cannot load '{name}': {e}");
        return;
    }

    let sql = "SELECT
        COALESCE(c.kernel_name, o.kernel_name) as name,
        COALESCE(o.total, 0) as before,
        COALESCE(c.total, 0) as after
       FROM
        (SELECT kernel_name, SUM(duration_us) as total FROM launches GROUP BY kernel_name) c
       FULL OUTER JOIN
        (SELECT kernel_name, SUM(duration_us) as total FROM other.launches GROUP BY kernel_name) o
       ON c.kernel_name = o.kernel_name
       ORDER BY ABS(COALESCE(c.total,0) - COALESCE(o.total,0)) DESC
       LIMIT 15";

    let rows: Vec<(String, f64, f64)> = db.query_vec(
        sql, [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    );

    println!("  Diff: current vs {name}\n");
    println!("  Kernel                            Before     After      Delta");
    println!("  ────────────────────────────────── ────────── ────────── ──────────");
    for (kname, before, after) in &rows {
        let delta = if *before > 0.0 {
            let pct = (after - before) / before * 100.0;
            format!("{}{pct:.1}%", if pct >= 0.0 { "+" } else { "" })
        } else { "new".into() };
        println!("  {:<34} {:>10} {:>10} {:>10}",
            trunc(kname, 34), fmt_us(*before), fmt_us(*after), delta);
    }

    let _ = db.detach("other");
}
