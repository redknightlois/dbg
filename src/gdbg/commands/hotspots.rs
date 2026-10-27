use super::{GpuDb, fmt_us, gpu_busy_us, like_param, parse_count, parse_pattern, require_op_layer};
use rusqlite::params_from_iter;

// ---------------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------------

pub fn cmd_stats(db: &GpuDb) {
    let target = db.meta("target");
    let device = db.meta("device");
    let wall_us: f64 = db.meta("wall_time_us").parse().unwrap_or(0.0);
    let gpu_us = db.total_gpu_time_us();
    let counter_only = db.timeline_layer_id().is_none() && db.has_layer("ncu");
    let launch_count = db.total_launch_count();
    let measured_kernels = db.kernels_with_metrics();
    let transfer_filter = db.timeline_filter_for("transfers");
    let xfer_us: f64 = db.scalar_f64(&format!(
        "SELECT COALESCE(SUM(duration_us),0) FROM transfers WHERE {transfer_filter}"
    ));

    println!("GPU Profile Summary");
    println!("  Target:       {target}");
    if !device.is_empty() {
        println!("  Device:       {device}");
    }
    if wall_us > 0.0 {
        println!("  Wall time:    {}", fmt_us(wall_us));
    } else {
        println!("  Wall time:    N/A (timeline unavailable)");
    }
    let fmt_pct = |v: f64| {
        if wall_us > 0.0 {
            format!("{:.1}% of wall", v / wall_us * 100.0)
        } else {
            "wall=N/A".to_string()
        }
    };
    if counter_only {
        println!("  Timeline time: N/A (timeline unavailable)");
        if launch_count == 0 {
            println!("  NCU replay time: N/A (duration unavailable)");
        } else {
            println!("  NCU replay time: {gpu_us:.3}us (instrumented, selected instances)");
        }
    } else {
        println!("  Kernel time:  {} ({})", fmt_us(gpu_us), fmt_pct(gpu_us));
    }
    if xfer_us > 0.0 {
        println!(
            "  Transfer time: {} ({})",
            fmt_us(xfer_us),
            fmt_pct(xfer_us)
        );
    }

    // Efficiency = GPU-not-idle wall time / program wall time.
    // "Useful" = the union of kernel and transfer intervals: time the GPU was doing
    // something (running a kernel OR moving data). Multi-stream concurrency and
    // kernel/transfer overlap are handled by interval-merging, so no double-counting.
    if wall_us > 0.0 && db.has_layer("nsys") {
        let useful = gpu_busy_us(db);
        println!(
            "  Efficiency:   {:.1}% ({} useful GPU / {} wall)",
            useful / wall_us * 100.0,
            fmt_us(useful),
            fmt_us(wall_us)
        );
    }
    if counter_only {
        if launch_count == 0 {
            println!(
                "  Kernels:      {measured_kernels} counter-only kernels; selected instances N/A"
            );
        } else {
            println!(
                "  Kernels:      {launch_count} selected NCU instances, {} unique",
                db.unique_kernel_count()
            );
        }
        println!("  Transfers:    N/A (timeline unavailable)");
        let streams = db.stream_count();
        if streams > 0 {
            println!("  Streams:      {streams} selected NCU streams");
        } else {
            println!("  Streams:      N/A (NCU stream metadata unavailable)");
        }
    } else {
        println!(
            "  Kernels:      {launch_count} launches, {} unique",
            db.unique_kernel_count()
        );
        println!("  Transfers:    {}", db.transfer_count());
        println!("  Streams:      {}", db.stream_count());
    }

    let layers = db.layer_names();
    let has_nsys = db.has_layer("nsys");
    let has_ncu = db.has_layer("ncu");
    let has_torch = db.has_layer("torch");

    if layers.is_empty() {
        println!("Layers: | (none)");
    } else {
        println!("  Layers:       {}", layers.join(" + "));
    }
    let mut missing = Vec::new();
    if !has_nsys {
        missing.push("nsys");
    }
    if !has_ncu {
        missing.push("ncu");
    }
    if !has_torch && target.ends_with(".py") {
        missing.push("torch");
    }
    if !missing.is_empty() {
        println!("  Missing:      {} (run 'suggest')", missing.join(", "));
    }

    let uk = db.unique_kernel_count();
    let wm = db.kernels_with_metrics();
    if counter_only {
        println!("  Deep metrics: {wm} counter-only kernels");
    } else {
        println!("  Deep metrics: {wm}/{uk} kernels");
    }

    let wo = db.kernels_with_ops();
    if wo > 0 {
        println!("  Op mapping:   {wo}/{uk} kernels");
    }

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

// ---------------------------------------------------------------------------
// kernels
// ---------------------------------------------------------------------------

pub fn cmd_kernels(db: &GpuDb, args: &[&str]) {
    let n = parse_count(args);
    let pattern = parse_pattern(args);
    let filter = db.kernel_filter_params();
    let tl = db.timeline_filter();

    let pattern_clause = pattern
        .map(|_| "AND launches.kernel_name LIKE ? ESCAPE '\\'")
        .unwrap_or_default();
    let metrics_filter = db.metric_filter_for("m");

    let sql = format!(
        "SELECT launches.kernel_name,
                COUNT(*) as cnt,
                SUM(launches.duration_us) as total,
                AVG(launches.duration_us) as avg,
                AVG(launches.duration_us * launches.duration_us)
                    - AVG(launches.duration_us) * AVG(launches.duration_us) as var,
                m.boundedness,
                m.compute_throughput_pct,
                m.memory_throughput_pct, m.kernel_name
         FROM launches
         LEFT JOIN metrics m ON m.kernel_name = launches.kernel_name AND {metrics_filter}
         WHERE {} AND {tl} {pattern_clause}
         GROUP BY launches.kernel_name
         ORDER BY total DESC
         LIMIT ?",
        filter.clause()
    );

    let gpu_total = db.total_gpu_time_us();
    let mut query_params = filter.params();
    let pattern_param;
    if let Some(p) = pattern {
        pattern_param = like_param(p);
        query_params.push(&pattern_param);
    }
    let limit = n as i64;
    query_params.push(&limit);

    let rows: Vec<_> = db.query_vec(&sql, params_from_iter(query_params), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, f64>(3)?,
            row.get::<_, f64>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<f64>>(6)?,
            row.get::<_, Option<f64>>(7)?,
            row.get::<_, Option<String>>(8)?,
        ))
    });

    if db.timeline_layer_id().is_none() && db.has_layer("ncu") {
        println!("Timeline duration: N/A (timeline unavailable)");
        if rows.is_empty() {
            let sql = format!(
                r"SELECT kernel_name FROM metrics m WHERE {} AND kernel_name LIKE ?1 ESCAPE '\' ORDER BY kernel_name LIMIT ?2",
                db.metric_filter_for("m")
            );
            let names: Vec<String> = db.query_vec(
                &sql,
                rusqlite::params![like_param(pattern.unwrap_or("")), n as i64],
                |row| row.get(0),
            );
            println!(
                "{} counter-only kernels; selected instances and NCU replay duration N/A",
                names.len()
            );
            for name in names {
                println!("  {name}");
            }
            return;
        }
        println!(
            "# | Kernel | Instrumented NCU replay time | % selected replay | Selected instances | Replay avg | Replay stddev | Replay tail% | Bound"
        );
    } else {
        println!("# | Kernel | Time | % | Launches | Avg | Stddev | Tail% | Bound");
    }
    for (i, (name, cnt, total, avg, var, bound, cmp, mem, metric_name)) in rows.iter().enumerate() {
        let pct = if gpu_total > 0.0 {
            total / gpu_total * 100.0
        } else {
            0.0
        };
        let stddev = var.max(0.0).sqrt();
        let tail_pct = tail_over_2x_median(db, name, &tl);
        let bound_str = match bound.as_deref().filter(|_| cmp.is_some() && mem.is_some()) {
            Some("compute") => cmp
                .map(|v| format!("cmp {v:.0}%"))
                .unwrap_or_else(|| "cmp N/A".into()),
            Some("memory") => mem
                .map(|v| format!("mem {v:.0}%"))
                .unwrap_or_else(|| "mem N/A".into()),
            Some("latency") => "latency".into(),
            _ if metric_name.is_some() => "[unclassified]".into(),
            _ => "[no ncu]".into(),
        };
        let tail_str = match tail_pct {
            Some(p) => format!("{p:.1}%"),
            None => "—".into(),
        };
        println!(
            "{} | {} | {} | {:.1}% | {} | {} | {} | {} | {}",
            i + 1,
            name,
            fmt_us(*total),
            pct,
            cnt,
            fmt_us(*avg),
            fmt_us(stddev),
            tail_str,
            bound_str
        );
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
    if durs.len() < 4 {
        return None;
    }
    let mut sorted = durs.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted[sorted.len() / 2];
    if median <= 0.0 {
        return None;
    }
    let thresh = median * 2.0;
    let tail = durs.iter().filter(|&&d| d > thresh).count();
    Some(tail as f64 / durs.len() as f64 * 100.0)
}

// ---------------------------------------------------------------------------
// ops
// ---------------------------------------------------------------------------

pub fn cmd_ops(db: &GpuDb, args: &[&str]) {
    if !require_op_layer(db) {
        return;
    }
    let n = parse_count(args);
    let pattern = parse_pattern(args);
    let pattern_clause = pattern
        .map(|_| "AND name LIKE ?2 ESCAPE '\\'")
        .unwrap_or_default();

    let sql = format!(
        "SELECT name, module_path, cpu_time_us
         FROM ops WHERE 1=1 {pattern_clause}
         ORDER BY cpu_time_us DESC LIMIT ?1"
    );

    let rows: Vec<(String, Option<String>, f64)> = match pattern {
        Some(p) => db.query_vec(
            &sql,
            rusqlite::params![n as i64, super::like_param(p)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ),
        None => db.query_vec(&sql, [n as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        }),
    };

    println!("# | Op | CPU Time | Module");
    for (i, (name, module, cpu_time)) in rows.iter().enumerate() {
        println!(
            "{} | {} | {} | {}",
            i + 1,
            name,
            fmt_us(*cpu_time),
            module.as_deref().unwrap_or("")
        );
    }
}
