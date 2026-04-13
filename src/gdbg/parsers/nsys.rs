use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};

/// Parse an nsys-rep SQLite database and INSERT into our session DB.
pub fn import_nsys_rep(dest: &Connection, nsys_path: &Path, layer_id: i64) -> Result<()> {
    let src = Connection::open_with_flags(
        nsys_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("cannot open {}", nsys_path.display()))?;

    let has_kernels = import_kernels(dest, &src, layer_id)?;
    import_transfers(dest, &src, layer_id)?;
    import_allocations(dest, &src, layer_id)?;
    import_nvtx_regions(dest, &src, layer_id)?;
    import_device_info(dest, &src)?;

    if !has_kernels {
        // No GPU kernel data — WSL2 or missing CUPTI permissions.
        // Fall back to CUDA runtime API data for basic launch counts.
        import_runtime_api(dest, &src, layer_id)?;
    }

    import_wall_time(dest)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Kernel launches
// ---------------------------------------------------------------------------

/// Import GPU kernel launches. Returns true if kernel data was found.
fn import_kernels(dest: &Connection, src: &Connection, layer_id: i64) -> Result<bool> {
    let table = match find_table(src, &[
        "CUPTI_ACTIVITY_KIND_KERNEL",
        "CUPTI_ACTIVITY_KIND_CONCURRENT_KERNEL",
    ]) {
        Ok(t) => t,
        Err(_) => return Ok(false),
    };

    // Newer nsys versions (2025+) store demangledName as an INTEGER foreign
    // key into the StringIds table, while older versions store it as TEXT.
    // Detect which format we have and build the query accordingly.
    let has_string_ids = find_table(src, &["StringIds"]).is_ok();
    let name_is_integer = is_column_integer(src, &table, "demangledName");
    let use_join = has_string_ids && name_is_integer;

    let sql = if use_join {
        format!(
            "SELECT s.value, k.start, k.end,
                    k.gridX, k.gridY, k.gridZ,
                    k.blockX, k.blockY, k.blockZ,
                    k.streamId, k.correlationId
             FROM {table} k
             JOIN StringIds s ON s.id = k.demangledName
             ORDER BY k.start"
        )
    } else {
        format!(
            "SELECT demangledName, start, end,
                    gridX, gridY, gridZ,
                    blockX, blockY, blockZ,
                    streamId, correlationId
             FROM {table}
             ORDER BY start"
        )
    };

    let mut read = src.prepare(&sql)?;

    let mut write = dest.prepare(
        "INSERT INTO launches
            (kernel_name, duration_us, grid_x, grid_y, grid_z,
             block_x, block_y, block_z, stream_id, start_us,
             correlation_id, layer_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
    )?;

    let rows = read.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,   // start ns
            row.get::<_, i64>(2)?,   // end ns
            row.get::<_, u32>(3)?,   // grid_x
            row.get::<_, u32>(4)?,
            row.get::<_, u32>(5)?,
            row.get::<_, u32>(6)?,   // block_x
            row.get::<_, u32>(7)?,
            row.get::<_, u32>(8)?,
            row.get::<_, u32>(9)?,   // stream_id
            row.get::<_, i64>(10)?,  // correlation_id
        ))
    })?;

    for row in rows {
        let (name, start_ns, end_ns, gx, gy, gz, bx, by, bz, sid, cid) = row?;
        let duration_us = (end_ns - start_ns) as f64 / 1000.0;
        let start_us = start_ns as f64 / 1000.0;
        write.execute(params![
            name, duration_us,
            gx, gy, gz, bx, by, bz,
            sid, start_us, cid, layer_id
        ])?;
    }

    Ok(true)
}

// ---------------------------------------------------------------------------
// Memory transfers
// ---------------------------------------------------------------------------

fn import_transfers(dest: &Connection, src: &Connection, layer_id: i64) -> Result<()> {
    let table = match find_table(src, &["CUPTI_ACTIVITY_KIND_MEMCPY"]) {
        Ok(t) => t,
        Err(_) => return Ok(()),
    };

    let mut read = src.prepare(&format!(
        "SELECT copyKind, start, end, bytes, streamId FROM {table} ORDER BY start"
    ))?;

    let mut write = dest.prepare(
        "INSERT INTO transfers (kind, bytes, duration_us, start_us, stream_id, layer_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
    )?;

    let rows = read.query_map([], |row| {
        Ok((
            row.get::<_, i32>(0)?,
            row.get::<_, i64>(1)?,  // start ns
            row.get::<_, i64>(2)?,  // end ns
            row.get::<_, i64>(3)?,  // bytes
            row.get::<_, u32>(4)?,  // stream_id
        ))
    })?;

    for row in rows {
        let (kind, start_ns, end_ns, bytes, sid) = row?;
        let kind_str = match kind {
            1 => "H2D",
            2 => "D2H",
            3 => "D2D",
            _ => "Peer",
        };
        write.execute(params![
            kind_str,
            bytes,
            (end_ns - start_ns) as f64 / 1000.0,
            start_ns as f64 / 1000.0,
            sid,
            layer_id
        ])?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// GPU memory allocations
// ---------------------------------------------------------------------------

/// Import CUDA device memory alloc/free events.  Requires nsys to have been
/// run with `--cuda-memory-usage=true`; otherwise the source table is absent
/// and we skip silently.
///
/// `memoryOperationType` in the nsys schema: 0 = Allocation, 1 = Deallocation.
/// Our `allocations.bytes` is always positive — the `op` column distinguishes.
fn import_allocations(dest: &Connection, src: &Connection, layer_id: i64) -> Result<()> {
    let table = match find_table(src, &["CUDA_GPU_MEMORY_USAGE_EVENTS"]) {
        Ok(t) => t,
        Err(_) => return Ok(()),
    };

    let mut read = src.prepare(&format!(
        "SELECT start, memoryOperationType, address, bytes, streamId FROM {table} ORDER BY start"
    ))?;

    let mut write = dest.prepare(
        "INSERT INTO allocations (op, address, bytes, start_us, stream_id, layer_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
    )?;

    let rows = read.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,           // start_ns
            row.get::<_, i32>(1)?,           // oper type
            row.get::<_, i64>(2)?,           // address
            row.get::<_, i64>(3)?,           // bytes
            row.get::<_, Option<u32>>(4)?,   // streamId
        ))
    })?;

    for row in rows {
        let (start_ns, oper, addr, bytes, sid) = row?;
        // nsys: 0 = Allocation, 1 = Deallocation. Skip any future oper types
        // rather than silently misclassifying them as frees and corrupting
        // peak/leak analysis.
        let op = match oper {
            0 => "alloc",
            1 => "free",
            _ => continue,
        };
        write.execute(params![
            op,
            addr,
            bytes,
            start_ns as f64 / 1000.0,
            sid,
            layer_id
        ])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// NVTX regions
// ---------------------------------------------------------------------------

fn import_nvtx_regions(dest: &Connection, src: &Connection, layer_id: i64) -> Result<()> {
    let table = match find_table(src, &["NVTX_EVENTS"]) {
        Ok(t) => t,
        Err(_) => return Ok(()),
    };

    let mut read = src.prepare(&format!(
        "SELECT text, start, end FROM {table}
         WHERE end > start AND text IS NOT NULL
         ORDER BY start"
    ))?;

    let mut write = dest.prepare(
        "INSERT INTO regions (name, start_us, duration_us, layer_id)
         VALUES (?1, ?2, ?3, ?4)"
    )?;

    let rows = read.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;

    for row in rows {
        let (name, start_ns, end_ns) = row?;
        write.execute(params![
            name,
            start_ns as f64 / 1000.0,
            (end_ns - start_ns) as f64 / 1000.0,
            layer_id
        ])?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Fallback: CUDA runtime API (when GPU kernel tracing unavailable, e.g. WSL2)
// ---------------------------------------------------------------------------

fn import_runtime_api(dest: &Connection, src: &Connection, layer_id: i64) -> Result<()> {
    let table = match find_table(src, &["CUPTI_ACTIVITY_KIND_RUNTIME"]) {
        Ok(t) => t,
        Err(_) => return Ok(()),
    };

    // StringIds table maps nameId → function name
    let has_strings = find_table(src, &["StringIds"]).is_ok();
    if !has_strings {
        return Ok(());
    }

    // Get cudaLaunchKernel calls with timing from the runtime API
    let sql = format!(
        "SELECT s.value, r.start, r.end, r.correlationId
         FROM {table} r
         JOIN StringIds s ON s.id = r.nameId
         WHERE s.value LIKE 'cudaLaunchKernel%'
         ORDER BY r.start"
    );

    let mut read = src.prepare(&sql)?;
    let mut write = dest.prepare(
        "INSERT INTO launches
            (kernel_name, duration_us, start_us, correlation_id, layer_id)
         VALUES (?1, ?2, ?3, ?4, ?5)"
    )?;

    let rows = read.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<i64>>(3)?,
        ))
    })?;

    let mut count = 0;
    for row in rows {
        let (_api_name, start_ns, end_ns, corr_id) = row?;
        let duration_us = (end_ns - start_ns) as f64 / 1000.0;
        let start_us = start_ns as f64 / 1000.0;
        // We only know this is a cudaLaunchKernel call — the actual kernel name
        // is in the GPU activity trace which isn't available.
        write.execute(params![
            "cudaLaunchKernel (GPU trace unavailable)",
            duration_us,
            start_us,
            corr_id,
            layer_id
        ])?;
        count += 1;
    }

    if count > 0 {
        // Store a note that this is CPU-side API timing, not GPU kernel timing
        dest.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('nsys_warning', ?1)",
            params!["GPU kernel tracing unavailable (WSL2 or missing permissions). \
                     Showing CPU-side cudaLaunchKernel API timing only. \
                     For full GPU profiling, run on native Linux with root or appropriate permissions."],
        )?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Device info
// ---------------------------------------------------------------------------

fn import_device_info(dest: &Connection, src: &Connection) -> Result<()> {
    let table = match find_table(src, &["TARGET_INFO_CUDA_GPU", "TARGET_INFO_GPU"]) {
        Ok(t) => t,
        Err(_) => return Ok(()),
    };

    let name: Option<String> = src
        .query_row(&format!("SELECT name FROM {table} LIMIT 1"), [], |row| {
            row.get(0)
        })
        .ok();

    if let Some(name) = name {
        dest.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('device', ?1)",
            params![name],
        )?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Wall time — computed from launch + transfer span
// ---------------------------------------------------------------------------

pub(crate) fn import_wall_time(dest: &Connection) -> Result<()> {
    // Span covers both kernel launches and memory transfers — whichever
    // starts earliest to whichever ends latest.
    let wall: f64 = dest
        .query_row(
            "SELECT COALESCE(MAX(end_us) - MIN(start_us), 0) FROM (
                 SELECT start_us, start_us + duration_us AS end_us
                 FROM launches WHERE start_us IS NOT NULL
                 UNION ALL
                 SELECT start_us, start_us + duration_us AS end_us
                 FROM transfers WHERE start_us IS NOT NULL
             )",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0.0);

    dest.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('wall_time_us', ?1)",
        params![wall.to_string()],
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn find_table(conn: &Connection, candidates: &[&str]) -> Result<String> {
    for name in candidates {
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |row| row.get::<_, i64>(0),
            )
            .map(|c| c > 0)
            .unwrap_or(false);
        if exists {
            return Ok(name.to_string());
        }
    }
    bail!("no matching table (tried: {})", candidates.join(", "))
}

/// Check whether a column is declared as INTEGER in the table schema.
/// Used to detect nsys schema changes (e.g. demangledName: TEXT vs INTEGER FK).
fn is_column_integer(conn: &Connection, table: &str, column: &str) -> bool {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(1)?,  // name
            row.get::<_, String>(2)?,  // type
        ))
    });
    match rows {
        Ok(rows) => {
            for row in rows.flatten() {
                if row.0.eq_ignore_ascii_case(column) {
                    return row.1.eq_ignore_ascii_case("INTEGER");
                }
            }
            false
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::GpuDb;

    #[test]
    fn find_table_missing() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(find_table(&conn, &["NOPE"]).is_err());
    }

    #[test]
    fn import_wall_time_empty() {
        let db = GpuDb::create(&tempfile::tempdir().unwrap().into_path().join("t.db")).unwrap();
        import_wall_time(&db.conn).unwrap();
        assert_eq!(db.meta("wall_time_us"), "0");
    }
}
