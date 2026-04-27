use std::path::PathBuf;

use super::{
    GpuDb, compute_gpu_gaps, detect_warmup_count, escape_sql_like, find_hottest_window,
    fmt_bytes, fmt_us, like_param, merge_intervals, parse_count, parse_pattern,
    require_op_layer, trunc,
};

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
// warmup
// ---------------------------------------------------------------------------

pub fn cmd_warmup(db: &GpuDb) {
    let tl = db.timeline_filter();

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
// small
// ---------------------------------------------------------------------------

pub fn cmd_small(db: &GpuDb, args: &[&str]) {
    let n = parse_count(args);
    let threshold_us = 10.0;
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
    let overhead_est = total_launches as f64 * 5.0;

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
// fuse
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

    type PatternKey = Vec<usize>;
    struct Found { names: Vec<String>, reps: usize, total_us: f64 }
    let mut found: std::collections::HashMap<PatternKey, Found> = std::collections::HashMap::new();

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
            let pat = &ids[i..i + len];
            if pat.iter().all(|&x| x == pat[0]) { i += 1; continue; }

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
                i = j;
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
// concurrency
// ---------------------------------------------------------------------------

pub fn cmd_concurrency(db: &GpuDb) {
    let total_launches = db.total_launch_count();

    if total_launches == 0 {
        println!("no launch data");
        return;
    }

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
// hotpath
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
// compare-ops
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
// top-ops
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
// breakdown
// ---------------------------------------------------------------------------

pub fn cmd_breakdown(db: &GpuDb, args: &[&str]) {
    let pattern = match args.first() {
        Some(p) => *p,
        None => { println!("usage: breakdown <op_pattern>"); return; }
    };

    if !require_op_layer(db) { return; }

    let op_sql = r"SELECT id, name, cpu_time_us, gpu_time_us FROM ops WHERE name LIKE ?1 ESCAPE '\'";
    let ops: Vec<(i64, String, f64, f64)> = db.query_vec(
        op_sql, [like_param(pattern)],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );

    if ops.is_empty() { println!("no op matching '{pattern}'"); return; }

    for (op_id, op_name, cpu_time, gpu_time) in &ops {
        println!("Op: {op_name}");
        println!("  CPU: {}  GPU: {}", fmt_us(*cpu_time), fmt_us(*gpu_time));

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
// idle-between
// ---------------------------------------------------------------------------

pub fn cmd_idle_between(db: &GpuDb, args: &[&str]) {
    if args.len() < 2 {
        println!("usage: idle-between <op_a_pattern> <op_b_pattern>");
        return;
    }
    let pat_a = args[0];
    let pat_b = args[1];

    if !require_op_layer(db) { return; }

    let torch_layer = db.conn.query_row(
        "SELECT id FROM layers WHERE source IN ('torch', 'proton') ORDER BY id LIMIT 1",
        [], |row| row.get::<_, i64>(0),
    );
    let tl = match torch_layer {
        Ok(id) => format!("launches.layer_id = {id}"),
        Err(_) => db.timeline_filter(),
    };

    let ka_sql = r"SELECT DISTINCT kernel_name FROM op_kernel_map okm
                  JOIN ops o ON o.id = okm.op_id
                  WHERE o.name LIKE ?1 ESCAPE '\'";
    let kernels_a: Vec<String> = db.query_vec(ka_sql, [like_param(pat_a)], |row| row.get(0));
    let kernels_b: Vec<String> = db.query_vec(ka_sql, [like_param(pat_b)], |row| row.get(0));

    if kernels_a.is_empty() { println!("no kernels found for op '{pat_a}'"); return; }
    if kernels_b.is_empty() { println!("no kernels found for op '{pat_b}'"); return; }

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

    let mut gaps: Vec<f64> = Vec::new();
    let mut b_idx = 0;
    for a_end in &a_ends {
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
// outliers
// ---------------------------------------------------------------------------

pub fn cmd_outliers(db: &GpuDb, args: &[&str]) {
    let pattern = match args.first() {
        Some(p) => *p,
        None => { println!("usage: outliers <kernel_pattern>"); return; }
    };
    let tl = db.timeline_filter();

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
    let pct_idx = |p: f64| -> usize {
        let k = (p * cnt as f64).ceil() as isize - 1;
        k.clamp(0, cnt as isize - 1) as usize
    };
    let median = sorted[pct_idx(0.50)];
    let p90 = sorted[pct_idx(0.90)];
    let p99 = sorted[pct_idx(0.99)];

    let mut indexed: Vec<(usize, f64, f64)> = launches.iter().enumerate()
        .map(|(i, (s, d))| (i, *s, *d)).collect();
    indexed.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
    let top_n = (cnt / 10).max(3).min(cnt);
    let outliers = &indexed[..top_n];

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
// source
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
// memory
// ---------------------------------------------------------------------------

pub fn cmd_memory(db: &GpuDb, args: &[&str]) {
    let total: i64 = db.scalar_f64("SELECT COUNT(*) FROM allocations") as i64;
    if total == 0 {
        println!("no allocation data");
        println!("(re-profile to capture it — memory tracking is enabled by default in this build)");
        return;
    }
    let n = parse_count(args);

    let (n_alloc, n_free, sum_alloc): (i64, i64, i64) = db.conn.query_row(
        "SELECT SUM(CASE WHEN op = 'alloc' THEN 1 ELSE 0 END),
                SUM(CASE WHEN op = 'free'  THEN 1 ELSE 0 END),
                COALESCE(SUM(CASE WHEN op = 'alloc' THEN bytes ELSE 0 END), 0)
         FROM allocations",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    ).unwrap_or((0, 0, 0));

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

    if !alloc_lifetimes.is_empty() {
        let short_threshold = 100.0;
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
// bandwidth
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
// critical-path
// ---------------------------------------------------------------------------

pub fn cmd_critical_path(db: &GpuDb, args: &[&str]) {
    if !db.has_layer("nsys") && !db.has_layer("torch") {
        println!("no timeline data — need nsys or torch layer");
        return;
    }
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

    struct Chain {
        stream: u32,
        start: f64,
        end: f64,
        kernel_time: f64,
        kernels: Vec<(String, f64)>,
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

    chains.sort_by(|a, b| {
        let sa = a.end - a.start;
        let sb = b.end - b.start;
        sb.partial_cmp(&sa).unwrap()
            .then_with(|| b.kernel_time.partial_cmp(&a.kernel_time).unwrap())
    });

    println!("  Critical path chains (same stream, gap ≤ {}):\n", fmt_us(gap_thresh));
    let Some(best) = chains.first() else {
        println!("  (no chains to report)");
        return;
    };
    let best_span = best.end - best.start;
    let utilization = if best_span > 0.0 { best.kernel_time / best_span * 100.0 } else { 0.0 };
    println!("  Longest chain: stream {}  span {}  active {} ({utilization:.0}%)  {} kernel(s)",
        best.stream, fmt_us(best_span), fmt_us(best.kernel_time), best.kernels.len());

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
// stream-graph
// ---------------------------------------------------------------------------

pub fn cmd_stream_graph(db: &GpuDb, args: &[&str]) {
    let width: usize = args.first()
        .and_then(|s| s.parse().ok())
        .filter(|&w: &usize| (20..=500).contains(&w))
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

    use std::collections::BTreeMap;
    let mut by_stream: BTreeMap<u32, Vec<(String, f64, f64)>> = BTreeMap::new();
    for (name, start, dur, stream) in &rows {
        by_stream.entry(*stream).or_default().push((name.clone(), *start, *dur));
    }

    let mut kernel_time: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for (name, _, dur, _) in &rows {
        *kernel_time.entry(name.clone()).or_insert(0.0) += dur;
    }
    let mut kernel_rank: Vec<(String, f64)> = kernel_time.into_iter().collect();
    kernel_rank.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
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
    let axis: String = "─".repeat(width);
    println!("        └{axis}┘");

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
// hotspot
// ---------------------------------------------------------------------------

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
// launches
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
    let rows: Vec<_> = db.query_vec(
        &sql, rusqlite::params![name, limit as i64],
        |row| Ok((
            row.get::<_, f64>(0)?, row.get::<_, f64>(1)?,
            row.get::<_, Option<u32>>(2)?, row.get::<_, Option<u32>>(3)?, row.get::<_, Option<u32>>(4)?,
            row.get::<_, Option<u32>>(5)?, row.get::<_, Option<u32>>(6)?, row.get::<_, Option<u32>>(7)?,
            row.get::<_, Option<u32>>(8)?,
        )),
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
// compare
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

    let metrics_of = |name: &str| {
        db.conn.query_row(
            "SELECT occupancy_pct, compute_throughput_pct, memory_throughput_pct,
                    achieved_bandwidth_gb_s, boundedness
             FROM metrics WHERE kernel_name = ?1",
            [name],
            |row| Ok((
                row.get::<_, Option<f64>>(0)?, row.get::<_, Option<f64>>(1)?,
                row.get::<_, Option<f64>>(2)?, row.get::<_, Option<f64>>(3)?,
                row.get::<_, Option<String>>(4)?,
            )),
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
// regressions
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
    if !other_path.exists() {
        println!("cannot load '{name}': no such session at {}", other_path.display());
        return;
    }
    if let Err(e) = db.attach(other_path.to_str().unwrap_or(""), "other") {
        println!("cannot load '{name}': {e}");
        return;
    }

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
    let all: Vec<_> = db.query_vec(sql, [], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?, row.get::<_, f64>(2)?,
            row.get::<_, i64>(3)?, row.get::<_, i64>(4)?))
    });

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
