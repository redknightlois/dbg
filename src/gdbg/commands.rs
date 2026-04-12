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

/// Compute true GPU idle gaps by merging overlapping launch intervals across streams.
/// Returns (gap_start, gap_duration) pairs sorted by start time.
fn compute_gpu_gaps(db: &GpuDb) -> Vec<(f64, f64)> {
    let tl = db.timeline_filter();
    let sql = format!(
        "SELECT start_us, start_us + duration_us AS end_us
         FROM launches WHERE start_us IS NOT NULL AND {tl}
         ORDER BY start_us"
    );
    let intervals: Vec<(f64, f64)> = db.query_vec(&sql, [], |row| {
        Ok((row.get::<_,f64>(0)?, row.get::<_,f64>(1)?))
    });

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

    println!("GPU Profile Summary");
    println!("  Target:       {target}");
    if !device.is_empty() { println!("  Device:       {device}"); }
    println!("  Wall time:    {}", fmt_us(wall_us));
    println!("  GPU active:   {} ({:.1}%)", fmt_us(gpu_us),
        if wall_us > 0.0 { gpu_us / wall_us * 100.0 } else { 0.0 });
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

    let pattern_clause = pattern
        .map(|p| format!("AND launches.kernel_name LIKE '%{}%'", escape_sql_like(p)))
        .unwrap_or_default();

    let sql = format!(
        "SELECT launches.kernel_name,
                COUNT(*) as cnt,
                SUM(launches.duration_us) as total,
                m.boundedness,
                m.compute_throughput_pct,
                m.memory_throughput_pct
         FROM launches
         LEFT JOIN metrics m ON m.kernel_name = launches.kernel_name
         WHERE {filter} {pattern_clause}
         GROUP BY launches.kernel_name
         ORDER BY total DESC
         LIMIT ?1"
    );

    let gpu_total = db.total_gpu_time_us();
    let mut stmt = db.conn.prepare(&sql).unwrap();
    let rows = stmt
        .query_map([n as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<f64>>(4)?,
                row.get::<_, Option<f64>>(5)?,
            ))
        })
        .unwrap();

    println!("  #  Kernel                            Time      %      Bound        Launches");
    println!("  ── ────────────────────────────────── ──────── ────── ──────────── ────────");
    for (i, row) in rows.enumerate() {
        let (name, cnt, total, bound, cmp, mem) = row.unwrap();
        let pct = if gpu_total > 0.0 { total / gpu_total * 100.0 } else { 0.0 };
        let bound_str = match bound.as_deref() {
            Some("compute") => format!("cmp {:.0}%", cmp.unwrap_or(0.0)),
            Some("memory") => format!("mem {:.0}%", mem.unwrap_or(0.0)),
            Some("latency") => "latency".into(),
            _ => "[no ncu]".into(),
        };
        println!("  {:<2} {:<34} {:>8} {:>5.1}%  {:<12} {:>7}",
            i + 1, trunc(&name, 34), fmt_us(total), pct, bound_str, cnt);
    }
}

// ---------------------------------------------------------------------------
// ops
// ---------------------------------------------------------------------------

pub fn cmd_ops(db: &GpuDb, args: &[&str]) {
    if !require_op_layer(db) { return; }
    let n = parse_count(args);
    let pattern = parse_pattern(args);
    let pattern_clause = pattern
        .map(|p| format!("AND name LIKE '%{}%'", escape_sql_like(p)))
        .unwrap_or_default();

    let sql = format!(
        "SELECT name, module_path, cpu_time_us, gpu_time_us, input_shapes
         FROM ops WHERE 1=1 {pattern_clause}
         ORDER BY cpu_time_us DESC LIMIT ?1"
    );

    let mut stmt = db.conn.prepare(&sql).unwrap();
    let rows = stmt.query_map([n as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, f64>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    }).unwrap();

    println!("  #  Op                               CPU Time    Module");
    println!("  ── ───────────────────────────────── ────────── ────────────");
    for (i, row) in rows.enumerate() {
        let (name, module, cpu_time, _, _) = row.unwrap();
        println!("  {:<2} {:<34} {:>9}  {}",
            i + 1, trunc(&name, 34), fmt_us(cpu_time),
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

    // Get kernel aggregate
    let sql = "SELECT kernel_name, COUNT(*), SUM(duration_us), AVG(duration_us),
                      MIN(duration_us), MAX(duration_us)
               FROM launches WHERE kernel_name LIKE ?1
               GROUP BY kernel_name";
    let mut stmt = db.conn.prepare(sql).unwrap();
    let mut rows = stmt.query_map([like_param(pattern)], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, f64>(3)?,
            row.get::<_, f64>(4)?,
            row.get::<_, f64>(5)?,
        ))
    }).unwrap();

    let (name, cnt, total, avg, min, max) = match rows.next() {
        Some(Ok(r)) => r,
        _ => { println!("no kernel matching '{pattern}'"); return; }
    };
    if rows.next().is_some() {
        // Multiple matches — list them
        println!("multiple matches for '{pattern}':");
        println!("  {name}");
        for row in rows { if let Ok((n,_,_,_,_,_)) = row { println!("  {n}"); } }
        println!("narrow the pattern");
        return;
    }
    drop(rows);
    drop(stmt);

    println!("Kernel: {name}");
    println!("  Launches: {cnt}");
    println!("  Total:    {}", fmt_us(total));
    println!("  Average:  {}", fmt_us(avg));
    if cnt > 1 { println!("  Min:      {}", fmt_us(min)); println!("  Max:      {}", fmt_us(max)); }

    // Launch config — most common
    let config_sql = "SELECT grid_x, grid_y, grid_z, block_x, block_y, block_z,
                             COUNT(*) as cnt
                      FROM launches WHERE kernel_name = ?1
                      AND grid_x IS NOT NULL
                      GROUP BY grid_x, grid_y, grid_z, block_x, block_y, block_z
                      ORDER BY cnt DESC LIMIT 5";
    let mut stmt = db.conn.prepare(config_sql).unwrap();
    let configs: Vec<_> = stmt.query_map([&name], |row| {
        Ok((row.get::<_,u32>(0)?, row.get::<_,u32>(1)?, row.get::<_,u32>(2)?,
            row.get::<_,u32>(3)?, row.get::<_,u32>(4)?, row.get::<_,u32>(5)?,
            row.get::<_,i64>(6)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

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
    let mut stmt = db.conn.prepare(op_sql).unwrap();
    let ops: Vec<_> = stmt.query_map([&name], |row| {
        Ok((row.get::<_,String>(0)?, row.get::<_,Option<String>>(1)?, row.get::<_,Option<String>>(2)?))
    }).unwrap().filter_map(|r| r.ok()).collect();
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
               FROM metrics m WHERE m.kernel_name LIKE ?1";
    let mut stmt = db.conn.prepare(sql).unwrap();
    let rows: Vec<_> = stmt.query_map([like_param(pattern)], |row| {
        Ok((
            row.get::<_,String>(0)?, row.get::<_,Option<String>>(1)?,
            row.get::<_,Option<f64>>(2)?, row.get::<_,Option<f64>>(3)?,
            row.get::<_,Option<f64>>(4)?, row.get::<_,Option<f64>>(5)?,
            row.get::<_,Option<f64>>(6)?, row.get::<_,Option<f64>>(7)?,
        ))
    }).unwrap().filter_map(|r| r.ok()).collect();

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
    let pat = pattern.map(|p| like_param(p)).unwrap_or_else(|| "%".into());

    let sql = "SELECT kernel_name, boundedness, compute_throughput_pct,
                      memory_throughput_pct, occupancy_pct
               FROM metrics WHERE kernel_name LIKE ?1
               ORDER BY kernel_name";
    let mut stmt = db.conn.prepare(sql).unwrap();
    let rows: Vec<_> = stmt.query_map([&pat], |row| {
        Ok((row.get::<_,String>(0)?, row.get::<_,Option<String>>(1)?,
            row.get::<_,Option<f64>>(2)?, row.get::<_,Option<f64>>(3)?,
            row.get::<_,Option<f64>>(4)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

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
    let mut stmt = db.conn.prepare(sql).unwrap();
    let rows: Vec<_> = stmt.query_map([n as i64], |row| {
        Ok((row.get::<_,String>(0)?, row.get::<_,f64>(1)?,
            row.get::<_,Option<i64>>(2)?, row.get::<_,Option<i64>>(3)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

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

    // Summary
    let (total_bytes, total_time): (i64, f64) = db.conn.query_row(
        "SELECT COALESCE(SUM(bytes),0), COALESCE(SUM(duration_us),0) FROM transfers",
        [], |row| Ok((row.get(0)?, row.get(1)?))
    ).unwrap();
    println!("  Total: {} transfers, {}, {}\n",
        db.transfer_count(), fmt_bytes(total_bytes), fmt_us(total_time));

    let sql = "SELECT kind, bytes, duration_us, stream_id
               FROM transfers ORDER BY duration_us DESC LIMIT ?1";
    let mut stmt = db.conn.prepare(sql).unwrap();
    let rows: Vec<_> = stmt.query_map([n as i64], |row| {
        Ok((row.get::<_,String>(0)?, row.get::<_,i64>(1)?,
            row.get::<_,f64>(2)?, row.get::<_,Option<u32>>(3)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

    println!("  #  Kind  Size        Duration    BW          Stream");
    println!("  ── ───── ────────── ────────── ──────────── ──────");
    for (i, (kind, bytes, dur, sid)) in rows.iter().enumerate() {
        let bw = if *dur > 0.0 { format!("{:.1} GB/s", *bytes as f64 / dur / 1000.0) }
            else { "?".into() };
        println!("  {:<2} {:<5} {:>10} {:>10} {:>11} {:>6}",
            i+1, kind, fmt_bytes(*bytes), fmt_us(*dur), bw,
            sid.map(|s| s.to_string()).unwrap_or_else(|| "?".into()));
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
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    rows.truncate(n);

    if rows.is_empty() { println!("no GPU idle gaps detected"); return; }

    let total_gap: f64 = rows.iter().map(|r| r.1).sum();
    println!("  Top {n} GPU idle gaps (total idle: {})\n", fmt_us(total_gap));
    println!("  #  Start        Duration");
    println!("  ── ──────────── ────────────");
    for (i, (start, dur)) in rows.iter().enumerate() {
        println!("  {:<2} {:>12} {:>12}", i+1, fmt_us(*start), fmt_us(*dur));
    }
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

    println!("  Compute/Transfer Overlap:");
    println!("    GPU kernel time:   {}", fmt_us(gpu_us));
    println!("    Transfer time:     {}", fmt_us(xfer_time));
    if wall_us > 0.0 {
        println!("    GPU utilization:   {:.1}%", gpu_us / wall_us * 100.0);
    }
}

// ---------------------------------------------------------------------------
// streams
// ---------------------------------------------------------------------------

pub fn cmd_streams(db: &GpuDb) {
    let sql = "SELECT stream_id, COUNT(*) as cnt, SUM(duration_us) as total
               FROM launches WHERE stream_id IS NOT NULL
               GROUP BY stream_id ORDER BY total DESC";
    let mut stmt = db.conn.prepare(sql).unwrap();
    let rows: Vec<_> = stmt.query_map([], |row| {
        Ok((row.get::<_,u32>(0)?, row.get::<_,i64>(1)?, row.get::<_,f64>(2)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

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
    let mut stmt = db.conn.prepare(&sql).unwrap();
    let rows: Vec<_> = stmt.query_map([n as i64], |row| {
        Ok((row.get::<_,String>(0)?, row.get::<_,f64>(1)?,
            row.get::<_,f64>(2)?, row.get::<_,Option<u32>>(3)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

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

    let sql = "SELECT id, name, module_path, cpu_time_us, input_shapes
               FROM ops WHERE name LIKE ?1";
    let mut stmt = db.conn.prepare(sql).unwrap();
    let ops: Vec<_> = stmt.query_map([like_param(pattern)], |row| {
        Ok((row.get::<_,i64>(0)?, row.get::<_,String>(1)?,
            row.get::<_,Option<String>>(2)?, row.get::<_,f64>(3)?,
            row.get::<_,Option<String>>(4)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

    if ops.is_empty() { println!("no op matching '{pattern}'"); return; }

    for (op_id, name, module, cpu_time, shapes) in &ops {
        println!("Op: {name}");
        if let Some(m) = module { println!("  Module: {m}"); }
        if let Some(s) = shapes { println!("  Shapes: {s}"); }
        println!("  CPU: {}", fmt_us(*cpu_time));
        // Find linked kernels
        let k_sql = "SELECT kernel_name FROM op_kernel_map WHERE op_id = ?1";
        let mut k_stmt = db.conn.prepare(k_sql).unwrap();
        let kernels: Vec<String> = k_stmt.query_map([op_id], |row| row.get(0))
            .unwrap().filter_map(|r| r.ok()).collect();
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

    let sql = "SELECT DISTINCT o.name, o.module_path
               FROM op_kernel_map okm JOIN ops o ON o.id = okm.op_id
               WHERE okm.kernel_name LIKE ?1";
    let mut stmt = db.conn.prepare(sql).unwrap();
    let rows: Vec<_> = stmt.query_map([like_param(pattern)], |row| {
        Ok((row.get::<_,String>(0)?, row.get::<_,Option<String>>(1)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

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
    let mut stmt = db.conn.prepare(sql).unwrap();
    let rows: Vec<_> = stmt.query_map([], |row| {
        Ok((row.get::<_,i64>(0)?, row.get::<_,String>(1)?,
            row.get::<_,String>(2)?, row.get::<_,String>(3)?,
            row.get::<_,Option<f64>>(4)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

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
        let top_sql = "SELECT kernel_name, SUM(duration_us) as total
                       FROM launches GROUP BY kernel_name ORDER BY total DESC LIMIT 5";
        let mut stmt = db.conn.prepare(top_sql).unwrap();
        let top: Vec<(String, f64)> = stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?))
        }).unwrap().filter_map(|r| r.ok()).collect();

        if !top.is_empty() {
            let gpu_total = db.total_gpu_time_us();
            let pct: f64 = top.iter().map(|t| if gpu_total > 0.0 { t.1 / gpu_total * 100.0 } else { 0.0 }).sum();
            let regex = top.iter().map(|t| t.0.as_str()).collect::<Vec<_>>().join("|");
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
    let var_sql = "SELECT kernel_name, COUNT(*) as cnt, AVG(duration_us) as avg,
                   AVG(duration_us * duration_us) - AVG(duration_us) * AVG(duration_us) as var
                   FROM launches GROUP BY kernel_name
                   HAVING cnt > 5 AND var > 0
                   ORDER BY SUM(duration_us) DESC LIMIT 5";
    let mut stmt = db.conn.prepare(var_sql).unwrap();
    let vars: Vec<_> = stmt.query_map([], |row| {
        Ok((row.get::<_,String>(0)?, row.get::<_,f64>(2)?, row.get::<_,f64>(3)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

    for (name, avg, var) in &vars {
        let stddev = var.sqrt();
        let cv = if *avg > 0.0 { stddev / avg } else { 0.0 };
        if cv > 0.3 {
            println!("  {n}. '{}' has high variance (CV={cv:.2}).", name);
            println!("     May indicate: data-dependent paths, cache effects, or varying input sizes.\n");
            n += 1;
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

    let mut stmt = db.conn.prepare(sql).unwrap();
    let rows: Vec<_> = stmt.query_map([], |row| {
        Ok((row.get::<_,String>(0)?, row.get::<_,f64>(1)?, row.get::<_,f64>(2)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

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
            let sql = "SELECT name, duration_us FROM regions ORDER BY start_us";
            let mut stmt = db.conn.prepare(sql).unwrap();
            let rows: Vec<_> = stmt.query_map([], |row| {
                Ok((row.get::<_,String>(0)?, row.get::<_,f64>(1)?))
            }).unwrap().filter_map(|r| r.ok()).collect();
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

    let sql = "SELECT kernel_name, COUNT(*), AVG(duration_us),
                      MIN(duration_us), MAX(duration_us),
                      AVG(duration_us * duration_us) - AVG(duration_us) * AVG(duration_us)
               FROM launches WHERE kernel_name LIKE ?1
               GROUP BY kernel_name";
    let mut stmt = db.conn.prepare(sql).unwrap();
    let rows: Vec<_> = stmt.query_map([like_param(pattern)], |row| {
        Ok((row.get::<_,String>(0)?, row.get::<_,i64>(1)?,
            row.get::<_,f64>(2)?, row.get::<_,f64>(3)?,
            row.get::<_,f64>(4)?, row.get::<_,f64>(5)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

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

pub fn cmd_warmup(db: &GpuDb) {
    let tl = db.timeline_filter();
    let sql = format!("SELECT start_us, duration_us, kernel_name
               FROM launches WHERE start_us IS NOT NULL AND {tl}
               ORDER BY start_us LIMIT 200");
    let mut stmt = db.conn.prepare(&sql).unwrap();
    let rows: Vec<_> = stmt.query_map([], |row| {
        Ok((row.get::<_,f64>(0)?, row.get::<_,f64>(1)?, row.get::<_,String>(2)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

    if rows.len() < 5 {
        println!("not enough launches to detect warmup");
        return;
    }

    // Compute rolling median over a window to detect stabilization
    let window = 5;
    let mut warmup_end = 0;
    let mut steady_durations: Vec<f64> = Vec::new();

    // First pass: find where things stabilize
    // A launch is "warm" when its duration is within 3x of the median of the next window
    for i in window..rows.len() {
        let mut window_durs: Vec<f64> = rows[i..rows.len().min(i + window)]
            .iter().map(|r| r.1).collect();
        window_durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = window_durs[window_durs.len() / 2];

        if rows[i].1 <= median * 3.0 {
            if warmup_end == 0 {
                warmup_end = i;
            }
            steady_durations.push(rows[i].1);
        }
    }

    if warmup_end == 0 { warmup_end = 1; }

    let warmup_total: f64 = rows[..warmup_end].iter().map(|r| r.1).sum();
    let steady_avg = if steady_durations.is_empty() { 0.0 }
        else { steady_durations.iter().sum::<f64>() / steady_durations.len() as f64 };

    let wall_us: f64 = db.meta("wall_time_us").parse().unwrap_or(0.0);
    let warmup_pct = if wall_us > 0.0 { warmup_total / wall_us * 100.0 } else { 0.0 };

    println!("  Warmup Detection:\n");
    println!("  Launch   Duration    Cumulative");
    let mut cumulative = 0.0;
    for (i, (_, dur, _)) in rows.iter().take(warmup_end + 3).enumerate() {
        cumulative += dur;
        let marker = if i < warmup_end { "  ← warmup" } else if i == warmup_end { "  ← stabilized" } else { "" };
        println!("  {:<6}   {:>10}  {:>10}{marker}", i + 1, fmt_us(*dur), fmt_us(cumulative));
    }

    println!("\n  Warmup:       {} launches ({}, {warmup_pct:.1}% of wall time)", warmup_end, fmt_us(warmup_total));
    println!("  Steady state: {} avg/launch (excluding warmup)", fmt_us(steady_avg));
}

// ---------------------------------------------------------------------------
// small — kernels where launch overhead likely exceeds kernel duration
// ---------------------------------------------------------------------------

pub fn cmd_small(db: &GpuDb, args: &[&str]) {
    let n = parse_count(args);
    let threshold_us = 10.0; // typical cudaLaunchKernel overhead

    let sql = format!(
        "SELECT kernel_name, COUNT(*) as cnt, AVG(duration_us) as avg,
                SUM(duration_us) as total
         FROM launches
         WHERE {} GROUP BY kernel_name
         HAVING avg < ?1
         ORDER BY cnt DESC LIMIT ?2",
        db.kernel_filter()
    );

    let mut stmt = db.conn.prepare(&sql).unwrap();
    let rows: Vec<_> = stmt.query_map(rusqlite::params![threshold_us, n as i64], |row| {
        Ok((row.get::<_,String>(0)?, row.get::<_,i64>(1)?,
            row.get::<_,f64>(2)?, row.get::<_,f64>(3)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

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

    let mut stmt = db.conn.prepare(&sql).unwrap();
    let rows: Vec<_> = stmt.query_map([], |row| {
        Ok((row.get::<_,String>(0)?, row.get::<_,String>(1)?,
            row.get::<_,f64>(2)?, row.get::<_,f64>(3)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

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

    println!("  Fusion Candidates (sequential kernels, < 5us gap):\n");
    println!("  #  Kernel A → Kernel B                              Count  Avg Gap");
    println!("  ── ──────────────────────────────────────────────── ────── ────────");
    for (i, ((a, b), (gap_sum, _, count))) in sorted.iter().enumerate() {
        let avg_gap = gap_sum / *count as f64;
        println!("  {:<2} {} → {}  {:>5}  {:>7}",
            i + 1, trunc(a, 24), trunc(b, 24), count, fmt_us(avg_gap));
    }
    println!("\n  Total fusable gap: {} across {} pairs", fmt_us(total_gap), rows.len());
    println!("  Tip: torch.compile() or manual kernel fusion can eliminate launch gaps");
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
    let sql = "SELECT stream_id, COUNT(*) as cnt, SUM(duration_us) as total
               FROM launches WHERE stream_id IS NOT NULL
               GROUP BY stream_id ORDER BY total DESC";
    let mut stmt = db.conn.prepare(sql).unwrap();
    let streams: Vec<_> = stmt.query_map([], |row| {
        Ok((row.get::<_,u32>(0)?, row.get::<_,i64>(1)?, row.get::<_,f64>(2)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

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
    let mut stmt = db.conn.prepare(sql).unwrap();
    let ops: Vec<_> = stmt.query_map([], |row| {
        Ok((row.get::<_,String>(0)?, row.get::<_,f64>(1)?,
            row.get::<_,f64>(2)?, row.get::<_,Option<String>>(3)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

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
    let mut stmt = db.conn.prepare(sql).unwrap();
    let ops: Vec<_> = stmt.query_map([n as i64], |row| {
        Ok((row.get::<_,String>(0)?, row.get::<_,f64>(1)?, row.get::<_,f64>(2)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

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
        .map(|p| format!("AND o.name LIKE '%{}%'", escape_sql_like(p)))
        .unwrap_or_default();

    let sql = format!(
        "SELECT o.name, o.cpu_time_us, o.gpu_time_us, o.module_path
         FROM ops o
         WHERE o.gpu_time_us > 0 {pat_clause}
         ORDER BY o.gpu_time_us DESC
         LIMIT ?1"
    );

    let mut stmt = db.conn.prepare(&sql).unwrap();
    let rows: Vec<_> = stmt.query_map([n as i64], |row| {
        Ok((row.get::<_,String>(0)?, row.get::<_,f64>(1)?,
            row.get::<_,f64>(2)?, row.get::<_,Option<String>>(3)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

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
    let op_sql = "SELECT id, name, cpu_time_us, gpu_time_us FROM ops WHERE name LIKE ?1";
    let mut stmt = db.conn.prepare(op_sql).unwrap();
    let ops: Vec<_> = stmt.query_map([like_param(pattern)], |row| {
        Ok((row.get::<_,i64>(0)?, row.get::<_,String>(1)?,
            row.get::<_,f64>(2)?, row.get::<_,f64>(3)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

    if ops.is_empty() { println!("no op matching '{pattern}'"); return; }

    for (op_id, op_name, cpu_time, gpu_time) in &ops {
        println!("Op: {op_name}");
        println!("  CPU: {}  GPU: {}", fmt_us(*cpu_time), fmt_us(*gpu_time));

        // Find kernels this op launches.
        // Restrict to timeline layer to avoid double-counting across nsys+torch.
        let tl = db.timeline_filter();
        let k_sql = format!(
            "SELECT okm.kernel_name,
                    COUNT(*) as launches,
                    SUM(l.duration_us) as total_us,
                    AVG(l.duration_us) as avg_us
             FROM op_kernel_map okm
             JOIN launches l ON l.kernel_name = okm.kernel_name AND {tl}
             WHERE okm.op_id = ?1
             GROUP BY okm.kernel_name
             ORDER BY total_us DESC"
        );
        let mut k_stmt = db.conn.prepare(&k_sql).unwrap();
        let kernels: Vec<_> = k_stmt.query_map([op_id], |row| {
            Ok((row.get::<_,String>(0)?, row.get::<_,i64>(1)?,
                row.get::<_,f64>(2)?, row.get::<_,f64>(3)?))
        }).unwrap().filter_map(|r| r.ok()).collect();

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
        Ok(id) => format!("layer_id = {id}"),
        Err(_) => db.timeline_filter(),
    };

    // Find kernel launches correlated to each op, compute gaps.

    // Get kernels belonging to op A
    let ka_sql = "SELECT DISTINCT kernel_name FROM op_kernel_map okm
                  JOIN ops o ON o.id = okm.op_id
                  WHERE o.name LIKE ?1";
    let mut stmt = db.conn.prepare(ka_sql).unwrap();
    let kernels_a: Vec<String> = stmt.query_map([like_param(pat_a)], |row| row.get(0))
        .unwrap().filter_map(|r| r.ok()).collect();

    let kb_sql = "SELECT DISTINCT kernel_name FROM op_kernel_map okm
                  JOIN ops o ON o.id = okm.op_id
                  WHERE o.name LIKE ?1";
    let mut stmt = db.conn.prepare(kb_sql).unwrap();
    let kernels_b: Vec<String> = stmt.query_map([like_param(pat_b)], |row| row.get(0))
        .unwrap().filter_map(|r| r.ok()).collect();

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

    let mut a_stmt = db.conn.prepare(&a_sql).unwrap();
    let a_ends: Vec<f64> = a_stmt.query_map([], |row| row.get(0))
        .unwrap().filter_map(|r| r.ok()).collect();

    let mut b_stmt = db.conn.prepare(&b_sql).unwrap();
    let b_starts: Vec<f64> = b_stmt.query_map([], |row| row.get(0))
        .unwrap().filter_map(|r| r.ok()).collect();

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

use std::path::PathBuf;
