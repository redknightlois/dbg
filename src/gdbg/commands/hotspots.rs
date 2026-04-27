use super::{
    GpuDb, escape_sql_like, fmt_us, merge_intervals, parse_count, parse_pattern,
    require_op_layer, trunc,
};

// ---------------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------------

pub fn cmd_stats(db: &GpuDb) {
    let target = db.meta("target");
    let device = db.meta("device");
    let wall_us: f64 = db.meta("wall_time_us").parse().unwrap_or(0.0);
    let gpu_us = db.total_gpu_time_us();
    let xfer_us: f64 = db.scalar_f64("SELECT COALESCE(SUM(duration_us),0) FROM transfers");

    println!("GPU Profile Summary");
    println!("  Target:       {target}");
    if !device.is_empty() { println!("  Device:       {device}"); }
    // When wall_time_us is missing / zero, the underlying collection
    // failed (e.g. nsys import errored out). Printing "0.0us" and
    // "0.0% of wall" for every row made broken sessions look merely
    // quiet. Surface the absence explicitly so agents don't draw
    // wrong conclusions from phantom percentages.
    if wall_us > 0.0 {
        println!("  Wall time:    {}", fmt_us(wall_us));
    } else {
        println!("  Wall time:    N/A (nsys did not record — re-run `gdbg collect`)");
    }
    let fmt_pct = |v: f64| {
        if wall_us > 0.0 {
            format!("{:.1}% of wall", v / wall_us * 100.0)
        } else {
            "wall=N/A".to_string()
        }
    };
    println!("  Kernel time:  {} ({})", fmt_us(gpu_us), fmt_pct(gpu_us));
    if xfer_us > 0.0 {
        println!("  Transfer time: {} ({})", fmt_us(xfer_us), fmt_pct(xfer_us));
    }

    // Efficiency = GPU-not-idle wall time / program wall time.
    // "Useful" = the union of kernel and transfer intervals: time the GPU was doing
    // something (running a kernel OR moving data). Multi-stream concurrency and
    // kernel/transfer overlap are handled by interval-merging, so no double-counting.
    if wall_us > 0.0 && db.has_layer("nsys") {
        let useful = gpu_busy_us(db);
        println!("  Efficiency:   {:.1}% ({} useful GPU / {} wall)",
            useful / wall_us * 100.0, fmt_us(useful), fmt_us(wall_us));
    }
    println!("  Kernels:      {} launches, {} unique",
        db.total_launch_count(), db.unique_kernel_count());
    println!("  Transfers:    {}", db.transfer_count());
    println!("  Streams:      {}", db.stream_count());

    let layers = db.layer_names();
    let has_nsys = db.has_layer("nsys");
    let has_ncu = db.has_layer("ncu");
    let has_torch = db.has_layer("torch");

    if layers.is_empty() {
        println!("  Layers:       (none)");
    } else {
        println!("  Layers:       {}", layers.join(" + "));
    }
    let mut missing = Vec::new();
    if !has_nsys { missing.push("nsys"); }
    if !has_ncu { missing.push("ncu"); }
    if !has_torch && target.ends_with(".py") { missing.push("torch"); }
    if !missing.is_empty() {
        println!("  Missing:      {} (run 'suggest')", missing.join(", "));
    }

    let uk = db.unique_kernel_count();
    let wm = db.kernels_with_metrics();
    println!("  Deep metrics: {wm}/{uk} kernels");

    let wo = db.kernels_with_ops();
    if wo > 0 { println!("  Op mapping:   {wo}/{uk} kernels"); }

    let failures = db.failures();
    if !failures.is_empty() {
        println!("  Failures:     {} (run 'suggest')", failures.len());
    }

    // nsys GPU tracing warning
    let nsys_warn = db.meta("nsys_warning");
    if !nsys_warn.is_empty() {
        println!("  WARNING:      {nsys_warn}");
    }

    // Consistency warnings
    if let Some(w) = db.check_target_consistency() {
        println!("  WARNING:      {w}");
    }
    for w in db.check_kernel_consistency() {
        println!("  WARNING:      {w}");
    }
}

/// Total time (us) during which the GPU was doing work — either a kernel or a
/// transfer. Kernel and transfer intervals are unioned and merged, so concurrent
/// activity is only counted once.
fn gpu_busy_us(db: &GpuDb) -> f64 {
    let mut intervals = db.kernel_intervals();
    intervals.extend(db.transfer_intervals(None));
    intervals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    merge_intervals(&intervals).iter().map(|(s, e)| e - s).sum()
}

// ---------------------------------------------------------------------------
// kernels
// ---------------------------------------------------------------------------

pub fn cmd_kernels(db: &GpuDb, args: &[&str]) {
    let n = parse_count(args);
    let pattern = parse_pattern(args);
    let filter = db.kernel_filter();
    let tl = db.timeline_filter();

    let pattern_clause = pattern
        .map(|p| format!(r"AND launches.kernel_name LIKE '%{}%' ESCAPE '\'", escape_sql_like(p)))
        .unwrap_or_default();

    let sql = format!(
        "SELECT launches.kernel_name,
                COUNT(*) as cnt,
                SUM(launches.duration_us) as total,
                AVG(launches.duration_us) as avg,
                AVG(launches.duration_us * launches.duration_us)
                    - AVG(launches.duration_us) * AVG(launches.duration_us) as var,
                m.boundedness,
                m.compute_throughput_pct,
                m.memory_throughput_pct
         FROM launches
         LEFT JOIN metrics m ON m.kernel_name = launches.kernel_name
         WHERE {filter} AND {tl} {pattern_clause}
         GROUP BY launches.kernel_name
         ORDER BY total DESC
         LIMIT ?1"
    );

    let gpu_total = db.total_gpu_time_us();
    let rows: Vec<_> = db.query_vec(&sql, [n as i64], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?,
            row.get::<_, f64>(2)?, row.get::<_, f64>(3)?, row.get::<_, f64>(4)?,
            row.get::<_, Option<String>>(5)?, row.get::<_, Option<f64>>(6)?,
            row.get::<_, Option<f64>>(7)?))
    });

    println!("  #  Kernel                          Time      %     Launches   Avg       Stddev    Tail%  Bound");
    println!("  ── ──────────────────────────────── ──────── ────── ────────── ───────── ───────── ────── ────────────");
    for (i, (name, cnt, total, avg, var, bound, cmp, mem)) in rows.iter().enumerate() {
        let pct = if gpu_total > 0.0 { total / gpu_total * 100.0 } else { 0.0 };
        let stddev = var.max(0.0).sqrt();
        let tail_pct = tail_over_2x_median(db, name, &tl);
        let bound_str = match bound.as_deref() {
            Some("compute") => format!("cmp {:.0}%", cmp.unwrap_or(0.0)),
            Some("memory") => format!("mem {:.0}%", mem.unwrap_or(0.0)),
            Some("latency") => "latency".into(),
            _ => "[no ncu]".into(),
        };
        let tail_str = match tail_pct {
            Some(p) => format!("{p:.1}%"),
            None => "—".into(),
        };
        println!("  {:<2} {:<32} {:>8} {:>5.1}% {:>9} {:>9} {:>9} {:>6} {:<12}",
            i + 1, trunc(name, 32), fmt_us(*total), pct, cnt,
            fmt_us(*avg), fmt_us(stddev), tail_str, bound_str);
    }
}

/// Percentage of launches whose duration exceeds 2x the median — quick variance indicator.
/// Returns None when there are fewer than 4 launches.
fn tail_over_2x_median(db: &GpuDb, kernel_name: &str, tl: &str) -> Option<f64> {
    let sql = format!(
        "SELECT duration_us FROM launches
         WHERE kernel_name = ?1 AND {tl}"
    );
    let durs: Vec<f64> = db.query_vec(&sql, [kernel_name], |row| row.get(0));
    if durs.len() < 4 { return None; }
    let mut sorted = durs.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted[sorted.len() / 2];
    if median <= 0.0 { return None; }
    let thresh = median * 2.0;
    let tail = durs.iter().filter(|&&d| d > thresh).count();
    Some(tail as f64 / durs.len() as f64 * 100.0)
}

// ---------------------------------------------------------------------------
// ops
// ---------------------------------------------------------------------------

pub fn cmd_ops(db: &GpuDb, args: &[&str]) {
    if !require_op_layer(db) { return; }
    let n = parse_count(args);
    let pattern = parse_pattern(args);
    let pattern_clause = pattern
        .map(|p| format!(r"AND name LIKE '%{}%' ESCAPE '\'", escape_sql_like(p)))
        .unwrap_or_default();

    let sql = format!(
        "SELECT name, module_path, cpu_time_us
         FROM ops WHERE 1=1 {pattern_clause}
         ORDER BY cpu_time_us DESC LIMIT ?1"
    );

    let rows: Vec<(String, Option<String>, f64)> = db.query_vec(
        &sql, [n as i64],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    );

    println!("  #  Op                               CPU Time    Module");
    println!("  ── ───────────────────────────────── ────────── ────────────");
    for (i, (name, module, cpu_time)) in rows.iter().enumerate() {
        println!("  {:<2} {:<34} {:>9}  {}",
            i + 1, trunc(name, 34), fmt_us(*cpu_time),
            module.as_deref().unwrap_or(""));
    }
}
