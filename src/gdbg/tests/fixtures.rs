//! Session builders for GPU profiling test fixtures.
//!
//! Each builder creates a realistic `GpuDb` populated with data modeled
//! after real CUDA / Triton / PyTorch workloads.

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
pub(super) fn build_session() -> GpuDb {
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

    // As the real collect pipeline does: re-correlate against timeline layer
    // so ops.gpu_time_us reflects nsys kernel durations (not torch-layer)
    db.recompute_op_gpu_times();

    db
}

pub(super) fn build_cuda_only_session() -> GpuDb {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.into_path().join("cuda_only.gpu.db");
    let db = GpuDb::create(&path).unwrap();

    db.set_meta("target", "matmul.cu").unwrap();
    db.set_meta("device", "NVIDIA RTX 4090").unwrap();
    db.set_meta("wall_time_us", "100000").unwrap();

    let nsys_id = db
        .add_layer("nsys", "/tmp/trace.nsys-rep", Some("nsys profile ./matmul"), Some(30.0), Some("def456"))
        .unwrap();

    // Single-stream GEMM-heavy workload — no torch layer at all
    let kernels = [
        ("volta_sgemm_128x64_nn", 7_u32, vec![
            (100.0_f64, 2000.0_f64, 64_u32, 2, 1, 256, 1, 1),
            (5000.0, 1950.0, 64, 2, 1, 256, 1, 1),
            (10000.0, 2100.0, 64, 2, 1, 256, 1, 1),
            (15000.0, 1980.0, 64, 2, 1, 256, 1, 1),
            (20000.0, 2050.0, 64, 2, 1, 256, 1, 1),
        ]),
        ("void memcpy_kernel<float>", 7, vec![
            (50.0, 50.0, 4, 1, 1, 128, 1, 1),
            (4900.0, 55.0, 4, 1, 1, 128, 1, 1),
            (9900.0, 48.0, 4, 1, 1, 128, 1, 1),
        ]),
        ("void reduce_sum<float, 256>", 7, vec![
            (25000.0, 800.0, 16, 1, 1, 256, 1, 1),
            (30000.0, 790.0, 16, 1, 1, 256, 1, 1),
        ]),
    ];

    for (name, stream, launches) in &kernels {
        for &(start, dur, gx, gy, gz, bx, by, bz) in launches {
            db.conn.execute(
                "INSERT INTO launches
                 (kernel_name, duration_us, grid_x, grid_y, grid_z,
                  block_x, block_y, block_z, stream_id, start_us, layer_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![name, dur, gx, gy, gz, bx, by, bz, stream, start, nsys_id],
            ).unwrap();
        }
    }

    // ncu metrics for GEMM kernel only
    let ncu_id = db
        .add_layer("ncu", "/tmp/ncu.csv", Some("ncu --set full"), Some(90.0), Some("def456"))
        .unwrap();

    db.conn.execute(
        "INSERT INTO metrics
         (kernel_name, occupancy_pct, compute_throughput_pct, memory_throughput_pct,
          registers_per_thread, shared_mem_static_bytes, shared_mem_dynamic_bytes,
          l2_hit_rate_pct, achieved_bandwidth_gb_s, peak_bandwidth_gb_s,
          boundedness, layer_id)
         VALUES (?1, 90.0, 88.0, 20.0, 32, 0, 32768, 92.0, 500.0, 1008.0, 'compute', ?2)",
        params!["volta_sgemm_128x64_nn", ncu_id],
    ).unwrap();

    // Transfers
    db.conn.execute(
        "INSERT INTO transfers (kind, bytes, duration_us, start_us, stream_id, layer_id)
         VALUES ('H2D', 33554432, 250.0, 10.0, 7, ?1)",
        params![nsys_id],
    ).unwrap();
    db.conn.execute(
        "INSERT INTO transfers (kind, bytes, duration_us, start_us, stream_id, layer_id)
         VALUES ('D2H', 1048576, 60.0, 35000.0, 7, ?1)",
        params![nsys_id],
    ).unwrap();

    db
}

pub(super) fn build_triton_inference_session() -> GpuDb {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.into_path().join("triton_inf.gpu.db");
    let db = GpuDb::create(&path).unwrap();

    db.set_meta("target", "serve.py").unwrap();
    db.set_meta("device", "NVIDIA H100-SXM5-80GB").unwrap();
    db.set_meta("wall_time_us", "5000").unwrap();

    // Only a proton layer (Triton profiler), no nsys
    let proton_id = db
        .add_layer("proton", "/tmp/proton_trace.json", Some("proton"), Some(2.0), Some("ghi789"))
        .unwrap();

    // Triton fused attention kernel — many launches, tight loop
    let kernels = [
        ("triton_flash_attention_fwd_kernel", 7_u32, vec![
            (100.0_f64, 300.0_f64, 128_u32, 8, 1, 128, 1, 1),
            (500.0, 295.0, 128, 8, 1, 128, 1, 1),
            (900.0, 310.0, 128, 8, 1, 128, 1, 1),
            (1300.0, 290.0, 128, 8, 1, 128, 1, 1),
            (1700.0, 305.0, 128, 8, 1, 128, 1, 1),
            (2100.0, 298.0, 128, 8, 1, 128, 1, 1),
        ]),
        ("triton_matmul_kernel", 7, vec![
            (2500.0, 150.0, 64, 4, 1, 64, 1, 1),
            (2750.0, 155.0, 64, 4, 1, 64, 1, 1),
            (3000.0, 148.0, 64, 4, 1, 64, 1, 1),
        ]),
        ("triton_layernorm_fwd_kernel", 7, vec![
            (3200.0, 20.0, 32, 1, 1, 256, 1, 1),
            (3300.0, 22.0, 32, 1, 1, 256, 1, 1),
            (3400.0, 19.0, 32, 1, 1, 256, 1, 1),
            (3500.0, 21.0, 32, 1, 1, 256, 1, 1),
        ]),
        // Tiny kernel — 0.5us each, dominated by launch overhead
        ("triton_add_kernel", 7, vec![
            (3600.0, 0.5, 1, 1, 1, 64, 1, 1),
            (3650.0, 0.4, 1, 1, 1, 64, 1, 1),
            (3700.0, 0.6, 1, 1, 1, 64, 1, 1),
            (3750.0, 0.5, 1, 1, 1, 64, 1, 1),
            (3800.0, 0.4, 1, 1, 1, 64, 1, 1),
            (3850.0, 0.5, 1, 1, 1, 64, 1, 1),
        ]),
    ];

    for (name, stream, launches) in &kernels {
        for &(start, dur, gx, gy, gz, bx, by, bz) in launches {
            db.conn.execute(
                "INSERT INTO launches
                 (kernel_name, duration_us, grid_x, grid_y, grid_z,
                  block_x, block_y, block_z, stream_id, start_us, layer_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![name, dur, gx, gy, gz, bx, by, bz, stream, start, proton_id],
            ).unwrap();
        }
    }

    // Ops from proton
    let ops = [
        ("attention", None::<&str>, 200.0_f64, 0.0_f64, Some("[[1, 128, 64]]")),
        ("linear", None, 100.0, 0.0, None),
        ("layer_norm", None, 30.0, 0.0, None),
    ];

    for (name, module, cpu_time, gpu_time, shapes) in &ops {
        db.conn.execute(
            "INSERT INTO ops (name, module_path, cpu_time_us, gpu_time_us, input_shapes, layer_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![name, module, cpu_time, gpu_time, shapes, proton_id],
        ).unwrap();
    }

    // op → kernel mapping
    let attn_id: i64 = db.conn.query_row(
        "SELECT id FROM ops WHERE name = 'attention'", [], |r| r.get(0),
    ).unwrap();
    let linear_id: i64 = db.conn.query_row(
        "SELECT id FROM ops WHERE name = 'linear'", [], |r| r.get(0),
    ).unwrap();
    let ln_id: i64 = db.conn.query_row(
        "SELECT id FROM ops WHERE name = 'layer_norm'", [], |r| r.get(0),
    ).unwrap();

    db.conn.execute(
        "INSERT INTO op_kernel_map (op_id, kernel_name) VALUES (?1, ?2)",
        params![attn_id, "triton_flash_attention_fwd_kernel"],
    ).unwrap();
    db.conn.execute(
        "INSERT INTO op_kernel_map (op_id, kernel_name) VALUES (?1, ?2)",
        params![linear_id, "triton_matmul_kernel"],
    ).unwrap();
    db.conn.execute(
        "INSERT INTO op_kernel_map (op_id, kernel_name) VALUES (?1, ?2)",
        params![ln_id, "triton_layernorm_fwd_kernel"],
    ).unwrap();

    // Update gpu_time from kernel launches
    db.conn.execute(
        "UPDATE ops SET gpu_time_us = (
            SELECT COALESCE(SUM(l.duration_us), 0)
            FROM op_kernel_map okm
            JOIN launches l ON l.kernel_name = okm.kernel_name AND l.layer_id = ?1
            WHERE okm.op_id = ops.id
        ) WHERE layer_id = ?1",
        params![proton_id],
    ).unwrap();

    db
}

pub(super) fn build_multi_stream_session() -> GpuDb {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.into_path().join("multi_stream.gpu.db");
    let db = GpuDb::create(&path).unwrap();

    db.set_meta("target", "pipeline.py").unwrap();
    db.set_meta("device", "NVIDIA A100-SXM4-80GB").unwrap();
    db.set_meta("wall_time_us", "20000").unwrap();

    let nsys_id = db
        .add_layer("nsys", "/tmp/trace.nsys-rep", Some("nsys profile"), Some(60.0), Some("jkl012"))
        .unwrap();

    // 4 streams doing overlapping work — models pipeline parallelism
    let kernels = [
        // Stream 1: compute micro-batch 0
        ("void gemm_fp16<128,128>", 1_u32, vec![
            (0.0_f64, 1000.0_f64, 256_u32, 1, 1, 128, 1, 1),
            (2000.0, 1000.0, 256, 1, 1, 128, 1, 1),
            (4000.0, 1000.0, 256, 1, 1, 128, 1, 1),
        ]),
        // Stream 2: compute micro-batch 1 (overlapping with stream 1)
        ("void gemm_fp16<128,128>", 2_u32, vec![
            (500.0, 1000.0, 256, 1, 1, 128, 1, 1),
            (2500.0, 1000.0, 256, 1, 1, 128, 1, 1),
            (4500.0, 1000.0, 256, 1, 1, 128, 1, 1),
        ]),
        // Stream 3: allreduce (overlapping with compute)
        ("void nccl_AllReduce_sum_fp16", 3_u32, vec![
            (1200.0, 2000.0, 512, 1, 1, 256, 1, 1),
            (5200.0, 2000.0, 512, 1, 1, 256, 1, 1),
        ]),
        // Stream 4: D2H copies (result offloading)
        ("void copy_d2h_async<__half>", 4_u32, vec![
            (3500.0, 100.0, 8, 1, 1, 256, 1, 1),
            (7500.0, 100.0, 8, 1, 1, 256, 1, 1),
        ]),
        // Stream 1: tiny epilogue kernels
        ("void elementwise_add_<__half>", 1_u32, vec![
            (1100.0, 5.0, 1, 1, 1, 32, 1, 1),
            (3100.0, 5.0, 1, 1, 1, 32, 1, 1),
            (5100.0, 5.0, 1, 1, 1, 32, 1, 1),
        ]),
    ];

    for (name, stream, launches) in &kernels {
        for &(start, dur, gx, gy, gz, bx, by, bz) in launches {
            db.conn.execute(
                "INSERT INTO launches
                 (kernel_name, duration_us, grid_x, grid_y, grid_z,
                  block_x, block_y, block_z, stream_id, start_us, layer_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![name, dur, gx, gy, gz, bx, by, bz, stream, start, nsys_id],
            ).unwrap();
        }
    }

    // Transfers on dedicated stream
    for &(kind, bytes, dur, start) in &[
        ("H2D", 67108864_i64, 500.0_f64, 0.0_f64),
        ("D2H", 16777216, 200.0, 8000.0),
    ] {
        db.conn.execute(
            "INSERT INTO transfers (kind, bytes, duration_us, start_us, stream_id, layer_id)
             VALUES (?1, ?2, ?3, ?4, 4, ?5)",
            params![kind, bytes, dur, start, nsys_id],
        ).unwrap();
    }

    // NVTX regions — pipeline stages
    for (i, start) in [0.0_f64, 2000.0, 4000.0].iter().enumerate() {
        db.conn.execute(
            "INSERT INTO regions (name, start_us, duration_us, layer_id)
             VALUES (?1, ?2, 2000.0, ?3)",
            params![format!("PipelineStage#{}", i), start, nsys_id],
        ).unwrap();
    }

    db
}
