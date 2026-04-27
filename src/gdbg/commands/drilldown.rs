use super::{
    GpuDb, fmt_bytes, fmt_us, like_param, require_op_layer,
};

// ---------------------------------------------------------------------------
// inspect
// ---------------------------------------------------------------------------

pub fn cmd_inspect(db: &GpuDb, args: &[&str]) {
    let pattern = match args.first() {
        Some(p) => *p,
        None => { println!("usage: inspect <kernel_pattern>"); return; }
    };

    // Get kernel aggregate (restrict to timeline layer to avoid double-counting)
    let tl = db.timeline_filter();
    let sql = format!(r"SELECT kernel_name, COUNT(*), SUM(duration_us), AVG(duration_us),
                      MIN(duration_us), MAX(duration_us)
               FROM launches WHERE kernel_name LIKE ?1 ESCAPE '\' AND {tl}
               GROUP BY kernel_name");
    let rows: Vec<(String, i64, f64, f64, f64, f64)> = db.query_vec(
        &sql, [like_param(pattern)],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
    );
    if rows.is_empty() { println!("no kernel matching '{pattern}'"); return; }
    if rows.len() > 1 {
        println!("multiple matches for '{pattern}':");
        for (n, ..) in &rows { println!("  {n}"); }
        println!("narrow the pattern");
        return;
    }
    let (name, cnt, total, avg, min, max) = rows.into_iter().next().unwrap();

    println!("Kernel: {name}");
    println!("  Launches: {cnt}");
    println!("  Total:    {}", fmt_us(total));
    println!("  Average:  {}", fmt_us(avg));
    if cnt > 1 { println!("  Min:      {}", fmt_us(min)); println!("  Max:      {}", fmt_us(max)); }

    // Launch config — most common
    let config_sql = format!("SELECT grid_x, grid_y, grid_z, block_x, block_y, block_z,
                             COUNT(*) as cnt
                      FROM launches WHERE kernel_name = ?1
                      AND grid_x IS NOT NULL AND {tl}
                      GROUP BY grid_x, grid_y, grid_z, block_x, block_y, block_z
                      ORDER BY cnt DESC LIMIT 5");
    let configs: Vec<(u32, u32, u32, u32, u32, u32, i64)> = db.query_vec(
        &config_sql, [&name],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?,
                  row.get(4)?, row.get(5)?, row.get(6)?)),
    );

    if !configs.is_empty() {
        println!();
        for (gx,gy,gz,bx,by,bz,c) in &configs {
            let threads = *bx as u64 * *by as u64 * *bz as u64;
            println!("  grid=({gx},{gy},{gz}) block=({bx},{by},{bz}) threads/block={threads} x{c}");
        }
    }

    // Metrics
    let m_sql = "SELECT occupancy_pct, compute_throughput_pct, memory_throughput_pct,
                        registers_per_thread, shared_mem_static_bytes, shared_mem_dynamic_bytes,
                        l2_hit_rate_pct, achieved_bandwidth_gb_s, peak_bandwidth_gb_s,
                        boundedness
                 FROM metrics WHERE kernel_name = ?1";
    if let Ok(m) = db.conn.query_row(m_sql, [&name], |row| {
        Ok((
            row.get::<_,Option<f64>>(0)?,  row.get::<_,Option<f64>>(1)?,
            row.get::<_,Option<f64>>(2)?,  row.get::<_,Option<i64>>(3)?,
            row.get::<_,Option<i64>>(4)?,  row.get::<_,Option<i64>>(5)?,
            row.get::<_,Option<f64>>(6)?,  row.get::<_,Option<f64>>(7)?,
            row.get::<_,Option<f64>>(8)?,  row.get::<_,Option<String>>(9)?,
        ))
    }) {
        println!("\n  Hardware Metrics (ncu):");
        if let Some(b) = &m.9 { println!("    Boundedness:       {b}"); }
        if let Some(v) = m.0 { println!("    Occupancy:         {v:.1}%"); }
        if let Some(v) = m.1 { println!("    Compute throughput: {v:.1}%"); }
        if let Some(v) = m.2 { println!("    Memory throughput:  {v:.1}%"); }
        if let Some(v) = m.3 { println!("    Registers/thread:  {v}"); }
        let shmem = m.4.unwrap_or(0) + m.5.unwrap_or(0);
        if shmem > 0 { println!("    Shared memory:     {}", fmt_bytes(shmem)); }
        if let Some(v) = m.6 { println!("    L2 hit rate:       {v:.1}%"); }
        if let (Some(a), Some(p)) = (m.7, m.8) {
            println!("    Bandwidth:         {a:.1} / {p:.1} GB/s ({:.1}%)", a / p * 100.0);
        } else if let Some(a) = m.7 { println!("    Bandwidth:         {a:.1} GB/s"); }
    } else {
        println!("\n  No hardware metrics (need ncu)");
    }

    // Op mapping
    let op_sql = "SELECT o.name, o.module_path, o.input_shapes
                  FROM op_kernel_map okm JOIN ops o ON o.id = okm.op_id
                  WHERE okm.kernel_name = ?1";
    let ops: Vec<(String, Option<String>, Option<String>)> = db.query_vec(
        op_sql, [&name],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    );
    if !ops.is_empty() {
        println!("\n  Origin (torch/proton):");
        for (opname, modpath, shapes) in &ops {
            println!("    Op: {opname}");
            if let Some(m) = modpath { println!("    Module: {m}"); }
            if let Some(s) = shapes { println!("    Shapes: {s}"); }
        }
    }
}

// ---------------------------------------------------------------------------
// trace (op → kernels)
// ---------------------------------------------------------------------------

pub fn cmd_trace(db: &GpuDb, args: &[&str]) {
    let pattern = match args.first() {
        Some(p) => *p,
        None => { println!("usage: trace <op_pattern>"); return; }
    };
    if !require_op_layer(db) { return; }

    let sql = r"SELECT id, name, module_path, cpu_time_us, input_shapes
               FROM ops WHERE name LIKE ?1 ESCAPE '\'";
    let ops: Vec<_> = db.query_vec(sql, [like_param(pattern)], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?, row.get::<_, f64>(3)?,
            row.get::<_, Option<String>>(4)?))
    });

    if ops.is_empty() { println!("no op matching '{pattern}'"); return; }

    for (op_id, name, module, cpu_time, shapes) in &ops {
        println!("Op: {name}");
        if let Some(m) = module { println!("  Module: {m}"); }
        if let Some(s) = shapes { println!("  Shapes: {s}"); }
        println!("  CPU: {}", fmt_us(*cpu_time));
        let kernels: Vec<String> = db.query_vec(
            "SELECT kernel_name FROM op_kernel_map WHERE op_id = ?1",
            [op_id], |row| row.get(0),
        );
        if !kernels.is_empty() {
            println!("  Kernels: {}", kernels.join(", "));
        }
    }
}

// ---------------------------------------------------------------------------
// callers (kernel → ops)
// ---------------------------------------------------------------------------

pub fn cmd_callers(db: &GpuDb, args: &[&str]) {
    let pattern = match args.first() {
        Some(p) => *p,
        None => { println!("usage: callers <kernel_pattern>"); return; }
    };
    if !require_op_layer(db) { return; }

    let sql = r"SELECT DISTINCT o.name, o.module_path
               FROM op_kernel_map okm JOIN ops o ON o.id = okm.op_id
               WHERE okm.kernel_name LIKE ?1 ESCAPE '\'";
    let rows: Vec<(String, Option<String>)> = db.query_vec(
        sql, [like_param(pattern)],
        |row| Ok((row.get(0)?, row.get(1)?)),
    );

    if rows.is_empty() { println!("no op mapping for kernels matching '{pattern}'"); return; }
    for (name, module) in &rows {
        println!("  {} ({})", name, module.as_deref().unwrap_or("?"));
    }
}
