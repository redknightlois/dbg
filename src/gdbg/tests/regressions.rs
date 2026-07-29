//! Regression tests for bugs caught during end-to-end audit against real
//! CUDA kernels.  Each test calls the actual production function that was
//! fixed, so reverting the fix will break the test.

use crate::commands::{
    compute_gpu_gaps, detect_warmup_count, find_hottest_window, xfer_kernel_overlap,
};
use crate::db::{GpuDb, like_param};
use crate::parsers::nsys::import_wall_time;
use rusqlite::params;
use tempfile::TempDir;

// -----------------------------------------------------------------------
// Shared builder: populate DB but do NOT set wall_time_us (we want to test
// the parser's computation, not the fixture's).
// -----------------------------------------------------------------------

fn make_db(
    kernels: &[(&str, f64, f64, u32)],   // (name, start, dur, stream)
    transfers: &[(&str, f64, f64, i64)], // (kind, start, dur, bytes)
) -> (GpuDb, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.gpu.db");
    let db = GpuDb::create(&path).unwrap();
    db.set_meta("target", "./bin").unwrap();

    let layer_id = db
        .add_layer("nsys", "/tmp/t.nsys-rep", None, Some(1.0), None)
        .unwrap();

    for &(name, start, dur, sid) in kernels {
        db.conn
            .execute(
                "INSERT INTO launches (kernel_name, duration_us, start_us, stream_id, layer_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![name, dur, start, sid, layer_id],
            )
            .unwrap();
    }

    for &(kind, start, dur, bytes) in transfers {
        db.conn
            .execute(
                "INSERT INTO transfers (kind, bytes, duration_us, start_us, stream_id, layer_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![kind, bytes, dur, start, 7_u32, layer_id],
            )
            .unwrap();
    }

    (db, dir)
}

#[test]
fn kernel_identity_correlates_torch_nsys_and_ncu_spellings() {
    let dir = tempfile::tempdir().unwrap();
    let db = GpuDb::create(&dir.path().join("identity.gpu.db")).unwrap();
    let nsys = db.add_layer("nsys", "trace", None, None, None).unwrap();
    let torch = db.add_layer("torch", "trace", None, None, None).unwrap();
    let ncu = db.add_layer("ncu", "metrics", None, None, None).unwrap();

    db.conn
        .execute(
            "INSERT INTO launches (kernel_name, duration_us, start_us, layer_id)
             VALUES ('void fused::kernel(float)', 10.0, 1.0, ?1)",
            params![nsys],
        )
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO launches (kernel_name, duration_us, start_us, layer_id)
             VALUES ('fused::kernel', 9.0, 1.0, ?1)",
            params![torch],
        )
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO metrics (kernel_name, occupancy_pct, layer_id)
             VALUES ('fused::kernel(float)', 80.0, ?1)",
            params![ncu],
        )
        .unwrap();

    db.normalize_kernel_names().unwrap();

    let names: Vec<String> = db.query_vec(
        "SELECT DISTINCT kernel_name FROM launches ORDER BY kernel_name",
        [],
        |row| row.get(0),
    );
    assert_eq!(names, vec!["fused::kernel"]);
    assert_eq!(db.kernels_with_metrics(), 1);
    assert!(db.check_kernel_consistency().is_empty());
}

#[test]
fn legacy_metric_normalization_merges_complementary_values() {
    let dir = tempfile::tempdir().unwrap();
    let db = GpuDb::create(&dir.path().join("metric-merge.gpu.db")).unwrap();
    let ncu = db.add_layer("ncu", "metrics", None, None, None).unwrap();

    db.conn
        .execute(
            "INSERT INTO metrics (kernel_name, occupancy_pct, layer_id)
             VALUES ('void fused::kernel(float)', 80.0, ?1)",
            params![ncu],
        )
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO metrics (kernel_name, compute_throughput_pct, layer_id)
             VALUES ('fused::kernel [12]', 65.0, ?1)",
            params![ncu],
        )
        .unwrap();

    db.normalize_kernel_names().unwrap();

    let values: (String, Option<f64>, Option<f64>) = db
        .conn
        .query_row(
            "SELECT kernel_name, occupancy_pct, compute_throughput_pct
             FROM metrics WHERE layer_id = ?1",
            params![ncu],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(values.0, "fused::kernel");
    assert_eq!(values.1, Some(80.0));
    assert_eq!(values.2, Some(65.0));
}

#[test]
fn kernel_mapping_normalization_deduplicates_null_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db = GpuDb::create(&dir.path().join("mapping-merge.gpu.db")).unwrap();
    let torch = db.add_layer("torch", "trace", None, None, None).unwrap();
    db.conn
        .execute(
            "INSERT INTO ops (name, layer_id) VALUES ('fused op', ?1)",
            params![torch],
        )
        .unwrap();
    for name in [
        "void fused::kernel(float)",
        "fused::kernel",
        "fused::kernel",
    ] {
        db.conn
            .execute(
                "INSERT INTO op_kernel_map (op_id, kernel_name, launch_id)
                 VALUES (1, ?1, NULL)",
                params![name],
            )
            .unwrap();
    }

    db.normalize_kernel_names().unwrap();

    let mappings: Vec<(i64, String, Option<i64>)> = db.query_vec(
        "SELECT op_id, kernel_name, launch_id FROM op_kernel_map",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    );
    assert_eq!(mappings, vec![(1, "fused::kernel".to_string(), None)]);
}

// =======================================================================
// Bug 4: import_wall_time must include transfer span
// =======================================================================

#[test]
fn wall_time_includes_transfers() {
    // Transfer runs 0..1000; kernel runs 2000..2500. Span = 2500us.
    // With the old "launches only" logic this would have returned 500us.
    let (db, _d) = make_db(
        &[("k", 2000.0, 500.0, 7)],
        &[("H2D", 0.0, 1000.0, 1_000_000)],
    );

    import_wall_time(&db.conn).unwrap();
    let wall: f64 = db.meta("wall_time_us").parse().unwrap();

    assert!(
        (wall - 2500.0).abs() < 0.01,
        "wall_time must span transfer start → kernel end = 2500us, got {wall}"
    );
}

#[test]
fn wall_time_launches_only_when_no_transfers() {
    let (db, _d) = make_db(&[("k", 100.0, 50.0, 7), ("k", 200.0, 50.0, 7)], &[]);
    import_wall_time(&db.conn).unwrap();
    let wall: f64 = db.meta("wall_time_us").parse().unwrap();
    assert!(
        (wall - 150.0).abs() < 0.01,
        "wall = 250 - 100 = 150us, got {wall}"
    );
}

// =======================================================================
// Bug 7: compute_gpu_gaps must exclude transfer-busy time
// =======================================================================

#[test]
fn gaps_exclude_transfer_busy_time() {
    // Kernel at 0..100, kernel at 500..600, transfer at 100..500 covers gap.
    // Old code (launches only): reports 400us gap.
    // Fixed code: reports 0 gap because transfer fills it.
    let (db, _d) = make_db(
        &[("k", 0.0, 100.0, 7), ("k", 500.0, 100.0, 7)],
        &[("H2D", 100.0, 400.0, 1000)],
    );
    let gaps = compute_gpu_gaps(&db);
    let total: f64 = gaps.iter().map(|g| g.1).sum();
    assert!(
        total < 1.0,
        "GPU is always busy (kernel→transfer→kernel); compute_gpu_gaps should report ~0 gap, got {total}us across {} gaps",
        gaps.len()
    );
}

#[test]
fn gaps_detect_real_idle_between_phases() {
    // Kernel 0..100, big idle, kernel 5100..5200. Real 5000us idle.
    let (db, _d) = make_db(&[("k", 0.0, 100.0, 7), ("k", 5100.0, 100.0, 7)], &[]);
    let gaps = compute_gpu_gaps(&db);
    let total: f64 = gaps.iter().map(|g| g.1).sum();
    assert!(
        (total - 5000.0).abs() < 1.0,
        "should detect 5000us idle, got {total}us"
    );
}

// =======================================================================
// Bug 3: compute_xfer_kernel_overlap must measure real concurrent time
// =======================================================================

#[test]
fn overlap_zero_when_serialized() {
    // Transfer 0..1000, kernels 2000..3000. No overlap.
    let (db, _d) = make_db(
        &[("k", 2000.0, 500.0, 7), ("k", 2500.0, 500.0, 7)],
        &[("H2D", 0.0, 1000.0, 1_000_000)],
    );
    let (_, overlap) = xfer_kernel_overlap(&db, None);
    assert!(overlap < 0.01, "serialized → 0 overlap, got {overlap}us");
}

#[test]
fn overlap_positive_when_concurrent() {
    // Kernel 0..1000, transfer 500..1500 → 500us overlap.
    let (db, _d) = make_db(
        &[("k", 0.0, 1000.0, 7)],
        &[("H2D", 500.0, 1000.0, 1_000_000)],
    );
    let (_, overlap) = xfer_kernel_overlap(&db, None);
    assert!(
        (overlap - 500.0).abs() < 0.01,
        "concurrent 500us should yield overlap=500us, got {overlap}"
    );
}

#[test]
fn overlap_across_multiple_kernels() {
    // Two kernels covering 0..500, 1000..1500; transfer 400..1100 overlaps both.
    //   0..500 ∩ 400..1100 = 100us
    //   1000..1500 ∩ 400..1100 = 100us
    //   total = 200us
    let (db, _d) = make_db(
        &[("k", 0.0, 500.0, 7), ("k", 1000.0, 500.0, 7)],
        &[("H2D", 400.0, 700.0, 1000)],
    );
    let (_, overlap) = xfer_kernel_overlap(&db, None);
    assert!(
        (overlap - 200.0).abs() < 0.01,
        "expected 200us overlap (100+100), got {overlap}"
    );
}

// =======================================================================
// Bug 8: escape_sql_like + LIKE ESCAPE must match names with underscores
// =======================================================================

#[test]
fn sql_like_with_underscore_matches_literal() {
    // Before: escape_sql_like('vector_add') → 'vector\_add'; LIKE without
    // ESCAPE '\' treats that as literal backslash+underscore → 0 matches.
    // After: `_` is no longer escaped, so pattern matches.
    let (db, _d) = make_db(
        &[
            ("vector_add(float *)", 0.0, 100.0, 7),
            ("matmul(float *)", 100.0, 100.0, 7),
        ],
        &[],
    );
    let pat = like_param("vector_add");
    let count: i64 = db
        .conn
        .query_row(
            r"SELECT COUNT(*) FROM launches WHERE kernel_name LIKE ?1 ESCAPE '\'",
            [&pat],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "'vector_add' must match 1 launch, got {count}");
}

#[test]
fn sql_like_percent_still_escaped() {
    // '%' must still be escaped; a pattern '50%' should only match literal "50%".
    let (db, _d) = make_db(
        &[
            ("op_50%_done", 0.0, 100.0, 7),
            ("op_completely_done", 100.0, 100.0, 7),
        ],
        &[],
    );
    let pat = like_param("50%");
    let count: i64 = db
        .conn
        .query_row(
            r"SELECT COUNT(*) FROM launches WHERE kernel_name LIKE ?1 ESCAPE '\'",
            [&pat],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "literal '50%' must match 1 launch, got {count}");
}

#[test]
fn focus_filter_matches_underscored_kernel() {
    let (mut db, _d) = make_db(
        &[
            ("vector_add", 0.0, 100.0, 7),
            ("vector_mul", 200.0, 100.0, 7),
            ("matmul_naive", 400.0, 100.0, 7),
        ],
        &[],
    );
    db.focus = Some("vector_add".to_string());
    // kernel_filter_params() is used by cmd_kernels and other filtered commands.
    let filter = db.kernel_filter_params();
    let sql = format!("SELECT COUNT(*) FROM launches WHERE {}", filter.clause());
    let count: i64 = db
        .conn
        .query_row(&sql, rusqlite::params_from_iter(filter.params()), |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        count, 1,
        "focus='vector_add' must match 1 launch, got {count}"
    );
}

// =======================================================================
// Bug 1+2: detect_warmup_count must require real margin, be per-kernel
// =======================================================================

#[test]
fn warmup_no_false_positive_on_stable_kernel() {
    // Durations within 3% — not warmup.  Before the fix the algorithm
    // would label the first 5 as warmup.
    let durs: Vec<f64> = (0..10).map(|i| 100.0 + (i as f64) * 0.3).collect();
    let n = detect_warmup_count(&durs);
    assert_eq!(n, 0, "stable series should report 0 warmup, got {n}");
}

#[test]
fn warmup_detects_slow_leading_launches() {
    // 3x slower leading launches, then stable.
    let durs = vec![300.0, 300.0, 100.0, 100.0, 100.0, 100.0, 100.0];
    let n = detect_warmup_count(&durs);
    assert_eq!(n, 2, "two slow leading launches, got {n}");
}

#[test]
fn warmup_threshold_is_20_percent() {
    // First launch is only 15% slower than median — should not count.
    let durs = vec![115.0, 100.0, 100.0, 100.0, 100.0, 100.0];
    assert_eq!(
        detect_warmup_count(&durs),
        0,
        "under 20% margin should not flag warmup"
    );

    // First launch is 25% slower — should count as warmup.
    let durs = vec![125.0, 100.0, 100.0, 100.0, 100.0, 100.0];
    assert_eq!(
        detect_warmup_count(&durs),
        1,
        "over 20% margin should flag warmup"
    );
}

// =======================================================================
// Bug 5: gaps total must be the sum across ALL gaps, not a truncated set
// =======================================================================
//
// compute_gpu_gaps returns every gap; the display code then truncates for
// presentation.  The test below verifies the total is computed over the
// full set.

#[test]
fn gaps_total_across_all_gaps() {
    // Four kernels with gaps 100, 200, 300, 400us — total 1000us.
    let (db, _d) = make_db(
        &[
            ("k", 0.0, 100.0, 7),
            ("k", 200.0, 100.0, 7),  // 100us gap
            ("k", 500.0, 100.0, 7),  // 200us gap
            ("k", 900.0, 100.0, 7),  // 300us gap
            ("k", 1400.0, 100.0, 7), // 400us gap
        ],
        &[],
    );
    let gaps = compute_gpu_gaps(&db);
    let total: f64 = gaps.iter().map(|g| g.1).sum();
    assert_eq!(gaps.len(), 4, "4 gaps expected, got {}", gaps.len());
    assert!(
        (total - 1000.0).abs() < 0.01,
        "sum of all gaps = 100+200+300+400 = 1000us, got {total}"
    );
}

// =======================================================================
// Bug 6: hotspot must evaluate both {sᵢ} and {eᵢ − W} candidate windows
// =======================================================================
//
// The busy function f(w) is piecewise linear with breakpoints at every
// launch start AND every launch-end-minus-window-width.  A start-only sweep
// misses the peak when concurrent launches overlap mid-way between starts.

#[test]
fn hotspot_handles_overlapping_streams() {
    // Two launches on different streams:
    //   A: [0, 100]
    //   B: [80, 120]
    // Window W = 50.
    //   w=0:  busy = 50  (A only)
    //   w=80: busy = 60  (A[80..100]=20  +  B[80..120]=40)
    //   w=50: busy = 70  (A[50..100]=50  +  B[80..100]=20)  ← TRUE MAX
    // w=50 is exactly e_A − W = 100 − 50, a breakpoint the start-only sweep misses.
    let intervals = vec![(0.0, 100.0), (80.0, 40.0)];
    let (busy, w_start, _, _) = find_hottest_window(&intervals, 50.0);
    assert!(
        (busy - 70.0).abs() < 0.01,
        "expected busy=70 at w=50, got busy={busy} at w={w_start}"
    );
    assert!(
        (w_start - 50.0).abs() < 0.01,
        "expected w_start=50, got {w_start}"
    );
}

#[test]
fn hotspot_empty_and_degenerate() {
    // Empty input → zeros.
    let (b, w, lo, hi) = find_hottest_window(&[], 100.0);
    assert_eq!((b, w, lo, hi), (0.0, 0.0, 0, 0));

    // Single launch, window wider than it → captures full duration.
    let intervals = vec![(10.0, 40.0)];
    let (b, _, _, _) = find_hottest_window(&intervals, 100.0);
    assert!(
        (b - 40.0).abs() < 0.01,
        "single launch fully inside → busy == its duration"
    );

    // Zero/negative window → zeros (defensive).
    let (b, _, _, _) = find_hottest_window(&intervals, 0.0);
    assert_eq!(b, 0.0);
}
