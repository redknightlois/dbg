use std::collections::HashMap;
use std::{fs::File, io::Read, path::Path};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};

const MAX_IMPORT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_NCU_ROWS: usize = 1_000_000;
const MAX_KERNELS: usize = 100_000;

/// Parse ncu CSV output and INSERT metrics into the session DB.
pub fn import_ncu_csv(dest: &Connection, csv_path: &Path, layer_id: i64) -> Result<()> {
    let size = std::fs::metadata(csv_path)
        .with_context(|| format!("stat {}", csv_path.display()))?
        .len();
    if size > MAX_IMPORT_BYTES {
        bail!(
            "NCU CSV {} is too large ({size} bytes; maximum is {MAX_IMPORT_BYTES})",
            csv_path.display()
        );
    }
    let content = read_bounded_text(csv_path)?;

    // Find headers
    let mut lines = content.lines();
    let header = loop {
        match lines.next() {
            Some(line) if line.contains("Kernel Name") && line.contains("Metric") => break line,
            Some(_) => continue,
            None => bail!("NCU CSV contains no candidate header"),
        }
    };

    let headers = parse_csv_line(header);
    let kernel_idx = find_col(&headers, "Kernel Name");
    let metric_name_idx = find_col(&headers, "Metric Name");
    let metric_value_idx = find_col(&headers, "Metric Value");

    let (kernel_idx, metric_name_idx, metric_value_idx) = match (
        kernel_idx,
        metric_name_idx,
        metric_value_idx,
    ) {
        (Some(k), Some(n), Some(v)) => (k, n, v),
        _ => bail!(
            "NCU CSV header is missing one or more required columns: Kernel Name, Metric Name, Metric Value"
        ),
    };

    // Collect all metrics per kernel
    let mut kernel_metrics: HashMap<String, HashMap<String, f64>> = HashMap::new();

    let mut row_count = 0usize;
    for line in lines {
        row_count += 1;
        if row_count > MAX_NCU_ROWS {
            bail!("NCU CSV exceeds the maximum of {MAX_NCU_ROWS} data rows");
        }
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
            .filter(|v| v.is_finite());
        let Some(metric_value) = metric_value else {
            continue;
        };

        kernel_metrics
            .entry(kernel)
            .or_default()
            .insert(metric_name, metric_value);
        if kernel_metrics.len() > MAX_KERNELS {
            bail!("NCU CSV exceeds the maximum of {MAX_KERNELS} kernels");
        }
    }

    // Insert into metrics table
    let mut stmt = dest.prepare(
        "INSERT OR REPLACE INTO metrics
            (kernel_name, occupancy_pct, compute_throughput_pct, memory_throughput_pct,
             registers_per_thread, shared_mem_static_bytes, shared_mem_dynamic_bytes,
             l2_hit_rate_pct, achieved_bandwidth_gb_s, peak_bandwidth_gb_s,
             boundedness, layer_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;

    // Also insert per-launch data if available
    let mut launch_stmt = dest.prepare(
        "INSERT INTO launches
            (kernel_name, duration_us, grid_x, grid_y, grid_z,
             block_x, block_y, block_z, stream_id, layer_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;

    for (name, m) in &kernel_metrics {
        let occupancy = m.get("sm__warps_active.avg.pct_of_peak_sustained_active");
        let compute_tp = m
            .get("sm__throughput.avg.pct_of_peak_sustained_elapsed")
            .or_else(|| m.get("sm__pipe_tensor_cycles_active.avg.pct_of_peak_sustained_elapsed"));
        let memory_tp = m
            .get("dram__throughput.avg.pct_of_peak_sustained_elapsed")
            .or_else(|| m.get("gpu__dram_throughput.avg.pct_of_peak_sustained_elapsed"))
            // Newer architectures expose the Speed-of-Light "Memory
            // Throughput" value under this aggregate GPU metric instead of a
            // `dram__throughput` metric.  Nsight Compute 2025.x on Blackwell
            // is one example.
            .or_else(|| m.get("gpu__compute_memory_throughput.avg.pct_of_peak_sustained_elapsed"));
        let registers = m
            .get("launch__registers_per_thread")
            .and_then(|v| nonnegative_i64(*v));
        let shmem_static = m
            .get("launch__shared_mem_per_block_static")
            .and_then(|v| nonnegative_i64(*v));
        let shmem_dynamic = m
            .get("launch__shared_mem_per_block_dynamic")
            .and_then(|v| nonnegative_i64(*v));
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
            None::<f64>, // peak_bandwidth — would need device spec
            boundedness,
            layer_id,
        ])?;

        // Insert a launch record if we have duration and config
        let duration_ns = m
            .get("gpu__time_duration.sum")
            .or_else(|| m.get("Duration"))
            .copied()
            .unwrap_or(0.0);
        if duration_ns.is_finite() && duration_ns > 0.0 {
            let gx = valid_dim(
                m.get("launch__grid_size_x").copied().unwrap_or(0.0),
                "grid_x",
            )?;
            let gy = valid_dim(
                m.get("launch__grid_size_y").copied().unwrap_or(0.0),
                "grid_y",
            )?;
            let gz = valid_dim(
                m.get("launch__grid_size_z").copied().unwrap_or(0.0),
                "grid_z",
            )?;
            let bx = valid_dim(
                m.get("launch__block_size_x").copied().unwrap_or(0.0),
                "block_x",
            )?;
            let by = valid_dim(
                m.get("launch__block_size_y").copied().unwrap_or(0.0),
                "block_y",
            )?;
            let bz = valid_dim(
                m.get("launch__block_size_z").copied().unwrap_or(0.0),
                "block_z",
            )?;
            let sid = m.get("launch__stream_id").map(|v| *v as i64);

            launch_stmt.execute(params![
                name,
                duration_ns / 1000.0,
                gx,
                gy,
                gz,
                bx,
                by,
                bz,
                sid,
                layer_id,
            ])?;
        }
    }

    Ok(())
}

fn read_bounded_text(path: &Path) -> Result<String> {
    read_bounded_text_with_limit(path, MAX_IMPORT_BYTES)
}

fn read_bounded_text_with_limit(path: &Path, limit: u64) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let remaining = limit.saturating_sub(bytes.len() as u64);
        if remaining == 0 {
            let mut probe = [0_u8; 1];
            if file.read(&mut probe)? != 0 {
                bail!(
                    "NCU CSV {} grew beyond the maximum input size of {limit} bytes",
                    path.display()
                );
            }
            break;
        }
        let chunk_len = (remaining as usize).min(chunk.len());
        let read = file.read(&mut chunk[..chunk_len])?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8(bytes)
        .with_context(|| format!("NCU CSV {} is not valid UTF-8", path.display()))
}

pub fn classify_boundedness(compute: Option<f64>, memory: Option<f64>) -> Option<String> {
    let (c, m) = match (compute, memory) {
        (Some(c), Some(m)) => (c, m),
        _ => return None,
    };
    if !c.is_finite() || !m.is_finite() || c < 0.0 || m < 0.0 {
        return None;
    }
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

fn find_col(headers: &[String], name: &str) -> Option<usize> {
    headers.iter().position(|h| {
        h.trim()
            .trim_start_matches('\u{feff}')
            .eq_ignore_ascii_case(name)
    })
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(field.trim().to_string());
                field.clear();
            }
            _ => field.push(ch),
        }
    }
    if in_quotes {
        return Vec::new();
    }
    fields.push(field.trim().to_string());
    fields
}

fn nonnegative_i64(value: f64) -> Option<i64> {
    if value.is_finite() && value >= 0.0 && value <= i64::MAX as f64 {
        Some(value as i64)
    } else {
        None
    }
}

fn valid_dim(value: f64, name: &str) -> Result<u32> {
    if !value.is_finite() || value < 0.0 || value > u32::MAX as f64 || value.fract() != 0.0 {
        bail!("NCU {name} is not a valid non-negative integer: {value}");
    }
    Ok(value as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_memory_bound() {
        assert_eq!(
            classify_boundedness(Some(30.0), Some(80.0)).as_deref(),
            Some("memory")
        );
    }

    #[test]
    fn classify_compute_bound() {
        assert_eq!(
            classify_boundedness(Some(85.0), Some(20.0)).as_deref(),
            Some("compute")
        );
    }

    #[test]
    fn classify_latency_bound() {
        assert_eq!(
            classify_boundedness(Some(5.0), Some(3.0)).as_deref(),
            Some("latency")
        );
    }

    #[test]
    fn import_ncu_csv_basic() {
        use crate::db::GpuDb;

        let db = GpuDb::create(&tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let lid = db.add_layer("ncu", "test.csv", None, None, None).unwrap();

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        use std::io::Write;
        writeln!(
            tmp,
            r#""ID","Kernel Name","Metric Name","Metric Unit","Metric Value""#
        )
        .unwrap();
        writeln!(
            tmp,
            r#""1","my_kernel","gpu__time_duration.sum","nsecond","500000""#
        )
        .unwrap();
        writeln!(
            tmp,
            r#""1","my_kernel","sm__warps_active.avg.pct_of_peak_sustained_active","%","67.5""#
        )
        .unwrap();
        writeln!(
            tmp,
            r#""1","my_kernel","sm__throughput.avg.pct_of_peak_sustained_elapsed","%","31.2""#
        )
        .unwrap();
        writeln!(
            tmp,
            r#""1","my_kernel","dram__throughput.avg.pct_of_peak_sustained_elapsed","%","78.4""#
        )
        .unwrap();

        import_ncu_csv(&db.conn, tmp.path(), lid).unwrap();

        let occ: f64 = db
            .conn
            .query_row(
                "SELECT occupancy_pct FROM metrics WHERE kernel_name = 'my_kernel'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!((occ - 67.5).abs() < 0.1);

        let bound: String = db
            .conn
            .query_row(
                "SELECT boundedness FROM metrics WHERE kernel_name = 'my_kernel'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bound, "memory");
    }

    #[test]
    fn bounded_read_accepts_exact_limit_and_rejects_one_more_byte() {
        use std::io::Write;

        let mut exact = tempfile::NamedTempFile::new().unwrap();
        exact.write_all(b"12345678").unwrap();
        assert_eq!(
            read_bounded_text_with_limit(exact.path(), 8).unwrap(),
            "12345678"
        );

        let mut oversized = tempfile::NamedTempFile::new().unwrap();
        oversized.write_all(b"123456789").unwrap();
        let error = read_bounded_text_with_limit(oversized.path(), 8).unwrap_err();
        assert!(error.to_string().contains("maximum input size of 8 bytes"));
    }
}
