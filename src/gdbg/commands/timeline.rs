use super::{
    GpuDb, compute_gpu_gaps, fmt_bytes, fmt_us, parse_count, trunc, xfer_kernel_overlap,
};

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
