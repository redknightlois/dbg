use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

/// Parse ncu CSV output and INSERT metrics into the session DB.
pub fn import_ncu_csv(dest: &Connection, csv_path: &Path, layer_id: i64) -> Result<()> {
    let content = std::fs::read_to_string(csv_path)
        .with_context(|| format!("cannot read {}", csv_path.display()))?;

    // Find headers
    let mut lines = content.lines();
    let header = loop {
        match lines.next() {
            Some(line) if line.contains("Kernel Name") && line.contains("Metric") => break line,
            Some(_) => continue,
            None => return Ok(()),
        }
    };

    let headers: Vec<&str> = parse_csv_line(header);
    let kernel_idx = find_col(&headers, "Kernel Name");
    let metric_name_idx = find_col(&headers, "Metric Name");
    let metric_value_idx = find_col(&headers, "Metric Value");

    let (kernel_idx, metric_name_idx, metric_value_idx) =
        match (kernel_idx, metric_name_idx, metric_value_idx) {
            (Some(k), Some(n), Some(v)) => (k, n, v),
            _ => return Ok(()),
        };

    // Collect all metrics per kernel
    let mut kernel_metrics: HashMap<String, HashMap<String, f64>> = HashMap::new();

    for line in lines {
        let line = line.trim();
        if line.is_empty() || line.starts_with("==") {
            continue;
        }
        let fields = parse_csv_line(line);

        let kernel = match fields.get(kernel_idx) {
            Some(k) if !k.is_empty() => k.to_string(),
            _ => continue,
        };
        let metric_name = match fields.get(metric_name_idx) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => continue,
        };
        let metric_value = fields
            .get(metric_value_idx)
            .and_then(|v| v.replace(',', "").parse::<f64>().ok())
            .unwrap_or(0.0);

        kernel_metrics
            .entry(kernel)
            .or_default()
            .insert(metric_name, metric_value);
    }

    // Insert into metrics table
    let mut stmt = dest.prepare(
        "INSERT OR REPLACE INTO metrics
            (kernel_name, occupancy_pct, compute_throughput_pct, memory_throughput_pct,
             registers_per_thread, shared_mem_static_bytes, shared_mem_dynamic_bytes,
             l2_hit_rate_pct, achieved_bandwidth_gb_s, peak_bandwidth_gb_s,
             boundedness, layer_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
    )?;

    // Also insert per-launch data if available
    let mut launch_stmt = dest.prepare(
        "INSERT INTO launches
            (kernel_name, duration_us, grid_x, grid_y, grid_z,
             block_x, block_y, block_z, stream_id, layer_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
    )?;

    for (name, m) in &kernel_metrics {
        let occupancy = m.get("sm__warps_active.avg.pct_of_peak_sustained_active");
        let compute_tp = m
            .get("sm__throughput.avg.pct_of_peak_sustained_elapsed")
            .or_else(|| m.get("sm__pipe_tensor_cycles_active.avg.pct_of_peak_sustained_elapsed"));
        let memory_tp = m
            .get("dram__throughput.avg.pct_of_peak_sustained_elapsed")
            .or_else(|| m.get("gpu__dram_throughput.avg.pct_of_peak_sustained_elapsed"));
        let registers = m.get("launch__registers_per_thread").map(|v| *v as i64);
        let shmem_static = m.get("launch__shared_mem_per_block_static").map(|v| *v as i64);
        let shmem_dynamic = m.get("launch__shared_mem_per_block_dynamic").map(|v| *v as i64);
        let l2_hit = m
            .get("lts__t_sector_hit_rate.pct")
            .or_else(|| m.get("l2__t_sector_hit_rate.pct"));
        let achieved_bw = m.get("dram__bytes.sum.per_second").map(|v| v / 1e9);

        let boundedness = classify_boundedness(compute_tp.copied(), memory_tp.copied());

        stmt.execute(params![
            name,
            occupancy,
            compute_tp,
            memory_tp,
            registers,
            shmem_static,
            shmem_dynamic,
            l2_hit,
            achieved_bw,
            None::<f64>,  // peak_bandwidth — would need device spec
            boundedness,
            layer_id,
        ])?;

        // Insert a launch record if we have duration and config
        let duration_ns = m
            .get("gpu__time_duration.sum")
            .or_else(|| m.get("Duration"))
            .copied()
            .unwrap_or(0.0);
        if duration_ns > 0.0 {
            let gx = m.get("launch__grid_size_x").unwrap_or(&0.0);
            let gy = m.get("launch__grid_size_y").unwrap_or(&0.0);
            let gz = m.get("launch__grid_size_z").unwrap_or(&0.0);
            let bx = m.get("launch__block_size_x").unwrap_or(&0.0);
            let by = m.get("launch__block_size_y").unwrap_or(&0.0);
            let bz = m.get("launch__block_size_z").unwrap_or(&0.0);
            let sid = m.get("launch__stream_id").map(|v| *v as i64);

            launch_stmt.execute(params![
                name,
                duration_ns / 1000.0,
                *gx as u32, *gy as u32, *gz as u32,
                *bx as u32, *by as u32, *bz as u32,
                sid,
                layer_id,
            ])?;
        }
    }

    Ok(())
}

fn classify_boundedness(compute: Option<f64>, memory: Option<f64>) -> Option<String> {
    let (c, m) = match (compute, memory) {
        (Some(c), Some(m)) => (c, m),
        _ => return None,
    };
    if c < 10.0 && m < 10.0 {
        Some("latency".into())
    } else if m > c * 1.5 {
        Some("memory".into())
    } else if c > m * 1.5 {
        Some("compute".into())
    } else if m >= c {
        Some("memory".into())
    } else {
        Some("compute".into())
    }
}

fn find_col(headers: &[&str], name: &str) -> Option<usize> {
    headers.iter().position(|h| h.contains(name))
}

fn parse_csv_line(line: &str) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;

    for (i, ch) in line.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(line[start..i].trim().trim_matches('"'));
                start = i + 1;
            }
            _ => {}
        }
    }
    fields.push(line[start..].trim().trim_matches('"'));
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_memory_bound() {
        assert_eq!(classify_boundedness(Some(30.0), Some(80.0)).as_deref(), Some("memory"));
    }

    #[test]
    fn classify_compute_bound() {
        assert_eq!(classify_boundedness(Some(85.0), Some(20.0)).as_deref(), Some("compute"));
    }

    #[test]
    fn classify_latency_bound() {
        assert_eq!(classify_boundedness(Some(5.0), Some(3.0)).as_deref(), Some("latency"));
    }

    #[test]
    fn import_ncu_csv_basic() {
        use crate::db::GpuDb;

        let db = GpuDb::create(&tempfile::tempdir().unwrap().into_path().join("t.db")).unwrap();
        let lid = db.add_layer("ncu", "test.csv", None, None, None).unwrap();

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        use std::io::Write;
        writeln!(tmp, r#""ID","Kernel Name","Metric Name","Metric Unit","Metric Value""#).unwrap();
        writeln!(tmp, r#""1","my_kernel","gpu__time_duration.sum","nsecond","500000""#).unwrap();
        writeln!(tmp, r#""1","my_kernel","sm__warps_active.avg.pct_of_peak_sustained_active","%","67.5""#).unwrap();
        writeln!(tmp, r#""1","my_kernel","sm__throughput.avg.pct_of_peak_sustained_elapsed","%","31.2""#).unwrap();
        writeln!(tmp, r#""1","my_kernel","dram__throughput.avg.pct_of_peak_sustained_elapsed","%","78.4""#).unwrap();

        import_ncu_csv(&db.conn, tmp.path(), lid).unwrap();

        let occ: f64 = db.conn.query_row(
            "SELECT occupancy_pct FROM metrics WHERE kernel_name = 'my_kernel'",
            [], |row| row.get(0),
        ).unwrap();
        assert!((occ - 67.5).abs() < 0.1);

        let bound: String = db.conn.query_row(
            "SELECT boundedness FROM metrics WHERE kernel_name = 'my_kernel'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(bound, "memory");
    }
}
