use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::de::{Error as _, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::kernel::normalize_kernel_name;

#[derive(Deserialize)]
struct ChromeTrace {
    #[serde(rename = "traceEvents")]
    trace_events: TraceEvents,
}

struct TraceEvents(Vec<TraceEvent>);

impl<'de> Deserialize<'de> for TraceEvents {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct EventsVisitor;

        impl<'de> Visitor<'de> for EventsVisitor {
            type Value = TraceEvents;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Chrome trace event array")
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut events = Vec::new();
                while let Some(event) = sequence.next_element::<TraceEvent>()? {
                    if event.ph != "X"
                        || !(event.cat == "kernel"
                            || is_op_category(&event.cat)
                            || is_launch_category(&event.cat))
                    {
                        continue;
                    }
                    if events.len() == MAX_TRACE_EVENTS {
                        return Err(A::Error::custom(format!(
                            "Chrome trace contains more than {MAX_TRACE_EVENTS} supported events"
                        )));
                    }
                    events.push(event);
                }
                Ok(TraceEvents(events))
            }
        }

        deserializer.deserialize_seq(EventsVisitor)
    }
}

#[derive(Clone, Deserialize, Eq, Hash, PartialEq)]
#[serde(untagged)]
enum TraceId {
    Integer(i64),
    Text(String),
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
    pid: Option<TraceId>,
    #[serde(default)]
    tid: Option<TraceId>,
    #[serde(default, deserialize_with = "deserialize_args")]
    args: Option<serde_json::Value>,
}

const MAX_TRACE_EVENTS: usize = 1_000_000;

/// Parse a Chrome Trace JSON (torch.profiler export) and INSERT into session DB.
pub fn import_chrome_trace(dest: &Connection, json_path: &Path, layer_id: i64) -> Result<()> {
    let file =
        File::open(json_path).with_context(|| format!("cannot read {}", json_path.display()))?;
    let trace: ChromeTrace = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("cannot parse {}", json_path.display()))?;

    validate_supported_events(&trace.trace_events.0)?;

    import_kernel_events(dest, &trace.trace_events.0, layer_id)?;
    import_ops(dest, &trace.trace_events.0, layer_id)?;

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

        let name = normalize_kernel_name(&event.name);
        stmt.execute(params![
            name,
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
        pid: Option<TraceId>,
        tid: Option<TraceId>,
        external_id: Option<i64>,
    }

    let mut invocations: Vec<OpInvocation> = Vec::new();

    for event in events {
        if event.ph != "X" {
            continue;
        }
        if !is_op_category(&event.cat) {
            continue;
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
            pid: event.pid.clone(),
            tid: event.tid.clone(),
            external_id: external_id(&event.args)?,
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

    // Duplicate external IDs cannot identify a unique operator invocation.
    let mut external_ops = HashMap::new();
    for (index, inv) in invocations.iter().enumerate() {
        if let Some(id) = inv.external_id {
            external_ops
                .entry(id)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(index));
        }
    }

    let mut kern_stmt = dest.prepare(
        "SELECT id, kernel_name, start_us FROM launches
         WHERE layer_id = ?1 AND start_us IS NOT NULL ORDER BY id",
    )?;
    let kernel_rows: Vec<(i64, String, f64)> = kern_stmt
        .query_map(params![layer_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    // Runtime correlation identifies the launch thread even after its op ends.
    let launcher_by_correlation = events
        .iter()
        .filter(|event| event.ph == "X" && is_launch_category(&event.cat))
        .filter_map(|event| {
            Some((
                event_id(event, "correlation")?,
                ((event.pid.clone(), event.tid.clone()), event.ts?),
            ))
        })
        .collect::<HashMap<_, _>>();
    let kernel_probes = events
        .iter()
        .filter(|event| event.ph == "X" && event.cat == "kernel")
        .map(|event| {
            if let Some(id) = external_id(&event.args)? {
                match external_ops.get(&id) {
                    Some(Some(index)) => return Ok(Probe::Op(*index)),
                    Some(None) => return Ok(Probe::Unmapped),
                    None => {}
                }
            }
            if let Some((context, ts)) =
                event_id(event, "correlation").and_then(|id| launcher_by_correlation.get(&id))
            {
                Ok(Probe::At(*ts, context.clone()))
            } else {
                Ok(Probe::At(
                    event_timing(event)?.0,
                    (event.pid.clone(), event.tid.clone()),
                ))
            }
        })
        .collect::<Result<Vec<_>>>()?;
    if kernel_rows.len() != kernel_probes.len() {
        bail!("Chrome trace kernel identity count changed during import");
    }
    let mut kernels = kernel_rows
        .into_iter()
        .zip(kernel_probes)
        .map(|((id, name, _), probe)| (id, name, probe))
        .collect::<Vec<_>>();
    kernels.sort_by(|a, b| {
        a.2.time()
            .partial_cmp(&b.2.time())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut starts = (0..invocations.len()).collect::<Vec<_>>();
    starts.sort_by(|&a, &b| {
        invocations[a]
            .start_us
            .partial_cmp(&invocations[b].start_us)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut ends = (0..invocations.len()).collect::<Vec<_>>();
    ends.sort_by(|&a, &b| {
        invocations[a]
            .end_us
            .partial_cmp(&invocations[b].end_us)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut next_start = 0;
    let mut next_end = 0;
    let mut active: HashMap<(Option<TraceId>, Option<TraceId>), BTreeSet<(u64, usize)>> =
        HashMap::new();

    let mut map_stmt = dest.prepare(
        "INSERT OR IGNORE INTO op_kernel_map (op_id, kernel_name, launch_id) VALUES (?1, ?2, ?3)",
    )?;
    for (launch_id, kernel_name, probe) in &kernels {
        let index = match probe {
            Probe::Op(index) => Some(*index),
            Probe::Unmapped => None,
            Probe::At(at, context) => {
                while next_start < starts.len() && invocations[starts[next_start]].start_us <= *at {
                    let index = starts[next_start];
                    let inv = &invocations[index];
                    active
                        .entry((inv.pid.clone(), inv.tid.clone()))
                        .or_default()
                        .insert(((inv.end_us - inv.start_us).to_bits(), index));
                    next_start += 1;
                }
                while next_end < ends.len() && invocations[ends[next_end]].end_us < *at {
                    let index = ends[next_end];
                    let inv = &invocations[index];
                    if let Some(set) = active.get_mut(&(inv.pid.clone(), inv.tid.clone())) {
                        set.remove(&((inv.end_us - inv.start_us).to_bits(), index));
                    }
                    next_end += 1;
                }
                active
                    .get(context)
                    .and_then(|set| set.first())
                    .map(|&(_, index)| index)
            }
        };
        if let Some(&op_id) = index.and_then(|index| op_ids.get(index)) {
            map_stmt.execute(params![op_id, kernel_name, launch_id])?;
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

/// A kernel's unique op, context-specific timing probe, or unresolved identity.
enum Probe {
    Op(usize),
    At(f64, (Option<TraceId>, Option<TraceId>)),
    Unmapped,
}

impl Probe {
    fn time(&self) -> f64 {
        match self {
            Probe::Op(_) | Probe::Unmapped => f64::NEG_INFINITY,
            Probe::At(at, ..) => *at,
        }
    }
}

fn is_op_category(cat: &str) -> bool {
    matches!(cat, "cpu_op" | "user_annotation" | "Operator")
}

/// Host-side CUDA API calls (`cuda_runtime`, `cuda_driver`, legacy `Runtime`).
fn is_launch_category(cat: &str) -> bool {
    cat.as_bytes()
        .windows(7)
        .any(|part| part.eq_ignore_ascii_case(b"runtime"))
        || cat
            .as_bytes()
            .windows(6)
            .any(|part| part.eq_ignore_ascii_case(b"driver"))
}

fn event_id(event: &TraceEvent, key: &str) -> Option<i64> {
    value_for(&event.args, key)?.as_i64()
}

// ---------------------------------------------------------------------------
// JSON field extractors
// ---------------------------------------------------------------------------

fn deserialize_args<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<serde_json::Value>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ArgsVisitor;

    impl<'de> Visitor<'de> for ArgsVisitor {
        type Value = Option<serde_json::Value>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("Chrome trace event arguments")
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut fields = serde_json::Map::new();
            while let Some(key) = map.next_key::<String>()? {
                if matches!(
                    key.as_str(),
                    "grid"
                        | "block"
                        | "grid_x"
                        | "grid_y"
                        | "grid_z"
                        | "block_x"
                        | "block_y"
                        | "block_z"
                        | "stream"
                        | "stream_id"
                        | "correlation"
                        | "External id"
                        | "external id"
                        | "Python module id"
                        | "module"
                        | "Input Dims"
                        | "input_shapes"
                ) {
                    fields.insert(key, map.next_value()?);
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            Ok(Some(serde_json::Value::Object(fields)))
        }

        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            while sequence.next_element::<IgnoredAny>()?.is_some() {}
            Ok(Some(serde_json::Value::Null))
        }

        fn visit_bool<E>(self, _: bool) -> std::result::Result<Self::Value, E> {
            Ok(Some(serde_json::Value::Null))
        }

        fn visit_i64<E>(self, _: i64) -> std::result::Result<Self::Value, E> {
            Ok(Some(serde_json::Value::Null))
        }

        fn visit_u64<E>(self, _: u64) -> std::result::Result<Self::Value, E> {
            Ok(Some(serde_json::Value::Null))
        }

        fn visit_f64<E>(self, _: f64) -> std::result::Result<Self::Value, E> {
            Ok(Some(serde_json::Value::Null))
        }

        fn visit_str<E>(self, _: &str) -> std::result::Result<Self::Value, E> {
            Ok(Some(serde_json::Value::Null))
        }
    }

    deserializer.deserialize_any(ArgsVisitor)
}

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

fn external_id(args: &Option<serde_json::Value>) -> Result<Option<i64>> {
    value_for(args, "External id")
        .or_else(|| value_for(args, "external id"))
        .map(|value| parse_i64(value, "External id"))
        .transpose()
}

fn extract_string(args: &Option<serde_json::Value>, key: &str) -> Option<String> {
    args.as_ref()?.get(key)?.as_str().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::GpuDb;
    use std::io::{BufWriter, Write};

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
                    "pid": 1, "tid": 1
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

        let mapped: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM op_kernel_map", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mapped, 1);
    }

    #[test]
    fn streams_large_ignored_args_and_more_than_a_million_metadata_events() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.json");
        let mut output = BufWriter::new(File::create(&path).unwrap());
        output.write_all(br#"{"traceEvents":[{"name":"aten::linear","cat":"cpu_op","ph":"X","ts":1,"dur":9,"args":{"External id":7,"Python module id":42,"module":"linear","Input Dims":[[1,4]],"Call stack":""#).unwrap();
        let chunk = [b'x'; 64 * 1024];
        for _ in 0..4097 {
            output.write_all(&chunk).unwrap();
        }
        output.write_all(br#""}}"#).unwrap();
        let metadata = r#",{"ph":"M","args":[true]}"#.repeat(1024);
        for _ in 0..=(MAX_TRACE_EVENTS / 1024) {
            output.write_all(metadata.as_bytes()).unwrap();
        }
        output.write_all(br#",{"name":"kernel","cat":"kernel","ph":"X","ts":11,"dur":2,"args":{"external id":7,"grid":[1,1,1],"block":[32,1,1]}}]}"#).unwrap();
        output.flush().unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > 256 * 1024 * 1024);

        let db = GpuDb::create(&directory.path().join("session.db")).unwrap();
        let layer = db
            .add_layer("torch", "large.json", None, None, None)
            .unwrap();
        import_chrome_trace(&db.conn, &path, layer).unwrap();
        assert_eq!(db.total_launch_count(), 1);
        let result: (String, String, f64) = db
            .conn
            .query_row(
                "SELECT module_path, input_shapes, gpu_time_us FROM ops",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(result, ("linear".into(), "[[1,4]]".into(), 2.0));
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
