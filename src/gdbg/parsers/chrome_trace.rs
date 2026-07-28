use std::path::Path;
use std::{fs::File, io::Read};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::Deserialize;

#[derive(Deserialize)]
struct ChromeTrace {
    #[serde(rename = "traceEvents")]
    trace_events: Vec<TraceEvent>,
}

#[derive(Deserialize)]
struct TraceEvent {
    #[serde(default)]
    name: String,
    #[serde(default)]
    cat: String,
    #[serde(default)]
    ph: String,
    ts: Option<f64>,
    dur: Option<f64>,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

const MAX_IMPORT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TRACE_EVENTS: usize = 1_000_000;

/// Parse a Chrome Trace JSON (torch.profiler export) and INSERT into session DB.
pub fn import_chrome_trace(dest: &Connection, json_path: &Path, layer_id: i64) -> Result<()> {
    let size = std::fs::metadata(json_path)
        .with_context(|| format!("stat {}", json_path.display()))?
        .len();
    if size > MAX_IMPORT_BYTES {
        bail!(
            "Chrome trace {} is too large ({size} bytes; maximum is {MAX_IMPORT_BYTES})",
            json_path.display()
        );
    }
    let content = read_bounded_text(json_path, "Chrome trace")?;

    let trace: ChromeTrace = serde_json::from_str(&content)
        .with_context(|| format!("cannot parse {}", json_path.display()))?;
    if trace.trace_events.len() > MAX_TRACE_EVENTS {
        bail!(
            "Chrome trace contains {} events; maximum is {MAX_TRACE_EVENTS}",
            trace.trace_events.len()
        );
    }

    validate_supported_events(&trace.trace_events)?;

    import_kernel_events(dest, &trace.trace_events, layer_id)?;
    import_ops(dest, &trace.trace_events, layer_id)?;

    Ok(())
}

/// Validate only the event forms this importer persists. Metadata and flow
/// events are allowed to omit timing, but a supported complete event must
/// not turn serde defaults into a fabricated zero-time database row.
fn validate_supported_events(events: &[TraceEvent]) -> Result<()> {
    let mut supported = 0;
    for event in events {
        if event.ph != "X"
            || !matches!(
                event.cat.as_str(),
                "kernel" | "cpu_op" | "user_annotation" | "Operator"
            )
        {
            continue;
        }
        supported += 1;
        if event.name.trim().is_empty() {
            bail!("Chrome trace supported event has an empty name");
        }
        if let Some(args) = &event.args {
            if !args.is_object() {
                bail!("Chrome trace event args must be an object");
            }
        }
        if event.cat == "kernel" {
            extract_tuple(&event.args, "grid", "grid_x", "grid_y", "grid_z")?;
            extract_tuple(&event.args, "block", "block_x", "block_y", "block_z")?;
        }
        let (ts, dur) = event_timing(event)?;
        if !ts.is_finite() || ts < 0.0 || !dur.is_finite() || dur < 0.0 {
            bail!("Chrome trace event has invalid timing: ts={ts}, dur={dur}");
        }
        if ts + dur > f64::MAX {
            bail!("Chrome trace event timing overflows its end timestamp");
        }
    }
    if supported == 0 {
        bail!("Chrome trace contains no supported complete events");
    }
    Ok(())
}

fn event_timing(event: &TraceEvent) -> Result<(f64, f64)> {
    let ts = event
        .ts
        .ok_or_else(|| anyhow::anyhow!("Chrome trace event is missing ts"))?;
    let dur = event
        .dur
        .ok_or_else(|| anyhow::anyhow!("Chrome trace event is missing dur"))?;
    Ok((ts, dur))
}

/// Read an import without allowing a file that grows after the metadata
/// check to make the process accumulate unbounded input. Reads are capped to
/// the remaining capacity, and a one-byte probe detects growth at the exact
/// limit without appending that byte to the buffer.
fn read_bounded_text(path: &Path, kind: &str) -> Result<String> {
    read_bounded_text_with_limit(path, kind, MAX_IMPORT_BYTES)
}

fn read_bounded_text_with_limit(path: &Path, kind: &str, limit: u64) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let remaining = limit.saturating_sub(bytes.len() as u64);
        if remaining == 0 {
            let mut probe = [0_u8; 1];
            if file.read(&mut probe)? != 0 {
                bail!(
                    "{kind} {} grew beyond the maximum input size of {limit} bytes",
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
        .with_context(|| format!("{kind} {} is not valid UTF-8", path.display()))
}

// ---------------------------------------------------------------------------
// GPU kernel events
// ---------------------------------------------------------------------------

fn import_kernel_events(dest: &Connection, events: &[TraceEvent], layer_id: i64) -> Result<()> {
    let mut stmt = dest.prepare(
        "INSERT INTO launches
            (kernel_name, duration_us, grid_x, grid_y, grid_z,
             block_x, block_y, block_z, stream_id, start_us,
             correlation_id, layer_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;

    for event in events {
        if event.ph != "X" {
            continue;
        }
        if event.cat != "kernel" {
            continue;
        }

        let (ts, dur) = event_timing(event)?;

        let grid = extract_tuple(&event.args, "grid", "grid_x", "grid_y", "grid_z")?;
        let block = extract_tuple(&event.args, "block", "block_x", "block_y", "block_z")?;
        let stream = match value_for(&event.args, "stream") {
            Some(value) => Some(parse_u32(value, "stream")?),
            None => value_for(&event.args, "stream_id")
                .map(|value| parse_u32(value, "stream_id"))
                .transpose()?,
        };
        let corr = match value_for(&event.args, "correlation") {
            Some(value) => Some(parse_i64(value, "correlation")?),
            None => value_for(&event.args, "external id")
                .map(|value| parse_i64(value, "external id"))
                .transpose()?,
        };

        stmt.execute(params![
            event.name,
            dur,
            grid.map(|g| g.0),
            grid.map(|g| g.1),
            grid.map(|g| g.2),
            block.map(|b| b.0),
            block.map(|b| b.1),
            block.map(|b| b.2),
            stream,
            ts,
            corr,
            layer_id,
        ])?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// CPU ops + op↔kernel mapping
// ---------------------------------------------------------------------------

fn import_ops(dest: &Connection, events: &[TraceEvent], layer_id: i64) -> Result<()> {
    // Step 1: Collect per-invocation op events with their time windows
    struct OpInvocation {
        name: String,
        start_us: f64,
        end_us: f64,
        module_path: Option<String>,
        input_shapes: Option<String>,
    }

    let mut invocations: Vec<OpInvocation> = Vec::new();

    for event in events {
        if event.ph != "X" {
            continue;
        }
        match event.cat.as_str() {
            "cpu_op" | "user_annotation" | "Operator" => {}
            _ => continue,
        }
        let (ts, dur) = event_timing(event)?;
        invocations.push(OpInvocation {
            name: event.name.clone(),
            start_us: ts,
            end_us: ts + dur,
            module_path: extract_string(&event.args, "Python module id")
                .or_else(|| extract_string(&event.args, "module")),
            input_shapes: event.args.as_ref().and_then(|a| {
                a.get("Input Dims")
                    .or_else(|| a.get("input_shapes"))
                    .map(|v| v.to_string())
            }),
        });
    }

    // Keep one row per invocation. The same operator name can occur more
    // than once in a trace; merging by name loses the identity needed to
    // assign a kernel launch to the correct invocation.
    let mut op_stmt = dest.prepare(
        "INSERT INTO ops (name, module_path, cpu_time_us, gpu_time_us, input_shapes, layer_id)
         VALUES (?1, ?2, ?3, 0, ?4, ?5)",
    )?;
    let mut op_ids = Vec::with_capacity(invocations.len());
    for inv in &invocations {
        op_stmt.execute(params![
            inv.name,
            inv.module_path,
            inv.end_us - inv.start_us,
            inv.input_shapes,
            layer_id
        ])?;
        op_ids.push(dest.last_insert_rowid());
    }

    // Step 3: Correlate kernel launches to ops by temporal containment.
    // A kernel belongs to the innermost (shortest) op whose time window contains
    // the kernel's start timestamp.
    // Sort invocations by duration ascending so innermost ops are checked first.
    let mut containment_order: Vec<usize> = (0..invocations.len()).collect();
    containment_order.sort_by(|&a, &b| {
        let da = invocations[a].end_us - invocations[a].start_us;
        let db = invocations[b].end_us - invocations[b].start_us;
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });

    // Collect kernel launches from this layer
    let mut kern_stmt = dest.prepare(
        "SELECT id, kernel_name, start_us FROM launches WHERE layer_id = ?1 AND start_us IS NOT NULL"
    )?;
    let kernels: Vec<(i64, String, f64)> = kern_stmt
        .query_map(params![layer_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut map_stmt = dest.prepare(
        "INSERT OR IGNORE INTO op_kernel_map (op_id, kernel_name, launch_id) VALUES (?1, ?2, ?3)",
    )?;
    for (launch_id, kernel_name, k_start) in &kernels {
        // Find innermost containing op
        for &index in &containment_order {
            let inv = &invocations[index];
            if *k_start >= inv.start_us && *k_start <= inv.end_us {
                if let Some(&op_id) = op_ids.get(index) {
                    map_stmt.execute(params![op_id, kernel_name, launch_id])?;
                    // Accumulate GPU time for this op
                    // (We don't have per-launch duration easily here, query it)
                    break;
                }
            }
        }
    }

    // Step 4: Update ops.gpu_time_us from correlated kernel launches
    let update_sql = "UPDATE ops SET gpu_time_us = (
        SELECT COALESCE(SUM(l.duration_us), 0)
        FROM op_kernel_map okm
        JOIN launches l ON l.layer_id = ?1
            AND ((okm.launch_id IS NOT NULL AND l.id = okm.launch_id)
                 OR (okm.launch_id IS NULL AND l.kernel_name = okm.kernel_name))
        WHERE okm.op_id = ops.id
    ) WHERE layer_id = ?1";
    dest.execute(update_sql, params![layer_id])?;

    Ok(())
}

// ---------------------------------------------------------------------------
// JSON field extractors
// ---------------------------------------------------------------------------

fn extract_tuple(
    args: &Option<serde_json::Value>,
    array_key: &str,
    x_key: &str,
    y_key: &str,
    z_key: &str,
) -> Result<Option<(u32, u32, u32)>> {
    let Some(args) = args.as_ref() else {
        return Ok(None);
    };
    if let Some(value) = args.get(array_key) {
        let Some(arr) = value.as_array() else {
            bail!("Chrome trace field {array_key} must be an array")
        };
        if arr.len() < 3 {
            bail!("Chrome trace field {array_key} must contain three integers")
        }
        return Ok(Some((
            parse_u32(&arr[0], &format!("{array_key}[0]"))?,
            parse_u32(&arr[1], &format!("{array_key}[1]"))?,
            parse_u32(&arr[2], &format!("{array_key}[2]"))?,
        )));
    }
    let (Some(x), Some(y), Some(z)) = (args.get(x_key), args.get(y_key), args.get(z_key)) else {
        return Ok(None);
    };
    Ok(Some((
        parse_u32(x, x_key)?,
        parse_u32(y, y_key)?,
        parse_u32(z, z_key)?,
    )))
}

fn value_for<'a>(args: &'a Option<serde_json::Value>, key: &str) -> Option<&'a serde_json::Value> {
    args.as_ref()?.get(key)
}

fn parse_u32(value: &serde_json::Value, key: &str) -> Result<u32> {
    let number = value.as_u64().ok_or_else(|| {
        anyhow::anyhow!("Chrome trace field {key} must be a non-negative integer")
    })?;
    u32::try_from(number).map_err(|_| anyhow::anyhow!("Chrome trace field {key} exceeds u32::MAX"))
}

fn parse_i64(value: &serde_json::Value, key: &str) -> Result<i64> {
    let number = value.as_u64().ok_or_else(|| {
        anyhow::anyhow!("Chrome trace field {key} must be a non-negative integer")
    })?;
    i64::try_from(number)
        .map_err(|_| anyhow::anyhow!("Chrome trace field {key} exceeds SQLite integer range"))
}

fn extract_string(args: &Option<serde_json::Value>, key: &str) -> Option<String> {
    args.as_ref()?.get(key)?.as_str().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::GpuDb;
    use std::io::Write;

    #[test]
    fn import_chrome_trace_basic() {
        let db = GpuDb::create(&tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let lid = db
            .add_layer("torch", "test.json", None, None, None)
            .unwrap();

        let trace = r#"{
            "traceEvents": [
                {
                    "name": "ampere_sgemm_128x32",
                    "cat": "kernel",
                    "ph": "X",
                    "ts": 1000.0,
                    "dur": 50.5,
                    "pid": 1, "tid": 1,
                    "args": {"grid": [128, 1, 1], "block": [256, 1, 1], "stream": 7}
                },
                {
                    "name": "aten::linear",
                    "cat": "cpu_op",
                    "ph": "X",
                    "ts": 900.0,
                    "dur": 120.0,
                    "pid": 1, "tid": 0
                }
            ]
        }"#;

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "{trace}").unwrap();

        import_chrome_trace(&db.conn, tmp.path(), lid).unwrap();

        assert_eq!(db.unique_kernel_count(), 1);
        assert_eq!(db.total_launch_count(), 1);

        let op_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM ops", [], |row| row.get(0))
            .unwrap();
        assert_eq!(op_count, 1);
    }

    #[test]
    fn bounded_read_accepts_exact_limit_and_rejects_one_more_byte() {
        let mut exact = tempfile::NamedTempFile::new().unwrap();
        exact.write_all(b"12345678").unwrap();
        assert_eq!(
            read_bounded_text_with_limit(exact.path(), "Chrome trace", 8).unwrap(),
            "12345678"
        );

        let mut oversized = tempfile::NamedTempFile::new().unwrap();
        oversized.write_all(b"123456789").unwrap();
        let error = read_bounded_text_with_limit(oversized.path(), "Chrome trace", 8).unwrap_err();
        assert!(error.to_string().contains("maximum input size of 8 bytes"));
    }

    #[test]
    fn malformed_supported_event_is_rejected_without_a_zero_row() {
        let db = GpuDb::create(&tempfile::tempdir().unwrap().keep().join("bad.db")).unwrap();
        let lid = db.add_layer("torch", "bad.json", None, None, None).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            r#"{"traceEvents":[{"name":"k","cat":"kernel","ph":"X","dur":1.0}]}"#,
        )
        .unwrap();
        let error = import_chrome_trace(&db.conn, tmp.path(), lid).unwrap_err();
        assert!(error.to_string().contains("missing ts"));
        assert_eq!(db.total_launch_count(), 0);
    }

    #[test]
    fn trace_with_no_supported_events_is_rejected() {
        let db = GpuDb::create(&tempfile::tempdir().unwrap().keep().join("empty.db")).unwrap();
        let lid = db
            .add_layer("torch", "empty.json", None, None, None)
            .unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            r#"{"traceEvents":[{"ph":"M","name":"process_name"}]}"#,
        )
        .unwrap();
        let error = import_chrome_trace(&db.conn, tmp.path(), lid).unwrap_err();
        assert!(error.to_string().contains("no supported complete events"));
    }
}
