use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};

/// A GPU profiling session backed by a SQLite database.
pub struct GpuDb {
    pub conn: Connection,
    pub path: PathBuf,
    /// Active focus filter (kernel name substring).
    pub focus: Option<String>,
    /// Active ignore filter (kernel name substring).
    pub ignore: Option<String>,
    /// Active region filter (region name substring).
    pub region_filter: Option<String>,
}

impl GpuDb {
    /// Create a new session database at the given path.
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        init_schema(&conn)?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
            focus: None,
            ignore: None,
            region_filter: None,
        })
    }

    /// Open an existing session database.
    pub fn open(path: &Path) -> Result<Self> {
        if !path.exists() {
            bail!("session not found: {}", path.display());
        }
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
            focus: None,
            ignore: None,
            region_filter: None,
        })
    }

    /// Get the session storage directory for saved sessions.
    /// Walks up to find `.git` and uses that root; falls back to cwd.
    pub fn session_dir() -> PathBuf {
        find_project_root().join(".dbg").join("gpu")
    }

    /// Save this session by copying the DB to `.dbg/gpu/<name>.gpu.db`.
    pub fn save(&self, name: &str) -> Result<PathBuf> {
        let dir = Self::session_dir();
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join(format!("{name}.gpu.db"));
        // Use SQLite backup API for safe copy of a live DB
        let mut dest_conn = Connection::open(&dest)?;
        let backup = rusqlite::backup::Backup::new(&self.conn, &mut dest_conn)?;
        backup.run_to_completion(100, std::time::Duration::from_millis(10), None)?;
        Ok(dest)
    }

    /// Load a saved session by name or path.
    pub fn load(name_or_path: &str) -> Result<Self> {
        let path = if name_or_path.ends_with(".gpu.db") || name_or_path.contains('/') {
            PathBuf::from(name_or_path)
        } else {
            Self::session_dir().join(format!("{name_or_path}.gpu.db"))
        };
        Self::open(&path)
    }

    /// List all saved sessions.
    pub fn list_saved() -> Result<Vec<SavedSession>> {
        let dir = Self::session_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut sessions = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "db")
                && path.to_str().is_some_and(|s| s.contains(".gpu."))
            {
                if let Ok(db) = Self::open(&path) {
                    let name = path
                        .file_stem()
                        .unwrap_or_default()
                        .to_str()
                        .unwrap_or_default()
                        .strip_suffix(".gpu")
                        .unwrap_or_default()
                        .to_string();
                    sessions.push(SavedSession {
                        name,
                        target: db.meta("target"),
                        device: db.meta("device"),
                        kernel_count: db.unique_kernel_count(),
                        launch_count: db.total_launch_count(),
                        layers: db.layer_names(),
                        created: db.meta("created"),
                    });
                }
            }
        }
        sessions.sort_by(|a, b| b.created.cmp(&a.created));
        Ok(sessions)
    }

    // -----------------------------------------------------------------------
    // Meta
    // -----------------------------------------------------------------------

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> String {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |row| {
                row.get(0)
            })
            .unwrap_or_default()
    }

    // -----------------------------------------------------------------------
    // Layers
    // -----------------------------------------------------------------------

    pub fn add_layer(
        &self,
        source: &str,
        file: &str,
        command: Option<&str>,
        secs: Option<f64>,
        target_hash: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO layers (source, file, collected_at, command_used, collection_secs, target_hash)
             VALUES (?1, ?2, datetime('now'), ?3, ?4, ?5)",
            params![source, file, command, secs, target_hash],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Execute a query and collect all rows via a mapping function.
    /// Returns an empty Vec on any error (safe for diagnostic/display code).
    pub fn query_vec<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        f: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Vec<T> {
        let Ok(mut stmt) = self.conn.prepare(sql) else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map(params, f) else {
            return Vec::new();
        };
        rows.filter_map(|r| r.ok()).collect()
    }

    /// Check if target hashes are consistent across all layers.
    /// Returns None if consistent, Some(warning) if mismatched.
    pub fn check_target_consistency(&self) -> Option<String> {
        let rows: Vec<(String, String)> = self.query_vec(
            "SELECT source, target_hash FROM layers WHERE target_hash IS NOT NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );

        if rows.len() < 2 {
            return None;
        }

        let first_hash = &rows[0].1;
        let mismatched: Vec<&str> = rows
            .iter()
            .filter(|(_, h)| h != first_hash)
            .map(|(s, _)| s.as_str())
            .collect();

        if mismatched.is_empty() {
            None
        } else {
            Some(format!(
                "target file changed between collection phases: {} vs {}",
                rows[0].0,
                mismatched.join(", ")
            ))
        }
    }

    /// Check kernel population consistency across layers.
    /// Returns warnings about kernels that appear in some layers but not others.
    pub fn check_kernel_consistency(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        let has_nsys = self.has_layer("nsys");
        let has_torch = self.has_layer("torch");

        if !has_nsys || !has_torch {
            return warnings;
        }

        let orphans: Vec<String> = self.query_vec(
            "SELECT DISTINCT l.kernel_name
             FROM launches l
             WHERE l.layer_id IN (SELECT id FROM layers WHERE source = 'torch')
               AND l.kernel_name NOT IN (
                 SELECT DISTINCT kernel_name FROM launches
                 WHERE layer_id IN (SELECT id FROM layers WHERE source = 'nsys')
               )",
            [],
            |row| row.get(0),
        );

        if !orphans.is_empty() {
            warnings.push(format!(
                "{} kernels in torch layer but not nsys (different run?): {}",
                orphans.len(),
                orphans.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
            ));
        }

        warnings
    }

    pub fn layer_names(&self) -> Vec<String> {
        self.query_vec(
            "SELECT DISTINCT source FROM layers ORDER BY id",
            [],
            |row| row.get(0),
        )
    }

    pub fn has_layer(&self, source: &str) -> bool {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM layers WHERE source = ?1",
                params![source],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0
    }

    /// Get the layer ID to use for timeline queries (prefer nsys, fall back to torch).
    /// Returns None if no timeline layer exists.
    pub fn timeline_layer_id(&self) -> Option<i64> {
        // Prefer nsys (has real timestamps), fall back to torch
        for source in &["nsys", "torch", "proton"] {
            if let Ok(id) = self.conn.query_row(
                "SELECT id FROM layers WHERE source = ?1 ORDER BY id LIMIT 1",
                params![source],
                |row| row.get::<_, i64>(0),
            ) {
                return Some(id);
            }
        }
        None
    }

    /// SQL fragment to filter launches to the best timeline layer.
    /// Uses `launches.layer_id` to be safe in JOIN contexts where the launches
    /// table is not aliased.  Use `timeline_filter_for("alias")` when the
    /// launches table has a different alias.
    pub fn timeline_filter(&self) -> String {
        self.timeline_filter_for("launches")
    }

    /// Like `timeline_filter`, but with a custom table alias.
    pub fn timeline_filter_for(&self, alias: &str) -> String {
        match self.timeline_layer_id() {
            Some(id) => format!("{alias}.layer_id = {id}"),
            None => "1=1".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // Failures
    // -----------------------------------------------------------------------

    pub fn add_failure(&self, phase: &str, error: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO failures (phase, error) VALUES (?1, ?2)",
            params![phase, error],
        )?;
        Ok(())
    }

    pub fn failures(&self) -> Vec<(String, String)> {
        self.query_vec(
            "SELECT phase, error FROM failures",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    }

    // -----------------------------------------------------------------------
    // Op GPU time recomputation
    // -----------------------------------------------------------------------

    /// Re-compute `ops.gpu_time_us` against the best timeline layer.
    ///
    /// During import, `ops.gpu_time_us` is computed from the torch/proton
    /// layer's kernel launches.  When an nsys layer is also present, its
    /// kernel durations are more accurate (lower profiler overhead).  This
    /// method re-correlates every op's GPU time against whichever layer
    /// `timeline_filter` selects, so that `top-ops`, `compare-ops`, and
    /// `hotpath` stay consistent with `breakdown` and `kernels`.
    pub fn recompute_op_gpu_times(&self) {
        let Some(tl_id) = self.timeline_layer_id() else { return };

        // Check whether the timeline layer is already the op layer —
        // if so, nothing to fix.
        let op_layers: Vec<String> = self.query_vec(
            "SELECT DISTINCT source FROM layers WHERE id IN (SELECT DISTINCT layer_id FROM ops)",
            [],
            |row| row.get(0),
        );
        let tl_source: String = self.conn.query_row(
            "SELECT source FROM layers WHERE id = ?1",
            params![tl_id],
            |row| row.get(0),
        ).unwrap_or_default();

        // If the only op layer is also the timeline layer, no recomputation needed.
        if op_layers.len() == 1 && op_layers[0] == tl_source {
            return;
        }

        // Re-correlate: for each op, sum kernel durations from the timeline layer.
        let _ = self.conn.execute(
            "UPDATE ops SET gpu_time_us = (
                SELECT COALESCE(SUM(l.duration_us), 0)
                FROM op_kernel_map okm
                JOIN launches l ON l.kernel_name = okm.kernel_name AND l.layer_id = ?1
                WHERE okm.op_id = ops.id
            )",
            params![tl_id],
        );
    }

    // -----------------------------------------------------------------------
    // Scalar query helpers
    // -----------------------------------------------------------------------

    /// Execute a SQL query that returns a single integer, defaulting to 0.
    pub fn count(&self, sql: &str) -> usize {
        self.conn
            .query_row(sql, [], |row| row.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }

    /// Execute a SQL query that returns a single float, defaulting to 0.0.
    pub fn scalar_f64(&self, sql: &str) -> f64 {
        self.conn
            .query_row(sql, [], |row| row.get(0))
            .unwrap_or(0.0)
    }

    // -----------------------------------------------------------------------
    // Counts
    // -----------------------------------------------------------------------

    pub fn unique_kernel_count(&self) -> usize {
        let tl = self.timeline_filter();
        self.count(&format!("SELECT COUNT(DISTINCT kernel_name) FROM launches WHERE {tl}"))
    }

    pub fn total_launch_count(&self) -> usize {
        let tl = self.timeline_filter();
        self.count(&format!("SELECT COUNT(*) FROM launches WHERE {tl}"))
    }

    pub fn total_gpu_time_us(&self) -> f64 {
        let tl = self.timeline_filter();
        self.scalar_f64(&format!("SELECT COALESCE(SUM(duration_us), 0) FROM launches WHERE {tl}"))
    }

    pub fn transfer_count(&self) -> usize {
        self.count("SELECT COUNT(*) FROM transfers")
    }

    pub fn stream_count(&self) -> usize {
        let tl = self.timeline_filter();
        self.count(&format!("SELECT COUNT(DISTINCT stream_id) FROM launches WHERE stream_id IS NOT NULL AND {tl}"))
    }

    pub fn kernels_with_metrics(&self) -> usize {
        self.count("SELECT COUNT(*) FROM metrics")
    }

    pub fn kernels_with_ops(&self) -> usize {
        self.count("SELECT COUNT(DISTINCT kernel_name) FROM op_kernel_map")
    }

    // -----------------------------------------------------------------------
    // Filter helpers — builds WHERE clause fragments
    // -----------------------------------------------------------------------

    pub fn kernel_filter(&self) -> String {
        let mut clauses = Vec::new();
        if let Some(ref f) = self.focus {
            clauses.push(format!(r"launches.kernel_name LIKE '%{}%' ESCAPE '\'", escape_sql_like(f)));
        }
        if let Some(ref ig) = self.ignore {
            clauses.push(format!(r"launches.kernel_name NOT LIKE '%{}%' ESCAPE '\'", escape_sql_like(ig)));
        }
        if let Some(ref r) = self.region_filter {
            // Only include launches whose start_us falls within a matching region.
            clauses.push(format!(
                r"start_us IS NOT NULL AND EXISTS (
                   SELECT 1 FROM regions
                   WHERE name LIKE '%{}%' ESCAPE '\'
                     AND launches.start_us >= regions.start_us
                     AND launches.start_us <= regions.start_us + regions.duration_us
                 )",
                escape_sql_like(r)
            ));
        }
        if clauses.is_empty() {
            "1=1".to_string()
        } else {
            clauses.join(" AND ")
        }
    }

    // -----------------------------------------------------------------------
    // Attach another DB for diff
    // -----------------------------------------------------------------------

    pub fn attach(&self, path: &str, alias: &str) -> Result<()> {
        self.conn.execute_batch(&format!(
            "ATTACH DATABASE '{}' AS {alias}",
            path.replace('\'', "''")
        ))?;
        Ok(())
    }

    pub fn detach(&self, alias: &str) -> Result<()> {
        self.conn
            .execute_batch(&format!("DETACH DATABASE {alias}"))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT
        );

        CREATE TABLE IF NOT EXISTS layers (
            id              INTEGER PRIMARY KEY,
            source          TEXT NOT NULL,
            file            TEXT,
            collected_at    TEXT,
            command_used    TEXT,
            collection_secs REAL,
            target_hash     TEXT
        );

        CREATE TABLE IF NOT EXISTS launches (
            id             INTEGER PRIMARY KEY,
            kernel_name    TEXT NOT NULL,
            duration_us    REAL NOT NULL,
            grid_x         INTEGER,
            grid_y         INTEGER,
            grid_z         INTEGER,
            block_x        INTEGER,
            block_y        INTEGER,
            block_z        INTEGER,
            stream_id      INTEGER,
            start_us       REAL,
            correlation_id INTEGER,
            layer_id       INTEGER REFERENCES layers(id)
        );

        CREATE TABLE IF NOT EXISTS metrics (
            kernel_name              TEXT PRIMARY KEY,
            occupancy_pct            REAL,
            compute_throughput_pct   REAL,
            memory_throughput_pct    REAL,
            registers_per_thread     INTEGER,
            shared_mem_static_bytes  INTEGER,
            shared_mem_dynamic_bytes INTEGER,
            l2_hit_rate_pct          REAL,
            achieved_bandwidth_gb_s  REAL,
            peak_bandwidth_gb_s      REAL,
            boundedness              TEXT,
            layer_id                 INTEGER REFERENCES layers(id)
        );

        CREATE TABLE IF NOT EXISTS transfers (
            id          INTEGER PRIMARY KEY,
            kind        TEXT NOT NULL,
            bytes       INTEGER,
            duration_us REAL,
            start_us    REAL,
            stream_id   INTEGER,
            layer_id    INTEGER REFERENCES layers(id)
        );

        CREATE TABLE IF NOT EXISTS ops (
            id           INTEGER PRIMARY KEY,
            name         TEXT NOT NULL,
            module_path  TEXT,
            cpu_time_us  REAL,
            gpu_time_us  REAL,
            input_shapes TEXT,
            layer_id     INTEGER REFERENCES layers(id)
        );

        CREATE TABLE IF NOT EXISTS op_kernel_map (
            op_id       INTEGER REFERENCES ops(id),
            kernel_name TEXT,
            PRIMARY KEY (op_id, kernel_name)
        );

        CREATE TABLE IF NOT EXISTS allocations (
            id        INTEGER PRIMARY KEY,
            op        TEXT NOT NULL,        -- 'alloc' or 'free'
            address   INTEGER NOT NULL,
            bytes     INTEGER NOT NULL,     -- 0 for frees when size unknown
            start_us  REAL,
            stream_id INTEGER,
            layer_id  INTEGER REFERENCES layers(id)
        );

        CREATE INDEX IF NOT EXISTS idx_alloc_addr ON allocations(address);
        CREATE INDEX IF NOT EXISTS idx_alloc_time ON allocations(start_us);

        CREATE TABLE IF NOT EXISTS regions (
            id          INTEGER PRIMARY KEY,
            name        TEXT NOT NULL,
            start_us    REAL,
            duration_us REAL,
            layer_id    INTEGER REFERENCES layers(id)
        );

        CREATE TABLE IF NOT EXISTS failures (
            phase TEXT,
            error TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_launches_kernel ON launches(kernel_name);
        CREATE INDEX IF NOT EXISTS idx_launches_start ON launches(start_us);
        CREATE INDEX IF NOT EXISTS idx_launches_stream ON launches(stream_id);
        CREATE INDEX IF NOT EXISTS idx_transfers_start ON transfers(start_us);
        ",
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Project root detection
// ---------------------------------------------------------------------------

/// Walk up from cwd to find a `.git` directory. Returns that parent, or cwd.
fn find_project_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut dir = cwd.as_path();
    loop {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return cwd,
        }
    }
}

// ---------------------------------------------------------------------------
// SQL safety
// ---------------------------------------------------------------------------

/// Escape a value for safe interpolation into a SQL LIKE pattern.
/// Doubles single quotes and escapes LIKE wildcards.
/// Escape a user pattern for use in SQL LIKE.
///
/// - Quotes are doubled for SQL string safety.
/// - `%` is escaped with backslash (the wildcard meaning is reserved internally).
/// - `_` is NOT escaped: kernel names contain many underscores and users
///   typing "vector_add" expect a literal match, not a wildcard.  Allowing
///   `_` as a single-char wildcard is harmless in practice.
///
/// Callers using this helper must append `ESCAPE '\'` to their LIKE clause
/// so the backslash-escaped `%` is recognized.
pub fn escape_sql_like(s: &str) -> String {
    s.replace('\'', "''").replace('%', "\\%")
}

// ---------------------------------------------------------------------------
// Saved session info
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct SavedSession {
    pub name: String,
    pub target: String,
    pub device: String,
    pub kernel_count: usize,
    pub launch_count: usize,
    pub layers: Vec<String>,
    pub created: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> GpuDb {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.into_path().join("test.gpu.db");
        GpuDb::create(&path).unwrap()
    }

    #[test]
    fn meta_roundtrip() {
        let db = temp_db();
        db.set_meta("target", "train.py").unwrap();
        db.set_meta("device", "A100").unwrap();
        assert_eq!(db.meta("target"), "train.py");
        assert_eq!(db.meta("device"), "A100");
        assert_eq!(db.meta("missing"), "");
    }

    #[test]
    fn add_layer() {
        let db = temp_db();
        let id = db.add_layer("nsys", "/tmp/trace.nsys-rep", Some("nsys profile"), Some(12.5), None).unwrap();
        assert_eq!(id, 1);
        assert!(db.has_layer("nsys"));
        assert!(!db.has_layer("ncu"));
        assert_eq!(db.layer_names(), vec!["nsys"]);
    }

    #[test]
    fn kernel_counts() {
        let db = temp_db();
        let lid = db.add_layer("nsys", "test", None, None, None).unwrap();
        db.conn.execute(
            "INSERT INTO launches (kernel_name, duration_us, layer_id) VALUES ('k1', 100.0, ?1)",
            params![lid],
        ).unwrap();
        db.conn.execute(
            "INSERT INTO launches (kernel_name, duration_us, layer_id) VALUES ('k1', 200.0, ?1)",
            params![lid],
        ).unwrap();
        db.conn.execute(
            "INSERT INTO launches (kernel_name, duration_us, layer_id) VALUES ('k2', 50.0, ?1)",
            params![lid],
        ).unwrap();

        assert_eq!(db.unique_kernel_count(), 2);
        assert_eq!(db.total_launch_count(), 3);
        assert!((db.total_gpu_time_us() - 350.0).abs() < 0.1);
    }

    #[test]
    fn failures() {
        let db = temp_db();
        db.add_failure("ncu", "ncu not found").unwrap();
        let f = db.failures();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].0, "ncu");
        assert_eq!(f[0].1, "ncu not found");
    }

    #[test]
    fn kernel_filter() {
        let mut db = temp_db();
        assert_eq!(db.kernel_filter(), "1=1");
        db.focus = Some("sgemm".into());
        assert!(db.kernel_filter().contains("launches.kernel_name LIKE '%sgemm%'"));
        db.ignore = Some("copy".into());
        assert!(db.kernel_filter().contains("NOT LIKE '%copy%'"));
        // Verify table-qualified to avoid ambiguity in JOINs
        assert!(db.kernel_filter().contains("launches.kernel_name"));
    }

    #[test]
    fn save_and_load() {
        let db = temp_db();
        db.set_meta("target", "test.py").unwrap();
        let lid = db.add_layer("nsys", "test", None, None, None).unwrap();
        db.conn.execute(
            "INSERT INTO launches (kernel_name, duration_us, layer_id) VALUES ('k1', 100.0, ?1)",
            params![lid],
        ).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("saved.gpu.db");
        // Save via file copy (simpler test than going through .dbg/gpu/)
        {
            let mut dest_conn = Connection::open(&dest).unwrap();
            let backup = rusqlite::backup::Backup::new(&db.conn, &mut dest_conn).unwrap();
            backup.run_to_completion(100, std::time::Duration::from_millis(10), None).unwrap();
        }

        let loaded = GpuDb::open(&dest).unwrap();
        assert_eq!(loaded.meta("target"), "test.py");
        assert_eq!(loaded.unique_kernel_count(), 1);
    }
}
