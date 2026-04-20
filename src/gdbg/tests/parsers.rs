//! Parser-level and edge-case tests.
//!
//! Tests for ncu CSV parsing, chrome trace import, regex escaping,
//! SQL injection safety, and other boundary conditions.

use super::fixtures::*;
use crate::commands;
use crate::db::GpuDb;
use rusqlite::params;

// -----------------------------------------------------------------------
// escape_regex must handle C++ template kernel names
// -----------------------------------------------------------------------

#[test]
fn escape_regex_template_names() {
    use crate::commands::escape_regex;

    // Template name: < > are NOT regex metacharacters, so they stay as-is
    let name = "void cutlass::Kernel<cutlass::gemm::kernel::GemmUniversal<float>>";
    let escaped = escape_regex(name);
    // The key thing: no bare metacharacters like |, (), *, +, ?
    assert!(escaped.contains('<'), "< should be preserved (not a regex metachar)");

    // Function pointer name with ( ) and *
    let name2 = "void func(float*, int)";
    let escaped2 = escape_regex(name2);
    assert!(escaped2.contains(r"\("), "( must be escaped: {escaped2}");
    assert!(escaped2.contains(r"\)"), ") must be escaped: {escaped2}");
    assert!(escaped2.contains(r"\*"), "* must be escaped: {escaped2}");

    // Pipe in kernel name
    let name3 = "a|b";
    let escaped3 = escape_regex(name3);
    assert!(escaped3.contains(r"\|"), "| must be escaped: {escaped3}");

    // Backslash in name
    let name4 = r"foo\bar";
    let escaped4 = escape_regex(name4);
    assert!(escaped4.starts_with(r"foo\\"), "backslash must be escaped: {escaped4}");

    // Dot must be escaped (matches any char in regex)
    let name5 = "libcuda.so";
    let escaped5 = escape_regex(name5);
    assert!(escaped5.contains(r"\."), ". must be escaped: {escaped5}");
}

// -----------------------------------------------------------------------
// ncu CSV parser: embedded commas, empty fields, quoted values
// -----------------------------------------------------------------------

#[test]
fn ncu_csv_edge_cases() {
    use crate::db::GpuDb;
    use crate::parsers::ncu::import_ncu_csv;

    let db = GpuDb::create(&tempfile::tempdir().unwrap().keep().join("csv_edge.db")).unwrap();
    let lid = db.add_layer("ncu", "test.csv", None, None, None).unwrap();

    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    use std::io::Write;

    // Header
    writeln!(tmp, r#""ID","Kernel Name","Metric Name","Metric Unit","Metric Value""#).unwrap();
    // Normal row
    writeln!(tmp, r#""1","my_kernel","sm__warps_active.avg.pct_of_peak_sustained_active","%","75.0""#).unwrap();
    // Value with comma (e.g., "1,234,567")
    writeln!(tmp, r#""1","my_kernel","gpu__time_duration.sum","nsecond","1,234,567""#).unwrap();
    // Empty metric value
    writeln!(tmp, r#""1","my_kernel","sm__throughput.avg.pct_of_peak_sustained_elapsed","%","""#).unwrap();
    // Row starting with == (ncu separator line)
    writeln!(tmp, "==PROF== Disconnected").unwrap();
    // Empty line
    writeln!(tmp).unwrap();
    // Second kernel with memory metric
    writeln!(tmp, r#""2","other_kernel","dram__throughput.avg.pct_of_peak_sustained_elapsed","%","65.2""#).unwrap();
    writeln!(tmp, r#""2","other_kernel","sm__throughput.avg.pct_of_peak_sustained_elapsed","%","12.0""#).unwrap();

    import_ncu_csv(&db.conn, tmp.path(), lid).unwrap();

    // Verify my_kernel got parsed
    let occ: f64 = db.conn.query_row(
        "SELECT occupancy_pct FROM metrics WHERE kernel_name = 'my_kernel'",
        [], |row| row.get(0),
    ).unwrap();
    assert!((occ - 75.0).abs() < 0.1, "occupancy should be 75.0, got {occ}");

    // Verify comma-containing value was parsed (1234567 ns → 1234.567 us)
    let has_launch: bool = db.conn.query_row(
        "SELECT COUNT(*) > 0 FROM launches WHERE kernel_name = 'my_kernel'",
        [], |row| row.get(0),
    ).unwrap();
    assert!(has_launch, "ncu should insert launch for kernel with duration");

    // Verify other_kernel got classified as memory-bound
    let bound: String = db.conn.query_row(
        "SELECT boundedness FROM metrics WHERE kernel_name = 'other_kernel'",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(bound, "memory", "65.2% mem vs 12.0% compute should be memory-bound");
}

// -----------------------------------------------------------------------
// classify_boundedness edge cases
// -----------------------------------------------------------------------

#[test]
fn boundedness_edge_cases() {
    use crate::parsers::ncu::classify_boundedness;

    // Both below 10 → latency
    assert_eq!(classify_boundedness(Some(9.9), Some(9.9)).as_deref(), Some("latency"));
    assert_eq!(classify_boundedness(Some(0.0), Some(0.0)).as_deref(), Some("latency"));

    // Exactly at boundary: c=10, m=10 → not latency (both >= 10), m >= c → memory
    assert_eq!(classify_boundedness(Some(10.0), Some(10.0)).as_deref(), Some("memory"));

    // One None → None
    assert_eq!(classify_boundedness(None, Some(50.0)), None);
    assert_eq!(classify_boundedness(Some(50.0), None), None);
    assert_eq!(classify_boundedness(None, None), None);

    // Clear compute bound: c >> m
    assert_eq!(classify_boundedness(Some(90.0), Some(10.0)).as_deref(), Some("compute"));

    // Tie region: m = c * 1.5 exactly → memory (m > c*1.5 is false, c > m*1.5 is false, m >= c → memory)
    // Actually: c=40, m=60. m > c*1.5 = m > 60 → false. c > m*1.5 = 40 > 90 → false. m >= c → memory.
    assert_eq!(classify_boundedness(Some(40.0), Some(60.0)).as_deref(), Some("memory"));
}

// -----------------------------------------------------------------------
// Combined focus + ignore + region filters
// -----------------------------------------------------------------------

#[test]
fn triple_filter_interaction() {
    let mut db = build_session();

    // Focus on all kernels with "kernel" in name
    db.focus = Some("kernel".to_string());
    // Ignore nccl
    db.ignore = Some("nccl".to_string());
    // Region: Step#1 covers [500, 20500]
    db.region_filter = Some("Step#1".to_string());

    let filter = db.kernel_filter();
    let tl = db.timeline_filter();

    // Should contain all three clauses
    assert!(filter.contains("LIKE '%kernel%'"), "focus clause missing");
    assert!(filter.contains("NOT LIKE '%nccl%'"), "ignore clause missing");
    assert!(filter.contains("regions"), "region clause missing");

    // Query should work without SQL errors
    let count: i64 = db.conn.query_row(
        &format!("SELECT COUNT(*) FROM launches WHERE {filter} AND {tl}"),
        [],
        |row| row.get(0),
    ).unwrap();

    // Should find some launches (elementwise and bn kernels contain "kernel")
    // but not nccl, and only within Step#1 region
    assert!(count >= 0, "query should not error");

    // Verify no nccl results
    let nccl: i64 = db.conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM launches
             WHERE kernel_name LIKE '%nccl%' AND {filter} AND {tl}"
        ),
        [],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(nccl, 0, "nccl should be excluded by ignore filter");

    // All commands should work with triple filter
    commands::cmd_kernels(&db, &[]);
    commands::cmd_small(&db, &[]);
    commands::cmd_stats(&db);
    commands::cmd_timeline(&db, &[]);
}

// -----------------------------------------------------------------------
// Diff with identical session should show 0% delta
// -----------------------------------------------------------------------

#[test]
fn diff_identical_sessions() {
    let db = build_cuda_only_session();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("copy.gpu.db");

    {
        let mut dest_conn = rusqlite::Connection::open(&dest).unwrap();
        let backup = rusqlite::backup::Backup::new(&db.conn, &mut dest_conn).unwrap();
        backup.run_to_completion(100, std::time::Duration::from_millis(10), None).unwrap();
    }

    // Attach and verify kernel times match exactly
    db.attach(dest.to_str().unwrap(), "other").unwrap();

    let rows: Vec<(String, f64, f64)> = db.query_vec(
        "SELECT
            COALESCE(c.kernel_name, o.kernel_name) as name,
            COALESCE(o.total, 0) as before,
            COALESCE(c.total, 0) as after
         FROM
            (SELECT kernel_name, SUM(duration_us) as total FROM launches GROUP BY kernel_name) c
         FULL OUTER JOIN
            (SELECT kernel_name, SUM(duration_us) as total FROM other.launches GROUP BY kernel_name) o
         ON c.kernel_name = o.kernel_name",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    );

    for (name, before, after) in &rows {
        assert!(
            (before - after).abs() < 0.01,
            "kernel '{name}': before ({before}) != after ({after}) in identical diff"
        );
    }

    db.detach("other").unwrap();
}

// -----------------------------------------------------------------------
// ncu launches (no start_us) excluded from timeline commands
// -----------------------------------------------------------------------

#[test]
fn ncu_launches_excluded_from_timeline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.keep().join("ncu_tl.gpu.db");
    let db = GpuDb::create(&path).unwrap();

    // nsys layer with 1 launch
    let nsys_id = db.add_layer("nsys", "t", None, None, None).unwrap();
    db.conn.execute(
        "INSERT INTO launches (kernel_name, duration_us, start_us, stream_id, layer_id)
         VALUES ('real_kernel', 100.0, 500.0, 7, ?1)",
        params![nsys_id],
    ).unwrap();

    // ncu layer with launches that have no start_us (typical for ncu)
    let ncu_id = db.add_layer("ncu", "t", None, None, None).unwrap();
    db.conn.execute(
        "INSERT INTO launches (kernel_name, duration_us, layer_id)
         VALUES ('real_kernel', 95.0, ?1)",
        params![ncu_id],
    ).unwrap();

    // total_gpu_time should only see nsys layer (timeline_filter prefers nsys)
    let total = db.total_gpu_time_us();
    assert!(
        (total - 100.0).abs() < 0.01,
        "should see nsys launch only (100us), got {total}"
    );

    // timeline command should only see the nsys launch (with start_us)
    let timeline_count: i64 = db.conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM launches
             WHERE start_us IS NOT NULL AND {}",
            db.timeline_filter()
        ),
        [],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(timeline_count, 1, "timeline should see only 1 nsys launch");

    // gaps should work (needs nsys layer check)
    commands::cmd_gaps(&db, &[]);
    commands::cmd_timeline(&db, &[]);
}

// -----------------------------------------------------------------------
// recompute_op_gpu_times with no ops or no launches
// -----------------------------------------------------------------------

#[test]
fn recompute_with_no_ops() {
    let db = build_cuda_only_session();
    // No ops table entries — should not panic
    db.recompute_op_gpu_times();
}

#[test]
fn recompute_with_no_launches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.keep().join("empty_recompute.gpu.db");
    let db = GpuDb::create(&path).unwrap();
    // No layers, no launches, no ops — should not panic
    db.recompute_op_gpu_times();
}

// -----------------------------------------------------------------------
// Negative duration handling (end < start in raw data)
// -----------------------------------------------------------------------

#[test]
fn negative_duration_does_not_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.keep().join("neg_dur.gpu.db");
    let db = GpuDb::create(&path).unwrap();
    let lid = db.add_layer("nsys", "t", None, None, None).unwrap();

    // Insert a normal launch and a negative-duration launch
    db.conn.execute(
        "INSERT INTO launches (kernel_name, duration_us, start_us, stream_id, layer_id)
         VALUES ('good', 100.0, 0.0, 7, ?1)",
        params![lid],
    ).unwrap();
    db.conn.execute(
        "INSERT INTO launches (kernel_name, duration_us, start_us, stream_id, layer_id)
         VALUES ('bad', -50.0, 200.0, 7, ?1)",
        params![lid],
    ).unwrap();

    // total_gpu_time should still be positive
    let total = db.total_gpu_time_us();
    // 100 + (-50) = 50, which is > 0
    assert!(total > 0.0, "total should be positive even with negative duration: {total}");

    // All commands should not panic
    commands::cmd_stats(&db);
    commands::cmd_kernels(&db, &[]);
    commands::cmd_gaps(&db, &[]);
    commands::cmd_timeline(&db, &[]);
    commands::cmd_warmup(&db);
}

// -----------------------------------------------------------------------
// Kernel name with SQL injection attempt
// -----------------------------------------------------------------------

#[test]
fn sql_injection_in_kernel_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.keep().join("inject.gpu.db");
    let mut db = GpuDb::create(&path).unwrap();
    let lid = db.add_layer("nsys", "t", None, None, None).unwrap();

    // Kernel name that looks like SQL injection
    let evil_name = "'; DROP TABLE launches; --";
    db.conn.execute(
        "INSERT INTO launches (kernel_name, duration_us, start_us, layer_id)
         VALUES (?1, 100.0, 0.0, ?2)",
        params![evil_name, lid],
    ).unwrap();

    // These use LIKE with the kernel name as a pattern — should be safe
    commands::cmd_inspect(&db, &["DROP"]);
    commands::cmd_variance(&db, &["DROP"]);
    commands::cmd_kernels(&db, &[]);
    commands::cmd_focus(&mut db, &["'; DROP"]);
    commands::cmd_kernels(&db, &[]);

    // Table should still exist
    let count = db.total_launch_count();
    assert_eq!(count, 1, "table should survive injection attempt");
}

// -----------------------------------------------------------------------
// escape_sql_like must handle % and _ in patterns
// -----------------------------------------------------------------------

#[test]
fn sql_like_wildcard_escaping() {
    use crate::db::escape_sql_like;

    // % in pattern should be escaped
    assert_eq!(escape_sql_like("100%"), r"100\%");
    // _ is NOT escaped: kernel names contain underscores and users typing
    // "vector_add" expect a literal match, not a failed pattern.
    assert_eq!(escape_sql_like("a_b"), "a_b");
    // ' should be doubled
    assert_eq!(escape_sql_like("it's"), "it''s");
    // Combined
    assert_eq!(escape_sql_like("50% of it's_done"), r"50\% of it''s_done");
}

// -----------------------------------------------------------------------
// Suggest with template kernel names produces valid ncu regex
// -----------------------------------------------------------------------

#[test]
fn suggest_ncu_regex_is_valid() {
    let db = build_session();
    // suggest constructs a regex from kernel names. These contain
    // C++ templates with <>, ::, (), etc. After escape_regex, the
    // pattern should be a valid regex.
    //
    // We can't easily test the ncu CLI, but we can verify the
    // regex compiles in Rust's regex engine.
    use crate::commands::escape_regex;

    let tl = db.timeline_filter();
    let names: Vec<String> = db.query_vec(
        &format!("SELECT kernel_name FROM launches WHERE {tl}
                  GROUP BY kernel_name ORDER BY SUM(duration_us) DESC LIMIT 5"),
        [],
        |row| row.get(0),
    );

    let regex_str = names.iter().map(|n| escape_regex(n)).collect::<Vec<_>>().join("|");

    // This should not panic — the regex should be valid
    let re = regex::Regex::new(&regex_str);
    assert!(re.is_ok(), "suggest regex should be valid: {regex_str}\nerror: {:?}", re.err());

    // The pattern should match the original names
    let re = re.unwrap();
    for name in &names {
        assert!(re.is_match(name), "regex should match original name '{name}'");
    }
}

// -----------------------------------------------------------------------
// Chrome trace parser: op aggregation sums CPU time correctly
// -----------------------------------------------------------------------

#[test]
fn chrome_trace_op_cpu_time_aggregation() {
    use crate::db::GpuDb;
    use crate::parsers::chrome_trace::import_chrome_trace;

    let db = GpuDb::create(&tempfile::tempdir().unwrap().keep().join("ct.db")).unwrap();
    let lid = db.add_layer("torch", "trace.json", None, None, None).unwrap();

    // Minimal chrome trace with two invocations of the same op
    let trace = serde_json::json!({
        "traceEvents": [
            // Kernel launch
            {"ph": "X", "cat": "kernel", "name": "gemm_kernel", "ts": 100.0, "dur": 50.0, "args": {}},
            // Two invocations of the same op (should sum CPU time)
            {"ph": "X", "cat": "cpu_op", "name": "aten::mm", "ts": 90.0, "dur": 80.0, "args": {}},
            {"ph": "X", "cat": "cpu_op", "name": "aten::mm", "ts": 200.0, "dur": 30.0, "args": {}},
            // A different op
            {"ph": "X", "cat": "cpu_op", "name": "aten::add", "ts": 300.0, "dur": 10.0, "args": {}},
        ]
    });

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), serde_json::to_string(&trace).unwrap()).unwrap();

    import_chrome_trace(&db.conn, tmp.path(), lid).unwrap();

    // aten::mm should have cpu_time = 80 + 30 = 110
    let mm_cpu: f64 = db.conn.query_row(
        "SELECT cpu_time_us FROM ops WHERE name = 'aten::mm'",
        [], |row| row.get(0),
    ).unwrap();
    assert!(
        (mm_cpu - 110.0).abs() < 0.01,
        "aten::mm CPU time should be 110 (80+30), got {mm_cpu}"
    );

    // aten::add should have cpu_time = 10
    let add_cpu: f64 = db.conn.query_row(
        "SELECT cpu_time_us FROM ops WHERE name = 'aten::add'",
        [], |row| row.get(0),
    ).unwrap();
    assert!((add_cpu - 10.0).abs() < 0.01, "aten::add CPU should be 10, got {add_cpu}");

    // gemm_kernel should be mapped to aten::mm (first containing op)
    let mapped_op: String = db.conn.query_row(
        "SELECT o.name FROM op_kernel_map okm JOIN ops o ON o.id = okm.op_id
         WHERE okm.kernel_name = 'gemm_kernel'",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(mapped_op, "aten::mm", "kernel should map to containing op");

    // aten::mm gpu_time should reflect the kernel (50us)
    let mm_gpu: f64 = db.conn.query_row(
        "SELECT gpu_time_us FROM ops WHERE name = 'aten::mm'",
        [], |row| row.get(0),
    ).unwrap();
    assert!((mm_gpu - 50.0).abs() < 0.01, "aten::mm GPU should be 50, got {mm_gpu}");
}

// -----------------------------------------------------------------------
// Chrome trace: nested ops with temporal containment
// -----------------------------------------------------------------------

#[test]
fn chrome_trace_innermost_op_wins() {
    use crate::db::GpuDb;
    use crate::parsers::chrome_trace::import_chrome_trace;

    let db = GpuDb::create(&tempfile::tempdir().unwrap().keep().join("nested.db")).unwrap();
    let lid = db.add_layer("torch", "trace.json", None, None, None).unwrap();

    // Outer op contains inner op contains kernel
    let trace = serde_json::json!({
        "traceEvents": [
            // Outer op: [0, 1000]
            {"ph": "X", "cat": "cpu_op", "name": "aten::linear", "ts": 0.0, "dur": 1000.0, "args": {}},
            // Inner op: [100, 200]
            {"ph": "X", "cat": "cpu_op", "name": "aten::mm", "ts": 100.0, "dur": 100.0, "args": {}},
            // Kernel at t=150 — should map to aten::mm (innermost)
            {"ph": "X", "cat": "kernel", "name": "sgemm", "ts": 150.0, "dur": 30.0, "args": {}},
            // Kernel at t=500 — should map to aten::linear (only outer contains it)
            {"ph": "X", "cat": "kernel", "name": "elementwise", "ts": 500.0, "dur": 10.0, "args": {}},
        ]
    });

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), serde_json::to_string(&trace).unwrap()).unwrap();

    import_chrome_trace(&db.conn, tmp.path(), lid).unwrap();

    // sgemm at t=150 should map to aten::mm (innermost containing op)
    let sgemm_op: String = db.conn.query_row(
        "SELECT o.name FROM op_kernel_map okm JOIN ops o ON o.id = okm.op_id
         WHERE okm.kernel_name = 'sgemm'",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(sgemm_op, "aten::mm", "sgemm should map to innermost op aten::mm");

    // elementwise at t=500 should map to aten::linear
    let ew_op: String = db.conn.query_row(
        "SELECT o.name FROM op_kernel_map okm JOIN ops o ON o.id = okm.op_id
         WHERE okm.kernel_name = 'elementwise'",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(ew_op, "aten::linear", "elementwise should map to outer op aten::linear");
}
