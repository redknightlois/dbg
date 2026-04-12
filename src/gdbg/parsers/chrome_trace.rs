use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
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
    #[serde(default)]
    ts: f64,
    #[serde(default)]
    dur: f64,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

/// Parse a Chrome Trace JSON (torch.profiler export) and INSERT into session DB.
pub fn import_chrome_trace(dest: &Connection, json_path: &Path, layer_id: i64) -> Result<()> {
    let content = std::fs::read_to_string(json_path)
        .with_context(|| format!("cannot read {}", json_path.display()))?;

    let trace: ChromeTrace = serde_json::from_str(&content)
        .with_context(|| format!("cannot parse {}", json_path.display()))?;

    import_kernel_events(dest, &trace.trace_events, layer_id)?;
    import_ops(dest, &trace.trace_events, layer_id)?;

    Ok(())
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
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
    )?;

    for event in events {
        if event.ph != "X" {
            continue;
        }
        if event.cat != "kernel" {
            continue;
        }

        let grid = extract_tuple(&event.args, "grid", "grid_x", "grid_y", "grid_z");
        let block = extract_tuple(&event.args, "block", "block_x", "block_y", "block_z");
        let stream = extract_u32(&event.args, "stream")
            .or_else(|| extract_u32(&event.args, "stream_id"));
        let corr = extract_u64(&event.args, "correlation")
            .or_else(|| extract_u64(&event.args, "external id"));

        stmt.execute(params![
            event.name,
            event.dur,
            grid.map(|g| g.0), grid.map(|g| g.1), grid.map(|g| g.2),
            block.map(|b| b.0), block.map(|b| b.1), block.map(|b| b.2),
            stream,
            event.ts,
            corr.map(|c| c as i64),
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
        invocations.push(OpInvocation {
            name: event.name.clone(),
            start_us: event.ts,
            end_us: event.ts + event.dur,
            module_path: extract_string(&event.args, "Python module id")
                .or_else(|| extract_string(&event.args, "module")),
            input_shapes: event.args.as_ref().and_then(|a| {
                a.get("Input Dims")
                    .or_else(|| a.get("input_shapes"))
                    .map(|v| v.to_string())
            }),
        });
    }

    // Step 2: Aggregate by name for the ops table
    let mut op_agg: HashMap<String, (f64, Option<String>, Option<String>)> = HashMap::new();
    for inv in &invocations {
        let entry = op_agg.entry(inv.name.clone()).or_insert((0.0, None, None));
        entry.0 += inv.end_us - inv.start_us;
        if entry.1.is_none() { entry.1 = inv.module_path.clone(); }
        if entry.2.is_none() { entry.2 = inv.input_shapes.clone(); }
    }

    let mut op_stmt = dest.prepare(
        "INSERT INTO ops (name, module_path, cpu_time_us, gpu_time_us, input_shapes, layer_id)
         VALUES (?1, ?2, ?3, 0, ?4, ?5)"
    )?;

    // Track op name → id for the correlation step
    let mut op_ids: HashMap<String, i64> = HashMap::new();
    for (name, (cpu_time, module_path, input_shapes)) in &op_agg {
        op_stmt.execute(params![name, module_path, cpu_time, input_shapes, layer_id])?;
        op_ids.insert(name.clone(), dest.last_insert_rowid());
    }

    // Step 3: Correlate kernel launches to ops by temporal containment.
    // A kernel belongs to the innermost (shortest) op whose time window contains
    // the kernel's start timestamp.
    // Sort invocations by duration ascending so innermost ops are checked first.
    invocations.sort_by(|a, b| {
        let da = a.end_us - a.start_us;
        let db = b.end_us - b.start_us;
        da.partial_cmp(&db).unwrap()
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
        "INSERT OR IGNORE INTO op_kernel_map (op_id, kernel_name) VALUES (?1, ?2)"
    )?;
    for (_, kernel_name, k_start) in &kernels {
        // Find innermost containing op
        for inv in &invocations {
            if *k_start >= inv.start_us && *k_start <= inv.end_us {
                if let Some(&op_id) = op_ids.get(&inv.name) {
                    map_stmt.execute(params![op_id, kernel_name])?;
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
        JOIN launches l ON l.kernel_name = okm.kernel_name AND l.layer_id = ?1
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
) -> Option<(u32, u32, u32)> {
    let args = args.as_ref()?;
    if let Some(arr) = args.get(array_key).and_then(|v| v.as_array()) {
        if arr.len() >= 3 {
            return Some((
                arr[0].as_u64()? as u32,
                arr[1].as_u64()? as u32,
                arr[2].as_u64()? as u32,
            ));
        }
    }
    Some((
        args.get(x_key)?.as_u64()? as u32,
        args.get(y_key)?.as_u64()? as u32,
        args.get(z_key)?.as_u64()? as u32,
    ))
}

fn extract_u32(args: &Option<serde_json::Value>, key: &str) -> Option<u32> {
    args.as_ref()?.get(key)?.as_u64().map(|v| v as u32)
}

fn extract_u64(args: &Option<serde_json::Value>, key: &str) -> Option<u64> {
    args.as_ref()?.get(key)?.as_u64()
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
        let db = GpuDb::create(&tempfile::tempdir().unwrap().into_path().join("t.db")).unwrap();
        let lid = db.add_layer("torch", "test.json", None, None, None).unwrap();

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

        let op_count: i64 = db.conn.query_row(
            "SELECT COUNT(*) FROM ops", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(op_count, 1);
    }
}
