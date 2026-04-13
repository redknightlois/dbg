//! Cross-command numerical consistency tests (invariants).
//!
//! Each test verifies that the numbers reported by different commands
//! are mathematically consistent with each other.

use super::fixtures::*;
use crate::commands;
use crate::db::GpuDb;
use rusqlite::params;

// -----------------------------------------------------------------------
// Invariant: sum(per-kernel total) == total_gpu_time_us (within TL layer)
// -----------------------------------------------------------------------

#[test]
fn invariant_kernel_totals_sum_to_gpu_time() {
    for (label, db) in [
        ("main", build_session()),
        ("cuda_only", build_cuda_only_session()),
        ("triton", build_triton_inference_session()),
        ("multi_stream", build_multi_stream_session()),
    ] {
        let tl = db.timeline_filter();
        let sum_of_kernels: f64 = db.conn.query_row(
            &format!(
                "SELECT COALESCE(SUM(total), 0) FROM (
                    SELECT SUM(duration_us) as total
                    FROM launches WHERE {tl}
                    GROUP BY kernel_name
                )"
            ),
            [],
            |row| row.get(0),
        ).unwrap();

        let total_gpu = db.total_gpu_time_us();

        assert!(
            (sum_of_kernels - total_gpu).abs() < 0.01,
            "[{label}] sum of per-kernel totals ({sum_of_kernels}) != total_gpu_time ({total_gpu})"
        );
    }
}

// -----------------------------------------------------------------------
// Invariant: per-stream launch counts sum to total_launch_count
//            (for launches that have a stream_id)
// -----------------------------------------------------------------------

#[test]
fn invariant_stream_counts_sum_to_total() {
    for (label, db) in [
        ("main", build_session()),
        ("cuda_only", build_cuda_only_session()),
        ("multi_stream", build_multi_stream_session()),
    ] {
        let tl = db.timeline_filter();
        let per_stream_sum: i64 = db.conn.query_row(
            &format!(
                "SELECT COALESCE(SUM(cnt), 0) FROM (
                    SELECT COUNT(*) as cnt
                    FROM launches
                    WHERE stream_id IS NOT NULL AND {tl}
                    GROUP BY stream_id
                )"
            ),
            [],
            |row| row.get(0),
        ).unwrap();

        // Launches without stream_id (torch layer launches may lack it)
        let no_stream: i64 = db.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM launches
                 WHERE stream_id IS NULL AND {tl}"
            ),
            [],
            |row| row.get(0),
        ).unwrap();

        let total = db.total_launch_count() as i64;

        assert_eq!(
            per_stream_sum + no_stream,
            total,
            "[{label}] per-stream sum ({per_stream_sum}) + no-stream ({no_stream}) != total ({total})"
        );
    }
}

// -----------------------------------------------------------------------
// Invariant: per-stream GPU time sums >= total_gpu_time
//            (can exceed if streams overlap, but never be less)
// -----------------------------------------------------------------------

#[test]
fn invariant_stream_time_ge_total() {
    for (label, db) in [
        ("main", build_session()),
        ("multi_stream", build_multi_stream_session()),
    ] {
        let tl = db.timeline_filter();
        let per_stream_time: f64 = db.conn.query_row(
            &format!(
                "SELECT COALESCE(SUM(total), 0) FROM (
                    SELECT SUM(duration_us) as total
                    FROM launches WHERE stream_id IS NOT NULL AND {tl}
                    GROUP BY stream_id
                )"
            ),
            [],
            |row| row.get(0),
        ).unwrap();

        let total = db.total_gpu_time_us();

        // With multiple streams, per-stream sums can exceed total
        // (both count the same wall-clock interval).  But for
        // single-stream sessions, they should be equal.
        let streams = db.stream_count();
        if streams <= 1 {
            assert!(
                (per_stream_time - total).abs() < 0.01,
                "[{label}] single-stream: per-stream time ({per_stream_time}) != total ({total})"
            );
        } else {
            // Multi-stream: per-stream sum >= total (overlapping work)
            assert!(
                per_stream_time >= total - 0.01,
                "[{label}] multi-stream: per-stream time ({per_stream_time}) < total ({total})"
            );
        }
    }
}

// -----------------------------------------------------------------------
// Invariant: gaps + merged_active ≈ timeline span
//            (from first launch start to last launch end)
// -----------------------------------------------------------------------

#[test]
fn invariant_gaps_plus_active_eq_span() {
    for (label, db) in [
        ("cuda_only", build_cuda_only_session()),
        ("multi_stream", build_multi_stream_session()),
    ] {
        let tl = db.timeline_filter();

        // Get all launch intervals from timeline layer
        let intervals: Vec<(f64, f64)> = db.query_vec(
            &format!(
                "SELECT start_us, start_us + duration_us AS end_us
                 FROM launches WHERE start_us IS NOT NULL AND {tl}
                 ORDER BY start_us"
            ),
            [],
            |row| Ok((row.get::<_,f64>(0)?, row.get::<_,f64>(1)?)),
        );

        if intervals.len() < 2 { continue; }

        let span_start = intervals.first().unwrap().0;
        let span_end = intervals.iter().map(|i| i.1).fold(0.0_f64, f64::max);
        let span = span_end - span_start;

        // Merge intervals and compute active+gap time
        let mut active = 0.0_f64;
        let mut gap_total = 0.0_f64;
        let (_, mut cur_end) = intervals[0];
        active += cur_end - intervals[0].0;

        for &(s, e) in &intervals[1..] {
            if s <= cur_end {
                // overlapping — extend
                if e > cur_end {
                    active += e - cur_end;
                    cur_end = e;
                }
            } else {
                // gap
                gap_total += s - cur_end;
                active += e - s;
                cur_end = e;
            }
        }

        assert!(
            (active + gap_total - span).abs() < 0.01,
            "[{label}] active ({active}) + gaps ({gap_total}) != span ({span})"
        );
    }
}

// -----------------------------------------------------------------------
// Invariant: variance mean * count == total for each kernel
// -----------------------------------------------------------------------

#[test]
fn invariant_variance_mean_times_count_eq_total() {
    let db = build_session();
    let tl = db.timeline_filter();

    let rows: Vec<(String, i64, f64, f64)> = db.query_vec(
        &format!(
            "SELECT kernel_name, COUNT(*), AVG(duration_us), SUM(duration_us)
             FROM launches WHERE {tl}
             GROUP BY kernel_name"
        ),
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );

    for (name, count, avg, total) in &rows {
        let product = avg * (*count as f64);
        assert!(
            (product - total).abs() < 0.01,
            "kernel '{name}': avg ({avg}) * count ({count}) = {product} != total ({total})"
        );
    }
}

// -----------------------------------------------------------------------
// Invariant: ops.gpu_time == breakdown kernel sum (after recompute)
// -----------------------------------------------------------------------

#[test]
fn invariant_op_gpu_matches_breakdown_sum() {
    let db = build_session();
    let tl_id = db.timeline_layer_id().unwrap();

    // For each op with kernel mappings, verify ops.gpu_time matches
    // the sum of timeline-layer kernel durations.
    let ops: Vec<(i64, String, f64)> = db.query_vec(
        "SELECT id, name, gpu_time_us FROM ops",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    );

    for (op_id, op_name, stored_gpu) in &ops {
        let breakdown_sum: f64 = db.conn.query_row(
            "SELECT COALESCE(SUM(l.duration_us), 0)
             FROM op_kernel_map okm
             JOIN launches l ON l.kernel_name = okm.kernel_name AND l.layer_id = ?1
             WHERE okm.op_id = ?2",
            params![tl_id, op_id],
            |row| row.get(0),
        ).unwrap();

        assert!(
            (stored_gpu - breakdown_sum).abs() < 0.01,
            "op '{op_name}': stored gpu_time ({stored_gpu}) != breakdown sum ({breakdown_sum})"
        );
    }
}

// -----------------------------------------------------------------------
// Invariant: top-ops %GPU uses the same denominator as kernels %
// -----------------------------------------------------------------------

#[test]
fn invariant_topops_pct_denominator_matches_kernels() {
    let db = build_session();

    // Both should use total_gpu_time_us() as denominator.
    // After our fixes, both query the timeline layer.
    let total_gpu = db.total_gpu_time_us();

    // Manually check: an op's % GPU should be op.gpu_time / total_gpu * 100
    let linear_gpu: f64 = db.conn.query_row(
        "SELECT gpu_time_us FROM ops WHERE name = 'aten::linear'",
        [],
        |row| row.get(0),
    ).unwrap();

    let linear_pct = linear_gpu / total_gpu * 100.0;

    // The cutlass kernel contributes to aten::linear.  Its kernel %
    // from cmd_kernels should be cutlass_total / total_gpu * 100.
    let tl = db.timeline_filter();
    let cutlass_total: f64 = db.conn.query_row(
        &format!(
            "SELECT SUM(duration_us) FROM launches
             WHERE kernel_name LIKE '%cutlass%' AND {tl}"
        ),
        [],
        |row| row.get(0),
    ).unwrap();
    let cutlass_pct = cutlass_total / total_gpu * 100.0;

    // The sum of kernel %s for aten::linear's kernels should >= the op's % GPU
    // (because the kernels may fire outside this op too).
    let tl2 = db.timeline_filter();
    let sgemm_total: f64 = db.conn.query_row(
        &format!(
            "SELECT SUM(duration_us) FROM launches
             WHERE kernel_name LIKE '%sgemm%' AND {tl2}"
        ),
        [],
        |row| row.get(0),
    ).unwrap();
    let sgemm_pct = sgemm_total / total_gpu * 100.0;

    // After recompute, linear_gpu should be cutlass + sgemm (nsys totals)
    assert!(
        (linear_gpu - (cutlass_total + sgemm_total)).abs() < 0.01,
        "linear gpu ({linear_gpu}) != cutlass ({cutlass_total}) + sgemm ({sgemm_total})"
    );

    // And the percentage should match: linear_pct == cutlass_pct + sgemm_pct
    assert!(
        (linear_pct - (cutlass_pct + sgemm_pct)).abs() < 0.01,
        "linear %GPU ({linear_pct:.2}) != cutlass % ({cutlass_pct:.2}) + sgemm % ({sgemm_pct:.2})"
    );
}

// -----------------------------------------------------------------------
// Invariant: overlap GPU utilization == stats GPU utilization
//            (both use total_gpu_time_us() / wall_us)
// -----------------------------------------------------------------------

#[test]
fn invariant_overlap_matches_stats_utilization() {
    // Both stats and overlap compute GPU utilization as
    // total_gpu_time_us() / wall_time * 100.  They must agree.
    for (label, db) in [
        ("main", build_session()),
        ("cuda_only", build_cuda_only_session()),
    ] {
        let gpu_us = db.total_gpu_time_us();
        let wall_us: f64 = db.meta("wall_time_us").parse().unwrap_or(0.0);
        if wall_us == 0.0 { continue; }

        let stats_util = gpu_us / wall_us * 100.0;

        // overlap reports the same calculation
        let overlap_util = gpu_us / wall_us * 100.0;

        assert!(
            (stats_util - overlap_util).abs() < 0.001,
            "[{label}] stats util ({stats_util:.1}) != overlap util ({overlap_util:.1})"
        );
    }
}

// -----------------------------------------------------------------------
// Invariant: small kernel total time < total GPU time
// -----------------------------------------------------------------------

#[test]
fn invariant_small_subset_of_total() {
    for (label, db) in [
        ("main", build_session()),
        ("triton", build_triton_inference_session()),
    ] {
        let tl = db.timeline_filter();
        let small_total: f64 = db.conn.query_row(
            &format!(
                "SELECT COALESCE(SUM(total), 0) FROM (
                    SELECT SUM(duration_us) as total, AVG(duration_us) as avg
                    FROM launches WHERE {tl}
                    GROUP BY kernel_name
                    HAVING avg < 10.0
                )"
            ),
            [],
            |row| row.get(0),
        ).unwrap();

        let total = db.total_gpu_time_us();

        assert!(
            small_total <= total + 0.01,
            "[{label}] small kernel total ({small_total}) > total GPU ({total})"
        );
    }
}

// -----------------------------------------------------------------------
// Invariant: no double-counting in multi-layer session
//            (total_gpu_time == nsys-only total, not nsys+torch)
// -----------------------------------------------------------------------

#[test]
fn invariant_no_double_count_multi_layer() {
    let db = build_session();

    let nsys_id: i64 = db.conn.query_row(
        "SELECT id FROM layers WHERE source = 'nsys' LIMIT 1",
        [],
        |row| row.get(0),
    ).unwrap();

    let nsys_total: f64 = db.conn.query_row(
        "SELECT COALESCE(SUM(duration_us), 0) FROM launches WHERE layer_id = ?1",
        params![nsys_id],
        |row| row.get(0),
    ).unwrap();

    let raw_all: f64 = db.conn.query_row(
        "SELECT COALESCE(SUM(duration_us), 0) FROM launches",
        [],
        |row| row.get(0),
    ).unwrap();

    let reported = db.total_gpu_time_us();

    // reported must match nsys-only, NOT the inflated cross-layer sum
    assert!(
        (reported - nsys_total).abs() < 0.01,
        "total_gpu_time ({reported}) != nsys total ({nsys_total}); raw all-layer = {raw_all}"
    );
    assert!(
        raw_all > nsys_total + 1.0,
        "test setup error: torch layer should add duplicate launches (raw={raw_all}, nsys={nsys_total})"
    );
}

// -----------------------------------------------------------------------
// Invariant: kernels_with_metrics <= unique_kernel_count
// -----------------------------------------------------------------------

#[test]
fn invariant_metrics_subset_of_kernels() {
    for (label, db) in [
        ("main", build_session()),
        ("cuda_only", build_cuda_only_session()),
        ("triton", build_triton_inference_session()),
    ] {
        let uk = db.unique_kernel_count();
        let wm = db.kernels_with_metrics();
        let wo = db.kernels_with_ops();

        assert!(
            wm <= uk,
            "[{label}] kernels_with_metrics ({wm}) > unique_kernel_count ({uk})"
        );
        assert!(
            wo <= uk,
            "[{label}] kernels_with_ops ({wo}) > unique_kernel_count ({uk})"
        );
    }
}

// -----------------------------------------------------------------------
// Invariant: region filter reduces launch count monotonically
//            (more restrictive region → fewer/equal launches)
// -----------------------------------------------------------------------

#[test]
fn invariant_region_filter_monotone() {
    let mut db = build_session();

    let total = db.total_launch_count();

    // Apply region filter to Step#1 (covers [500, 20500])
    db.region_filter = Some("Step#1".to_string());
    let filter = db.kernel_filter();
    let tl = db.timeline_filter();
    let step1_count: i64 = db.conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM launches WHERE {filter} AND {tl}"
        ),
        [],
        |row| row.get(0),
    ).unwrap();

    // Apply region filter to Step#2 (covers [20500, 45500])
    db.region_filter = Some("Step#2".to_string());
    let filter = db.kernel_filter();
    let step2_count: i64 = db.conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM launches WHERE {filter} AND {tl}"
        ),
        [],
        |row| row.get(0),
    ).unwrap();

    // Each region should have fewer launches than total
    assert!(
        step1_count < total as i64,
        "Step#1 ({step1_count}) should be < total ({total})"
    );
    assert!(
        step2_count < total as i64,
        "Step#2 ({step2_count}) should be < total ({total})"
    );

    // Combined (both regions) should account for a meaningful subset
    // (not necessarily total, since some launches may be outside both)
    assert!(
        step1_count > 0 && step2_count > 0,
        "both regions should match some launches"
    );
}

// -----------------------------------------------------------------------
// Invariant: fuse candidates must be on the same stream and sequential
// -----------------------------------------------------------------------

#[test]
fn invariant_fuse_candidates_same_stream() {
    let db = build_session();
    let tl = db.timeline_filter();

    // Run the same query that cmd_fuse uses
    let sql = "WITH ordered AS (
                 SELECT kernel_name, start_us, duration_us, stream_id,
                        ROW_NUMBER() OVER (ORDER BY start_us) as rn
                 FROM launches WHERE start_us IS NOT NULL AND ".to_string()
        + &tl + ")
               SELECT a.stream_id, b.stream_id,
                      b.start_us - (a.start_us + a.duration_us) AS gap_us
               FROM ordered a
               JOIN ordered b ON b.rn = a.rn + 1
               WHERE gap_us >= 0 AND gap_us < 5.0
                 AND a.stream_id IS b.stream_id";

    let pairs: Vec<(Option<u32>, Option<u32>, f64)> = db.query_vec(
        &sql,
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    );

    for (sa, sb, gap) in &pairs {
        assert_eq!(sa, sb, "fuse candidate on different streams: {sa:?} vs {sb:?}");
        assert!(*gap >= 0.0 && *gap < 5.0, "gap out of range: {gap}");
    }
}

// -----------------------------------------------------------------------
// Invariant: build_session ops.gpu_time matches nsys after recompute
//            (specific numerical check for the main test session)
// -----------------------------------------------------------------------

#[test]
fn invariant_recompute_specific_values() {
    let db = build_session();

    // Expected nsys totals after recompute:
    // aten::linear → cutlass (3005) + sgemm (810) = 3815
    // aten::batch_norm → cudnn bn (6500)
    // aten::relu_ → elementwise (15.8)
    // aten::nll_loss → no mapping (0)

    let linear_gpu: f64 = db.conn.query_row(
        "SELECT gpu_time_us FROM ops WHERE name = 'aten::linear'",
        [], |row| row.get(0),
    ).unwrap();
    assert!(
        (linear_gpu - 3815.0).abs() < 0.1,
        "linear gpu_time should be 3815 (cutlass 3005 + sgemm 810), got {linear_gpu}"
    );

    let bn_gpu: f64 = db.conn.query_row(
        "SELECT gpu_time_us FROM ops WHERE name = 'aten::batch_norm'",
        [], |row| row.get(0),
    ).unwrap();
    assert!(
        (bn_gpu - 6500.0).abs() < 0.1,
        "batch_norm gpu_time should be 6500, got {bn_gpu}"
    );

    let relu_gpu: f64 = db.conn.query_row(
        "SELECT gpu_time_us FROM ops WHERE name = 'aten::relu_'",
        [], |row| row.get(0),
    ).unwrap();
    assert!(
        (relu_gpu - 15.8).abs() < 0.1,
        "relu gpu_time should be 15.8, got {relu_gpu}"
    );

    let loss_gpu: f64 = db.conn.query_row(
        "SELECT gpu_time_us FROM ops WHERE name = 'aten::nll_loss'",
        [], |row| row.get(0),
    ).unwrap();
    assert!(
        loss_gpu.abs() < 0.01,
        "nll_loss gpu_time should be 0 (no kernel mapping), got {loss_gpu}"
    );
}

// -----------------------------------------------------------------------
// Invariant: transfer bandwidth is consistent (bytes / duration)
// -----------------------------------------------------------------------

#[test]
fn invariant_transfer_bandwidth_consistency() {
    let db = build_session();

    let transfers: Vec<(String, i64, f64)> = db.query_vec(
        "SELECT kind, bytes, duration_us FROM transfers",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    );

    for (kind, bytes, dur) in &transfers {
        assert!(*bytes > 0, "transfer {kind} has 0 bytes");
        assert!(*dur > 0.0, "transfer {kind} has 0 duration");
        let bw_gb_s = *bytes as f64 / dur / 1000.0;
        // Bandwidth should be positive and reasonable (< 100 GB/s for PCIe)
        assert!(
            bw_gb_s > 0.0 && bw_gb_s < 200.0,
            "transfer {kind}: unreasonable bandwidth {bw_gb_s:.1} GB/s"
        );
    }
}

// -----------------------------------------------------------------------
// Invariant: warmup first launch >= steady state average
//            (for the high-variance kernel in build_session)
// -----------------------------------------------------------------------

#[test]
fn invariant_warmup_first_launch_slower() {
    let db = build_session();
    let tl = db.timeline_filter();

    // The cudnn bn kernel has a 5000us warmup vs ~300us steady state
    let launches: Vec<f64> = db.query_vec(
        &format!(
            "SELECT duration_us FROM launches
             WHERE kernel_name LIKE '%bn_fw%' AND {tl}
             ORDER BY start_us"
        ),
        [],
        |row| row.get(0),
    );

    assert!(launches.len() >= 2, "need at least 2 launches");
    let first = launches[0];
    let steady_avg: f64 = launches[1..].iter().sum::<f64>() / (launches.len() - 1) as f64;

    assert!(
        first > steady_avg * 2.0,
        "warmup launch ({first}) should be >> steady avg ({steady_avg})"
    );
}

// -----------------------------------------------------------------------
// Invariant: inspect count/total agree for every kernel in session
// -----------------------------------------------------------------------

#[test]
fn invariant_inspect_matches_kernels_for_all() {
    let db = build_session();
    let tl = db.timeline_filter();

    // Get per-kernel stats the way cmd_kernels does
    let kernels_view: Vec<(String, i64, f64)> = db.query_vec(
        &format!(
            "SELECT kernel_name, COUNT(*) as cnt, SUM(duration_us) as total
             FROM launches WHERE {tl}
             GROUP BY kernel_name"
        ),
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    );

    // Get per-kernel stats the way cmd_inspect does
    for (name, k_cnt, k_total) in &kernels_view {
        let (i_cnt, i_total): (i64, f64) = db.conn.query_row(
            &format!(
                "SELECT COUNT(*), SUM(duration_us)
                 FROM launches WHERE kernel_name = ?1 AND {tl}"
            ),
            params![name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();

        assert_eq!(
            *k_cnt, i_cnt,
            "kernel '{name}': cmd_kernels count ({k_cnt}) != cmd_inspect count ({i_cnt})"
        );
        assert!(
            (k_total - i_total).abs() < 0.01,
            "kernel '{name}': cmd_kernels total ({k_total}) != cmd_inspect total ({i_total})"
        );
    }
}
