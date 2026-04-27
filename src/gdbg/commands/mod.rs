use super::db::{GpuDb, escape_sql_like};

mod analysis;
mod data;
mod drilldown;
mod filters;
mod hotspots;
mod timeline;

pub use analysis::*;
pub use data::*;
pub use drilldown::*;
pub use filters::*;
pub use hotspots::*;
pub use timeline::*;

// ---------------------------------------------------------------------------
// Shared helpers used across the category modules.
// ---------------------------------------------------------------------------

pub(crate) fn parse_count(args: &[&str]) -> usize {
    args.first()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(10)
}

pub(crate) fn parse_pattern<'a>(args: &'a [&'a str]) -> Option<&'a str> {
    if args.is_empty() { return None; }
    if args[0].parse::<usize>().is_ok() {
        args.get(1).copied()
    } else {
        Some(args[0])
    }
}

pub(crate) fn fmt_us(us: f64) -> String {
    if us >= 1_000_000.0 { format!("{:.2}s", us / 1_000_000.0) }
    else if us >= 1_000.0 { format!("{:.1}ms", us / 1_000.0) }
    else { format!("{:.1}us", us) }
}

pub(crate) fn fmt_bytes(b: i64) -> String {
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
pub(crate) fn like_param(pattern: &str) -> String {
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

/// Check that the DB has at least one of the required layers.
/// Prints a message and returns false if none are present.
pub(crate) fn require_op_layer(db: &GpuDb) -> bool {
    if db.has_layer("torch") || db.has_layer("proton") {
        true
    } else {
        println!("no op data — need torch.profiler or proton layer");
        false
    }
}

/// Merge overlapping or adjacent intervals into non-overlapping sorted intervals.
pub(crate) fn merge_intervals(intervals: &[(f64, f64)]) -> Vec<(f64, f64)> {
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
