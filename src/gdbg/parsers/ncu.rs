use std::collections::{HashMap, HashSet};
use std::{fs::File, io::Read, path::Path};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};

use crate::db::set_layer_meta;
use crate::kernel::normalize_kernel_name;

const MAX_IMPORT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_NCU_ROWS: usize = 1_000_000;
const MAX_KERNELS: usize = 100_000;

/// Import NCU details/raw CSV metrics and selected replay instances.
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

    let mut headers = Vec::new();
    let mut duration_unit = "nsecond".to_string();
    let mut kernel_metrics: HashMap<String, HashMap<String, f64>> = HashMap::new();
    let mut launches: HashMap<(String, String, String, String), NcuLaunch> = HashMap::new();
    let mut instance_ids_available = true;
    let mut saw_header = false;
    let mut row_count = 0usize;
    for line in content.lines() {
        if is_ncu_csv_header(line) {
            headers = parse_csv_line(line);
            instance_ids_available &= find_col(&headers, "ID").is_some();
            saw_header = true;
            duration_unit = "nsecond".into();
            continue;
        }
        if line.contains("Kernel Name") && find_col(&parse_csv_line(line), "Kernel Name").is_some()
        {
            bail!("NCU CSV header is missing one or more required columns");
        }
        if headers.is_empty() || line.trim().is_empty() || line.starts_with("==") {
            continue;
        }
        row_count += 1;
        if row_count > MAX_NCU_ROWS {
            bail!("NCU CSV exceeds the maximum of {MAX_NCU_ROWS} data rows");
        }
        let fields = parse_csv_line(line);
        let field = |name: &str| {
            find_col(&headers, name)
                .and_then(|index| fields.get(index))
                .map(String::as_str)
                .unwrap_or("")
        };
        let kernel = field("Kernel Name");
        if kernel.is_empty() {
            let unit = field("gpu__time_duration.sum");
            if !unit.is_empty() {
                duration_unit = unit.into();
            }
            continue;
        }
        if field("ID").is_empty() {
            instance_ids_available = false;
        }
        let kernel = normalize_kernel_name(kernel);
        let metrics = kernel_metrics.entry(kernel.clone()).or_default();
        let sample = launches
            .entry((
                kernel.clone(),
                field("Process ID").into(),
                field("Context").into(),
                field("ID").into(),
            ))
            .or_insert_with(|| NcuLaunch {
                kernel,
                ..Default::default()
            });
        for (target, name) in [
            (&mut sample.grid, "Grid Size"),
            (&mut sample.block, "Block Size"),
        ] {
            if !field(name).is_empty() {
                *target = parse_size(field(name), name)?;
            }
        }
        if !field("Stream").is_empty() && !field("Stream").eq_ignore_ascii_case("n/a") {
            sample.stream = Some(field("Stream").parse().context("invalid NCU stream ID")?);
        }
        if let Some(index) = find_col(&headers, "Metric Name") {
            let name = fields.get(index).map(String::as_str).unwrap_or("");
            if let Some(value) = number(field("Metric Value")) {
                metrics.insert(name.into(), value);
                sample.metric(name, value, field("Metric Unit"))?;
            }
        } else {
            for (name, value) in headers.iter().zip(&fields) {
                if name.contains("__") {
                    if let Some(value) = number(value) {
                        metrics.insert(name.clone(), value);
                        sample.metric(name, value, &duration_unit)?;
                    }
                }
            }
        }
        if kernel_metrics.len() > MAX_KERNELS {
            bail!("NCU CSV exceeds the maximum of {MAX_KERNELS} kernels");
        }
    }
    if headers.is_empty() {
        bail!("NCU CSV contains no candidate header");
    }

    let mut instance_ids: HashMap<String, HashSet<(String, String, String)>> = HashMap::new();
    for (kernel, process_id, context, instance_id) in launches.keys() {
        if !instance_id.is_empty() {
            instance_ids.entry(kernel.clone()).or_default().insert((
                process_id.clone(),
                context.clone(),
                instance_id.clone(),
            ));
        }
    }
    let counts_key = format!("ncu_instance_counts@{layer_id}");
    let mut instance_counts: HashMap<String, usize> = dest
        .query_row("SELECT value FROM meta WHERE key=?1", [counts_key], |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default();
    for (kernel, ids) in instance_ids {
        *instance_counts.entry(kernel).or_default() += ids.len();
    }
    let available_key = format!("ncu_instance_ids_available@{layer_id}");
    let previously_available = dest
        .query_row(
            "SELECT value FROM meta WHERE key=?1",
            [available_key],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .map(|value| value == "true")
        .unwrap_or(true);
    set_layer_meta(
        dest,
        layer_id,
        "ncu_instance_counts",
        &serde_json::to_string(&instance_counts)?,
    )?;
    set_layer_meta(
        dest,
        layer_id,
        "ncu_instance_ids_available",
        &(saw_header && instance_ids_available && previously_available).to_string(),
    )?;

    // Insert into metrics table
    let mut stmt = dest.prepare(
        "INSERT OR REPLACE INTO metrics
            (kernel_name, occupancy_pct, compute_throughput_pct, memory_throughput_pct,
             registers_per_thread, shared_mem_static_bytes, shared_mem_dynamic_bytes,
             l2_hit_rate_pct, achieved_bandwidth_gb_s, peak_bandwidth_gb_s,
             boundedness, layer_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;

    for (raw_name, m) in &kernel_metrics {
        let name = normalize_kernel_name(raw_name);
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
            &name,
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
    }

    let mut launch_stmt = dest.prepare(
        "INSERT INTO launches
            (kernel_name, duration_us, grid_x, grid_y, grid_z,
             block_x, block_y, block_z, stream_id, layer_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    for sample in launches.values() {
        if let Some(duration) = sample.duration_us.filter(|value| *value > 0.0) {
            launch_stmt.execute(params![
                sample.kernel,
                duration,
                sample.grid[0],
                sample.grid[1],
                sample.grid[2],
                sample.block[0],
                sample.block[1],
                sample.block[2],
                sample.stream,
                layer_id
            ])?;
        }
    }

    Ok(())
}

#[derive(Default)]
struct NcuLaunch {
    kernel: String,
    duration_us: Option<f64>,
    grid: [Option<u32>; 3],
    block: [Option<u32>; 3],
    stream: Option<i64>,
}

impl NcuLaunch {
    fn metric(&mut self, name: &str, value: f64, unit: &str) -> Result<()> {
        match name {
            "gpu__time_duration.sum" | "Duration" => {
                self.duration_us = Some(duration_us(value, unit)?)
            }
            "launch__stream_id" => self.stream = self.stream.or_else(|| nonnegative_i64(value)),
            name if name.starts_with("launch__") => {
                for (dims, axis) in [(&mut self.grid, "grid"), (&mut self.block, "block")] {
                    for (i, key) in ["x", "y", "z"].iter().enumerate() {
                        if name == format!("launch__{axis}_size_{key}") {
                            dims[i].get_or_insert(valid_dim(value, key)?);
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

pub fn is_ncu_csv_header(line: &str) -> bool {
    if !line.contains("Kernel Name") {
        return false;
    }
    let headers = parse_csv_line(line);
    find_col(&headers, "Kernel Name").is_some()
        && ((find_col(&headers, "Metric Name").is_some()
            && find_col(&headers, "Metric Value").is_some())
            || (find_col(&headers, "ID").is_some() && find_col(&headers, "Grid Size").is_some()))
}

fn number(value: &str) -> Option<f64> {
    value
        .replace(',', "")
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

fn duration_us(value: f64, unit: &str) -> Result<f64> {
    let scale = match unit {
        "" | "nsecond" | "ns" => 0.001,
        "usecond" | "us" => 1.0,
        "msecond" | "ms" => 1000.0,
        "second" | "s" => 1_000_000.0,
        _ => bail!("unsupported NCU duration unit: {unit}"),
    };
    if value < 0.0 {
        bail!("negative NCU duration: {value}");
    }
    let duration = value * scale;
    if !duration.is_finite() {
        bail!("NCU duration is out of range: {value}");
    }
    Ok(duration)
}

fn parse_size(value: &str, name: &str) -> Result<[Option<u32>; 3]> {
    if value.eq_ignore_ascii_case("n/a") {
        return Ok([None; 3]);
    }
    let parts: Vec<_> = value.trim_matches(['(', ')']).split(',').collect();
    if parts.len() != 3 {
        bail!("NCU {name} must contain three dimensions: {value}");
    }
    let mut result = [None; 3];
    for (target, part) in result.iter_mut().zip(parts) {
        let value = part
            .trim()
            .parse::<f64>()
            .with_context(|| format!("invalid NCU {name}: {value}"))?;
        *target = Some(valid_dim(value, name)?);
    }
    Ok(result)
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
    if !value.is_finite() || value <= 0.0 || value > u32::MAX as f64 || value.fract() != 0.0 {
        bail!("NCU {name} is not a valid positive integer: {value}");
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
    fn counter_only_raw_page_counts_instances_without_timeline_launches() {
        use crate::db::GpuDb;

        let db =
            GpuDb::create(&tempfile::tempdir().unwrap().keep().join("counter-only.db")).unwrap();
        let layer = db
            .add_layer("ncu", "counter-only.csv", None, None, None)
            .unwrap();
        let mut csv = tempfile::NamedTempFile::new().unwrap();
        use std::io::Write;
        writeln!(csv, "\"ID\",\"Process ID\",\"Kernel Name\",\"Context\",\"Stream\",\"Block Size\",\"Grid Size\",\"smsp__pcsamp_sample_count\"").unwrap();
        writeln!(
            csv,
            "\"0\",\"429257\",\"humming_w13\",\"1\",\"9\",\"(256,1,1)\",\"(528,1,1)\",\"5\""
        )
        .unwrap();
        writeln!(
            csv,
            "\"1\",\"429257\",\"humming_w2\",\"1\",\"9\",\"(256,1,1)\",\"(528,1,1)\",\"7\""
        )
        .unwrap();

        import_ncu_csv(&db.conn, csv.path(), layer).unwrap();

        assert_eq!(db.total_launch_count(), 0);
        assert_eq!(db.unique_kernel_count(), 0);
        assert!(db.ncu_instance_ids_available());
        assert_eq!(db.ncu_instance_counts().unwrap().values().sum::<usize>(), 2);
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
