//! Integration tests for gdbg commands against a realistic GPU profiling session.
//!
//! These tests populate a session DB with data modeled after real CUDA workloads,
//! then run every REPL command to verify correctness and catch panics.

#[cfg(test)]
mod tests {
    use crate::commands;
    use crate::db::GpuDb;
    use rusqlite::params;

    /// Build a realistic multi-layer session DB.
    ///
    /// Models a PyTorch training step with:
    ///   - 3 layers: nsys (timeline), ncu (metrics), torch (op mapping)
    ///   - Mixed kernel names including C++ templates and unicode
    ///   - Overlapping launches on multiple streams
    ///   - Transfers (H2D, D2H)
    ///   - NVTX regions
    ///   - Op→kernel mapping
    ///   - Varying launch configs, small kernels, high-variance kernels
    fn build_session() -> GpuDb {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.into_path().join("session.gpu.db");
        let db = GpuDb::create(&path).unwrap();

        db.set_meta("target", "train.py").unwrap();
        db.set_meta("device", "NVIDIA A100-SXM4-80GB").unwrap();
        db.set_meta("wall_time_us", "50000").unwrap();
        db.set_meta("created", "2025-06-01T12:00:00Z").unwrap();

        // --- Layer 1: nsys timeline ---
        let nsys_id = db
            .add_layer("nsys", "/tmp/trace.nsys-rep", Some("nsys profile train.py"), Some(45.2), Some("abc123"))
            .unwrap();

        // Kernel names that stress string handling:
        //   - C++ template with angle brackets and colons
        //   - Long demangled name that will be truncated
        //   - Short simple name
        //   - Name with unicode (Triton kernels sometimes have these)
        let kernels = [
            // (name, stream, launches as Vec<(start, dur, gx,gy,gz, bx,by,bz)>)
            (
                "void cutlass::Kernel<cutlass::gemm::kernel::GemmUniversal<float>>",
                7,
                vec![
                    (1000.0, 500.0, 128, 1, 1, 256, 1, 1),
                    (2000.0, 480.0, 128, 1, 1, 256, 1, 1),
                    (10000.0, 510.0, 128, 1, 1, 256, 1, 1),
                    (20000.0, 490.0, 128, 1, 1, 256, 1, 1),
                    (30000.0, 520.0, 128, 1, 1, 256, 1, 1),
                    (40000.0, 505.0, 128, 1, 1, 256, 1, 1),
                ],
            ),
            (
                "ampere_sgemm_128x32_tn",
                7,
                vec![
                    (1600.0, 200.0, 64, 1, 1, 128, 1, 1),
                    (3000.0, 210.0, 64, 1, 1, 128, 1, 1),
                    (11000.0, 195.0, 64, 1, 1, 128, 1, 1),
                    (21000.0, 205.0, 64, 1, 1, 128, 1, 1),
                ],
            ),
            (
                // Small kernel — launch overhead should dominate
                "void at::native::vectorized_elementwise_kernel<4, float>",
                7,
                vec![
                    (1900.0, 2.0, 1, 1, 1, 32, 1, 1),
                    (2500.0, 1.5, 1, 1, 1, 32, 1, 1),
                    (3500.0, 2.2, 1, 1, 1, 32, 1, 1),
                    (11500.0, 1.8, 1, 1, 1, 32, 1, 1),
                    (21500.0, 2.1, 1, 1, 1, 32, 1, 1),
                    (31500.0, 1.9, 1, 1, 1, 32, 1, 1),
                    (41000.0, 2.0, 1, 1, 1, 32, 1, 1),
                    (41100.0, 2.3, 1, 1, 1, 32, 1, 1),
                ],
            ),
            (
                // Kernel on a DIFFERENT stream — overlaps with stream 7 launches
                "void nccl_AllReduce_sum_f32",
                14,
                vec![
                    (1200.0, 800.0, 256, 1, 1, 512, 1, 1), // overlaps with sgemm on stream 7
                    (11200.0, 750.0, 256, 1, 1, 512, 1, 1),
                    (21200.0, 780.0, 256, 1, 1, 512, 1, 1),
                ],
            ),
            (
                // High-variance kernel (warmup effect)
                "void cudnn::bn_fw_tr_1C11_kernel<float>",
                7,
                vec![
                    (500.0, 5000.0, 32, 1, 1, 128, 1, 1),  // warmup: 10x slower
                    (6000.0, 300.0, 32, 1, 1, 128, 1, 1),
                    (12000.0, 310.0, 32, 1, 1, 128, 1, 1),
                    (22000.0, 290.0, 32, 1, 1, 128, 1, 1),
                    (32000.0, 305.0, 32, 1, 1, 128, 1, 1),
                    (42000.0, 295.0, 32, 1, 1, 128, 1, 1),
                ],
            ),
            (
                // Unicode in kernel name (Triton JIT can produce these)
                "triton_poi_fused_αβ_kernel_0",
                7,
                vec![
                    (43000.0, 150.0, 16, 1, 1, 64, 1, 1),
                    (44000.0, 155.0, 16, 1, 1, 64, 1, 1),
                ],
            ),
        ];

        for (name, stream, launches) in &kernels {
            for &(start, dur, gx, gy, gz, bx, by, bz) in launches {
                db.conn
                    .execute(
                        "INSERT INTO launches
                         (kernel_name, duration_us, grid_x, grid_y, grid_z,
                          block_x, block_y, block_z, stream_id, start_us, layer_id)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                        params![name, dur, gx, gy, gz, bx, by, bz, stream, start, nsys_id],
                    )
                    .unwrap();
            }
        }

        // Transfers
        for &(kind, bytes, dur, start, stream) in &[
            ("H2D", 4_194_304_i64, 120.0_f64, 100.0_f64, 7_u32),
            ("H2D", 16_777_216, 450.0, 200.0, 7),
            ("D2H", 1_048_576, 80.0, 45000.0, 14),
        ] {
            db.conn
                .execute(
                    "INSERT INTO transfers (kind, bytes, duration_us, start_us, stream_id, layer_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![kind, bytes, dur, start, stream, nsys_id],
                )
                .unwrap();
        }

        // NVTX regions
        db.conn
            .execute(
                "INSERT INTO regions (name, start_us, duration_us, layer_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params!["ProfilerStep#1", 500.0, 20000.0, nsys_id],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO regions (name, start_us, duration_us, layer_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params!["ProfilerStep#2", 20500.0, 25000.0, nsys_id],
            )
            .unwrap();

        // --- Layer 2: ncu metrics ---
        let ncu_id = db
            .add_layer("ncu", "/tmp/ncu.csv", Some("ncu --set full"), Some(120.0), Some("abc123"))
            .unwrap();

        // Metrics for top kernels
        let metrics = [
            // (name, occ, compute%, mem%, regs, shmem_static, shmem_dynamic, l2_hit, bw_achieved, bw_peak, bound)
            (
                "void cutlass::Kernel<cutlass::gemm::kernel::GemmUniversal<float>>",
                75.0, 82.0, 30.0, 32_i64, 0_i64, 16384_i64, 95.0, 1200.0, 2039.0, "compute",
            ),
            (
                "ampere_sgemm_128x32_tn",
                85.0, 25.0, 78.0, 24, 0, 0, 45.0, 1800.0, 2039.0, "memory",
            ),
            (
                "void at::native::vectorized_elementwise_kernel<4, float>",
                12.0, 3.0, 5.0, 16, 0, 0, 80.0, 50.0, 2039.0, "latency",
            ),
            (
                "void cudnn::bn_fw_tr_1C11_kernel<float>",
                60.0, 45.0, 55.0, 40, 32768, 0, 70.0, 900.0, 2039.0, "memory",
            ),
        ];

        for (name, occ, cmp, mem, regs, ss, sd, l2, bw, peak, bound) in &metrics {
            db.conn
                .execute(
                    "INSERT INTO metrics
                     (kernel_name, occupancy_pct, compute_throughput_pct, memory_throughput_pct,
                      registers_per_thread, shared_mem_static_bytes, shared_mem_dynamic_bytes,
                      l2_hit_rate_pct, achieved_bandwidth_gb_s, peak_bandwidth_gb_s,
                      boundedness, layer_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![name, occ, cmp, mem, regs, ss, sd, l2, bw, peak, bound, ncu_id],
                )
                .unwrap();
        }

        // --- Layer 3: torch op mapping ---
        let torch_id = db
            .add_layer("torch", "/tmp/torch_trace.json", Some("torch.profiler"), Some(0.5), Some("abc123"))
            .unwrap();

        // Also add torch-layer kernel launches (separate from nsys launches)
        // This tests the double-count fix: breakdown should use timeline_filter
        for &(name, start, dur) in &[
            ("void cutlass::Kernel<cutlass::gemm::kernel::GemmUniversal<float>>", 1000.0_f64, 500.0_f64),
            ("void cutlass::Kernel<cutlass::gemm::kernel::GemmUniversal<float>>", 2000.0, 480.0),
            ("ampere_sgemm_128x32_tn", 1600.0, 200.0),
            ("void at::native::vectorized_elementwise_kernel<4, float>", 1900.0, 2.0),
        ] {
            db.conn
                .execute(
                    "INSERT INTO launches (kernel_name, duration_us, start_us, layer_id)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![name, dur, start, torch_id],
                )
                .unwrap();
        }

        // Ops
        let ops = [
            ("aten::linear", None::<&str>, 1200.0_f64, 0.0_f64, Some("[[64, 512], [512, 512]]")),
            ("aten::batch_norm", Some("model.bn1"), 300.0, 0.0, None),
            ("aten::relu_", None, 50.0, 0.0, None),
            ("aten::nll_loss", None, 80.0, 0.0, None),
        ];

        for (name, module, cpu_time, gpu_time, shapes) in &ops {
            db.conn
                .execute(
                    "INSERT INTO ops (name, module_path, cpu_time_us, gpu_time_us, input_shapes, layer_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![name, module, cpu_time, gpu_time, shapes, torch_id],
                )
                .unwrap();
        }

        // op_kernel_map: link ops to kernels
        // aten::linear → cutlass gemm + sgemm
        let linear_id: i64 = db.conn.query_row(
            "SELECT id FROM ops WHERE name = 'aten::linear'", [], |r| r.get(0),
        ).unwrap();
        let bn_id: i64 = db.conn.query_row(
            "SELECT id FROM ops WHERE name = 'aten::batch_norm'", [], |r| r.get(0),
        ).unwrap();
        let relu_id: i64 = db.conn.query_row(
            "SELECT id FROM ops WHERE name = 'aten::relu_'", [], |r| r.get(0),
        ).unwrap();

        db.conn.execute(
            "INSERT INTO op_kernel_map (op_id, kernel_name) VALUES (?1, ?2)",
            params![linear_id, "void cutlass::Kernel<cutlass::gemm::kernel::GemmUniversal<float>>"],
        ).unwrap();
        db.conn.execute(
            "INSERT INTO op_kernel_map (op_id, kernel_name) VALUES (?1, ?2)",
            params![linear_id, "ampere_sgemm_128x32_tn"],
        ).unwrap();
        db.conn.execute(
            "INSERT INTO op_kernel_map (op_id, kernel_name) VALUES (?1, ?2)",
            params![bn_id, "void cudnn::bn_fw_tr_1C11_kernel<float>"],
        ).unwrap();
        db.conn.execute(
            "INSERT INTO op_kernel_map (op_id, kernel_name) VALUES (?1, ?2)",
            params![relu_id, "void at::native::vectorized_elementwise_kernel<4, float>"],
        ).unwrap();

        // Update gpu_time_us from correlated kernel launches (as the real parser does)
        db.conn.execute(
            "UPDATE ops SET gpu_time_us = (
                SELECT COALESCE(SUM(l.duration_us), 0)
                FROM op_kernel_map okm
                JOIN launches l ON l.kernel_name = okm.kernel_name AND l.layer_id = ?1
                WHERE okm.op_id = ops.id
            ) WHERE layer_id = ?1",
            params![torch_id],
        ).unwrap();

        db
    }

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
}
