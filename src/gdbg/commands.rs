use std::path::PathBuf;

use super::db::{GpuDb, escape_sql_like};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_count(args: &[&str]) -> usize {
    args.first()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(10)
}

fn parse_pattern<'a>(args: &'a [&'a str]) -> Option<&'a str> {
    if args.is_empty() { return None; }
    if args[0].parse::<usize>().is_ok() {
        args.get(1).copied()
    } else {
        Some(args[0])
    }
}

fn fmt_us(us: f64) -> String {
    if us >= 1_000_000.0 { format!("{:.2}s", us / 1_000_000.0) }
    else if us >= 1_000.0 { format!("{:.1}ms", us / 1_000.0) }
    else { format!("{:.1}us", us) }
}

fn fmt_bytes(b: i64) -> String {
    if b >= 1_073_741_824 { format!("{:.1} GB", b as f64 / 1_073_741_824.0) }
    else if b >= 1_048_576 { format!("{:.1} MB", b as f64 / 1_048_576.0) }
    else if b >= 1024 { format!("{:.1} KB", b as f64 / 1024.0) }
    else { format!("{b} B") }
}

pub(crate) fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end: String = s.chars().take(max - 3).collect();
        format!("{end}...")
    }
}

/// Build a SQL LIKE bind-parameter from a user pattern: `%escaped_pattern%`.
fn like_param(pattern: &str) -> String {
    format!("%{}%", escape_sql_like(pattern))
}

/// Escape regex metacharacters in a kernel name for use in ncu `--kernel-name "regex:..."`.
pub(crate) fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        if "\\^$.|?*+()[]{}".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Compute true GPU idle gaps by merging kernel and transfer intervals.
/// A gap is time when the GPU has no kernel running AND no DMA in flight.
/// Returns (gap_start, gap_duration) pairs sorted by start time.
pub(crate) fn compute_gpu_gaps(db: &GpuDb) -> Vec<(f64, f64)> {
    let mut intervals = db.kernel_intervals();
    intervals.extend(db.transfer_intervals(None));
    intervals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    let mut gaps = Vec::new();
    if let Some(&(_, mut cur_end)) = intervals.first() {
        for &(s, e) in &intervals[1..] {
            if s <= cur_end {
                if e > cur_end { cur_end = e; }
            } else {
                let gap = s - cur_end;
                if gap > 1.0 {
                    gaps.push((cur_end, gap));
                }
                cur_end = e;
            }
        }
    }
    gaps
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

/// Check that the DB has at least one of the required layers.
/// Prints a message and returns false if none are present.
fn require_op_layer(db: &GpuDb) -> bool {
    if db.has_layer("torch") || db.has_layer("proton") {
        true
    } else {
        println!("no op data — need torch.profiler or proton layer");
        false
    }
}

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

// ---------------------------------------------------------------------------
// inspect
// ---------------------------------------------------------------------------

pub fn cmd_inspect(db: &GpuDb, args: &[&str]) {
    let pattern = match args.first() {
        Some(p) => *p,
        None => { println!("usage: inspect <kernel_pattern>"); return; }
    };

    // Get kernel aggregate (restrict to timeline layer to avoid double-counting)
    let tl = db.timeline_filter();
    let sql = format!(r"SELECT kernel_name, COUNT(*), SUM(duration_us), AVG(duration_us),
                      MIN(duration_us), MAX(duration_us)
               FROM launches WHERE kernel_name LIKE ?1 ESCAPE '\' AND {tl}
               GROUP BY kernel_name");
    let rows: Vec<(String, i64, f64, f64, f64, f64)> = db.query_vec(
        &sql, [like_param(pattern)],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
    );
    if rows.is_empty() { println!("no kernel matching '{pattern}'"); return; }
    if rows.len() > 1 {
        println!("multiple matches for '{pattern}':");
        for (n, ..) in &rows { println!("  {n}"); }
        println!("narrow the pattern");
        return;
    }
    let (name, cnt, total, avg, min, max) = rows.into_iter().next().unwrap();

    println!("Kernel: {name}");
    println!("  Launches: {cnt}");
    println!("  Total:    {}", fmt_us(total));
    println!("  Average:  {}", fmt_us(avg));
    if cnt > 1 { println!("  Min:      {}", fmt_us(min)); println!("  Max:      {}", fmt_us(max)); }

    // Launch config — most common
    let config_sql = format!("SELECT grid_x, grid_y, grid_z, block_x, block_y, block_z,
                             COUNT(*) as cnt
                      FROM launches WHERE kernel_name = ?1
                      AND grid_x IS NOT NULL AND {tl}
                      GROUP BY grid_x, grid_y, grid_z, block_x, block_y, block_z
                      ORDER BY cnt DESC LIMIT 5");
    let configs: Vec<(u32, u32, u32, u32, u32, u32, i64)> = db.query_vec(
        &config_sql, [&name],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?,
                  row.get(4)?, row.get(5)?, row.get(6)?)),
    );

    if !configs.is_empty() {
        println!();
        for (gx,gy,gz,bx,by,bz,c) in &configs {
            let threads = *bx as u64 * *by as u64 * *bz as u64;
            println!("  grid=({gx},{gy},{gz}) block=({bx},{by},{bz}) threads/block={threads} x{c}");
        }
    }

    // Metrics
    let m_sql = "SELECT occupancy_pct, compute_throughput_pct, memory_throughput_pct,
                        registers_per_thread, shared_mem_static_bytes, shared_mem_dynamic_bytes,
                        l2_hit_rate_pct, achieved_bandwidth_gb_s, peak_bandwidth_gb_s,
                        boundedness
                 FROM metrics WHERE kernel_name = ?1";
    if let Ok(m) = db.conn.query_row(m_sql, [&name], |row| {
        Ok((
            row.get::<_,Option<f64>>(0)?,  row.get::<_,Option<f64>>(1)?,
            row.get::<_,Option<f64>>(2)?,  row.get::<_,Option<i64>>(3)?,
            row.get::<_,Option<i64>>(4)?,  row.get::<_,Option<i64>>(5)?,
            row.get::<_,Option<f64>>(6)?,  row.get::<_,Option<f64>>(7)?,
            row.get::<_,Option<f64>>(8)?,  row.get::<_,Option<String>>(9)?,
        ))
    }) {
        println!("\n  Hardware Metrics (ncu):");
        if let Some(b) = &m.9 { println!("    Boundedness:       {b}"); }
        if let Some(v) = m.0 { println!("    Occupancy:         {v:.1}%"); }
        if let Some(v) = m.1 { println!("    Compute throughput: {v:.1}%"); }
        if let Some(v) = m.2 { println!("    Memory throughput:  {v:.1}%"); }
        if let Some(v) = m.3 { println!("    Registers/thread:  {v}"); }
        let shmem = m.4.unwrap_or(0) + m.5.unwrap_or(0);
        if shmem > 0 { println!("    Shared memory:     {}", fmt_bytes(shmem)); }
        if let Some(v) = m.6 { println!("    L2 hit rate:       {v:.1}%"); }
        if let (Some(a), Some(p)) = (m.7, m.8) {
            println!("    Bandwidth:         {a:.1} / {p:.1} GB/s ({:.1}%)", a / p * 100.0);
        } else if let Some(a) = m.7 { println!("    Bandwidth:         {a:.1} GB/s"); }
    } else {
        println!("\n  No hardware metrics (need ncu)");
    }

    // Op mapping
    let op_sql = "SELECT o.name, o.module_path, o.input_shapes
                  FROM op_kernel_map okm JOIN ops o ON o.id = okm.op_id
                  WHERE okm.kernel_name = ?1";
    let ops: Vec<(String, Option<String>, Option<String>)> = db.query_vec(
        op_sql, [&name],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    );
    if !ops.is_empty() {
        println!("\n  Origin (torch/proton):");
        for (opname, modpath, shapes) in &ops {
            println!("    Op: {opname}");
            if let Some(m) = modpath { println!("    Module: {m}"); }
            if let Some(s) = shapes { println!("    Shapes: {s}"); }
        }
    }
}

// ---------------------------------------------------------------------------
// bound
// ---------------------------------------------------------------------------

pub fn cmd_bound(db: &GpuDb, args: &[&str]) {
    let pattern = match args.first() {
        Some(p) => *p,
        None => { println!("usage: bound <kernel_pattern>"); return; }
    };

    let sql = "SELECT m.kernel_name, m.boundedness,
                      m.compute_throughput_pct, m.memory_throughput_pct,
                      m.l2_hit_rate_pct, m.achieved_bandwidth_gb_s, m.peak_bandwidth_gb_s,
                      m.occupancy_pct
               FROM metrics m WHERE m.kernel_name LIKE ?1 ESCAPE '\'";
    let rows: Vec<_> = db.query_vec(sql, [like_param(pattern)], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<f64>>(2)?, row.get::<_, Option<f64>>(3)?,
            row.get::<_, Option<f64>>(4)?, row.get::<_, Option<f64>>(5)?,
            row.get::<_, Option<f64>>(6)?, row.get::<_, Option<f64>>(7)?))
    });

    if rows.is_empty() {
        println!("no metrics for kernel matching '{pattern}'");
        println!("need ncu data");
        return;
    }

    for (name, bound, cmp, mem, l2, bw, peak, occ) in &rows {
        println!("{name}:");
        match bound.as_deref() {
            Some("compute") => println!("  Compute-bound ({:.1}% compute, {:.1}% memory)", cmp.unwrap_or(0.0), mem.unwrap_or(0.0)),
            Some("memory") => {
                println!("  Memory-bound ({:.1}% memory, {:.1}% compute)", mem.unwrap_or(0.0), cmp.unwrap_or(0.0));
                if let Some(l) = l2 { println!("  L2 hit rate: {l:.1}%"); }
                if let (Some(a), Some(p)) = (bw, peak) {
                    println!("  Bandwidth: {a:.1} / {p:.1} GB/s ({:.1}% of peak)", a / p * 100.0);
                }
            }
            Some("latency") => {
                println!("  Latency-bound (low utilization)");
                if let Some(o) = occ { println!("  Occupancy: {o:.1}%"); }
            }
            _ => println!("  Compute: {:.1}%, Memory: {:.1}%", cmp.unwrap_or(0.0), mem.unwrap_or(0.0)),
        }
    }
}

// ---------------------------------------------------------------------------
// roofline
// ---------------------------------------------------------------------------

pub fn cmd_roofline(db: &GpuDb, args: &[&str]) {
    if !db.has_layer("ncu") {
        println!("no ncu metrics — roofline requires hardware counters");
        return;
    }
    let pattern = parse_pattern(args);
    let pat = pattern.map(like_param).unwrap_or_else(|| "%".into());

    let sql = "SELECT kernel_name, boundedness, compute_throughput_pct,
                      memory_throughput_pct, occupancy_pct
               FROM metrics WHERE kernel_name LIKE ?1 ESCAPE '\'
               ORDER BY kernel_name";
    let rows: Vec<_> = db.query_vec(sql, [&pat], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<f64>>(2)?, row.get::<_, Option<f64>>(3)?,
            row.get::<_, Option<f64>>(4)?))
    });

    println!("  Kernel                            Bound     Compute%  Memory%   Occupancy");
    println!("  ────────────────────────────────── ──────── ──────── ──────── ──────────");
    for (name, bound, cmp, mem, occ) in &rows {
        println!("  {:<34} {:<8} {:>7.1}% {:>7.1}% {:>8}",
            trunc(name, 34),
            bound.as_deref().unwrap_or("?"),
            cmp.unwrap_or(0.0), mem.unwrap_or(0.0),
            occ.map(|v| format!("{v:.1}%")).unwrap_or_else(|| "?".into()));
    }
}

// ---------------------------------------------------------------------------
// occupancy
// ---------------------------------------------------------------------------

pub fn cmd_occupancy(db: &GpuDb, args: &[&str]) {
    if !db.has_layer("ncu") { println!("no occupancy data — need ncu"); return; }
    let n = parse_count(args);

    let sql = "SELECT kernel_name, occupancy_pct, registers_per_thread,
                      shared_mem_static_bytes + shared_mem_dynamic_bytes as shmem
               FROM metrics WHERE occupancy_pct IS NOT NULL
               ORDER BY occupancy_pct ASC LIMIT ?1";
    let rows: Vec<(String, f64, Option<i64>, Option<i64>)> = db.query_vec(
        sql, [n as i64],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );

    println!("  Kernel                            Occupancy  Regs  ShmemK  Limiting");
    println!("  ────────────────────────────────── ───────── ───── ─────── ────────");
    for (name, occ, regs, shmem) in &rows {
        let limit = if regs.unwrap_or(0) > 64 { "registers" }
            else if shmem.unwrap_or(0) > 48 * 1024 { "shared mem" }
            else { "block size" };
        println!("  {:<34} {:>8.1}% {:>5} {:>6}  {}",
            trunc(name, 34), occ,
            regs.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
            shmem.map(|v| format!("{:.1}", v as f64 / 1024.0)).unwrap_or_else(|| "?".into()),
            limit);
    }
}

// ---------------------------------------------------------------------------
// transfers
// ---------------------------------------------------------------------------

pub fn cmd_transfers(db: &GpuDb, args: &[&str]) {
    if db.transfer_count() == 0 {
        println!("no memory transfers recorded");
        if !db.has_layer("nsys") { println!("need nsys layer for transfer data"); }
        return;
    }
    let n = parse_count(args);

    // --- Overall totals ---
    let (total_bytes, total_time): (i64, f64) = db.conn.query_row(
        "SELECT COALESCE(SUM(bytes),0), COALESCE(SUM(duration_us),0) FROM transfers",
        [], |row| Ok((row.get(0)?, row.get(1)?))
    ).unwrap();
    let kernel_time = db.total_gpu_time_us();
    let wall_us: f64 = db.meta("wall_time_us").parse().unwrap_or(0.0);

    println!("  Total: {} transfers, {}, {}",
        db.transfer_count(), fmt_bytes(total_bytes), fmt_us(total_time));
    if wall_us > 0.0 {
        println!("  {:.1}% of wall time spent on transfers", total_time / wall_us * 100.0);
    }

    // Transfer vs compute ratio — indicates bandwidth-bound workload
    if kernel_time > 0.0 {
        let ratio = total_time / kernel_time;
        let verdict = if ratio > 5.0 { "BANDWIDTH-BOUND — PCIe dominates" }
            else if ratio > 1.5 { "transfer-heavy — consider async transfers or larger batches" }
            else if ratio > 0.5 { "mixed compute/transfer" }
            else { "compute-dominated" };
        println!("  Transfer:compute ratio = {ratio:.2}:1  ({verdict})");
    }
    println!();

    // --- Breakdown by kind ---
    let kind_sql = "SELECT kind, COUNT(*), SUM(bytes), SUM(duration_us),
                           MIN(bytes), MAX(bytes)
                    FROM transfers GROUP BY kind ORDER BY SUM(duration_us) DESC";
    let kinds: Vec<(String, i64, i64, f64, i64, i64)> = db.query_vec(
        kind_sql, [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
    );

    println!("  By Direction:");
    println!("  Kind  Count    Total         Time        Avg BW       Size range");
    println!("  ───── ──────── ───────────── ─────────── ──────────── ────────────");
    for (kind, cnt, bytes, dur, min_b, max_b) in &kinds {
        let bw = if *dur > 0.0 { format!("{:.1} GB/s", *bytes as f64 / dur / 1000.0) }
            else { "?".into() };
        let range = if min_b == max_b { fmt_bytes(*min_b) }
            else { format!("{}-{}", fmt_bytes(*min_b), fmt_bytes(*max_b)) };
        println!("  {:<5} {:>8} {:>13} {:>11} {:>12} {}",
            kind, cnt, fmt_bytes(*bytes), fmt_us(*dur), bw, range);
    }
    println!();

    // --- Size distribution — flag small/large outliers ---
    let (small_cnt, small_bytes, small_time): (i64, i64, f64) = db.conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(bytes),0), COALESCE(SUM(duration_us),0)
         FROM transfers WHERE bytes < 1048576", // < 1 MB
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    ).unwrap();
    if small_cnt > 0 {
        let pct = if total_time > 0.0 { small_time / total_time * 100.0 } else { 0.0 };
        println!("  Small transfers: {} (<1 MB), {} total, {} time ({pct:.1}% of transfer time)",
            small_cnt, fmt_bytes(small_bytes), fmt_us(small_time));
        if small_cnt > 10 {
            println!("    → many small transfers — coalesce into fewer batched copies");
        }
    }

    // --- Cumulative size-vs-time distribution ---
    if total_time > 0.0 {
        print_transfer_cdf(db, total_time);
    }

    // --- Top N by duration ---
    let sql = "SELECT kind, bytes, duration_us, stream_id
               FROM transfers ORDER BY duration_us DESC LIMIT ?1";
    let rows: Vec<(String, i64, f64, Option<u32>)> = db.query_vec(
        sql, [n as i64],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );

    println!("\n  Top {} by Duration:", rows.len());
    println!("  #  Kind  Size        Duration    BW          Stream  Notes");
    println!("  ── ───── ────────── ────────── ──────────── ─────── ────────");
    for (i, (kind, bytes, dur, sid)) in rows.iter().enumerate() {
        let bw_num = if *dur > 0.0 { *bytes as f64 / dur / 1000.0 } else { 0.0 };
        let bw_str = if *dur > 0.0 { format!("{:.1} GB/s", bw_num) } else { "?".into() };
        // PCIe 4.0 x16 peak ≈ 31.5 GB/s, PCIe 3.0 x16 ≈ 15.75 GB/s.
        // A BW much lower than those for H2D/D2H on > 16 MB suggests non-pinned memory.
        let notes = if *bytes >= 16 * 1024 * 1024 && (kind == "H2D" || kind == "D2H") && bw_num < 6.0 {
            "pageable? consider cudaMallocHost"
        } else if *bytes < 4096 {
            "tiny — overhead-dominated"
        } else {
            ""
        };
        println!("  {:<2} {:<5} {:>10} {:>10} {:>11} {:>6}  {}",
            i+1, kind, fmt_bytes(*bytes), fmt_us(*dur), bw_str,
            sid.map(|s| s.to_string()).unwrap_or_else(|| "?".into()), notes);
    }
}

/// Print a cumulative-time-by-size distribution across standard size buckets.
/// Shows how much of the total transfer time lives in each byte-size range.
fn print_transfer_cdf(db: &GpuDb, total_time: f64) {
    // Fixed size buckets (bytes, human label).
    let buckets: [(i64, &str); 7] = [
        (4 * 1024,          "<4 KB"),
        (64 * 1024,         "<64 KB"),
        (1024 * 1024,       "<1 MB"),
        (16 * 1024 * 1024,  "<16 MB"),
        (128 * 1024 * 1024, "<128 MB"),
        (1024 * 1024 * 1024,"<1 GB"),
        (i64::MAX,          ">=1 GB"),
    ];

    // Pull (bytes, duration_us) ordered by size for a true CDF.
    let rows: Vec<(i64, f64)> = db.query_vec(
        "SELECT bytes, duration_us FROM transfers ORDER BY bytes ASC",
        [], |row| Ok((row.get(0)?, row.get(1)?))
    );
    if rows.is_empty() { return; }

    println!("\n  Cumulative time by transfer size:");
    println!("  Size bucket    Count    Time         Bucket %  Cumulative %");
    println!("  ────────────── ──────── ──────────── ───────── ────────────");
    let mut idx = 0usize;
    let mut cum = 0.0;
    for &(limit, label) in &buckets {
        let mut cnt = 0i64;
        let mut bucket_time = 0.0;
        while idx < rows.len() && rows[idx].0 < limit {
            bucket_time += rows[idx].1;
            cnt += 1;
            idx += 1;
        }
        if cnt == 0 { continue; }
        cum += bucket_time;
        let bpct = bucket_time / total_time * 100.0;
        let cpct = cum / total_time * 100.0;
        println!("  {:<14} {:>8} {:>12} {:>8.1}% {:>11.1}%",
            label, cnt, fmt_us(bucket_time), bpct, cpct);
    }
}

// ---------------------------------------------------------------------------
// gaps
// ---------------------------------------------------------------------------

pub fn cmd_gaps(db: &GpuDb, args: &[&str]) {
    if !db.has_layer("nsys") {
        println!("no timeline data — need nsys layer");
        return;
    }
    let n = parse_count(args);

    let mut rows = compute_gpu_gaps(db);
    if rows.is_empty() { println!("no GPU idle gaps detected"); return; }

    // Sum across ALL gaps first, then sort and truncate for display.
    let total_gap: f64 = rows.iter().map(|r| r.1).sum();
    let total_count = rows.len();

    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let shown = rows.len().min(n);
    rows.truncate(n);

    println!("  {} GPU idle gaps (total idle: {})", total_count, fmt_us(total_gap));
    if shown < total_count {
        println!("  Showing top {shown} by duration:\n");
    } else {
        println!();
    }
    println!("  #  Start        Duration     Before → After");
    println!("  ── ──────────── ──────────── ────────────────────────────────");
    for (i, (start, dur)) in rows.iter().enumerate() {
        let before = kernel_ending_at_or_before(db, *start);
        let after = kernel_starting_at_or_after(db, *start + *dur);
        let edge = format!("{} → {}",
            before.as_deref().map(|n| trunc(n, 22)).unwrap_or_else(|| "—".into()),
            after.as_deref().map(|n| trunc(n, 22)).unwrap_or_else(|| "—".into()));
        println!("  {:<2} {:>12} {:>12} {}", i+1, fmt_us(*start), fmt_us(*dur), edge);
    }
}

/// Most recent kernel that ended at or before `t` (in the timeline filter).
fn kernel_ending_at_or_before(db: &GpuDb, t: f64) -> Option<String> {
    let tl = db.timeline_filter();
    let sql = format!(
        "SELECT kernel_name FROM launches
         WHERE start_us IS NOT NULL AND (start_us + duration_us) <= ?1 + 0.5 AND {tl}
         ORDER BY (start_us + duration_us) DESC LIMIT 1"
    );
    db.conn.query_row(&sql, [t], |row| row.get::<_, String>(0)).ok()
}

/// First kernel that started at or after `t`.
fn kernel_starting_at_or_after(db: &GpuDb, t: f64) -> Option<String> {
    let tl = db.timeline_filter();
    let sql = format!(
        "SELECT kernel_name FROM launches
         WHERE start_us IS NOT NULL AND start_us >= ?1 - 0.5 AND {tl}
         ORDER BY start_us ASC LIMIT 1"
    );
    db.conn.query_row(&sql, [t], |row| row.get::<_, String>(0)).ok()
}

// ---------------------------------------------------------------------------
// overlap
// ---------------------------------------------------------------------------

pub fn cmd_overlap(db: &GpuDb) {
    if !db.has_layer("nsys") { println!("no timeline data — need nsys layer"); return; }

    let gpu_us = db.total_gpu_time_us();
    let wall_us: f64 = db.meta("wall_time_us").parse().unwrap_or(0.0);
    let xfer_time: f64 = db.conn.query_row(
        "SELECT COALESCE(SUM(duration_us),0) FROM transfers", [], |row| row.get(0)
    ).unwrap();

    // Compute actual overlap: how much transfer time runs concurrently with kernels.
    let (_, overlap_us) = xfer_kernel_overlap(db, None);

    println!("  Compute/Transfer Overlap:");
    println!("    GPU kernel time:   {}", fmt_us(gpu_us));
    println!("    Transfer time:     {}", fmt_us(xfer_time));
    if xfer_time > 0.0 && overlap_us > 0.0 {
        println!("    Concurrent:        {} ({:.1}% of transfers overlapped with compute)",
            fmt_us(overlap_us), overlap_us / xfer_time * 100.0);
    } else if xfer_time > 0.0 {
        println!("    Concurrent:        none (transfers and compute are serialized)");
    }
    if wall_us > 0.0 {
        println!("    GPU utilization:   {:.1}%", gpu_us / wall_us * 100.0);
    }

    // Break down overlap by transfer direction — only H2D typically overlaps
    // compute meaningfully (prefetch pattern), so call out D2H separately.
    let kinds: Vec<String> = db.query_vec(
        "SELECT DISTINCT kind FROM transfers WHERE start_us IS NOT NULL",
        [], |row| row.get(0)
    );
    if !kinds.is_empty() {
        println!("\n    By direction:");
        println!("    Kind   Transfer   Overlap    %");
        println!("    ────── ────────── ────────── ──────");
        for kind in &kinds {
            let (dir_time, dir_overlap) = xfer_kernel_overlap(db, Some(kind));
            if dir_time <= 0.0 { continue; }
            let pct = dir_overlap / dir_time * 100.0;
            println!("    {:<6} {:>10} {:>10} {:>5.1}%",
                kind, fmt_us(dir_time), fmt_us(dir_overlap), pct);
        }
    }
}

/// Compute (total_transfer_us, kernel_overlap_us) for transfers matching `kind`.
/// `None` means all transfers. Merges kernel intervals, then sums the per-transfer
/// overlap against the merged set.
pub(crate) fn xfer_kernel_overlap(db: &GpuDb, kind: Option<&str>) -> (f64, f64) {
    let merged = merge_intervals(&db.kernel_intervals());
    let t_intervals = db.transfer_intervals(kind);

    let total_time: f64 = t_intervals.iter().map(|(s, e)| e - s).sum();
    let mut overlap = 0.0;
    for &(ts, te) in &t_intervals {
        for &(ks, ke) in &merged {
            let os = ts.max(ks);
            let oe = te.min(ke);
            if os < oe { overlap += oe - os; }
        }
    }
    (total_time, overlap)
}

/// Merge overlapping or adjacent intervals into non-overlapping sorted intervals.
fn merge_intervals(intervals: &[(f64, f64)]) -> Vec<(f64, f64)> {
    if intervals.is_empty() { return Vec::new(); }
    let mut merged: Vec<(f64, f64)> = Vec::new();
    let (mut cur_s, mut cur_e) = intervals[0];
    for &(s, e) in &intervals[1..] {
        if s <= cur_e {
            if e > cur_e { cur_e = e; }
        } else {
            merged.push((cur_s, cur_e));
            cur_s = s;
            cur_e = e;
        }
    }
    merged.push((cur_s, cur_e));
    merged
}

// ---------------------------------------------------------------------------
// streams
// ---------------------------------------------------------------------------

pub fn cmd_streams(db: &GpuDb) {
    let tl = db.timeline_filter();
    let sql = format!("SELECT stream_id, COUNT(*) as cnt, SUM(duration_us) as total
               FROM launches WHERE stream_id IS NOT NULL AND {tl}
               GROUP BY stream_id ORDER BY total DESC");
    let rows: Vec<(u32, i64, f64)> = db.query_vec(
        &sql, [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    );

    if rows.is_empty() { println!("no stream data"); return; }
    println!("  Stream  Kernels  Active Time");
    println!("  ─────── ──────── ────────────");
    for (sid, cnt, total) in &rows {
        println!("  {:>6}  {:>7}  {:>11}", sid, cnt, fmt_us(*total));
    }
}

// ---------------------------------------------------------------------------
// timeline
// ---------------------------------------------------------------------------

pub fn cmd_timeline(db: &GpuDb, args: &[&str]) {
    let n = parse_count(args);
    let tl = db.timeline_filter();
    let sql = format!("SELECT kernel_name, start_us, duration_us, stream_id
               FROM launches WHERE start_us IS NOT NULL AND {tl}
               ORDER BY start_us LIMIT ?1");
    let rows: Vec<(String, f64, f64, Option<u32>)> = db.query_vec(
        &sql, [n as i64],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );

    if rows.is_empty() { println!("no timeline data"); return; }
    println!("  #  Start        Duration    Stream  Kernel");
    println!("  ── ──────────── ────────── ──────── ────────────────────────────────");
    for (i, (name, start, dur, sid)) in rows.iter().enumerate() {
        println!("  {:<2} {:>12} {:>10} {:>7}  {}",
            i+1, fmt_us(*start), fmt_us(*dur),
            sid.map(|s| s.to_string()).unwrap_or_else(|| "?".into()),
            trunc(name, 40));
    }
}

// ---------------------------------------------------------------------------
// trace (op → kernels)
// ---------------------------------------------------------------------------

pub fn cmd_trace(db: &GpuDb, args: &[&str]) {
    let pattern = match args.first() {
        Some(p) => *p,
        None => { println!("usage: trace <op_pattern>"); return; }
    };
    if !require_op_layer(db) { return; }

    let sql = r"SELECT id, name, module_path, cpu_time_us, input_shapes
               FROM ops WHERE name LIKE ?1 ESCAPE '\'";
    let ops: Vec<_> = db.query_vec(sql, [like_param(pattern)], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?, row.get::<_, f64>(3)?,
            row.get::<_, Option<String>>(4)?))
    });

    if ops.is_empty() { println!("no op matching '{pattern}'"); return; }

    for (op_id, name, module, cpu_time, shapes) in &ops {
        println!("Op: {name}");
        if let Some(m) = module { println!("  Module: {m}"); }
        if let Some(s) = shapes { println!("  Shapes: {s}"); }
        println!("  CPU: {}", fmt_us(*cpu_time));
        let kernels: Vec<String> = db.query_vec(
            "SELECT kernel_name FROM op_kernel_map WHERE op_id = ?1",
            [op_id], |row| row.get(0),
        );
        if !kernels.is_empty() {
            println!("  Kernels: {}", kernels.join(", "));
        }
    }
}

// ---------------------------------------------------------------------------
// callers (kernel → ops)
// ---------------------------------------------------------------------------

pub fn cmd_callers(db: &GpuDb, args: &[&str]) {
    let pattern = match args.first() {
        Some(p) => *p,
        None => { println!("usage: callers <kernel_pattern>"); return; }
    };
    if !require_op_layer(db) { return; }

    let sql = r"SELECT DISTINCT o.name, o.module_path
               FROM op_kernel_map okm JOIN ops o ON o.id = okm.op_id
               WHERE okm.kernel_name LIKE ?1 ESCAPE '\'";
    let rows: Vec<(String, Option<String>)> = db.query_vec(
        sql, [like_param(pattern)],
        |row| Ok((row.get(0)?, row.get(1)?)),
    );

    if rows.is_empty() { println!("no op mapping for kernels matching '{pattern}'"); return; }
    for (name, module) in &rows {
        println!("  {} ({})", name, module.as_deref().unwrap_or("?"));
    }
}

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
        // Show which kernels would benefit
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

    // High variance detection
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

    // Workload-specific advice (requires nsys for timing).
    if has_nsys {
        let gpu_us = db.total_gpu_time_us();
        let xfer_us: f64 = db.scalar_f64("SELECT COALESCE(SUM(duration_us),0) FROM transfers");

        // Transfer:compute ratio — PCIe-dominated workloads.
        if gpu_us > 0.0 && xfer_us > 0.0 {
            let ratio = xfer_us / gpu_us;
            if ratio > 5.0 {
                println!("  {n}. Transfer:compute ratio is {ratio:.1}:1 — PCIe dominates.");
                println!("     Try: cudaMallocHost (pinned memory), overlap via CUDA streams, or increase batch size.\n");
                n += 1;
            }
        }

        // Many tiny kernels — fusion candidates.
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

        // Single dominant kernel — bound analysis.
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

    // SQLite's ATTACH creates an empty DB at missing paths; guard first so
    // we fail loudly instead of silently creating junk at the target path.
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

// ---------------------------------------------------------------------------
// focus / ignore / region / reset
// ---------------------------------------------------------------------------

pub fn cmd_focus(db: &mut GpuDb, args: &[&str]) {
    match args.first() {
        Some(p) => { db.focus = Some(p.to_string()); println!("focus set to '{p}'"); }
        None => println!("usage: focus <pattern>"),
    }
}

pub fn cmd_ignore(db: &mut GpuDb, args: &[&str]) {
    match args.first() {
        Some(p) => { db.ignore = Some(p.to_string()); println!("ignoring '{p}'"); }
        None => println!("usage: ignore <pattern>"),
    }
}

pub fn cmd_region(db: &mut GpuDb, args: &[&str]) {
    match args.first() {
        Some(p) => { db.region_filter = Some(p.to_string()); println!("region filter set to '{p}'"); }
        None => {
            let rows: Vec<(String, f64)> = db.query_vec(
                "SELECT name, duration_us FROM regions ORDER BY start_us", [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            );
            if rows.is_empty() { println!("no NVTX regions"); }
            else { for (n, d) in &rows { println!("  {} ({})", n, fmt_us(*d)); } }
        }
    }
}

pub fn cmd_reset(db: &mut GpuDb) {
    db.focus = None;
    db.ignore = None;
    db.region_filter = None;
    println!("all filters cleared");
}

// ---------------------------------------------------------------------------
// variance
// ---------------------------------------------------------------------------

pub fn cmd_variance(db: &GpuDb, args: &[&str]) {
    let pattern = match args.first() {
        Some(p) => *p,
        None => { println!("usage: variance <kernel_pattern>"); return; }
    };

    let tl = db.timeline_filter();
    let sql = format!(r"SELECT kernel_name, COUNT(*), AVG(duration_us),
                      MIN(duration_us), MAX(duration_us),
                      AVG(duration_us * duration_us) - AVG(duration_us) * AVG(duration_us)
               FROM launches WHERE kernel_name LIKE ?1 ESCAPE '\' AND {tl}
               GROUP BY kernel_name");
    let rows: Vec<(String, i64, f64, f64, f64, f64)> = db.query_vec(
        &sql, [like_param(pattern)],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
    );

    if rows.is_empty() { println!("no kernel matching '{pattern}'"); return; }
    for (name, cnt, avg, min, max, var) in &rows {
        if *cnt < 2 { println!("{name}: only {cnt} launch"); continue; }
        let stddev = var.max(0.0).sqrt();
        let cv = if *avg > 0.0 { stddev / avg } else { 0.0 };
        println!("{name}:");
        println!("  Launches: {cnt}");
        println!("  Mean:     {}", fmt_us(*avg));
        println!("  Stddev:   {} (CV={cv:.3})", fmt_us(stddev));
        println!("  Min:      {}", fmt_us(*min));
        println!("  Max:      {}", fmt_us(*max));
    }
}

// ---------------------------------------------------------------------------
// warmup — detect warmup launches before timing stabilizes
// ---------------------------------------------------------------------------

/// Detect the number of warmup launches for a single kernel's duration series.
///
/// Returns the number of leading launches whose duration exceeds the
/// steady-state median (from the back half) by more than 20%.  Returns 0
/// when no meaningful warmup is detected.
pub(crate) fn detect_warmup_count(durations: &[f64]) -> usize {
    if durations.len() < 5 { return 0; }
    let half = durations.len() / 2;
    let mut tail: Vec<f64> = durations[half..].to_vec();
    tail.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let steady_median = tail[tail.len() / 2];
    if steady_median <= 0.0 { return 0; }

    let threshold = steady_median * 1.2;
    for (i, &d) in durations.iter().enumerate() {
        if d <= threshold { return i; }
    }
    0
}

pub fn cmd_warmup(db: &GpuDb) {
    let tl = db.timeline_filter();

    // Detect warmup per-kernel: only kernels with enough launches to analyze.
    let kernel_sql = format!(
        "SELECT kernel_name, COUNT(*) as cnt
         FROM launches WHERE start_us IS NOT NULL AND {tl}
         GROUP BY kernel_name HAVING cnt >= 5
         ORDER BY SUM(duration_us) DESC"
    );
    let kernels: Vec<(String, i64)> = db.query_vec(
        &kernel_sql, [], |row| Ok((row.get(0)?, row.get(1)?)),
    );

    if kernels.is_empty() {
        println!("not enough launches to detect warmup (need ≥5 of the same kernel)");
        return;
    }

    let wall_us: f64 = db.meta("wall_time_us").parse().unwrap_or(0.0);
    let mut found_warmup = false;

    for (kernel_name, _) in &kernels {
        let launch_sql = format!(
            "SELECT start_us, duration_us
             FROM launches WHERE kernel_name = ?1 AND start_us IS NOT NULL AND {tl}
             ORDER BY start_us LIMIT 200"
        );
        let launches: Vec<(f64, f64)> = db.query_vec(
            &launch_sql, [kernel_name], |row| Ok((row.get(0)?, row.get(1)?)),
        );

        if launches.len() < 5 { continue; }

        let durs: Vec<f64> = launches.iter().map(|r| r.1).collect();
        let steady_median = {
            let half = durs.len() / 2;
            let mut tail = durs[half..].to_vec();
            tail.sort_by(|a, b| a.partial_cmp(b).unwrap());
            tail[tail.len() / 2]
        };
        let warmup_end = detect_warmup_count(&durs);
        if warmup_end == 0 { continue; }

        found_warmup = true;
        let warmup_total: f64 = launches[..warmup_end].iter().map(|r| r.1).sum();
        let steady_avg = if launches.len() > warmup_end {
            launches[warmup_end..].iter().map(|r| r.1).sum::<f64>() / (launches.len() - warmup_end) as f64
        } else { 0.0 };
        let warmup_pct = if wall_us > 0.0 { warmup_total / wall_us * 100.0 } else { 0.0 };

        println!("  Warmup: {} ({})\n", trunc(kernel_name, 50), fmt_us(steady_median));
        println!("  Launch   Duration    Cumulative");
        let mut cumulative = 0.0;
        let show = (warmup_end + 3).min(launches.len());
        for (i, (_, dur)) in launches.iter().take(show).enumerate() {
            cumulative += dur;
            let marker = if i < warmup_end { "  ← warmup" } else if i == warmup_end { "  ← stabilized" } else { "" };
            println!("  {:<6}   {:>10}  {:>10}{marker}", i + 1, fmt_us(*dur), fmt_us(cumulative));
        }

        println!("\n  Warmup:       {} launches ({}, {warmup_pct:.1}% of wall time)", warmup_end, fmt_us(warmup_total));
        println!("  Steady state: {} avg/launch (excluding warmup)", fmt_us(steady_avg));
        let excess = warmup_total - steady_avg * warmup_end as f64;
        if excess > 0.0 {
            let wall_msg = if wall_us > 0.0 {
                format!(" out of {}", fmt_us(wall_us))
            } else { String::new() };
            println!("  Cold-start cost: first {} launch(es) cost {} extra{wall_msg} — dedicate a warmup pass to amortize",
                warmup_end, fmt_us(excess));
        }
        println!();
    }

    if !found_warmup {
        println!("no warmup detected (all kernels stable from first launch)");
    }
}

// ---------------------------------------------------------------------------
// small — kernels where launch overhead likely exceeds kernel duration
// ---------------------------------------------------------------------------

pub fn cmd_small(db: &GpuDb, args: &[&str]) {
    let n = parse_count(args);
    let threshold_us = 10.0; // typical cudaLaunchKernel overhead
    let tl = db.timeline_filter();

    let sql = format!(
        "SELECT kernel_name, COUNT(*) as cnt, AVG(duration_us) as avg,
                SUM(duration_us) as total
         FROM launches
         WHERE {} AND {tl} GROUP BY kernel_name
         HAVING avg < ?1
         ORDER BY cnt DESC LIMIT ?2",
        db.kernel_filter()
    );

    let rows: Vec<(String, i64, f64, f64)> = db.query_vec(
        &sql, rusqlite::params![threshold_us, n as i64],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );

    if rows.is_empty() {
        println!("no kernels averaging under {threshold_us:.0}us");
        return;
    }

    let total_launches: i64 = rows.iter().map(|r| r.1).sum();
    let total_time: f64 = rows.iter().map(|r| r.3).sum();
    let overhead_est = total_launches as f64 * 5.0; // ~5us per launch

    println!("  Small Kernels (avg < {threshold_us:.0}us, launch overhead may dominate):\n");
    println!("  #  Kernel                            Avg       Launches  Total");
    println!("  ── ────────────────────────────────── ───────── ──────── ─────────");
    for (i, (name, cnt, avg, total)) in rows.iter().enumerate() {
        println!("  {:<2} {:<34} {:>9} {:>8} {:>9}",
            i + 1, trunc(name, 34), fmt_us(*avg), cnt, fmt_us(*total));
    }
    println!("\n  {} kernels, {} total launches", rows.len(), total_launches);
    println!("  Estimated launch overhead: {} (at ~5us/launch)", fmt_us(overhead_est));
    println!("  Actual compute time:       {}", fmt_us(total_time));
    if overhead_est > total_time {
        println!("  Launch overhead EXCEEDS compute — consider kernel fusion or torch.compile()");
    }
}

// ---------------------------------------------------------------------------
// fuse — detect sequential kernel launches with small gaps (fusion candidates)
// ---------------------------------------------------------------------------

pub fn cmd_fuse(db: &GpuDb, args: &[&str]) {
    let n = parse_count(args);
    if !db.has_layer("nsys") && !db.has_layer("torch") {
        println!("no timeline data — need nsys or torch layer");
        return;
    }

    let tl = db.timeline_filter();
    let sql = "WITH ordered AS (
                 SELECT kernel_name, start_us, duration_us, stream_id,
                        ROW_NUMBER() OVER (ORDER BY start_us) as rn
                 FROM launches WHERE start_us IS NOT NULL AND ".to_string()
        + &tl + ")
               SELECT a.kernel_name, b.kernel_name,
                      b.start_us - (a.start_us + a.duration_us) AS gap_us,
                      a.duration_us + b.duration_us AS combined_us
               FROM ordered a
               JOIN ordered b ON b.rn = a.rn + 1
               WHERE gap_us >= 0 AND gap_us < 5.0
                 AND a.stream_id IS b.stream_id
               ORDER BY gap_us ASC
               LIMIT 500";

    let rows: Vec<(String, String, f64, f64)> = db.query_vec(
        &sql, [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );

    if rows.is_empty() {
        println!("no fusion candidates found (no sequential same-stream kernels with < 5us gap)");
        return;
    }

    // Aggregate by kernel pair (use full names as keys, truncate at display)
    let mut pairs: std::collections::HashMap<(String, String), (f64, f64, usize)> =
        std::collections::HashMap::new();
    for (a, b, gap, combined) in &rows {
        let key = (a.clone(), b.clone());
        let entry = pairs.entry(key).or_insert((0.0, 0.0, 0));
        entry.0 += gap;
        entry.1 += combined;
        entry.2 += 1;
    }

    let mut sorted: Vec<_> = pairs.into_iter().collect();
    sorted.sort_by(|a, b| b.1.2.cmp(&a.1.2));
    sorted.truncate(n);

    let total_gap: f64 = rows.iter().map(|r| r.2).sum();

    println!("  Sequential Launch Candidates (same stream, < 5us gap):\n");
    println!("  #  Kernel A → Kernel B                              Count  Avg Gap  Type");
    println!("  ── ──────────────────────────────────────────────── ────── ──────── ─────────");
    for (i, ((a, b), (gap_sum, _, count))) in sorted.iter().enumerate() {
        let avg_gap = gap_sum / *count as f64;
        let kind = if a == b { "batch" } else { "fuse" };
        println!("  {:<2} {} → {}  {:>5}  {:>7}  {}",
            i + 1, trunc(a, 24), trunc(b, 24), count, fmt_us(avg_gap), kind);
    }
    println!("\n  Total reclaimable gap: {} across {} pairs", fmt_us(total_gap), rows.len());
    println!("  'batch' = same kernel, use CUDA graphs or larger batch sizes");
    println!("  'fuse'  = different kernels, use torch.compile() or manual fusion");

    // Detect repeating kernel sequences — A→B→C→A→B→C patterns that CUDA graphs
    // can capture. Walks the full ordered launch stream (not just tight gaps).
    detect_kernel_sequences(db, n);
}

/// Scan the timeline for repeating kernel-name sequences of length 2..=5.
/// Reports patterns that repeat at least 3 times and cover meaningful GPU time.
fn detect_kernel_sequences(db: &GpuDb, limit: usize) {
    let tl = db.timeline_filter();
    let sql = format!(
        "SELECT kernel_name, duration_us FROM launches
         WHERE start_us IS NOT NULL AND {tl}
         ORDER BY start_us"
    );
    let launches: Vec<(String, f64)> = db.query_vec(&sql, [], |row| {
        Ok((row.get(0)?, row.get(1)?))
    });
    if launches.len() < 6 { return; }

    // For each candidate length, scan for back-to-back repeats of the same window.
    // Greedy, non-overlapping: once we accept a repeat starting at i, advance past it.
    // Prefer longer patterns (report them first) since a length-3 repeat subsumes a length-2.
    type PatternKey = Vec<usize>;
    struct Found { names: Vec<String>, reps: usize, total_us: f64 }
    let mut found: std::collections::HashMap<PatternKey, Found> = std::collections::HashMap::new();

    // Intern kernel names to ids for fast compare.
    let mut id_of: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut names: Vec<String> = Vec::new();
    let ids: Vec<usize> = launches.iter().map(|(n, _)| {
        if let Some(&i) = id_of.get(n) { i } else {
            let i = names.len();
            names.push(n.clone());
            id_of.insert(n.clone(), i);
            i
        }
    }).collect();
    let durs: Vec<f64> = launches.iter().map(|(_, d)| *d).collect();

    for len in (2..=5).rev() {
        if ids.len() < len * 2 { continue; }
        let mut i = 0;
        while i + 2 * len <= ids.len() {
            // Require the pattern itself to be non-constant (covered by 'batch' pairs already).
            let pat = &ids[i..i + len];
            if pat.iter().all(|&x| x == pat[0]) { i += 1; continue; }

            // Count consecutive non-overlapping repeats.
            let mut reps = 1usize;
            let mut j = i + len;
            while j + len <= ids.len() && ids[j..j + len] == *pat {
                reps += 1;
                j += len;
            }
            if reps >= 3 {
                let window_us: f64 = durs[i..j].iter().sum();
                let key: Vec<usize> = pat.to_vec();
                let entry = found.entry(key).or_insert(Found {
                    names: pat.iter().map(|&id| names[id].clone()).collect(),
                    reps: 0,
                    total_us: 0.0,
                });
                entry.reps += reps;
                entry.total_us += window_us;
                i = j; // skip past the whole run
            } else {
                i += 1;
            }
        }
    }

    if found.is_empty() { return; }

    let mut sorted: Vec<_> = found.into_iter().collect();
    sorted.sort_by(|a, b| b.1.total_us.partial_cmp(&a.1.total_us).unwrap());
    sorted.truncate(limit);

    println!("\n  Repeating Kernel Sequences (CUDA graph candidates):\n");
    println!("  #  Length  Reps    GPU Time     Sequence");
    println!("  ── ─────── ─────── ──────────── ─────────────────────────────────────────");
    for (i, (_, f)) in sorted.iter().enumerate() {
        let seq = f.names.iter().map(|n| trunc(n, 20)).collect::<Vec<_>>().join(" → ");
        println!("  {:<2} {:>7} {:>7} {:>12} {}",
            i + 1, f.names.len(), f.reps, fmt_us(f.total_us), seq);
    }
    println!("  → capture these with torch.cuda.graph or cudaGraph APIs to remove launch overhead");
}

// ---------------------------------------------------------------------------
// concurrency — stream utilization and parallelism opportunities
// ---------------------------------------------------------------------------

pub fn cmd_concurrency(db: &GpuDb) {
    let total_launches = db.total_launch_count();

    if total_launches == 0 {
        println!("no launch data");
        return;
    }

    // Per-stream breakdown
    let tl = db.timeline_filter();
    let sql = format!("SELECT stream_id, COUNT(*) as cnt, SUM(duration_us) as total
               FROM launches WHERE stream_id IS NOT NULL AND {tl}
               GROUP BY stream_id ORDER BY total DESC");
    let streams: Vec<(u32, i64, f64)> = db.query_vec(
        &sql, [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    );

    println!("  Stream Concurrency Analysis:\n");

    if streams.len() <= 1 {
        let sid = streams.first().map(|s| s.0.to_string()).unwrap_or_else(|| "?".into());
        println!("  All {} launches on stream {} (single stream)\n", total_launches, sid);
    } else {
        println!("  {} streams active:\n", streams.len());
        println!("  Stream  Launches  Active Time  % of Total");
        println!("  ─────── ──────── ──────────── ──────────");
        let gpu_total = db.total_gpu_time_us();
        for (sid, cnt, total) in &streams {
            let pct = if gpu_total > 0.0 { total / gpu_total * 100.0 } else { 0.0 };
            println!("  {:>6}  {:>7}  {:>11}  {:>9.1}%", sid, cnt, fmt_us(*total), pct);
        }
        println!();
    }

    // Parallelism index: sum-of-per-kernel-time / merged-active-time.
    // 1.0 = pure serial; N on N streams = perfect overlap.
    let gpu_total = db.total_gpu_time_us();
    let merged_active: f64 = merge_intervals(&db.kernel_intervals())
        .iter().map(|(s, e)| e - s).sum();
    if merged_active > 0.0 && gpu_total > 0.0 {
        let pindex = gpu_total / merged_active;
        let verdict = if pindex < 1.05 { "serial — no overlap" }
            else if pindex < 1.5 { "light overlap" }
            else if pindex < 2.5 { "moderate overlap" }
            else { "high overlap" };
        println!("  Parallelism index: {pindex:.2}x  ({verdict})");
        println!("    (sum of per-kernel time / merged active time — 1.0 = serial, N = perfect N-way overlap)\n");
    }

    // Detect true GPU idle gaps (merge overlapping intervals across streams)
    let gpu_gaps = compute_gpu_gaps(db);
    let total_gap: f64 = gpu_gaps.iter().map(|g| g.1).sum();
    let gap_count = gpu_gaps.len() as i64;

    let wall_us: f64 = db.meta("wall_time_us").parse().unwrap_or(0.0);

    if total_gap > 0.0 {
        println!("  GPU idle: {} across {} gaps ({:.1}% of wall time)",
            fmt_us(total_gap), gap_count,
            if wall_us > 0.0 { total_gap / wall_us * 100.0 } else { 0.0 });
    }

    if streams.len() <= 1 && gap_count > 10 {
        println!("  Multiple streams could reduce idle time by overlapping independent kernels");
        println!("  Tip: torch.cuda.Stream() for manual overlap, or CUDA graphs for replay");
    }
}

// ---------------------------------------------------------------------------
// hotpath — critical path through the training step
// ---------------------------------------------------------------------------

pub fn cmd_hotpath(db: &GpuDb) {
    if !require_op_layer(db) { return; }

    let sql = "SELECT name, cpu_time_us, gpu_time_us, module_path
               FROM ops
               WHERE cpu_time_us > 0
               ORDER BY cpu_time_us DESC
               LIMIT 20";
    let ops: Vec<(String, f64, f64, Option<String>)> = db.query_vec(
        sql, [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );

    if ops.is_empty() { println!("no op data"); return; }

    let total_cpu: f64 = ops.iter().map(|o| o.1).sum();
    let total_gpu = db.total_gpu_time_us();

    println!("  Critical Path (ops by CPU time):\n");
    println!("  Op                               CPU Time   GPU Time   Bound");
    println!("  ───────────────────────────────── ────────── ────────── ─────");
    for (name, cpu, gpu, _) in &ops {
        let bound = if *gpu < 0.01 {
            "overhead"
        } else if cpu / gpu.max(0.01) > 10.0 {
            "CPU"
        } else if gpu / cpu.max(0.01) > 2.0 {
            "GPU"
        } else {
            "balanced"
        };
        println!("  {:<34} {:>9} {:>9}  {bound}",
            trunc(name, 34), fmt_us(*cpu), fmt_us(*gpu));
    }

    println!("\n  Total CPU: {}  Total GPU: {}", fmt_us(total_cpu), fmt_us(total_gpu));
    let ratio = total_cpu / total_gpu.max(0.01);
    if ratio > 10.0 {
        println!("  Workload is CPU-bound ({ratio:.0}:1 CPU:GPU ratio)");
        println!("  Consider: larger batch size, torch.compile(), or CUDA graphs");
    } else if ratio < 0.5 {
        println!("  Workload is GPU-bound — optimize kernel efficiency");
    } else {
        println!("  Workload is balanced between CPU and GPU");
    }
}

// ---------------------------------------------------------------------------
// compare-ops — CPU vs GPU time ratio per operator
// ---------------------------------------------------------------------------

pub fn cmd_compare_ops(db: &GpuDb, args: &[&str]) {
    if !require_op_layer(db) { return; }
    let n = parse_count(args);

    let sql = "SELECT name, cpu_time_us, gpu_time_us
               FROM ops
               WHERE cpu_time_us > 0
               ORDER BY cpu_time_us DESC
               LIMIT ?1";
    let ops: Vec<(String, f64, f64)> = db.query_vec(
        sql, [n as i64],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    );

    if ops.is_empty() { println!("no op data"); return; }

    println!("  CPU vs GPU Time by Operator:\n");
    println!("  Op                               CPU Time   GPU Time   Ratio      Assessment");
    println!("  ───────────────────────────────── ────────── ────────── ────────── ──────────");
    for (name, cpu, gpu) in &ops {
        let (ratio_str, assessment) = if *gpu < 0.01 {
            ("∞".to_string(), "pure overhead")
        } else {
            let r = cpu / gpu;
            let a = if r > 100.0 { "CPU-bound" }
                else if r > 10.0 { "CPU-heavy" }
                else if r > 2.0 { "balanced" }
                else if r > 0.5 { "GPU-heavy" }
                else { "GPU-bound" };
            (format!("{r:.0}:1"), a)
        };
        println!("  {:<34} {:>9} {:>9} {:>10}  {assessment}",
            trunc(name, 34), fmt_us(*cpu), fmt_us(*gpu), ratio_str);
    }

    let total_cpu: f64 = ops.iter().map(|o| o.1).sum();
    let total_gpu: f64 = ops.iter().map(|o| o.2).sum();
    let gpu_util = if total_cpu > 0.0 { total_gpu / total_cpu * 100.0 } else { 0.0 };
    println!("\n  GPU utilization: {gpu_util:.1}% (GPU active time / CPU wall time)");
}

// ---------------------------------------------------------------------------
// top-ops — ops ranked by GPU time (not CPU time)
// ---------------------------------------------------------------------------

pub fn cmd_top_ops(db: &GpuDb, args: &[&str]) {
    if !require_op_layer(db) { return; }
    let n = parse_count(args);
    let pattern = parse_pattern(args);
    let pat_clause = pattern
        .map(|p| format!(r"AND o.name LIKE '%{}%' ESCAPE '\'", escape_sql_like(p)))
        .unwrap_or_default();

    let sql = format!(
        "SELECT o.name, o.cpu_time_us, o.gpu_time_us, o.module_path
         FROM ops o
         WHERE o.gpu_time_us > 0 {pat_clause}
         ORDER BY o.gpu_time_us DESC
         LIMIT ?1"
    );

    let rows: Vec<(String, f64, f64, Option<String>)> = db.query_vec(
        &sql, [n as i64],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );

    if rows.is_empty() {
        println!("no ops with GPU time (op->kernel correlation may be missing)");
        return;
    }

    let total_gpu = db.total_gpu_time_us();
    println!("  Ops by GPU Time:\n");
    println!("  #  Op                               GPU Time   % GPU    CPU Time   Ratio");
    println!("  ── ───────────────────────────────── ────────── ──────── ────────── ──────");
    for (i, (name, cpu, gpu, _)) in rows.iter().enumerate() {
        let pct = if total_gpu > 0.0 { gpu / total_gpu * 100.0 } else { 0.0 };
        let ratio = if *gpu > 0.01 { format!("{:.0}:1", cpu / gpu) } else { "∞".into() };
        println!("  {:<2} {:<34} {:>9} {:>7.1}% {:>9} {:>6}",
            i + 1, trunc(name, 34), fmt_us(*gpu), pct, fmt_us(*cpu), ratio);
    }
}

// ---------------------------------------------------------------------------
// breakdown — show which kernels an op expands into
// ---------------------------------------------------------------------------

pub fn cmd_breakdown(db: &GpuDb, args: &[&str]) {
    let pattern = match args.first() {
        Some(p) => *p,
        None => { println!("usage: breakdown <op_pattern>"); return; }
    };

    if !require_op_layer(db) { return; }

    // Find matching ops
    let op_sql = r"SELECT id, name, cpu_time_us, gpu_time_us FROM ops WHERE name LIKE ?1 ESCAPE '\'";
    let ops: Vec<(i64, String, f64, f64)> = db.query_vec(
        op_sql, [like_param(pattern)],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );

    if ops.is_empty() { println!("no op matching '{pattern}'"); return; }

    for (op_id, op_name, cpu_time, gpu_time) in &ops {
        println!("Op: {op_name}");
        println!("  CPU: {}  GPU: {}", fmt_us(*cpu_time), fmt_us(*gpu_time));

        // Find kernels this op launches.
        // Restrict to timeline layer to avoid double-counting across nsys+torch.
        let tl_l = db.timeline_filter_for("l");
        let k_sql = format!(
            "SELECT okm.kernel_name,
                    COUNT(*) as launches,
                    SUM(l.duration_us) as total_us,
                    AVG(l.duration_us) as avg_us
             FROM op_kernel_map okm
             JOIN launches l ON l.kernel_name = okm.kernel_name AND {tl_l}
             WHERE okm.op_id = ?1
             GROUP BY okm.kernel_name
             ORDER BY total_us DESC"
        );
        let kernels: Vec<(String, i64, f64, f64)> = db.query_vec(
            &k_sql, [op_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        );

        if kernels.is_empty() {
            println!("  (no correlated kernels)\n");
            continue;
        }

        let kernel_total: f64 = kernels.iter().map(|k| k.2).sum();
        println!("  Kernels ({} total GPU time):\n", fmt_us(kernel_total));
        println!("    Kernel                            Total      %     Avg       Launches");
        println!("    ────────────────────────────────── ───────── ────── ───────── ────────");
        for (kname, launches, total, avg) in &kernels {
            let pct = if kernel_total > 0.0 { total / kernel_total * 100.0 } else { 0.0 };
            println!("    {:<34} {:>9} {:>5.1}% {:>9} {:>7}",
                trunc(kname, 34), fmt_us(*total), pct, fmt_us(*avg), launches);
        }
        println!();
    }
}

// ---------------------------------------------------------------------------
// idle-between — measure GPU idle gap between two ops
// ---------------------------------------------------------------------------

pub fn cmd_idle_between(db: &GpuDb, args: &[&str]) {
    if args.len() < 2 {
        println!("usage: idle-between <op_a_pattern> <op_b_pattern>");
        return;
    }
    let pat_a = args[0];
    let pat_b = args[1];

    if !require_op_layer(db) { return; }

    // Use torch layer for idle-between since it has real kernel names + timestamps.
    // The nsys layer on WSL2 only has opaque "cudaLaunchKernel" names.
    let torch_layer = db.conn.query_row(
        "SELECT id FROM layers WHERE source IN ('torch', 'proton') ORDER BY id LIMIT 1",
        [], |row| row.get::<_, i64>(0),
    );
    let tl = match torch_layer {
        Ok(id) => format!("launches.layer_id = {id}"),
        Err(_) => db.timeline_filter(),
    };

    // Find kernel launches correlated to each op, compute gaps.

    // Get kernels belonging to op A
    let ka_sql = r"SELECT DISTINCT kernel_name FROM op_kernel_map okm
                  JOIN ops o ON o.id = okm.op_id
                  WHERE o.name LIKE ?1 ESCAPE '\'";
    let kernels_a: Vec<String> = db.query_vec(ka_sql, [like_param(pat_a)], |row| row.get(0));
    let kernels_b: Vec<String> = db.query_vec(ka_sql, [like_param(pat_b)], |row| row.get(0));

    if kernels_a.is_empty() { println!("no kernels found for op '{pat_a}'"); return; }
    if kernels_b.is_empty() { println!("no kernels found for op '{pat_b}'"); return; }

    // Get end times of A's kernels and start times of B's kernels
    let placeholders_a = kernels_a.iter().map(|k| format!("'{}'", k.replace('\'', "''"))).collect::<Vec<_>>().join(",");
    let placeholders_b = kernels_b.iter().map(|k| format!("'{}'", k.replace('\'', "''"))).collect::<Vec<_>>().join(",");

    let a_sql = format!(
        "SELECT start_us + duration_us AS end_us FROM launches
         WHERE kernel_name IN ({placeholders_a}) AND start_us IS NOT NULL AND {tl}
         ORDER BY start_us"
    );
    let b_sql = format!(
        "SELECT start_us FROM launches
         WHERE kernel_name IN ({placeholders_b}) AND start_us IS NOT NULL AND {tl}
         ORDER BY start_us"
    );

    let a_ends: Vec<f64> = db.query_vec(&a_sql, [], |row| row.get(0));
    let b_starts: Vec<f64> = db.query_vec(&b_sql, [], |row| row.get(0));

    // For each A end, find the next B start
    let mut gaps: Vec<f64> = Vec::new();
    let mut b_idx = 0;
    for a_end in &a_ends {
        // Advance b_idx to first B start after this A end
        while b_idx < b_starts.len() && b_starts[b_idx] < *a_end {
            b_idx += 1;
        }
        if b_idx < b_starts.len() {
            let gap = b_starts[b_idx] - a_end;
            if gap >= 0.0 {
                gaps.push(gap);
            }
        }
    }

    if gaps.is_empty() {
        println!("no transitions found from '{pat_a}' to '{pat_b}'");
        return;
    }

    let total: f64 = gaps.iter().sum();
    let avg = total / gaps.len() as f64;
    let min = gaps.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = gaps.iter().cloned().fold(0.0_f64, f64::max);

    println!("  Idle Between '{}' → '{}':\n", pat_a, pat_b);
    println!("  Transitions: {}", gaps.len());
    println!("  Total idle:  {}", fmt_us(total));
    println!("  Average:     {}", fmt_us(avg));
    println!("  Min:         {}", fmt_us(min));
    println!("  Max:         {}", fmt_us(max));

    let wall_us: f64 = db.meta("wall_time_us").parse().unwrap_or(0.0);
    if wall_us > 0.0 {
        println!("  % of wall:   {:.1}%", total / wall_us * 100.0);
    }
}

// ---------------------------------------------------------------------------
// outliers — slowest launches of a kernel with timeline position
// ---------------------------------------------------------------------------

pub fn cmd_outliers(db: &GpuDb, args: &[&str]) {
    let pattern = match args.first() {
        Some(p) => *p,
        None => { println!("usage: outliers <kernel_pattern>"); return; }
    };
    let tl = db.timeline_filter();

    // Resolve pattern to a single kernel_name (most common) to keep the report focused.
    let resolve_sql = format!(
        r"SELECT kernel_name, COUNT(*) FROM launches
          WHERE kernel_name LIKE ?1 ESCAPE '\' AND {tl}
          GROUP BY kernel_name ORDER BY COUNT(*) DESC LIMIT 1"
    );
    let (name, total_cnt) = match db.conn.query_row(
        &resolve_sql, [like_param(pattern)],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    ) {
        Ok(x) => x,
        Err(_) => { println!("no kernel matching '{pattern}'"); return; }
    };

    // Pull all launches ordered by launch-order (start_us) so we can assign a launch index.
    let all_sql = format!(
        "SELECT start_us, duration_us FROM launches
         WHERE kernel_name = ?1 AND start_us IS NOT NULL AND {tl}
         ORDER BY start_us"
    );
    let launches: Vec<(f64, f64)> = db.query_vec(&all_sql, [&name], |row| {
        Ok((row.get(0)?, row.get(1)?))
    });
    if launches.len() < 4 {
        println!("{name}: only {} launches — need ≥4 for outlier analysis", launches.len());
        return;
    }

    let cnt = launches.len();
    let mut sorted: Vec<f64> = launches.iter().map(|(_, d)| *d).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // Nearest-rank percentile: index = ceil(p * n) - 1, clamped to [0, n-1].
    // Formula preserves p50 == median and avoids reporting max() as p90 on
    // tiny samples (e.g. cnt=10 would otherwise make p90 == max).
    let pct_idx = |p: f64| -> usize {
        let k = (p * cnt as f64).ceil() as isize - 1;
        k.clamp(0, cnt as isize - 1) as usize
    };
    let median = sorted[pct_idx(0.50)];
    let p90 = sorted[pct_idx(0.90)];
    let p99 = sorted[pct_idx(0.99)];

    // Top-10% of launches by duration with their original launch index.
    let mut indexed: Vec<(usize, f64, f64)> = launches.iter().enumerate()
        .map(|(i, (s, d))| (i, *s, *d)).collect();
    indexed.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
    let top_n = (cnt / 10).max(3).min(cnt);
    let outliers = &indexed[..top_n];

    // Early/late clustering: count how many outliers fall in the first/last third.
    let third = cnt / 3;
    let mut early = 0;
    let mut late = 0;
    for &(idx, _, _) in outliers {
        if idx < third { early += 1; }
        else if idx >= cnt - third { late += 1; }
    }

    let t_min = launches.first().map(|(s, _)| *s).unwrap_or(0.0);
    let t_max = launches.last().map(|(s, d)| *s + *d).unwrap_or(0.0);
    let span = t_max - t_min;

    println!("  Outliers: {} ({} launches)\n", trunc(&name, 60), total_cnt);
    println!("  Distribution:");
    println!("    median: {}   p90: {}   p99: {}   max: {}",
        fmt_us(median), fmt_us(p90), fmt_us(p99), fmt_us(sorted[cnt - 1]));
    println!("    worst is {:.1}x median\n", sorted[cnt - 1] / median.max(1e-9));

    println!("  Worst {} launches (top {:.0}%):", top_n, top_n as f64 / cnt as f64 * 100.0);
    println!("  #   Idx   Timeline     Start        Duration    vs median");
    println!("  ─── ───── ──────────── ──────────── ─────────── ─────────");
    for (i, &(idx, start, dur)) in outliers.iter().enumerate() {
        let tpos = if span > 0.0 { (start - t_min) / span * 100.0 } else { 0.0 };
        let ratio = dur / median.max(1e-9);
        println!("  {:<3} {:>5} {:>11.1}% {:>12} {:>11} {:>7.1}x",
            i + 1, idx, tpos, fmt_us(start), fmt_us(dur), ratio);
    }

    println!();
    // Suppress the clustering verdict when the data can't support one:
    //  - too few launches for statistical signal
    //  - worst barely exceeds median (essentially uniform distribution)
    let worst_ratio = sorted[cnt - 1] / median.max(1e-9);
    if cnt < 20 {
        println!("  → {cnt} launches — too few to distinguish clustering from noise");
    } else if worst_ratio < 1.5 {
        println!("  → launches are uniform (worst {:.2}x median) — no meaningful outliers", worst_ratio);
    } else if early > 2 * late && early >= top_n / 2 {
        println!("  → clusters EARLY ({}/{} outliers in first third) — likely warmup / JIT / cache cold", early, top_n);
    } else if late > 2 * early && late >= top_n / 2 {
        println!("  → clusters LATE ({}/{} outliers in last third) — thermal throttling, memory fragmentation, or contention", late, top_n);
    } else {
        println!("  → outliers spread across the timeline — likely data-dependent work or scheduler jitter");
    }
}

// ---------------------------------------------------------------------------
// source — show which ops/files launched a kernel (needs torch/proton layer)
// ---------------------------------------------------------------------------

pub fn cmd_source(db: &GpuDb, args: &[&str]) {
    let pattern = match args.first() {
        Some(p) => *p,
        None => { println!("usage: source <kernel_pattern>"); return; }
    };

    if !db.has_layer("torch") && !db.has_layer("proton") {
        println!("no op-to-kernel mapping — need torch.profiler or proton layer");
        println!("(run 'suggest' for how to collect it)");
        return;
    }

    // Match kernel → ops via op_kernel_map. Aggregate by (op name, module_path).
    let sql = r"SELECT o.name, COALESCE(o.module_path, '') AS mp,
                       COUNT(DISTINCT o.id) AS op_hits,
                       SUM(COALESCE(o.gpu_time_us, 0)) AS gpu_us
                FROM op_kernel_map m
                JOIN ops o ON o.id = m.op_id
                WHERE m.kernel_name LIKE ?1 ESCAPE '\'
                GROUP BY o.name, mp
                ORDER BY gpu_us DESC
                LIMIT 20";
    let rows: Vec<(String, String, i64, f64)> = db.query_vec(
        sql, [like_param(pattern)],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    );

    if rows.is_empty() {
        println!("no op mapping found for pattern '{pattern}'");
        return;
    }

    println!("  Launch sites for kernels matching '{pattern}':\n");
    println!("  Op                                       Hits  GPU Time    Source");
    println!("  ──────────────────────────────────────── ───── ─────────── ──────────────────────────────");
    for (name, mp, hits, gpu_us) in &rows {
        let src = if mp.is_empty() { "—".to_string() } else { trunc(mp, 40) };
        println!("  {:<40} {:>5} {:>11} {}",
            trunc(name, 40), hits, fmt_us(*gpu_us), src);
    }
}

// ---------------------------------------------------------------------------
// memory — GPU memory allocation tracking (needs --cuda-memory-usage in nsys)
// ---------------------------------------------------------------------------

pub fn cmd_memory(db: &GpuDb, args: &[&str]) {
    // Gate: allocations table may be empty either because memory tracking
    // wasn't enabled or the run didn't allocate anything.
    let total: i64 = db.scalar_f64("SELECT COUNT(*) FROM allocations") as i64;
    if total == 0 {
        println!("no allocation data");
        println!("(re-profile to capture it — memory tracking is enabled by default in this build)");
        return;
    }
    let n = parse_count(args);

    // Totals.
    let (n_alloc, n_free, sum_alloc): (i64, i64, i64) = db.conn.query_row(
        "SELECT SUM(CASE WHEN op = 'alloc' THEN 1 ELSE 0 END),
                SUM(CASE WHEN op = 'free'  THEN 1 ELSE 0 END),
                COALESCE(SUM(CASE WHEN op = 'alloc' THEN bytes ELSE 0 END), 0)
         FROM allocations",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    ).unwrap_or((0, 0, 0));

    // Walk events chronologically to find peak live bytes and leaks.
    // Pair allocs and frees by address — last-writer-wins if an address
    // is reallocated before its previous free.
    let events: Vec<(f64, String, i64, i64)> = db.query_vec(
        "SELECT start_us, op, address, bytes FROM allocations ORDER BY start_us",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    );
    let mut live: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    let mut live_bytes: i64 = 0;
    let mut peak: i64 = 0;
    let mut peak_time: f64 = 0.0;
    let mut alloc_lifetimes: Vec<(i64, f64)> = Vec::new();
    let mut pending_start: std::collections::HashMap<i64, f64> = std::collections::HashMap::new();
    for (t, op, addr, bytes) in &events {
        if op == "alloc" {
            live.insert(*addr, *bytes);
            pending_start.insert(*addr, *t);
            live_bytes += *bytes;
            if live_bytes > peak { peak = live_bytes; peak_time = *t; }
        } else if op == "free" {
            if let Some(b) = live.remove(addr) {
                live_bytes -= b;
                if let Some(s) = pending_start.remove(addr) {
                    alloc_lifetimes.push((b, *t - s));
                }
            }
        }
    }
    let leaked: i64 = live.values().sum();
    let leak_count = live.len();

    println!("  GPU Memory Summary\n");
    println!("  Events:    {n_alloc} allocs, {n_free} frees");
    println!("  Total:     {} allocated across {n_alloc} events", fmt_bytes(sum_alloc));
    println!("  Peak live: {} at t={}", fmt_bytes(peak), fmt_us(peak_time));
    if leak_count > 0 {
        println!("  Leaked:    {} across {leak_count} allocations (not freed by exit)", fmt_bytes(leaked));
    } else {
        println!("  Leaked:    none");
    }
    println!();

    // Largest single allocations.
    let big_sql = "SELECT address, bytes, start_us FROM allocations
                   WHERE op = 'alloc' ORDER BY bytes DESC LIMIT ?1";
    let bigs: Vec<(i64, i64, f64)> = db.query_vec(big_sql, [n as i64], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    });
    if !bigs.is_empty() {
        println!("  Largest Allocations:");
        println!("  #  Size         Start        Lifetime");
        println!("  ── ──────────── ──────────── ──────────────");
        for (i, (addr, bytes, start)) in bigs.iter().enumerate() {
            // Find this allocation's free event, if any.
            let lifetime = db.conn.query_row(
                "SELECT start_us FROM allocations
                 WHERE op = 'free' AND address = ?1 AND start_us > ?2
                 ORDER BY start_us LIMIT 1",
                rusqlite::params![addr, start],
                |row| row.get::<_, f64>(0)
            ).ok().map(|fr| fmt_us(fr - *start)).unwrap_or_else(|| "leaked".into());
            println!("  {:<2} {:>12} {:>12} {}",
                i + 1, fmt_bytes(*bytes), fmt_us(*start), lifetime);
        }
    }

    // Lifetime stats — short-lived allocations are churn signals.
    if !alloc_lifetimes.is_empty() {
        let short_threshold = 100.0; // us
        let short_cnt = alloc_lifetimes.iter().filter(|(_, lt)| *lt < short_threshold).count();
        if short_cnt > 10 {
            let bytes_churned: i64 = alloc_lifetimes.iter()
                .filter(|(_, lt)| *lt < short_threshold)
                .map(|(b, _)| *b).sum();
            println!("\n  Churn: {short_cnt} allocations lived < 100us ({} total) — consider a pool allocator",
                fmt_bytes(bytes_churned));
        }
    }
}

// ---------------------------------------------------------------------------
// bandwidth — per-kernel achieved memory bandwidth (requires ncu)
// ---------------------------------------------------------------------------

pub fn cmd_bandwidth(db: &GpuDb, args: &[&str]) {
    if !db.has_layer("ncu") {
        println!("no bandwidth data — need ncu layer (achieved_bandwidth_gb_s)");
        return;
    }
    let n = parse_count(args);
    let pattern = parse_pattern(args);
    let pat_clause = pattern
        .map(|p| format!(r"AND kernel_name LIKE '%{}%' ESCAPE '\'", escape_sql_like(p)))
        .unwrap_or_default();

    // Pull achieved & peak per kernel. Kernels without an achieved value are skipped.
    let sql = format!(
        "SELECT kernel_name, achieved_bandwidth_gb_s, peak_bandwidth_gb_s, boundedness
         FROM metrics
         WHERE achieved_bandwidth_gb_s IS NOT NULL {pat_clause}
         ORDER BY achieved_bandwidth_gb_s DESC"
    );
    let rows: Vec<(String, f64, Option<f64>, Option<String>)> = db.query_vec(
        &sql, [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );
    if rows.is_empty() {
        println!("no kernels have bandwidth metrics");
        return;
    }

    // Join with per-kernel total GPU time from the timeline layer, so the rank
    // column reflects how much the kernel actually cost.
    let tl = db.timeline_filter();
    let time_sql = format!(
        "SELECT kernel_name, SUM(duration_us) FROM launches WHERE {tl} GROUP BY kernel_name"
    );
    let time_rows: Vec<(String, f64)> = db.query_vec(&time_sql, [], |row| {
        Ok((row.get(0)?, row.get(1)?))
    });
    let time_of: std::collections::HashMap<String, f64> = time_rows.into_iter().collect();

    println!("  Per-kernel Memory Bandwidth:\n");
    println!("  #  Kernel                            Achieved     Peak       % peak  Bound    GPU Time");
    println!("  ── ────────────────────────────────── ──────────── ────────── ─────── ──────── ──────────");
    let shown = rows.iter().take(n);
    let mut flagged = 0usize;
    for (i, (name, ach, peak, bound)) in shown.enumerate() {
        let pct = peak.filter(|&p| p > 0.0).map(|p| ach / p * 100.0);
        let pct_str = pct.map(|v| format!("{v:.1}%")).unwrap_or_else(|| "?".into());
        let peak_str = peak.map(|v| format!("{v:.1}")).unwrap_or_else(|| "?".into());
        let gpu_us = time_of.get(name).copied().unwrap_or(0.0);
        let flag = match pct {
            Some(v) if v < 50.0 => { flagged += 1; " ←low" }
            _ => "",
        };
        println!("  {:<2} {:<34} {:>9.1} GB/s {:>6} GB/s {:>6}  {:<8} {:>10}{flag}",
            i + 1, trunc(name, 34), ach, peak_str, pct_str,
            bound.as_deref().unwrap_or("?"), fmt_us(gpu_us));
    }
    if flagged > 0 {
        println!("\n  {flagged} kernel(s) under 50% of peak bandwidth — likely memory-access bound");
        println!("  (poor coalescing, low L2 hit rate, or uncoalesced strided loads)");
    }
}

// ---------------------------------------------------------------------------
// critical-path — longest same-stream kernel chain (sequential dependency)
// ---------------------------------------------------------------------------

pub fn cmd_critical_path(db: &GpuDb, args: &[&str]) {
    if !db.has_layer("nsys") && !db.has_layer("torch") {
        println!("no timeline data — need nsys or torch layer");
        return;
    }
    // Optional first arg: gap threshold in us (default 100us).
    let gap_thresh: f64 = args.first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100.0);

    let tl = db.timeline_filter();
    let sql = format!(
        "SELECT kernel_name, start_us, duration_us, stream_id
         FROM launches
         WHERE start_us IS NOT NULL AND stream_id IS NOT NULL AND {tl}
         ORDER BY stream_id, start_us"
    );
    let rows: Vec<(String, f64, f64, u32)> = db.query_vec(&sql, [], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    });
    if rows.is_empty() {
        println!("no stream-tagged launches available");
        return;
    }

    // Build chains per stream: split whenever gap-to-previous > threshold.
    struct Chain {
        stream: u32,
        start: f64,
        end: f64,
        kernel_time: f64,
        kernels: Vec<(String, f64)>, // (name, duration_us)
    }
    let mut chains: Vec<Chain> = Vec::new();
    let mut cur: Option<Chain> = None;
    for (name, start, dur, stream) in &rows {
        let end = start + dur;
        let extend = cur.as_ref().is_some_and(|c| {
            c.stream == *stream && start - c.end <= gap_thresh
        });
        if !extend {
            if let Some(c) = cur.take() { chains.push(c); }
            cur = Some(Chain {
                stream: *stream, start: *start, end,
                kernel_time: *dur, kernels: vec![(name.clone(), *dur)],
            });
        } else if let Some(c) = cur.as_mut() {
            c.end = end;
            c.kernel_time += dur;
            c.kernels.push((name.clone(), *dur));
        }
    }
    if let Some(c) = cur.take() { chains.push(c); }

    // Rank by span (end - start): that is the critical-path wall time this chain
    // occupies on its stream.  Tie-break on kernel_time (active work).
    chains.sort_by(|a, b| {
        let sa = a.end - a.start;
        let sb = b.end - b.start;
        sb.partial_cmp(&sa).unwrap()
            .then_with(|| b.kernel_time.partial_cmp(&a.kernel_time).unwrap())
    });

    println!("  Critical path chains (same stream, gap ≤ {}):\n", fmt_us(gap_thresh));
    // Defensive: rows.is_empty() returns early above, so chains has ≥1 entry.
    // Guard anyway to decouple from that invariant.
    let Some(best) = chains.first() else {
        println!("  (no chains to report)");
        return;
    };
    let best_span = best.end - best.start;
    let utilization = if best_span > 0.0 { best.kernel_time / best_span * 100.0 } else { 0.0 };
    println!("  Longest chain: stream {}  span {}  active {} ({utilization:.0}%)  {} kernel(s)",
        best.stream, fmt_us(best_span), fmt_us(best.kernel_time), best.kernels.len());

    // Aggregate kernels within the best chain by name.
    let mut agg: std::collections::HashMap<&str, (usize, f64)> = std::collections::HashMap::new();
    for (name, dur) in &best.kernels {
        let e = agg.entry(name.as_str()).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += dur;
    }
    let mut ranked: Vec<_> = agg.into_iter().collect();
    ranked.sort_by(|a, b| b.1.1.partial_cmp(&a.1.1).unwrap());
    println!("\n  Top kernels on chain:");
    println!("  Kernel                                     Launches  Time       % chain");
    println!("  ────────────────────────────────────────── ──────── ────────── ────────");
    for (name, (cnt, total)) in ranked.iter().take(8) {
        let pct = if best.kernel_time > 0.0 { total / best.kernel_time * 100.0 } else { 0.0 };
        println!("  {:<42} {:>8} {:>10} {:>6.1}%",
            trunc(name, 42), cnt, fmt_us(*total), pct);
    }

    // Report next few chains for contrast.
    if chains.len() > 1 {
        println!("\n  Other long chains:");
        println!("  #  Stream  Span        Active      Util   Kernels");
        println!("  ── ─────── ─────────── ─────────── ────── ────────");
        for (i, c) in chains.iter().skip(1).take(5).enumerate() {
            let span = c.end - c.start;
            let util = if span > 0.0 { c.kernel_time / span * 100.0 } else { 0.0 };
            println!("  {:<2} {:>7} {:>11} {:>11} {:>5.0}% {:>7}",
                i + 2, c.stream, fmt_us(span), fmt_us(c.kernel_time), util, c.kernels.len());
        }
    }

    let wall_us: f64 = db.meta("wall_time_us").parse().unwrap_or(0.0);
    if wall_us > 0.0 {
        println!("\n  Chain wall fraction: {:.1}% of wall time ({})",
            best_span / wall_us * 100.0, fmt_us(wall_us));
    }
}

// ---------------------------------------------------------------------------
// stream-graph — ASCII timeline with streams as rows
// ---------------------------------------------------------------------------

pub fn cmd_stream_graph(db: &GpuDb, args: &[&str]) {
    let width: usize = args.first()
        .and_then(|s| s.parse().ok())
        .filter(|&w: &usize| w >= 20 && w <= 500)
        .unwrap_or(100);

    let tl = db.timeline_filter();
    let sql = format!(
        "SELECT kernel_name, start_us, duration_us, stream_id
         FROM launches
         WHERE start_us IS NOT NULL AND stream_id IS NOT NULL AND {tl}
         ORDER BY stream_id, start_us"
    );
    let rows: Vec<(String, f64, f64, u32)> = db.query_vec(&sql, [], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    });
    if rows.is_empty() {
        println!("no timeline data");
        return;
    }

    let t_min = rows.iter().map(|r| r.1).fold(f64::INFINITY, f64::min);
    let t_max = rows.iter().map(|r| r.1 + r.2).fold(f64::NEG_INFINITY, f64::max);
    let span = t_max - t_min;
    if span <= 0.0 { println!("timeline has zero span"); return; }

    // Group by stream. Order streams by their first-launch time so reading
    // the graph top-to-bottom matches chronological launch order.
    use std::collections::BTreeMap;
    let mut by_stream: BTreeMap<u32, Vec<(String, f64, f64)>> = BTreeMap::new();
    for (name, start, dur, stream) in &rows {
        by_stream.entry(*stream).or_default().push((name.clone(), *start, *dur));
    }

    // Intern kernels to single-char glyphs, ordered by total time (highest gets 'A').
    let mut kernel_time: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for (name, _, dur, _) in &rows {
        *kernel_time.entry(name.clone()).or_insert(0.0) += dur;
    }
    let mut kernel_rank: Vec<(String, f64)> = kernel_time.into_iter().collect();
    kernel_rank.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    // Glyphs: A-Z, then a-z, then digits. After that, reuse '*' for the tail.
    let glyphs: Vec<char> = ('A'..='Z').chain('a'..='z').chain('0'..='9').collect();
    let glyph_of: std::collections::HashMap<String, char> = kernel_rank.iter().enumerate()
        .map(|(i, (name, _))| {
            let g = if i < glyphs.len() { glyphs[i] } else { '*' };
            (name.clone(), g)
        })
        .collect();

    println!("  Stream Graph ({} → {}, span {})\n",
        fmt_us(t_min), fmt_us(t_max), fmt_us(span));

    for (stream, launches) in &by_stream {
        let mut line = vec![' '; width];
        for (name, start, dur) in launches {
            let s = ((*start - t_min) / span * width as f64).floor() as usize;
            let e_raw = ((*start + *dur - t_min) / span * width as f64).ceil() as usize;
            let s = s.min(width - 1);
            let e = e_raw.clamp(s + 1, width);
            let g = glyph_of.get(name).copied().unwrap_or('?');
            for cell in line.iter_mut().take(e).skip(s) {
                *cell = g;
            }
        }
        let row: String = line.into_iter().collect();
        println!("  s{:<4} │{row}│", stream);
    }
    // Time axis underline.
    let axis: String = "─".repeat(width);
    println!("        └{axis}┘");

    // Legend (top-N most time-consuming kernels).
    println!("\n  Legend:");
    for (i, (name, total)) in kernel_rank.iter().take(glyphs.len().min(20)).enumerate() {
        let g = glyphs.get(i).copied().unwrap_or('*');
        println!("    {g}  {:<50} {}", trunc(name, 50), fmt_us(*total));
    }
    if kernel_rank.len() > 20 {
        println!("    ({} more kernels not shown)", kernel_rank.len() - 20);
    }
}

// ---------------------------------------------------------------------------
// hotspot — hottest N-microsecond window in the timeline
// ---------------------------------------------------------------------------

/// Find the window of width `window_us` that maximizes total busy kernel-time.
///
/// `intervals` are `(start_us, duration_us)` pairs pre-sorted by `start_us`.
/// Busy time sums contributions across all streams, so a 100us window with two
/// fully-overlapping launches reports 200us of busy time.
///
/// The busy function f(w) = Σ max(0, min(eᵢ, w+W) − max(sᵢ, w)) is piecewise
/// linear; its breakpoints lie at {sᵢ} and {eᵢ − W}. We evaluate f at every
/// breakpoint and return the best. A start-only sweep would miss the peak when
/// overlapping launches on different streams align mid-way between starts.
///
/// Returns `(busy_us, window_start_us, lo_idx, hi_idx)`: indices bracket the
/// launches that intersect the best window (`intervals[lo..hi]`).
pub(crate) fn find_hottest_window(
    intervals: &[(f64, f64)],
    window_us: f64,
) -> (f64, f64, usize, usize) {
    let n = intervals.len();
    if n == 0 || window_us <= 0.0 { return (0.0, 0.0, 0, 0); }

    let mut candidates: Vec<f64> = Vec::with_capacity(2 * n);
    for &(s, d) in intervals {
        candidates.push(s);
        candidates.push(s + d - window_us);
    }
    candidates.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let mut best = (0.0_f64, 0.0_f64, 0usize, 0usize);
    let mut lo = 0usize;
    for &w_start in &candidates {
        let w_end = w_start + window_us;
        while lo < n && intervals[lo].0 + intervals[lo].1 <= w_start { lo += 1; }
        let mut busy = 0.0_f64;
        let mut hi_scan = lo;
        while hi_scan < n && intervals[hi_scan].0 < w_end {
            let (s, d) = intervals[hi_scan];
            let e = s + d;
            let os = s.max(w_start);
            let oe = e.min(w_end);
            if os < oe { busy += oe - os; }
            hi_scan += 1;
        }
        if busy > best.0 {
            best = (busy, w_start, lo, hi_scan);
        }
    }
    best
}

pub fn cmd_hotspot(db: &GpuDb, args: &[&str]) {
    let window_us: f64 = match args.first().and_then(|s| s.parse::<f64>().ok()) {
        Some(v) if v > 0.0 => v,
        _ => { println!("usage: hotspot <window_us>  (e.g. 10000 for 10ms)"); return; }
    };
    let tl = db.timeline_filter();
    let sql = format!(
        "SELECT kernel_name, start_us, duration_us
         FROM launches
         WHERE start_us IS NOT NULL AND {tl}
         ORDER BY start_us"
    );
    let rows: Vec<(String, f64, f64)> = db.query_vec(&sql, [], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    });
    if rows.is_empty() { println!("no timeline data"); return; }

    let intervals: Vec<(f64, f64)> = rows.iter().map(|(_, s, d)| (*s, *d)).collect();
    let (busy, w_start, lo, hi_end) = find_hottest_window(&intervals, window_us);
    if busy == 0.0 {
        println!("no activity found in any window");
        return;
    }
    let w_end = w_start + window_us;
    let util = busy / window_us * 100.0;
    println!("  Hottest {} window:\n", fmt_us(window_us));
    println!("  Window:     {} → {}", fmt_us(w_start), fmt_us(w_end));
    println!("  Busy time:  {}  ({util:.1}% of window)", fmt_us(busy));
    println!("  Launches:   {}", hi_end - lo);

    // Aggregate kernels intersecting the best window by name.
    let mut agg: std::collections::HashMap<&str, (usize, f64)> = std::collections::HashMap::new();
    for (name, s, d) in rows.iter().take(hi_end).skip(lo) {
        let end = s + d;
        let os = s.max(w_start);
        let oe = end.min(w_end);
        if os < oe {
            let e = agg.entry(name.as_str()).or_insert((0, 0.0));
            e.0 += 1;
            e.1 += oe - os;
        }
    }
    let mut ranked: Vec<_> = agg.into_iter().collect();
    ranked.sort_by(|a, b| b.1.1.partial_cmp(&a.1.1).unwrap());

    println!("\n  Kernel                                     Launches  Time in window  % busy");
    println!("  ────────────────────────────────────────── ──────── ─────────────── ───────");
    for (name, (cnt, t)) in ranked.iter().take(15) {
        let pct = if busy > 0.0 { t / busy * 100.0 } else { 0.0 };
        println!("  {:<42} {:>8} {:>15} {:>6.1}%",
            trunc(name, 42), cnt, fmt_us(*t), pct);
    }
}

// ---------------------------------------------------------------------------
// launches — every launch of one kernel with timestamps + gap-to-previous
// ---------------------------------------------------------------------------

pub fn cmd_launches(db: &GpuDb, args: &[&str]) {
    let pattern = match args.first() {
        Some(p) => *p,
        None => { println!("usage: launches <kernel_pattern> [limit]"); return; }
    };
    let limit: usize = args.get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    let tl = db.timeline_filter();
    // Resolve pattern to the single best kernel match (most launches).
    let resolve_sql = format!(
        r"SELECT kernel_name, COUNT(*) FROM launches
          WHERE kernel_name LIKE ?1 ESCAPE '\' AND {tl}
          GROUP BY kernel_name ORDER BY COUNT(*) DESC LIMIT 1"
    );
    let (name, cnt) = match db.conn.query_row(
        &resolve_sql, [like_param(pattern)],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    ) {
        Ok(x) => x,
        Err(_) => { println!("no kernel matching '{pattern}'"); return; }
    };

    let sql = format!(
        "SELECT start_us, duration_us, grid_x, grid_y, grid_z,
                block_x, block_y, block_z, stream_id
         FROM launches
         WHERE kernel_name = ?1 AND start_us IS NOT NULL AND {tl}
         ORDER BY start_us LIMIT ?2"
    );
    let rows: Vec<(f64, f64, Option<u32>, Option<u32>, Option<u32>,
                   Option<u32>, Option<u32>, Option<u32>, Option<u32>)> = db.query_vec(
        &sql, rusqlite::params![name, limit as i64],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?,
                  row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?)),
    );

    println!("  Launches of {} ({} total, showing {})\n",
        trunc(&name, 70), cnt, rows.len());
    println!("  #    Start         Duration    Gap        Stream  Grid            Block");
    println!("  ──── ───────────── ─────────── ────────── ─────── ─────────────── ───────────────");
    let mut prev_end: Option<f64> = None;
    for (i, (start, dur, gx, gy, gz, bx, by, bz, sid)) in rows.iter().enumerate() {
        let gap = prev_end.map(|e| start - e);
        let gap_s = gap.map(|g| if g >= 0.0 { fmt_us(g) } else { format!("-{}", fmt_us(-g)) })
            .unwrap_or_else(|| "—".into());
        let grid = match (gx, gy, gz) {
            (Some(x), Some(y), Some(z)) => format!("({x},{y},{z})"),
            _ => "—".into(),
        };
        let block = match (bx, by, bz) {
            (Some(x), Some(y), Some(z)) => format!("({x},{y},{z})"),
            _ => "—".into(),
        };
        let sid_s = sid.map(|s| s.to_string()).unwrap_or_else(|| "?".into());
        println!("  {:<4} {:>13} {:>11} {:>10} {:>7} {:<15} {:<15}",
            i + 1, fmt_us(*start), fmt_us(*dur), gap_s, sid_s,
            trunc(&grid, 15), trunc(&block, 15));
        prev_end = Some(start + dur);
    }

    // Summary stats across the fetched launches.
    if rows.len() >= 2 {
        let gaps: Vec<f64> = rows.windows(2)
            .map(|w| w[1].0 - (w[0].0 + w[0].1))
            .filter(|g| *g >= 0.0)
            .collect();
        if !gaps.is_empty() {
            let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
            let min = gaps.iter().cloned().fold(f64::INFINITY, f64::min);
            let max = gaps.iter().cloned().fold(0.0_f64, f64::max);
            println!("\n  Gap between consecutive launches: mean {}, min {}, max {}",
                fmt_us(mean), fmt_us(min), fmt_us(max));
        }
    }
}

// ---------------------------------------------------------------------------
// compare — side-by-side stats for two kernels
// ---------------------------------------------------------------------------

pub fn cmd_compare(db: &GpuDb, args: &[&str]) {
    if args.len() < 2 {
        println!("usage: compare <kernel_a> <kernel_b>");
        return;
    }
    let tl = db.timeline_filter();

    let resolve = |pattern: &str| -> Option<(String, i64, f64, f64, f64, f64, f64)> {
        let sql = format!(
            r"SELECT kernel_name,
                     COUNT(*),
                     AVG(duration_us),
                     MIN(duration_us),
                     MAX(duration_us),
                     SUM(duration_us),
                     AVG(duration_us * duration_us) - AVG(duration_us) * AVG(duration_us)
              FROM launches
              WHERE kernel_name LIKE ?1 ESCAPE '\' AND {tl}
              GROUP BY kernel_name
              ORDER BY SUM(duration_us) DESC LIMIT 1"
        );
        db.conn.query_row(&sql, [like_param(pattern)], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?,
                row.get(4)?, row.get(5)?, row.get(6)?))
        }).ok()
    };
    let a = match resolve(args[0]) {
        Some(v) => v,
        None => { println!("no kernel matching '{}'", args[0]); return; }
    };
    let b = match resolve(args[1]) {
        Some(v) => v,
        None => { println!("no kernel matching '{}'", args[1]); return; }
    };
    if a.0 == b.0 {
        println!("both patterns resolved to the same kernel: {}", a.0);
        return;
    }

    // Optional ncu metrics per kernel.
    let metrics_of = |name: &str| -> Option<(Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<String>)> {
        db.conn.query_row(
            "SELECT occupancy_pct, compute_throughput_pct, memory_throughput_pct,
                    achieved_bandwidth_gb_s, boundedness
             FROM metrics WHERE kernel_name = ?1",
            [name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        ).ok()
    };
    let ma = metrics_of(&a.0);
    let mb = metrics_of(&b.0);

    let stddev = |var: f64| var.max(0.0).sqrt();
    let cv = |mean: f64, var: f64| if mean > 0.0 { stddev(var) / mean } else { 0.0 };

    println!("  Kernel A: {}", trunc(&a.0, 70));
    println!("  Kernel B: {}\n", trunc(&b.0, 70));
    println!("  Metric            A                 B                 Ratio B/A");
    println!("  ────────────────  ────────────────  ────────────────  ──────────");
    let row = |label: &str, va: String, vb: String, ratio: Option<f64>| {
        let r = ratio.map(|r| format!("{r:.2}x")).unwrap_or_else(|| "—".into());
        println!("  {:<16}  {:<16}  {:<16}  {r}", label, va, vb);
    };
    row("Launches", a.1.to_string(), b.1.to_string(),
        if a.1 > 0 { Some(b.1 as f64 / a.1 as f64) } else { None });
    row("Total time", fmt_us(a.5), fmt_us(b.5),
        if a.5 > 0.0 { Some(b.5 / a.5) } else { None });
    row("Mean", fmt_us(a.2), fmt_us(b.2),
        if a.2 > 0.0 { Some(b.2 / a.2) } else { None });
    row("Min", fmt_us(a.3), fmt_us(b.3), None);
    row("Max", fmt_us(a.4), fmt_us(b.4), None);
    row("Stddev",
        fmt_us(stddev(a.6)), fmt_us(stddev(b.6)), None);
    row("CV",
        format!("{:.3}", cv(a.2, a.6)),
        format!("{:.3}", cv(b.2, b.6)), None);

    if ma.is_some() || mb.is_some() {
        println!("\n  Hardware metrics (ncu):");
        let fmt_opt_pct = |v: Option<f64>| v.map(|x| format!("{x:.1}%")).unwrap_or_else(|| "?".into());
        let fmt_opt_bw = |v: Option<f64>| v.map(|x| format!("{x:.1} GB/s")).unwrap_or_else(|| "?".into());
        let fmt_opt_s = |v: Option<String>| v.unwrap_or_else(|| "?".into());
        let (oa, ca, mma, ba, bda) = ma.unwrap_or((None, None, None, None, None));
        let (ob, cb, mmb, bb, bdb) = mb.unwrap_or((None, None, None, None, None));
        println!("  Occupancy        {:<16}  {:<16}", fmt_opt_pct(oa), fmt_opt_pct(ob));
        println!("  Compute tput     {:<16}  {:<16}", fmt_opt_pct(ca), fmt_opt_pct(cb));
        println!("  Memory tput      {:<16}  {:<16}", fmt_opt_pct(mma), fmt_opt_pct(mmb));
        println!("  Bandwidth        {:<16}  {:<16}", fmt_opt_bw(ba), fmt_opt_bw(bb));
        println!("  Boundedness      {:<16}  {:<16}", fmt_opt_s(bda), fmt_opt_s(bdb));
    }
}

// ---------------------------------------------------------------------------
// regressions — like diff, but filtered by noise threshold
// ---------------------------------------------------------------------------

pub fn cmd_regressions(db: &GpuDb, args: &[&str]) {
    let name = match args.first() {
        Some(n) => *n,
        None => { println!("usage: regressions <saved_session> [pct=5] [min_us=10]"); return; }
    };
    let pct_thresh: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5.0);
    let abs_thresh_us: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10.0);

    let other_path = if name.ends_with(".gpu.db") || name.contains('/') {
        PathBuf::from(name)
    } else {
        GpuDb::session_dir().join(format!("{name}.gpu.db"))
    };
    // SQLite's ATTACH creates an empty DB at missing paths; guard first so
    // we fail loudly instead of silently creating junk and reporting a
    // spurious all-new-kernels diff.
    if !other_path.exists() {
        println!("cannot load '{name}': no such session at {}", other_path.display());
        return;
    }
    if let Err(e) = db.attach(other_path.to_str().unwrap_or(""), "other") {
        println!("cannot load '{name}': {e}");
        return;
    }

    // Pull per-kernel totals from both sides, joined by name.
    let sql = "SELECT COALESCE(c.kernel_name, o.kernel_name),
                      COALESCE(o.total, 0), COALESCE(c.total, 0),
                      COALESCE(o.cnt,   0), COALESCE(c.cnt,   0)
               FROM
                 (SELECT kernel_name, SUM(duration_us) AS total, COUNT(*) AS cnt
                  FROM launches GROUP BY kernel_name) c
               FULL OUTER JOIN
                 (SELECT kernel_name, SUM(duration_us) AS total, COUNT(*) AS cnt
                  FROM other.launches GROUP BY kernel_name) o
               ON c.kernel_name = o.kernel_name";
    let all: Vec<(String, f64, f64, i64, i64)> = db.query_vec(sql, [], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
    });

    // Partition into regressions (slower now), improvements (faster), new, gone.
    struct Change { name: String, before: f64, after: f64, delta_us: f64, delta_pct: f64, before_cnt: i64, after_cnt: i64 }
    let mut regressions: Vec<Change> = Vec::new();
    let mut improvements: Vec<Change> = Vec::new();
    let mut new_kernels: Vec<(String, f64, i64)> = Vec::new();
    let mut gone_kernels: Vec<(String, f64, i64)> = Vec::new();

    for (kname, before, after, bc, ac) in all {
        if before <= 0.0 && after > 0.0 {
            new_kernels.push((kname, after, ac));
            continue;
        }
        if after <= 0.0 && before > 0.0 {
            gone_kernels.push((kname, before, bc));
            continue;
        }
        let delta = after - before;
        if delta.abs() < abs_thresh_us { continue; }
        let pct = if before > 0.0 { delta / before * 100.0 } else { 0.0 };
        if pct.abs() < pct_thresh { continue; }
        let ch = Change {
            name: kname, before, after,
            delta_us: delta, delta_pct: pct,
            before_cnt: bc, after_cnt: ac,
        };
        if delta > 0.0 { regressions.push(ch); } else { improvements.push(ch); }
    }
    regressions.sort_by(|a, b| b.delta_us.partial_cmp(&a.delta_us).unwrap());
    improvements.sort_by(|a, b| a.delta_us.partial_cmp(&b.delta_us).unwrap());

    println!("  Regressions vs {name}   (threshold: ≥{pct_thresh}% AND ≥{abs_thresh_us}us)\n");
    let print_changes = |label: &str, v: &[Change]| {
        if v.is_empty() { return; }
        println!("  {label} ({})", v.len());
        println!("  Kernel                                     Before      After       Delta        %       Launches");
        println!("  ────────────────────────────────────────── ─────────── ─────────── ──────────── ──────── ─────────");
        for c in v.iter().take(15) {
            let sign = if c.delta_us >= 0.0 { "+" } else { "" };
            let launches = if c.before_cnt == c.after_cnt {
                format!("{}", c.after_cnt)
            } else {
                format!("{}→{}", c.before_cnt, c.after_cnt)
            };
            println!("  {:<42} {:>11} {:>11} {:>11} {sign}{:>6.1}% {:>9}",
                trunc(&c.name, 42), fmt_us(c.before), fmt_us(c.after),
                fmt_us(c.delta_us.abs()), c.delta_pct, launches);
        }
        println!();
    };
    print_changes("SLOWER", &regressions);
    print_changes("FASTER", &improvements);

    if !new_kernels.is_empty() {
        println!("  NEW kernels in current run ({}):", new_kernels.len());
        new_kernels.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        for (n, t, c) in new_kernels.iter().take(10) {
            println!("    + {:<50} {} ({} launches)", trunc(n, 50), fmt_us(*t), c);
        }
        println!();
    }
    if !gone_kernels.is_empty() {
        println!("  GONE from current run ({}):", gone_kernels.len());
        gone_kernels.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        for (n, t, c) in gone_kernels.iter().take(10) {
            println!("    - {:<50} {} ({} launches)", trunc(n, 50), fmt_us(*t), c);
        }
        println!();
    }

    let net_delta: f64 = regressions.iter().map(|c| c.delta_us).sum::<f64>()
        + improvements.iter().map(|c| c.delta_us).sum::<f64>();
    let sign = if net_delta >= 0.0 { "+" } else { "-" };
    println!("  Net change on filtered kernels: {sign}{} ({} regressions, {} improvements)",
        fmt_us(net_delta.abs()), regressions.len(), improvements.len());

    let _ = db.detach("other");
}
