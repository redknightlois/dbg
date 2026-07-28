use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, ToSql, params};

/// gdbg's schema version.
///
/// Kept separate from `dbg_cli::session_db::SCHEMA_VERSION` for now —
/// gdbg's tables are GPU-specific and don't include the `session_id`
/// column that the unified SessionDb format uses. When gdbg is fully
/// migrated to SessionDb (see plan task 10 "full rewrite deferred"),
/// the two versions will unify.
///
/// Bumping this invalidates every saved `.gpu.db` file: `GpuDb::open`
/// refuses to load anything that doesn't match, pointing the user at
/// the raw `.nsys-rep` + `.csv` artifacts to re-ingest.
pub const GDBG_SCHEMA_VERSION: i64 = 2;

/// A GPU profiling session backed by a SQLite database.
pub struct GpuDb {
    pub conn: Connection,
    pub _path: PathBuf,
    /// Keep the descriptor used to open the database alive. SQLite opens
    /// `/proc/self/fd/N`, so a pathname or ancestor swap cannot redirect it.
    database_file: std::fs::File,
    attached_files: Mutex<HashMap<String, std::fs::File>>,
    /// Active focus filter (kernel name substring).
    pub focus: Option<String>,
    /// Active ignore filter (kernel name substring).
    pub ignore: Option<String>,
    /// Active region filter (region name substring).
    pub region_filter: Option<String>,
}

impl std::fmt::Debug for GpuDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuDb")
            .field("path", &self._path)
            .field("focus", &self.focus)
            .field("ignore", &self.ignore)
            .field("region_filter", &self.region_filter)
            .finish()
    }
}

#[derive(Default)]
pub struct SqlFilter {
    clause: String,
    params: Vec<String>,
}

impl SqlFilter {
    pub fn clause(&self) -> &str {
        if self.clause.is_empty() {
            "1=1"
        } else {
            &self.clause
        }
    }

    pub fn params(&self) -> Vec<&dyn ToSql> {
        self.params.iter().map(|p| p as &dyn ToSql).collect()
    }
}

/// Build a SQL LIKE bind-parameter from a user pattern: `%escaped_pattern%`.
pub fn like_param(pattern: &str) -> String {
    format!("%{}%", escape_sql_like(pattern))
}

impl GpuDb {
    /// Create a new session database at the given path.
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let opened = open_sqlite_path(path, true)
            .with_context(|| format!("cannot create {}", path.display()))?;
        let conn = opened.conn;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        init_schema(&conn)?;
        conn.execute(&format!("PRAGMA user_version = {GDBG_SCHEMA_VERSION}"), [])?;
        Ok(Self {
            conn,
            _path: path.to_path_buf(),
            database_file: opened.file,
            attached_files: Mutex::new(HashMap::new()),
            focus: None,
            ignore: None,
            region_filter: None,
        })
    }

    /// Open an existing session database.
    ///
    /// Refuses to open any DB whose `user_version` doesn't match
    /// `GDBG_SCHEMA_VERSION`. There is no migration path — the raw
    /// `.nsys-rep` + `.csv` files under the session's collection
    /// directory are the durable artifact; re-run `gdbg <target>`
    /// to rebuild the index.
    pub fn open(path: &Path) -> Result<Self> {
        // open_sqlite_path walks and opens the parent from a descriptor, then
        // opens the final component with O_NOFOLLOW. Do not perform a public
        // pathname check first: that would create a check/use race.
        let opened = open_sqlite_path(path, false)
            .with_context(|| format!("cannot open {}", path.display()))?;
        Self::from_opened(path.to_path_buf(), opened)
    }

    fn from_opened(path: PathBuf, opened: OpenedSqlite) -> Result<Self> {
        let conn = opened.conn;
        let found: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if found != GDBG_SCHEMA_VERSION {
            bail!(
                "gdbg session DB at {path} has schema_version={found}, \
                 expected {expected}. No migration path — delete it and \
                 re-run `gdbg <target>` to rebuild from the raw captures.",
                path = path.display(),
                expected = GDBG_SCHEMA_VERSION,
            );
        }
        validate_schema_tables(&conn, &path)?;
        Ok(Self {
            conn,
            _path: path,
            database_file: opened.file,
            attached_files: Mutex::new(HashMap::new()),
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
        validate_saved_session_name(name)?;
        let public_dir = Self::session_dir();
        reject_path_symlinks(&public_dir, "GPU session directory")?;
        let dbg_dir = public_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("GPU session directory has no parent"))?;
        reject_symlink(dbg_dir, "GPU state directory")?;
        std::fs::create_dir_all(&public_dir)?;
        let (dir, dir_guard) = open_gpu_session_dir(&public_dir)?;
        let dest = dir.join(format!("{name}.gpu.db"));
        let public_dest = public_dir.join(format!("{name}.gpu.db"));
        reject_symlink(&dest, "GPU session destination")?;
        // Use a new sibling and atomic rename. The destination is never
        // reopened through a pathname which was checked earlier.
        let tmp = dir.join(format!(
            ".{}.tmp-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        #[cfg(unix)]
        let guard = open_file_at_dir(
            &dir_guard,
            Path::new(tmp.file_name().expect("temporary database has a file name")),
            true,
            true,
            true,
        )?;
        #[cfg(not(unix))]
        let guard = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        let mut dest_conn = open_sqlite_from_file(&guard, true)?;
        let backup = rusqlite::backup::Backup::new(&self.conn, &mut dest_conn)?;
        backup.run_to_completion(100, std::time::Duration::from_millis(10), None)?;
        drop(backup);
        drop(dest_conn);
        drop(guard);
        std::fs::rename(&tmp, &dest)?;
        Ok(public_dest)
    }

    /// Load a saved session by name or path.
    pub fn load(name_or_path: &str) -> Result<Self> {
        if !name_or_path.contains('/') {
            let name = name_or_path.strip_suffix(".gpu.db").unwrap_or(name_or_path);
            return Self::load_saved_from_dir(&Self::session_dir(), name);
        }
        Self::open(Path::new(name_or_path))
    }

    fn load_saved_from_dir(public_dir: &Path, name: &str) -> Result<Self> {
        validate_saved_session_name(name)?;
        let (scan_dir, dir_guard) = open_gpu_session_dir(public_dir)?;
        Self::load_saved_from_open_dir(public_dir, &scan_dir, &dir_guard, name)
    }

    #[cfg(unix)]
    fn load_saved_from_open_dir(
        public_dir: &Path,
        _scan_dir: &Path,
        dir_guard: &std::fs::File,
        name: &str,
    ) -> Result<Self> {
        validate_saved_session_name(name)?;
        let file_name = format!("{name}.gpu.db");
        let file = open_file_at_dir(dir_guard, Path::new(&file_name), false, false, false)?;
        let conn = open_sqlite_from_file(&file, false)?;
        let public_path = public_dir.join(file_name);
        Self::from_opened(public_path, OpenedSqlite { conn, file })
    }

    #[cfg(not(unix))]
    fn load_saved_from_open_dir(
        public_dir: &Path,
        scan_dir: &Path,
        _dir_guard: &std::fs::File,
        name: &str,
    ) -> Result<Self> {
        validate_saved_session_name(name)?;
        let mut db = Self::open(&scan_dir.join(format!("{name}.gpu.db")))?;
        db._path = public_dir.join(format!("{name}.gpu.db"));
        Ok(db)
    }

    /// Run one import and its layer creation as one SQLite transaction.
    /// Parsers can insert many child rows before discovering malformed input;
    /// rollback must remove those rows and the layer together.
    pub fn import_layer(
        &self,
        source: &str,
        file: &str,
        command: Option<&str>,
        secs: Option<f64>,
        target_hash: Option<&str>,
        import: impl FnOnce(&Connection, i64) -> Result<()>,
    ) -> Result<i64> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let layer_id = self.add_layer(source, file, command, secs, target_hash)?;
            import(&self.conn, layer_id)?;
            Ok(layer_id)
        })();
        match result {
            Ok(id) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(id)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// List all saved sessions.
    pub fn list_saved() -> Result<Vec<SavedSession>> {
        let dir = Self::session_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let (scan_dir, dir_guard) = open_gpu_session_dir(&dir)?;
        Self::list_saved_from_open_dir(&scan_dir, &dir_guard)
    }

    fn list_saved_from_open_dir(
        scan_dir: &Path,
        dir_guard: &std::fs::File,
    ) -> Result<Vec<SavedSession>> {
        let mut sessions = Vec::new();
        for entry in std::fs::read_dir(&scan_dir)? {
            let entry = entry?;
            let path = entry.path();
            if std::fs::symlink_metadata(&path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                continue;
            }
            if path.extension().is_some_and(|e| e == "db")
                && path.to_str().is_some_and(|s| s.contains(".gpu."))
            {
                #[cfg(unix)]
                let opened = {
                    let file_name = entry.file_name();
                    open_file_at_dir(&dir_guard, Path::new(&file_name), false, false, false)
                        .and_then(|file| {
                            let conn = open_sqlite_from_file(&file, false)?;
                            Self::from_opened(path.clone(), OpenedSqlite { conn, file })
                        })
                };
                #[cfg(not(unix))]
                let opened = Self::open(&path);
                if let Ok(db) = opened {
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
                        device: db.meta("device"),
                        kernel_count: db.unique_kernel_count(),
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
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
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

    /// Undo an `add_layer` when a downstream import fails. Without
    /// this, a failed nsys/ncu import leaves an empty layer row that
    /// makes `has_layer("nsys")` return true and the session summary
    /// falsely claim both layers are present.
    pub fn remove_layer(&self, layer_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM layers WHERE id = ?1", params![layer_id])?;
        Ok(())
    }

    /// Execute a query and collect all rows via a mapping function.
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

    pub fn query_vec_result<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        f: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, f)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
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
                orphans
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
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
        self.query_vec("SELECT phase, error FROM failures", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
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
        let Some(tl_id) = self.timeline_layer_id() else {
            return;
        };

        // Check whether the timeline layer is already the op layer —
        // if so, nothing to fix.
        let op_layers: Vec<String> = self.query_vec(
            "SELECT DISTINCT source FROM layers WHERE id IN (SELECT DISTINCT layer_id FROM ops)",
            [],
            |row| row.get(0),
        );
        let tl_source: String = self
            .conn
            .query_row(
                "SELECT source FROM layers WHERE id = ?1",
                params![tl_id],
                |row| row.get(0),
            )
            .unwrap_or_default();

        // If the only op layer is also the timeline layer, no recomputation needed.
        if op_layers.len() == 1 && op_layers[0] == tl_source {
            return;
        }

        // Re-correlate: for each op, sum kernel durations from the timeline layer.
        if let Err(e) = self.conn.execute(
            "UPDATE ops SET gpu_time_us = (
                SELECT COALESCE(SUM(l.duration_us), 0)
                FROM op_kernel_map okm
                JOIN launches l ON l.layer_id = ?1
                    AND (l.id = okm.launch_id
                         OR (okm.launch_id IS NULL
                             AND l.kernel_name = okm.kernel_name)
                         OR (NOT EXISTS (
                                SELECT 1 FROM launches mapped
                                WHERE mapped.id = okm.launch_id
                                  AND mapped.layer_id = ?1
                            )
                             AND l.kernel_name = okm.kernel_name))
                WHERE okm.op_id = ops.id
            )",
            params![tl_id],
        ) {
            eprintln!("recompute_op_gpu_times failed: {e}");
        }
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
        self.count(&format!(
            "SELECT COUNT(DISTINCT kernel_name) FROM launches WHERE {tl}"
        ))
    }

    pub fn total_launch_count(&self) -> usize {
        let tl = self.timeline_filter();
        self.count(&format!("SELECT COUNT(*) FROM launches WHERE {tl}"))
    }

    pub fn total_gpu_time_us(&self) -> f64 {
        let tl = self.timeline_filter();
        self.scalar_f64(&format!(
            "SELECT COALESCE(SUM(duration_us), 0) FROM launches WHERE {tl}"
        ))
    }

    /// Kernel `(start_us, end_us)` intervals from `launches`, timeline-filtered,
    /// ordered by `start_us`. Skips rows where `start_us IS NULL`.
    pub fn kernel_intervals(&self) -> Vec<(f64, f64)> {
        let tl = self.timeline_filter();
        let sql = format!(
            "SELECT start_us, start_us + duration_us FROM launches
             WHERE start_us IS NOT NULL AND {tl} ORDER BY start_us"
        );
        self.query_vec(&sql, [], |row| Ok((row.get(0)?, row.get(1)?)))
    }

    /// Transfer `(start_us, end_us)` intervals from `transfers`, ordered by `start_us`.
    /// Skips rows where `start_us IS NULL`. If `kind` is `Some`, restricts to that
    /// transfer kind (e.g. "H2D", "D2H", "D2D").
    pub fn transfer_intervals(&self, kind: Option<&str>) -> Vec<(f64, f64)> {
        match kind {
            Some(k) => self.query_vec(
                "SELECT start_us, start_us + duration_us FROM transfers
                 WHERE start_us IS NOT NULL AND kind = ?1 ORDER BY start_us",
                [k],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ),
            None => self.query_vec(
                "SELECT start_us, start_us + duration_us FROM transfers
                 WHERE start_us IS NOT NULL ORDER BY start_us",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ),
        }
    }

    pub fn transfer_count(&self) -> usize {
        self.count("SELECT COUNT(*) FROM transfers")
    }

    pub fn stream_count(&self) -> usize {
        let tl = self.timeline_filter();
        self.count(&format!(
            "SELECT COUNT(DISTINCT stream_id) FROM launches WHERE stream_id IS NOT NULL AND {tl}"
        ))
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

    pub fn kernel_filter_params(&self) -> SqlFilter {
        let mut clauses = Vec::new();
        let mut params = Vec::new();
        if let Some(ref f) = self.focus {
            clauses.push(r"launches.kernel_name LIKE ? ESCAPE '\'".to_string());
            params.push(like_param(f));
        }
        if let Some(ref ig) = self.ignore {
            clauses.push(r"launches.kernel_name NOT LIKE ? ESCAPE '\'".to_string());
            params.push(like_param(ig));
        }
        if let Some(ref r) = self.region_filter {
            // Only include launches whose start_us falls within a matching region.
            clauses.push(
                r"launches.start_us IS NOT NULL AND EXISTS (
                   SELECT 1 FROM regions
                   WHERE name LIKE ? ESCAPE '\'
                     AND launches.start_us >= regions.start_us
                     AND launches.start_us <= regions.start_us + regions.duration_us
                 )"
                .to_string(),
            );
            params.push(like_param(r));
        }
        SqlFilter {
            clause: clauses.join(" AND "),
            params,
        }
    }

    // -----------------------------------------------------------------------
    // Attach another DB for diff
    // -----------------------------------------------------------------------

    pub fn attach(&self, path: &str, alias: &str) -> Result<()> {
        validate_sql_identifier(alias)?;
        let path = Path::new(path);
        let file = open_existing_file(path)?;
        self.attach_file(file, alias)
    }

    /// Attach an already-open database. The duplicated descriptor remains
    /// owned by this connection until SQLite has completed `DETACH`.
    pub fn attach_db(&self, other: &Self, alias: &str) -> Result<()> {
        validate_sql_identifier(alias)?;
        self.attach_file(other.database_file.try_clone()?, alias)
    }

    fn attach_file(&self, file: std::fs::File, alias: &str) -> Result<()> {
        validate_sql_identifier(alias)?;
        #[cfg(unix)]
        {
            let attach_path = sqlite_fd_path(&file);
            self.conn.execute_batch(&format!(
                "ATTACH DATABASE '{}' AS {alias}",
                attach_path.to_string_lossy().replace('\'', "''")
            ))?;
            self.attached_files
                .lock()
                .unwrap()
                .insert(alias.to_string(), file);
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (file, alias);
            bail!("descriptor-backed SQLite attachments are unsupported on this platform")
        }
    }

    pub fn detach(&self, alias: &str) -> Result<()> {
        validate_sql_identifier(alias)?;
        self.conn
            .execute_batch(&format!("DETACH DATABASE {alias}"))?;
        // SQLite is done with the file only after DETACH succeeds.
        self.attached_files.lock().unwrap().remove(alias);
        Ok(())
    }
}

fn validate_schema_tables(conn: &Connection, path: &Path) -> Result<()> {
    const REQUIRED: &[&str] = &[
        "meta",
        "layers",
        "launches",
        "metrics",
        "transfers",
        "ops",
        "op_kernel_map",
        "allocations",
        "regions",
        "failures",
    ];
    let mut missing = Vec::new();
    for table in REQUIRED {
        let present: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )?;
        if present != 1 {
            missing.push(*table);
        }
    }
    if !missing.is_empty() {
        bail!(
            "gdbg session DB {} is missing required table(s): {}",
            path.display(),
            missing.join(", ")
        );
    }
    Ok(())
}

struct OpenedSqlite {
    conn: Connection,
    file: std::fs::File,
}

fn open_sqlite_path(path: &Path, writable: bool) -> Result<OpenedSqlite> {
    #[cfg(unix)]
    {
        let file = open_file(path, writable, writable)?;
        let conn = open_sqlite_from_file(&file, writable)?;
        return Ok(OpenedSqlite { conn, file });
    }
    #[cfg(not(unix))]
    {
        let _ = (path, writable);
        bail!("descriptor-backed SQLite access is unsupported on this platform")
    }
}

fn open_sqlite_from_file(file: &std::fs::File, writable: bool) -> Result<Connection> {
    #[cfg(unix)]
    {
        let fd_path = sqlite_fd_path(file);
        let flags = if writable {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        };
        return Connection::open_with_flags(fd_path, flags).map_err(Into::into);
    }
    #[cfg(not(unix))]
    {
        let _ = (file, writable);
        bail!("descriptor-backed SQLite access is unsupported on this platform")
    }
}

fn open_existing_file(path: &Path) -> Result<std::fs::File> {
    open_file(path, false, false)
}

fn open_file(path: &Path, writable: bool, create: bool) -> Result<std::fs::File> {
    #[cfg(unix)]
    {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("database path has no file name: {}", path.display()))?;
        let dir = open_directory_anchored(parent)?;
        return open_file_at_dir(&dir, Path::new(name), writable, create, false);
    }
    #[cfg(not(unix))]
    {
        let _ = (path, writable, create);
        bail!("descriptor-relative file access is unsupported on this platform")
    }
}

#[cfg(unix)]
fn open_file_at_dir(
    dir: &std::fs::File,
    name: &Path,
    writable: bool,
    create: bool,
    exclusive: bool,
) -> Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let name = CString::new(name.as_os_str().as_bytes())?;
    let mut flags = if writable {
        nix::libc::O_RDWR
    } else {
        nix::libc::O_RDONLY
    };
    flags |= nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC;
    if create {
        flags |= nix::libc::O_CREAT;
    }
    if exclusive {
        flags |= nix::libc::O_EXCL;
    }
    let fd = unsafe { nix::libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

fn open_gpu_session_dir(path: &Path) -> Result<(PathBuf, std::fs::File)> {
    #[cfg(unix)]
    {
        let guard = open_directory_anchored(path)?;
        return Ok((sqlite_fd_path(&guard), guard));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        bail!("descriptor-relative directory access is unsupported on this platform")
    }
}

#[cfg(unix)]
fn sqlite_fd_path(file: &std::fs::File) -> PathBuf {
    use std::os::fd::AsRawFd;

    #[cfg(target_os = "linux")]
    const FD_DIR: &str = "/proc/self/fd";
    #[cfg(not(target_os = "linux"))]
    const FD_DIR: &str = "/dev/fd";

    PathBuf::from(format!("{FD_DIR}/{}", file.as_raw_fd()))
}

/// Open a directory by walking each component from a stable descriptor.
/// Checking the complete pathname first is not sufficient: an attacker can
/// replace an ancestor with a symlink between that check and `open`. Every
/// component is therefore opened with `openat`, `O_DIRECTORY`, and
/// `O_NOFOLLOW` before the next component is resolved.
#[cfg(unix)]
fn open_directory_anchored(path: &Path) -> Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let root = CString::new("/").expect("literal has no NUL");
    #[cfg(target_os = "linux")]
    let root_flags = nix::libc::O_PATH;
    #[cfg(not(target_os = "linux"))]
    let root_flags = nix::libc::O_RDONLY;
    let root_fd = unsafe {
        nix::libc::open(
            root.as_ptr(),
            root_flags | nix::libc::O_DIRECTORY | nix::libc::O_CLOEXEC,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut current = unsafe { OwnedFd::from_raw_fd(root_fd) };
    for component in absolute.components() {
        let std::path::Component::Normal(name) = component else {
            if matches!(component, std::path::Component::ParentDir) {
                bail!(
                    "refusing parent component in GPU session directory {}",
                    path.display()
                );
            }
            continue;
        };
        let name = CString::new(name.as_bytes())?;
        #[cfg(target_os = "linux")]
        let component_flags = nix::libc::O_PATH;
        #[cfg(not(target_os = "linux"))]
        let component_flags = nix::libc::O_RDONLY;
        let fd = unsafe {
            nix::libc::openat(
                std::os::fd::AsRawFd::as_raw_fd(&current),
                name.as_ptr(),
                component_flags
                    | nix::libc::O_DIRECTORY
                    | nix::libc::O_NOFOLLOW
                    | nix::libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        current = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    Ok(std::fs::File::from(current))
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
            launch_id   INTEGER REFERENCES launches(id),
            PRIMARY KEY (op_id, kernel_name, launch_id)
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

fn validate_saved_session_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        bail!("invalid GPU saved-session name `{name}`; use only letters, digits, `-`, and `_`");
    }
    Ok(())
}

/// Validate a name interpolated into SQLite's identifier position.
///
/// SQLite does not provide a bind-parameter form for database aliases, so
/// callers must use a deliberately narrow identifier grammar.
fn validate_sql_identifier(identifier: &str) -> Result<()> {
    let mut chars = identifier.chars();
    let first = chars.next();
    if first.is_none_or(|ch| !(ch == '_' || ch.is_ascii_alphabetic()))
        || chars.any(|ch| !(ch == '_' || ch.is_ascii_alphanumeric()))
    {
        bail!("invalid SQLite identifier `{identifier}`");
    }
    Ok(())
}

fn reject_symlink(path: &Path, description: &str) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("refusing {description} symlink: {}", path.display());
    }
    Ok(())
}

fn reject_path_symlinks(path: &Path, description: &str) -> Result<()> {
    for ancestor in path.ancestors() {
        if std::fs::symlink_metadata(ancestor)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            bail!(
                "refusing {description} path through symlink: {}",
                ancestor.display()
            );
        }
    }
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
    pub device: String,
    pub kernel_count: usize,
    pub layers: Vec<String>,
    pub created: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> GpuDb {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep().join("test.gpu.db");
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
        let id = db
            .add_layer(
                "nsys",
                "/tmp/trace.nsys-rep",
                Some("nsys profile"),
                Some(12.5),
                None,
            )
            .unwrap();
        assert_eq!(id, 1);
        assert!(db.has_layer("nsys"));
        assert!(!db.has_layer("ncu"));
        assert_eq!(db.layer_names(), vec!["nsys"]);
    }

    #[test]
    fn import_layer_rolls_back_children_and_layer_together() {
        let db = temp_db();
        let result = db.import_layer("ncu", "metrics.csv", None, None, None, |conn, layer_id| {
            conn.execute(
                "INSERT INTO launches (kernel_name, duration_us, layer_id) VALUES ('bad', 1.0, ?1)",
                params![layer_id],
            )?;
            bail!("malformed import")
        });
        assert!(result.is_err());
        assert!(!db.has_layer("ncu"));
        assert_eq!(db.total_launch_count(), 0);
    }

    #[test]
    fn save_rejects_names_that_can_escape_gpu_directory() {
        let db = temp_db();
        for name in ["../outside", "a/b", "a\\b", "", ".", "..", "a space"] {
            assert!(db.save(name).is_err(), "accepted unsafe name {name:?}");
        }
    }

    #[test]
    fn save_rejects_symlinked_gpu_directory_or_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let link = tmp.path().join("gpu");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(reject_symlink(&link, "GPU session directory").is_err());

        let destination = tmp.path().join("session.gpu.db");
        std::fs::write(&outside.join("real.db"), b"database").unwrap();
        std::os::unix::fs::symlink(outside.join("real.db"), &destination).unwrap();
        assert!(reject_symlink(&destination, "GPU session destination").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn anchored_gpu_directory_rejects_a_symlinked_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(outside.join("gpu")).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(open_gpu_session_dir(&link.join("gpu")).is_err());
    }

    /// Regression: when the nsys import failed (nsys 2023 schema
    /// mismatch, for example), the layer row inserted before the
    /// import persisted and `has_layer("nsys")` returned true for an
    /// empty layer — causing the session summary to claim `Layers:
    /// nsys + ncu` with zero data. `remove_layer` lets the collector
    /// roll back the row on error.
    #[test]
    fn remove_layer_restores_has_layer_to_false() {
        let db = temp_db();
        let id = db
            .add_layer("nsys", "/tmp/trace.nsys-rep", None, None, None)
            .unwrap();
        assert!(db.has_layer("nsys"));
        db.remove_layer(id).unwrap();
        assert!(!db.has_layer("nsys"), "layer row must be gone after remove");
        assert!(db.layer_names().is_empty());
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
        db.conn
            .execute(
                "INSERT INTO launches (kernel_name, duration_us, layer_id) VALUES ('k2', 50.0, ?1)",
                params![lid],
            )
            .unwrap();

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
    fn kernel_filter_params() {
        let mut db = temp_db();
        assert_eq!(db.kernel_filter_params().clause(), "1=1");
        db.focus = Some("sgemm".into());
        assert!(
            db.kernel_filter_params()
                .clause()
                .contains("launches.kernel_name LIKE ?")
        );
        db.ignore = Some("copy".into());
        assert!(db.kernel_filter_params().clause().contains("NOT LIKE ?"));
        let filter = db.kernel_filter_params();
        assert_eq!(filter.params.len(), 2);
        // Verify table-qualified to avoid ambiguity in JOINs
        assert!(filter.clause().contains("launches.kernel_name"));
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
            backup
                .run_to_completion(100, std::time::Duration::from_millis(10), None)
                .unwrap();
        }

        let loaded = GpuDb::open(&dest).unwrap();
        assert_eq!(loaded.meta("target"), "test.py");
        assert_eq!(loaded.unique_kernel_count(), 1);
    }

    #[test]
    fn create_stamps_schema_version() {
        let db = temp_db();
        let v: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, GDBG_SCHEMA_VERSION);
    }

    #[test]
    fn open_refuses_unstamped_old_format() {
        // Simulate a pre-versioning `.gpu.db`: the tables + schema are
        // there but PRAGMA user_version is 0 (SQLite's default).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.gpu.db");
        {
            let conn = Connection::open(&path).unwrap();
            init_schema(&conn).unwrap();
            // deliberately NOT stamping user_version
        }
        let err = GpuDb::open(&path).unwrap_err().to_string();
        assert!(err.contains("schema_version=0"), "{err}");
        assert!(err.contains("No migration path"), "{err}");
        assert!(err.contains("re-run `gdbg"), "{err}");
    }

    #[test]
    fn open_refuses_future_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.gpu.db");
        {
            let conn = Connection::open(&path).unwrap();
            init_schema(&conn).unwrap();
            conn.execute("PRAGMA user_version = 99", []).unwrap();
        }
        let err = GpuDb::open(&path).unwrap_err().to_string();
        assert!(err.contains("schema_version=99"));
    }

    #[test]
    fn gpudb_open_rejects_missing_required_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("incomplete.gpu.db");
        {
            let conn = Connection::open(&path).unwrap();
            init_schema(&conn).unwrap();
            conn.execute("DROP TABLE metrics", []).unwrap();
            conn.execute(&format!("PRAGMA user_version = {GDBG_SCHEMA_VERSION}"), [])
                .unwrap();
        }
        let error = GpuDb::open(&path).unwrap_err().to_string();
        assert!(
            error.contains("missing required table(s): metrics"),
            "{error}"
        );
    }

    #[test]
    fn gdbg_comparison_database_validation_and_query_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("comparison.gpu.db");
        {
            let conn = Connection::open(&path).unwrap();
            init_schema(&conn).unwrap();
            conn.execute("DROP TABLE launches", []).unwrap();
            conn.execute(&format!("PRAGMA user_version = {GDBG_SCHEMA_VERSION}"), [])
                .unwrap();
        }
        let validation = GpuDb::open(&path).unwrap_err();
        assert!(validation.to_string().contains("missing required table"));

        let current = GpuDb::create(&dir.path().join("current.gpu.db")).unwrap();
        assert!(
            current
                .query_vec_result("SELECT * FROM definitely_missing", [], |row| {
                    row.get::<_, String>(0)
                })
                .is_err()
        );
    }

    #[test]
    fn save_preserves_version_through_backup() {
        let db = temp_db();
        db.set_meta("marker", "present").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("backed_up.gpu.db");
        {
            let mut dest_conn = Connection::open(&dest).unwrap();
            let backup = rusqlite::backup::Backup::new(&db.conn, &mut dest_conn).unwrap();
            backup
                .run_to_completion(100, std::time::Duration::from_millis(10), None)
                .unwrap();
        }
        // The backup path must pass the version gate cleanly.
        let loaded = GpuDb::open(&dest).unwrap();
        assert_eq!(loaded.meta("marker"), "present");
    }

    #[cfg(unix)]
    #[test]
    fn named_load_keeps_database_descriptor_across_ancestor_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let selected_dir = tmp.path().join("selected");
        std::fs::create_dir(&selected_dir).unwrap();
        let selected_path = selected_dir.join("comparison.gpu.db");
        let selected = GpuDb::create(&selected_path).unwrap();
        selected.set_meta("marker", "original").unwrap();
        drop(selected);

        // This is the named-session open step. Keep the directory descriptor
        // alive while opening the file, then replace the public ancestor.
        let (scan_dir, dir_guard) = open_gpu_session_dir(&selected_dir).unwrap();
        let comparison =
            GpuDb::load_saved_from_open_dir(&selected_dir, &scan_dir, &dir_guard, "comparison")
                .unwrap();

        let real_dir = tmp.path().join("selected-real");
        std::fs::rename(&selected_dir, &real_dir).unwrap();
        std::fs::create_dir(&selected_dir).unwrap();
        let replacement_path = selected_dir.join("comparison.gpu.db");
        let replacement = GpuDb::create(&replacement_path).unwrap();
        replacement.set_meta("marker", "replacement").unwrap();
        drop(replacement);

        let current = GpuDb::create(&tmp.path().join("current.gpu.db")).unwrap();
        current.attach_db(&comparison, "other").unwrap();
        let marker: String = current
            .conn
            .query_row(
                "SELECT value FROM other.meta WHERE key = 'marker'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker, "original");
        current.detach("other").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn named_list_reads_from_the_open_directory_after_ancestor_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let selected_dir = tmp.path().join("selected");
        std::fs::create_dir(&selected_dir).unwrap();
        let selected_path = selected_dir.join("comparison.gpu.db");
        let selected = GpuDb::create(&selected_path).unwrap();
        selected.set_meta("device", "original-device").unwrap();
        selected
            .set_meta("created", "2026-01-01T00:00:00Z")
            .unwrap();
        drop(selected);

        // list_saved opens the directory before it scans entries. The scan
        // must continue to use that descriptor if the public ancestor moves.
        let (scan_dir, dir_guard) = open_gpu_session_dir(&selected_dir).unwrap();
        let real_dir = tmp.path().join("selected-real");
        std::fs::rename(&selected_dir, &real_dir).unwrap();
        std::fs::create_dir(&selected_dir).unwrap();
        let replacement = GpuDb::create(&selected_dir.join("comparison.gpu.db")).unwrap();
        replacement
            .set_meta("device", "replacement-device")
            .unwrap();
        replacement
            .set_meta("created", "2026-01-02T00:00:00Z")
            .unwrap();
        drop(replacement);

        let sessions = GpuDb::list_saved_from_open_dir(&scan_dir, &dir_guard).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].device, "original-device");
    }

    #[cfg(unix)]
    #[test]
    fn named_diff_keeps_explicit_database_descriptor_across_ancestor_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let selected_dir = tmp.path().join("selected");
        std::fs::create_dir(&selected_dir).unwrap();
        let selected_path = selected_dir.join("comparison.gpu.db");
        let selected = GpuDb::create(&selected_path).unwrap();
        selected.set_meta("marker", "original").unwrap();
        drop(selected);

        // gdbg diff validates and opens an explicit pathname before it
        // attaches the comparison. The opened database, not the pathname,
        // must remain the source of truth.
        let comparison = GpuDb::open(&selected_path).unwrap();
        let real_dir = tmp.path().join("selected-real");
        std::fs::rename(&selected_dir, &real_dir).unwrap();
        std::fs::create_dir(&selected_dir).unwrap();
        let replacement = GpuDb::create(&selected_dir.join("comparison.gpu.db")).unwrap();
        replacement.set_meta("marker", "replacement").unwrap();
        drop(replacement);

        let current = GpuDb::create(&tmp.path().join("current.gpu.db")).unwrap();
        current.attach_db(&comparison, "other").unwrap();
        let marker: String = current
            .conn
            .query_row(
                "SELECT value FROM other.meta WHERE key = 'marker'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker, "original");
        current.detach("other").unwrap();
    }

    #[test]
    fn attach_and_detach_reject_sql_identifier_injection() {
        let db = temp_db();
        assert!(db.detach("other; DROP TABLE meta;--").is_err());
        assert!(db.attach_db(&db, "other; DROP TABLE meta;--").is_err());
        assert!(
            db.conn
                .query_row("SELECT COUNT(*) FROM meta", [], |row| {
                    row.get::<_, i64>(0)
                })
                .is_ok()
        );
    }
}
