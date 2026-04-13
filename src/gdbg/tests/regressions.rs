//! Regression tests for bugs caught during end-to-end audit against real
//! CUDA kernels.  Each test calls the actual production function that was
//! fixed, so reverting the fix will break the test.

use crate::commands::{compute_gpu_gaps, compute_xfer_kernel_overlap, detect_warmup_count};
use crate::db::{GpuDb, escape_sql_like};
use crate::parsers::nsys::import_wall_time;
use rusqlite::params;
use tempfile::TempDir;

// -----------------------------------------------------------------------
// Shared builder: populate DB but do NOT set wall_time_us (we want to test
// the parser's computation, not the fixture's).
// -----------------------------------------------------------------------

fn make_db(
    kernels: &[(&str, f64, f64, u32)],    // (name, start, dur, stream)
    transfers: &[(&str, f64, f64, i64)],  // (kind, start, dur, bytes)
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
    let (db, _d) = make_db(
        &[("k", 100.0, 50.0, 7), ("k", 200.0, 50.0, 7)],
        &[],
    );
    import_wall_time(&db.conn).unwrap();
    let wall: f64 = db.meta("wall_time_us").parse().unwrap();
    assert!((wall - 150.0).abs() < 0.01, "wall = 250 - 100 = 150us, got {wall}");
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
    let (db, _d) = make_db(
        &[("k", 0.0, 100.0, 7), ("k", 5100.0, 100.0, 7)],
        &[],
    );
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
    let overlap = compute_xfer_kernel_overlap(&db);
    assert!(overlap < 0.01, "serialized → 0 overlap, got {overlap}us");
}

#[test]
fn overlap_positive_when_concurrent() {
    // Kernel 0..1000, transfer 500..1500 → 500us overlap.
    let (db, _d) = make_db(
        &[("k", 0.0, 1000.0, 7)],
        &[("H2D", 500.0, 1000.0, 1_000_000)],
    );
    let overlap = compute_xfer_kernel_overlap(&db);
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
    let overlap = compute_xfer_kernel_overlap(&db);
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
    let pat = format!("%{}%", escape_sql_like("vector_add"));
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
        &[("op_50%_done", 0.0, 100.0, 7), ("op_completely_done", 100.0, 100.0, 7)],
        &[],
    );
    let pat = format!("%{}%", escape_sql_like("50%"));
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
    // kernel_filter() is used directly in WHERE clauses by cmd_kernels etc.
    let filter = db.kernel_filter();
    let sql = format!("SELECT COUNT(*) FROM launches WHERE {filter}");
    let count: i64 = db.conn.query_row(&sql, [], |row| row.get(0)).unwrap();
    assert_eq!(count, 1, "focus='vector_add' must match 1 launch, got {count}");
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
    assert_eq!(detect_warmup_count(&durs), 0, "under 20% margin should not flag warmup");

    // First launch is 25% slower — should count as warmup.
    let durs = vec![125.0, 100.0, 100.0, 100.0, 100.0, 100.0];
    assert_eq!(detect_warmup_count(&durs), 1, "over 20% margin should flag warmup");
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
            ("k", 200.0, 100.0, 7),     // 100us gap
            ("k", 500.0, 100.0, 7),     // 200us gap
            ("k", 900.0, 100.0, 7),     // 300us gap
            ("k", 1400.0, 100.0, 7),    // 400us gap
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
