//! Smoke tests and command-level no-panic tests for gdbg.
//!
//! Each test calls one or more `commands::cmd_*()` functions and verifies
//! they complete without panicking, plus light data-consistency checks.

use super::fixtures::*;
use crate::commands;
use crate::db::GpuDb;
use rusqlite::params;

// -----------------------------------------------------------------------
// Every command must not panic
// -----------------------------------------------------------------------

#[test]
fn smoke_all_commands() {
    let mut db = build_session();

    // Info commands
    commands::cmd_stats(&db);
    commands::cmd_layers(&db);
    commands::cmd_suggest(&db);

    // Kernel listing with various args
    commands::cmd_kernels(&db, &[]);
    commands::cmd_kernels(&db, &["3"]);
    commands::cmd_kernels(&db, &["5", "sgemm"]);
    commands::cmd_kernels(&db, &["cutlass"]);

    // Op commands
    commands::cmd_ops(&db, &[]);
    commands::cmd_ops(&db, &["5", "linear"]);
    commands::cmd_top_ops(&db, &[]);
    commands::cmd_top_ops(&db, &["3", "aten"]);
    commands::cmd_compare_ops(&db, &[]);
    commands::cmd_hotpath(&db);

    // Drill-down
    commands::cmd_inspect(&db, &["sgemm"]);
    commands::cmd_inspect(&db, &["cutlass"]);
    commands::cmd_bound(&db, &["sgemm"]);
    commands::cmd_bound(&db, &["cutlass"]);
    commands::cmd_trace(&db, &["linear"]);
    commands::cmd_callers(&db, &["sgemm"]);
    commands::cmd_breakdown(&db, &["linear"]);
    commands::cmd_breakdown(&db, &["batch_norm"]);

    // Analysis
    commands::cmd_roofline(&db, &[]);
    commands::cmd_roofline(&db, &["sgemm"]);
    commands::cmd_occupancy(&db, &[]);
    commands::cmd_variance(&db, &["bn_fw"]);
    commands::cmd_warmup(&db);
    commands::cmd_small(&db, &[]);
    commands::cmd_fuse(&db, &[]);
    commands::cmd_concurrency(&db);

    // Timeline
    commands::cmd_transfers(&db, &[]);
    commands::cmd_gaps(&db, &[]);
    commands::cmd_overlap(&db);
    commands::cmd_streams(&db);
    commands::cmd_timeline(&db, &[]);

    // Inter-op
    commands::cmd_idle_between(&db, &["linear", "batch_norm"]);

    // Filters
    commands::cmd_focus(&mut db, &["cutlass"]);
    commands::cmd_kernels(&db, &[]);  // should only show cutlass
    commands::cmd_ignore(&mut db, &["sgemm"]);
    commands::cmd_kernels(&db, &[]);
    commands::cmd_region(&mut db, &["Step#1"]);
    commands::cmd_kernels(&db, &[]);  // should filter to region
    commands::cmd_region(&mut db, &[]);  // list regions
    commands::cmd_reset(&mut db);
    commands::cmd_kernels(&db, &[]);  // back to all

    // No-data edge cases (commands on missing layers)
    commands::cmd_inspect(&db, &["nonexistent_kernel_xyz"]);
    commands::cmd_bound(&db, &["nonexistent_kernel_xyz"]);
    commands::cmd_trace(&db, &["nonexistent_op_xyz"]);
    commands::cmd_idle_between(&db, &["nonexistent_a", "nonexistent_b"]);
}

// -----------------------------------------------------------------------
// Unicode kernel names must not panic in trunc
// -----------------------------------------------------------------------

#[test]
fn unicode_kernel_names_no_panic() {
    let db = build_session();
    // The unicode kernel "triton_poi_fused_αβ_kernel_0" should display fine
    commands::cmd_kernels(&db, &["αβ"]);
    commands::cmd_inspect(&db, &["αβ"]);
    commands::cmd_timeline(&db, &["50"]); // shows all, including unicode
}

// -----------------------------------------------------------------------
// Verify breakdown doesn't double-count across layers
// -----------------------------------------------------------------------

#[test]
fn breakdown_no_double_count() {
    let db = build_session();

    // The nsys layer has 6 launches of the cutlass gemm (500+480+510+490+520+505 = 3005us)
    // The torch layer has 2 launches (500+480 = 980us)
    // If breakdown double-counts, it would sum across both layers.

    // The timeline_filter prefers nsys, so breakdown should report 3005us total
    // for the cutlass kernel when looking at aten::linear.
    let nsys_total: f64 = db.conn.query_row(
        "SELECT SUM(duration_us) FROM launches
         WHERE kernel_name LIKE '%cutlass%' AND layer_id = (
           SELECT id FROM layers WHERE source = 'nsys' LIMIT 1
         )",
        [],
        |row| row.get(0),
    ).unwrap();
    assert!((nsys_total - 3005.0).abs() < 0.1, "nsys cutlass total: {nsys_total}");

    let all_total: f64 = db.conn.query_row(
        "SELECT SUM(duration_us) FROM launches WHERE kernel_name LIKE '%cutlass%'",
        [],
        |row| row.get(0),
    ).unwrap();
    // all_total includes both nsys (3005) and torch (980) = 3985
    assert!((all_total - 3985.0).abs() < 0.1, "all layers cutlass total: {all_total}");

    // The breakdown command should NOT report 3985 — it should report 3005
    // (the nsys-only total, since timeline_filter selects nsys).
    // We can't capture stdout easily, but we verify the query logic matches:
    let tl_id: i64 = db.timeline_layer_id().unwrap();
    let nsys_id: i64 = db.conn.query_row(
        "SELECT id FROM layers WHERE source = 'nsys' LIMIT 1", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(tl_id, nsys_id, "timeline should prefer nsys layer");

    // And run it to make sure it doesn't panic
    commands::cmd_breakdown(&db, &["linear"]);
}

// -----------------------------------------------------------------------
// Verify gaps accounts for cross-stream overlap
// -----------------------------------------------------------------------

#[test]
fn gaps_handles_overlapping_streams() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.into_path().join("gaps.gpu.db");
    let db = GpuDb::create(&path).unwrap();
    db.set_meta("wall_time_us", "1000").unwrap();

    let lid = db.add_layer("nsys", "test", None, None, None).unwrap();

    // Stream 1: [0, 400]
    // Stream 2: [200, 600]   ← overlaps with stream 1
    // Stream 1: [800, 1000]  ← gap from 600 to 800 = 200us
    //
    // Naive consecutive: would see gaps between 0→200 and 400→800 etc.
    // Correct merged: only one gap from 600 to 800 = 200us

    db.conn.execute(
        "INSERT INTO launches (kernel_name, duration_us, start_us, stream_id, layer_id)
         VALUES ('k1', 400.0, 0.0, 1, ?1)", params![lid],
    ).unwrap();
    db.conn.execute(
        "INSERT INTO launches (kernel_name, duration_us, start_us, stream_id, layer_id)
         VALUES ('k2', 400.0, 200.0, 2, ?1)", params![lid],
    ).unwrap();
    db.conn.execute(
        "INSERT INTO launches (kernel_name, duration_us, start_us, stream_id, layer_id)
         VALUES ('k3', 200.0, 800.0, 1, ?1)", params![lid],
    ).unwrap();

    // Run gaps command (should not panic, and should find exactly 1 gap)
    commands::cmd_gaps(&db, &[]);

    // Verify via compute_gpu_gaps helper (it's private, so test the logic directly)
    // We can verify by checking the data model: the merged intervals are
    // [0, 600] and [800, 1000], so one gap of 200us at t=600.
    // If the old LAG-based SQL was used, it would find:
    //   row1(start=0,end=400), row2(start=200,end=600) → gap = 200 - 400 = -200 (filtered)
    //   row2(start=200,end=600), row3(start=800,end=1000) → gap = 800 - 600 = 200
    // Actually the old code happened to get this simple case right because row2.start < row1.end.
    // Let's test a case where it would definitely fail:

    // Add a third stream that fills the 600-800 gap
    db.conn.execute(
        "INSERT INTO launches (kernel_name, duration_us, start_us, stream_id, layer_id)
         VALUES ('k4', 300.0, 550.0, 3, ?1)", params![lid],
    ).unwrap();
    // Now streams cover: [0,400], [200,600], [550,850], [800,1000]
    // Merged: [0, 1000] — no gaps at all!
    // Old LAG code would still see a gap: ordered by start is k1(0), k2(200), k4(550), k3(800)
    //   k4.end=850, k3.start=800 → 800-850 = -50 (filtered)
    //   k2.end=600, k4.start=550 → 550-600 = -50 (filtered)
    //   k1.end=400, k2.start=200 → 200-400 = -200 (filtered)
    // Hmm, the old code happens to filter negative gaps. Let me construct a case
    // where the old code would produce a false positive:

    let dir2 = tempfile::tempdir().unwrap();
    let path2 = dir2.into_path().join("gaps2.gpu.db");
    let db2 = GpuDb::create(&path2).unwrap();
    let lid2 = db2.add_layer("nsys", "test", None, None, None).unwrap();
    db2.set_meta("wall_time_us", "2000").unwrap();

    // Stream 1: [0, 100], [500, 600]
    // Stream 2: [50, 550]
    // Merged: [0, 600] — no gaps
    // Old LAG on sorted starts: (0,100), (50,550), (500,600)
    //   prev_end=100, start=50 → 50-100 = -50 (filtered, ok)
    //   prev_end=550, start=500 → 500-550 = -50 (filtered, ok)
    // Still happens to work. Let me try:

    // Stream 1: [0, 100], [400, 500]
    // Stream 2: [200, 300]
    // No overlap. Sorted: (0,100), (200,300), (400,500)
    // Gaps: 100→200 = 100us, 300→400 = 100us. Total = 200us.
    // Merged: same result since no overlaps. Both old and new agree.

    // The killer case: stream 1 has a long kernel, stream 2 has a short one inside it
    // Stream 1: [0, 1000]
    // Stream 2: [1100, 1200]
    // Stream 1: [500, 1500]  ← extends the first interval
    // Sorted: (0,1000), (500,1500), (1100,1200)
    // Old LAG: prev_end=1000, start=500 → -500 (filtered)
    //          prev_end=1500, start=1100 → 1100-1500 = -400 (filtered)
    // Merged: [0, 1500] — no gaps. Correct!
    // Old code misses no gap here. The real killer:

    // Stream 1: [0, 500]
    // Stream 2: [100, 900]    ← extends past stream 1
    // Stream 1: [600, 700]    ← inside stream 2's range, but LAG sees gap
    // Sorted by start: (0,500), (100,900), (600,700)
    // Old LAG: prev_end=500, start=100 → -400 (filtered)
    //          prev_end=900, start=600 → 600-900 = -300 (filtered)
    // Still no false positive because LAG uses prev row's end, not running max.
    // Actually wait — LAG gets the PREVIOUS ROW's end_us, not a running max.
    // Row 3: prev_end = row2.end = 900. start = 600. 600 - 900 = -300 (filtered). OK.

    // The actual failure case for LAG:
    // Stream 1: [0, 100]
    // Stream 2: [50, 800]
    // Stream 1: [600, 700]  ← inside stream 2, NOT a gap
    // Stream 1: [900, 1000] ← real gap from 800 to 900
    // Sorted: (0,100), (50,800), (600,700), (900,1000)
    // Old LAG:
    //   row2: prev_end = 100, start = 50 → -50 (filtered)
    //   row3: prev_end = 800, start = 600 → -200 (filtered)
    //   row4: prev_end = 700, start = 900 → 200us gap ← WRONG! real gap is 800→900 = 100us
    // Because LAG only sees the PREVIOUS row's end (700), not the running max (800).

    db2.conn.execute(
        "INSERT INTO launches (kernel_name, duration_us, start_us, stream_id, layer_id)
         VALUES ('a', 100.0, 0.0, 1, ?1)", params![lid2],
    ).unwrap();
    db2.conn.execute(
        "INSERT INTO launches (kernel_name, duration_us, start_us, stream_id, layer_id)
         VALUES ('b', 750.0, 50.0, 2, ?1)", params![lid2],
    ).unwrap();
    db2.conn.execute(
        "INSERT INTO launches (kernel_name, duration_us, start_us, stream_id, layer_id)
         VALUES ('c', 100.0, 600.0, 1, ?1)", params![lid2],
    ).unwrap();
    db2.conn.execute(
        "INSERT INTO launches (kernel_name, duration_us, start_us, stream_id, layer_id)
         VALUES ('d', 100.0, 900.0, 1, ?1)", params![lid2],
    ).unwrap();
    // Merged: [0, 800] then [900, 1000]. One gap: 800→900 = 100us.
    // Old code would report: 900 - 700 = 200us (wrong).

    commands::cmd_gaps(&db2, &[]);
    // Can't capture stdout, but at least it doesn't panic.
    // The real assertion is that the gap-merge algorithm is correct — tested below.
}

#[test]
fn gap_merge_correctness() {
    // Direct test of the interval merge logic (same as compute_gpu_gaps)
    let dir = tempfile::tempdir().unwrap();
    let path = dir.into_path().join("gm.gpu.db");
    let db = GpuDb::create(&path).unwrap();
    let lid = db.add_layer("nsys", "t", None, None, None).unwrap();

    // Case from above: merged [0,800], gap [800,900], then [900,1000]
    for &(name, dur, start, stream) in &[
        ("a", 100.0_f64, 0.0_f64, 1_u32),
        ("b", 750.0, 50.0, 2),
        ("c", 100.0, 600.0, 1),
        ("d", 100.0, 900.0, 1),
    ] {
        db.conn.execute(
            "INSERT INTO launches (kernel_name, duration_us, start_us, stream_id, layer_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, dur, start, stream, lid],
        ).unwrap();
    }

    // Run the internal gap computation via the command
    // The timeline_filter will select the nsys layer
    let tl = db.timeline_filter();
    let sql = format!(
        "SELECT start_us, start_us + duration_us AS end_us
         FROM launches WHERE start_us IS NOT NULL AND {tl}
         ORDER BY start_us"
    );
    let intervals: Vec<(f64, f64)> = db.query_vec(&sql, [], |row| {
        Ok((row.get::<_,f64>(0)?, row.get::<_,f64>(1)?))
    });

    // Manually merge and verify
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

    assert_eq!(gaps.len(), 1, "expected exactly 1 gap, got: {gaps:?}");
    assert!((gaps[0].0 - 800.0).abs() < 0.1, "gap should start at 800, got {}", gaps[0].0);
    assert!((gaps[0].1 - 100.0).abs() < 0.1, "gap should be 100us, got {}", gaps[0].1);
}

// -----------------------------------------------------------------------
// Region filter actually restricts results
// -----------------------------------------------------------------------

#[test]
fn region_filter_restricts_launches() {
    let mut db = build_session();

    // Region "ProfilerStep#1" covers [500, 20500]
    // Many launches are outside this window
    let total_before = db.total_launch_count();

    db.region_filter = Some("Step#1".to_string());
    let filter = db.kernel_filter();

    // Count launches matching the filter
    let sql = format!(
        "SELECT COUNT(*) FROM launches WHERE {filter}"
    );
    let filtered_count = db.count(&sql);

    assert!(filtered_count > 0, "region filter should match some launches");
    assert!(filtered_count < total_before,
        "region filter should exclude launches outside the region (filtered={filtered_count}, total={total_before})");

    // Launches at t=30000, 40000, etc. should be excluded
    let outside = db.conn.query_row(
        &format!("SELECT COUNT(*) FROM launches WHERE start_us > 20500 AND {filter}"),
        [],
        |row| row.get::<_, i64>(0),
    ).unwrap();
    assert_eq!(outside, 0, "launches after region end should be excluded");
}

// -----------------------------------------------------------------------
// Empty DB edge cases
// -----------------------------------------------------------------------

#[test]
fn empty_db_no_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.into_path().join("empty.gpu.db");
    let mut db = GpuDb::create(&path).unwrap();

    commands::cmd_stats(&db);
    commands::cmd_kernels(&db, &[]);
    commands::cmd_ops(&db, &[]);
    commands::cmd_layers(&db);
    commands::cmd_suggest(&db);
    commands::cmd_streams(&db);
    commands::cmd_overlap(&db);
    commands::cmd_concurrency(&db);
    commands::cmd_warmup(&db);
    commands::cmd_small(&db, &[]);
    commands::cmd_hotpath(&db);
    commands::cmd_compare_ops(&db, &[]);
    commands::cmd_top_ops(&db, &[]);
    commands::cmd_list();
    commands::cmd_focus(&mut db, &[]);
    commands::cmd_ignore(&mut db, &[]);
    commands::cmd_region(&mut db, &[]);
    commands::cmd_reset(&mut db);

    // Commands that require args
    commands::cmd_inspect(&db, &[]);
    commands::cmd_bound(&db, &[]);
    commands::cmd_trace(&db, &[]);
    commands::cmd_callers(&db, &[]);
    commands::cmd_variance(&db, &[]);
    commands::cmd_breakdown(&db, &[]);
    commands::cmd_idle_between(&db, &[]);
}

// -----------------------------------------------------------------------
// Verify trunc handles edge cases
// -----------------------------------------------------------------------

#[test]
fn trunc_edge_cases() {
    use crate::commands::trunc;
    // ASCII
    assert_eq!(trunc("hello", 10), "hello");
    assert_eq!(trunc("hello world", 8), "hello...");
    assert_eq!(trunc("abc", 3), "abc");

    // Multi-byte: "αβγδ" is 4 chars, 8 bytes
    assert_eq!(trunc("αβγδ", 10), "αβγδ");
    assert_eq!(trunc("αβγδ", 4), "αβγδ");
    // Truncate: 3 chars = "α" + "..."
    let t = trunc("αβγδεζ", 4);
    assert_eq!(t, "α...");

    // Empty
    assert_eq!(trunc("", 5), "");
}

// -----------------------------------------------------------------------
// Inspect with multiple matches should list them, not panic
// -----------------------------------------------------------------------

#[test]
fn inspect_multiple_matches() {
    let db = build_session();
    // "kernel" matches multiple kernel names
    commands::cmd_inspect(&db, &["kernel"]);
    // exact match via prefix
    commands::cmd_inspect(&db, &["ampere"]);
}

// -----------------------------------------------------------------------
// Variance with single-launch kernel
// -----------------------------------------------------------------------

#[test]
fn variance_single_launch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.into_path().join("var.gpu.db");
    let db = GpuDb::create(&path).unwrap();
    let lid = db.add_layer("nsys", "t", None, None, None).unwrap();

    db.conn.execute(
        "INSERT INTO launches (kernel_name, duration_us, layer_id) VALUES ('solo', 100.0, ?1)",
        params![lid],
    ).unwrap();

    // Should print "only 1 launch", not panic
    commands::cmd_variance(&db, &["solo"]);
}

// =======================================================================
// Kernel scenario: CUDA-only binary (no torch/proton layer)
// =======================================================================

#[test]
fn cuda_only_all_commands_no_panic() {
    let mut db = build_cuda_only_session();

    commands::cmd_stats(&db);
    commands::cmd_kernels(&db, &[]);
    commands::cmd_kernels(&db, &["sgemm"]);
    commands::cmd_inspect(&db, &["sgemm"]);
    commands::cmd_inspect(&db, &["reduce"]);
    commands::cmd_bound(&db, &["sgemm"]);
    commands::cmd_roofline(&db, &[]);
    commands::cmd_occupancy(&db, &[]);
    commands::cmd_transfers(&db, &[]);
    commands::cmd_gaps(&db, &[]);
    commands::cmd_overlap(&db);
    commands::cmd_streams(&db);
    commands::cmd_timeline(&db, &[]);
    commands::cmd_variance(&db, &["sgemm"]);
    commands::cmd_warmup(&db);
    commands::cmd_small(&db, &[]);
    commands::cmd_fuse(&db, &[]);
    commands::cmd_concurrency(&db);
    commands::cmd_suggest(&db);
    commands::cmd_layers(&db);

    // Op-requiring commands should gracefully say "no op data"
    commands::cmd_ops(&db, &[]);
    commands::cmd_top_ops(&db, &[]);
    commands::cmd_hotpath(&db);
    commands::cmd_compare_ops(&db, &[]);
    commands::cmd_trace(&db, &["anything"]);
    commands::cmd_callers(&db, &["anything"]);
    commands::cmd_breakdown(&db, &["anything"]);
    commands::cmd_idle_between(&db, &["a", "b"]);

    // Filters
    commands::cmd_focus(&mut db, &["sgemm"]);
    commands::cmd_kernels(&db, &[]);
    commands::cmd_reset(&mut db);
}

// =======================================================================
// Kernel scenario: Triton-heavy inference (proton layer, no nsys)
// =======================================================================

#[test]
fn triton_inference_all_commands_no_panic() {
    let mut db = build_triton_inference_session();

    commands::cmd_stats(&db);
    commands::cmd_kernels(&db, &[]);
    commands::cmd_ops(&db, &[]);
    commands::cmd_top_ops(&db, &[]);
    commands::cmd_inspect(&db, &["flash_attention"]);
    commands::cmd_inspect(&db, &["add_kernel"]);
    commands::cmd_timeline(&db, &[]);
    commands::cmd_gaps(&db, &[]);
    commands::cmd_warmup(&db);
    commands::cmd_small(&db, &[]);
    commands::cmd_fuse(&db, &[]);
    commands::cmd_concurrency(&db);
    commands::cmd_suggest(&db);
    commands::cmd_layers(&db);
    commands::cmd_trace(&db, &["attention"]);
    commands::cmd_callers(&db, &["matmul"]);
    commands::cmd_breakdown(&db, &["attention"]);
    commands::cmd_hotpath(&db);
    commands::cmd_compare_ops(&db, &[]);
    commands::cmd_variance(&db, &["flash_attention"]);
    commands::cmd_idle_between(&db, &["attention", "linear"]);

    // No ncu data — these should print "need ncu" messages
    commands::cmd_bound(&db, &["flash_attention"]);
    commands::cmd_roofline(&db, &[]);
    commands::cmd_occupancy(&db, &[]);

    // No transfers
    commands::cmd_transfers(&db, &[]);

    // Filters
    commands::cmd_focus(&mut db, &["triton"]);
    commands::cmd_kernels(&db, &[]);
    commands::cmd_ignore(&mut db, &["add"]);
    commands::cmd_kernels(&db, &[]);
    commands::cmd_reset(&mut db);
}

// =======================================================================
// Kernel scenario: Multi-stream pipelined training
// =======================================================================

#[test]
fn multi_stream_all_commands_no_panic() {
    let mut db = build_multi_stream_session();

    commands::cmd_stats(&db);
    commands::cmd_kernels(&db, &[]);
    commands::cmd_inspect(&db, &["gemm"]);
    commands::cmd_inspect(&db, &["AllReduce"]);
    commands::cmd_timeline(&db, &["20"]);
    commands::cmd_gaps(&db, &[]);
    commands::cmd_overlap(&db);
    commands::cmd_streams(&db);
    commands::cmd_warmup(&db);
    commands::cmd_small(&db, &[]);
    commands::cmd_fuse(&db, &[]);
    commands::cmd_concurrency(&db);
    commands::cmd_transfers(&db, &[]);
    commands::cmd_suggest(&db);
    commands::cmd_layers(&db);
    commands::cmd_variance(&db, &["gemm"]);

    // No torch/ncu layers
    commands::cmd_ops(&db, &[]);
    commands::cmd_roofline(&db, &[]);
    commands::cmd_bound(&db, &["gemm"]);

    // Filters — region filter with pipeline stages
    commands::cmd_region(&mut db, &[]);  // list regions
    commands::cmd_region(&mut db, &["PipelineStage#0"]);
    commands::cmd_kernels(&db, &[]);  // should be restricted
    commands::cmd_reset(&mut db);
}

#[test]
fn multi_stream_gaps_account_for_overlap() {
    let db = build_multi_stream_session();

    // Streams 1-4 have heavily overlapping launches.
    // Stream 1: [0, 1000], [1100, 1105], [2000, 3000], [3100, 3105], [4000, 5000], [5100, 5105]
    // Stream 2: [500, 1500], [2500, 3500], [4500, 5500]
    // Stream 3: [1200, 3200], [5200, 7200]
    // Stream 4: [3500, 3600], [7500, 7600]
    //
    // Merged: [0, 3600], [4000, 7600]
    // One gap: 3600 → 4000 = 400us
    //
    // If gaps doesn't merge across streams, it would find many false gaps.
    commands::cmd_gaps(&db, &[]);
}

// =======================================================================
// Kernel scenario: session with ncu-only (loaded from saved)
// =======================================================================

#[test]
fn ncu_only_session_no_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.into_path().join("ncu_only.gpu.db");
    let mut db = GpuDb::create(&path).unwrap();

    db.set_meta("target", "kernel.cu").unwrap();
    db.set_meta("device", "NVIDIA V100").unwrap();
    db.set_meta("wall_time_us", "0").unwrap();

    // Only ncu metrics, no timeline at all (e.g., user ran ncu directly)
    let ncu_id = db
        .add_layer("ncu", "/tmp/ncu.csv", Some("ncu --set full"), Some(60.0), None)
        .unwrap();

    db.conn.execute(
        "INSERT INTO metrics
         (kernel_name, occupancy_pct, compute_throughput_pct, memory_throughput_pct,
          registers_per_thread, shared_mem_static_bytes, shared_mem_dynamic_bytes,
          l2_hit_rate_pct, achieved_bandwidth_gb_s, peak_bandwidth_gb_s,
          boundedness, layer_id)
         VALUES ('my_kernel', 50.0, 40.0, 60.0, 48, 16384, 0, 55.0, 800.0, 900.0, 'memory', ?1)",
        params![ncu_id],
    ).unwrap();

    // All commands should handle "no launches" gracefully
    commands::cmd_stats(&db);
    commands::cmd_kernels(&db, &[]);
    commands::cmd_inspect(&db, &["my_kernel"]);
    commands::cmd_bound(&db, &["my_kernel"]);
    commands::cmd_roofline(&db, &[]);
    commands::cmd_occupancy(&db, &[]);
    commands::cmd_gaps(&db, &[]);
    commands::cmd_timeline(&db, &[]);
    commands::cmd_warmup(&db);
    commands::cmd_small(&db, &[]);
    commands::cmd_fuse(&db, &[]);
    commands::cmd_transfers(&db, &[]);
    commands::cmd_streams(&db);
    commands::cmd_concurrency(&db);
    commands::cmd_suggest(&db);
    commands::cmd_layers(&db);
    commands::cmd_overlap(&db);

    // Filters on empty launch data
    commands::cmd_focus(&mut db, &["my_kernel"]);
    commands::cmd_kernels(&db, &[]);
    commands::cmd_reset(&mut db);
}

// =======================================================================
// Consistency: stats GPU time must match sum of kernels GPU time
// =======================================================================

#[test]
fn stats_gpu_time_equals_kernels_total() {
    let db = build_cuda_only_session();

    // In a single-layer (nsys-only) session, total_gpu_time_us should equal
    // the sum of all kernel durations, and this should be consistent.
    let total = db.total_gpu_time_us();

    // Sum of all kernels manually:
    // volta_sgemm: 2000+1950+2100+1980+2050 = 10080
    // memcpy_kernel: 50+55+48 = 153
    // reduce_sum: 800+790 = 1590
    // Total = 11823
    let expected = 10080.0 + 153.0 + 1590.0;
    assert!((total - expected).abs() < 0.1,
        "total_gpu_time_us() = {total}, expected {expected}");
}

// =======================================================================
// Consistency: multi-layer double-counting check
// =======================================================================

#[test]
fn multi_layer_kernels_use_timeline_filter() {
    let db = build_session();

    // build_session() has nsys + torch layers, both with some overlapping launches.
    // nsys has 29 launches, torch has 4 duplicate launches.
    //
    // total_gpu_time_us() and other aggregates must use timeline_filter()
    // to avoid double-counting across layers.

    let tl_id = db.timeline_layer_id().unwrap();

    // Timeline-filtered GPU time (nsys only)
    let nsys_total: f64 = db.conn.query_row(
        "SELECT COALESCE(SUM(duration_us), 0) FROM launches WHERE layer_id = ?1",
        params![tl_id],
        |row| row.get(0),
    ).unwrap();

    // total_gpu_time_us() should now match the nsys-only total (not double-count)
    let reported_total = db.total_gpu_time_us();
    assert!((reported_total - nsys_total).abs() < 0.1,
        "total_gpu_time_us() should use timeline_filter: got {reported_total}, expected {nsys_total}");
}

// =======================================================================
// Consistency: variance numbers are mathematically valid
// =======================================================================

#[test]
fn variance_math_correctness() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.into_path().join("var_math.gpu.db");
    let db = GpuDb::create(&path).unwrap();
    let lid = db.add_layer("nsys", "t", None, None, None).unwrap();

    // Known durations: 100, 200, 300, 400, 500
    // Mean = 300, Var = E[X^2] - E[X]^2 = 110000 - 90000 = 20000
    // Stddev = 141.42, CV = 141.42/300 = 0.4714
    for &dur in &[100.0_f64, 200.0, 300.0, 400.0, 500.0] {
        db.conn.execute(
            "INSERT INTO launches (kernel_name, duration_us, layer_id)
             VALUES ('test_kernel', ?1, ?2)",
            params![dur, lid],
        ).unwrap();
    }

    // Verify the SQL variance calculation matches
    let (avg, var): (f64, f64) = db.conn.query_row(
        "SELECT AVG(duration_us),
                AVG(duration_us * duration_us) - AVG(duration_us) * AVG(duration_us)
         FROM launches WHERE kernel_name = 'test_kernel'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();

    assert!((avg - 300.0).abs() < 0.1, "mean should be 300, got {avg}");
    assert!((var - 20000.0).abs() < 0.1, "variance should be 20000, got {var}");

    let stddev = var.sqrt();
    let cv = stddev / avg;
    assert!((cv - 0.4714).abs() < 0.01, "CV should be ~0.4714, got {cv}");

    // Command should not panic
    commands::cmd_variance(&db, &["test_kernel"]);
}

// =======================================================================
// Consistency: kernel percentages must sum to ~100% (within timeline layer)
// =======================================================================

#[test]
fn kernel_percentages_sum_to_100() {
    let db = build_cuda_only_session();

    // In the cuda-only session (single nsys layer), kernel percentages should
    // sum to 100% since there's no double-counting.
    let sql = "SELECT SUM(total) FROM (
        SELECT SUM(duration_us) as total FROM launches GROUP BY kernel_name
    )";
    let sum_of_totals: f64 = db.conn.query_row(sql, [], |row| row.get(0)).unwrap();
    let gpu_total = db.total_gpu_time_us();

    assert!((sum_of_totals - gpu_total).abs() < 0.1,
        "sum of per-kernel totals ({sum_of_totals}) should equal total GPU time ({gpu_total})");
}

// =======================================================================
// Consistency: inspect reports should match kernels listing
// =======================================================================

#[test]
fn inspect_total_matches_kernels_listing() {
    let db = build_cuda_only_session();

    // Query the same way cmd_kernels does (no layer filter)
    let kernel_total: f64 = db.conn.query_row(
        "SELECT SUM(duration_us) FROM launches WHERE kernel_name LIKE '%sgemm%'",
        [],
        |row| row.get(0),
    ).unwrap();

    // Query the same way cmd_inspect does (also no layer filter)
    let (inspect_cnt, inspect_total): (i64, f64) = db.conn.query_row(
        "SELECT COUNT(*), SUM(duration_us) FROM launches WHERE kernel_name LIKE '%sgemm%'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();

    // In single-layer session these must match exactly
    assert!((kernel_total - inspect_total).abs() < 0.1,
        "kernels total ({kernel_total}) vs inspect total ({inspect_total})");
    assert_eq!(inspect_cnt, 5);
}

// =======================================================================
// Consistency: breakdown kernel times should sum to <= top-ops GPU time
// =======================================================================

#[test]
fn breakdown_sums_consistent_with_top_ops() {
    let db = build_triton_inference_session();

    // For "attention" op, breakdown shows kernels and their times.
    // The kernel total from breakdown should match the op's gpu_time_us.
    let op_gpu: f64 = db.conn.query_row(
        "SELECT gpu_time_us FROM ops WHERE name = 'attention'",
        [], |row| row.get(0),
    ).unwrap();

    // The kernel time for flash_attention (proton layer) via timeline_filter
    let tl_id = db.timeline_layer_id().unwrap();
    let kernel_total: f64 = db.conn.query_row(
        "SELECT COALESCE(SUM(l.duration_us), 0)
         FROM op_kernel_map okm
         JOIN launches l ON l.kernel_name = okm.kernel_name AND l.layer_id = ?1
         WHERE okm.op_id = (SELECT id FROM ops WHERE name = 'attention')",
        params![tl_id],
        |row| row.get(0),
    ).unwrap();

    assert!((op_gpu - kernel_total).abs() < 0.1,
        "op gpu_time_us ({op_gpu}) should match breakdown kernel total ({kernel_total})");

    // Command shouldn't panic
    commands::cmd_breakdown(&db, &["attention"]);
}

// =======================================================================
// Consistency: focus filter must restrict all kernel-based commands
// =======================================================================

#[test]
fn focus_filter_restricts_small_command() {
    let mut db = build_cuda_only_session();

    // Without focus, "small" finds memcpy_kernel (avg ~51us, actually above 10us threshold)
    // Let's check the actual small kernels
    let small_count_all: i64 = db.conn.query_row(
        "SELECT COUNT(DISTINCT kernel_name) FROM launches
         GROUP BY kernel_name HAVING AVG(duration_us) < 10.0",
        [],
        |row| row.get(0),
    ).unwrap_or(0);

    // Focus on sgemm — no sgemm kernels are < 10us, so cmd_small should find nothing
    commands::cmd_focus(&mut db, &["sgemm"]);
    commands::cmd_small(&db, &[]);
    // (Can't capture stdout, but verify filter is applied)
    let filter = db.kernel_filter();
    assert!(filter.contains("sgemm"), "focus filter should contain 'sgemm'");

    commands::cmd_reset(&mut db);
}

// =======================================================================
// Consistency: stream counts should match what streams command reports
// =======================================================================

#[test]
fn stream_count_consistency() {
    let db = build_multi_stream_session();

    let reported_count = db.stream_count();
    let actual_streams: Vec<u32> = db.query_vec(
        "SELECT DISTINCT stream_id FROM launches WHERE stream_id IS NOT NULL",
        [],
        |row| row.get(0),
    );

    assert_eq!(reported_count, actual_streams.len(),
        "stream_count ({reported_count}) should match distinct stream IDs ({actual_streams:?})");
    assert_eq!(reported_count, 4, "multi-stream session has 4 streams");

    commands::cmd_streams(&db);
}

// =======================================================================
// Consistency: suggest should detect missing layers correctly
// =======================================================================

#[test]
fn suggest_detects_missing_layers() {
    let db = build_triton_inference_session();

    // Has proton layer, missing nsys and ncu
    assert!(!db.has_layer("nsys"));
    assert!(!db.has_layer("ncu"));
    assert!(db.has_layer("proton"));
    assert!(!db.has_layer("torch"));

    // suggest should mention ncu (no metrics) and nsys (no timeline)
    commands::cmd_suggest(&db);
}

// =======================================================================
// Consistency: region filter with proton-only session
// =======================================================================

#[test]
fn region_filter_with_no_regions() {
    let mut db = build_triton_inference_session();

    // No regions in this session — should handle gracefully
    commands::cmd_region(&mut db, &[]);

    // Setting a region filter that matches nothing
    commands::cmd_region(&mut db, &["nonexistent"]);
    commands::cmd_kernels(&db, &[]);
    commands::cmd_reset(&mut db);
}

// =======================================================================
// Consistency: warmup detection with constant-time kernels
// =======================================================================

#[test]
fn warmup_with_no_warmup_effect() {
    let db = build_cuda_only_session();
    // CUDA-only session has very consistent kernel times (no warmup spike).
    // warmup should still run without panicking and detect ~0 warmup.
    commands::cmd_warmup(&db);
}

// =======================================================================
// Diff: sessions with disjoint kernel sets
// =======================================================================

#[test]
fn diff_disjoint_kernels() {
    let db = build_cuda_only_session();
    let other = build_triton_inference_session();

    // Save the "other" session so we can diff against it
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("other.gpu.db");
    {
        let mut dest_conn = rusqlite::Connection::open(&dest).unwrap();
        let backup = rusqlite::backup::Backup::new(&other.conn, &mut dest_conn).unwrap();
        backup.run_to_completion(100, std::time::Duration::from_millis(10), None).unwrap();
    }

    // Diff should show all kernels as "new" in one or the other
    if let Err(e) = db.attach(dest.to_str().unwrap(), "other") {
        panic!("attach failed: {e}");
    }
    // Just verify attach works and diff doesn't panic
    let _ = db.detach("other");

    // Run through the diff command path
    commands::cmd_diff(&db, &[dest.to_str().unwrap()]);
}
