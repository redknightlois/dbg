//! SessionDb lifecycle: create, open (with strict schema-version check),
//! save-to, prune, session groups, raw-file paths.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use super::schema::{self, SCHEMA_VERSION};
use super::{SessionKind, TargetClass};

/// An open SessionDb — either in-memory (fresh) or backed by a file.
pub struct SessionDb {
    conn: Connection,
    session_id: String,
    label: String,
    kind: SessionKind,
    target_class: TargetClass,
    target: String,
    db_path: Option<PathBuf>,
}

impl std::fmt::Debug for SessionDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionDb")
            .field("session_id", &self.session_id)
            .field("label", &self.label)
            .field("kind", &self.kind)
            .field("target_class", &self.target_class)
            .field("target", &self.target)
            .field("db_path", &self.db_path)
            .finish()
    }
}

/// Parameters for `SessionDb::create`.
pub struct CreateOptions<'a> {
    pub kind: SessionKind,
    pub target: &'a str,
    pub target_class: TargetClass,
    pub cwd: &'a Path,
    /// `None` → in-memory DB. Call `save_to` later to persist.
    pub db_path: Option<&'a Path>,
    /// `None` → auto-generate `"{basename}-{yyyymmdd-hhmmss}"`.
    pub label: Option<String>,
    /// `None` → auto-compute `size:mtime_ns` if `target` is a file.
    pub target_hash: Option<String>,
}

/// Whether `prune` deletes only auto-created sessions or everything over age.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrunePolicy {
    AutoOnly,
    All,
}

impl SessionDb {
    /// Create a fresh SessionDb. Applies DDL, stamps `PRAGMA user_version`,
    /// inserts the session row, and records session-group metadata.
    pub fn create(opts: CreateOptions<'_>) -> Result<Self> {
        let conn = match opts.db_path {
            Some(p) => {
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent).ok();
                }
                Connection::open(p)?
            }
            None => Connection::open_in_memory()?,
        };
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        let foreign_keys: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
        if foreign_keys != 1 {
            bail!("SQLite foreign-key enforcement could not be enabled");
        }
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .ok(); // WAL unsupported in-memory — non-fatal.
        schema::apply(&conn, opts.target_class)?;
        conn.execute(
            &format!("PRAGMA user_version = {SCHEMA_VERSION}"),
            [],
        )?;

        let session_id = random_id(&conn)?;
        let label = match opts.label {
            Some(l) => l,
            None => auto_label(opts.target)?,
        };
        let target_hash = match opts.target_hash {
            Some(h) => Some(h),
            None => compute_target_hash(Path::new(opts.target)).ok(),
        };

        conn.execute(
            "INSERT INTO sessions
                (id, kind, target, target_class, target_hash,
                 started_at, label, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'), ?6, 'auto')",
            params![
                session_id,
                opts.kind.as_str(),
                opts.target,
                opts.target_class.as_str(),
                target_hash,
                label,
            ],
        )?;

        // Record session-group key under meta so peers can be discovered
        // by scanning `.dbg/sessions/*.db` and matching on this value.
        let cwd_canonical = opts
            .cwd
            .canonicalize()
            .unwrap_or_else(|_| opts.cwd.to_path_buf());
        let group = group_key(&cwd_canonical, target_hash.as_deref().unwrap_or(""));
        conn.execute(
            "INSERT INTO meta (session_id, key, value) VALUES (?1, 'session_group_key', ?2)",
            params![session_id, group],
        )?;
        conn.execute(
            "INSERT INTO meta (session_id, key, value) VALUES (?1, 'cwd', ?2)",
            params![session_id, cwd_canonical.to_string_lossy().as_ref()],
        )?;

        Ok(SessionDb {
            conn,
            session_id,
            label,
            kind: opts.kind,
            target_class: opts.target_class,
            target: opts.target.to_string(),
            db_path: opts.db_path.map(Path::to_path_buf),
        })
    }

    /// Open an existing SessionDb from disk. Refuses to open DBs with a
    /// mismatched `PRAGMA user_version`: the caller must re-collect.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
    }

    /// Open an existing session without granting SQLite write access.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        Self::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    }

    fn open_with_flags(path: &Path, flags: rusqlite::OpenFlags) -> Result<Self> {
        if !path.exists() {
            bail!("session DB not found: {}", path.display());
        }
        let conn = Connection::open_with_flags(path, flags)
            .with_context(|| format!("opening session DB {}", path.display()))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .with_context(|| format!("enabling SQLite foreign keys for {}", path.display()))?;
        let foreign_keys: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
        if foreign_keys != 1 {
            bail!("SQLite foreign-key enforcement is disabled for {}", path.display());
        }

        let found: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .with_context(|| format!("reading user_version from {}", path.display()))?;
        if found != SCHEMA_VERSION {
            bail!(
                "session DB schema_version={found}, expected {SCHEMA_VERSION} at {path}.\n\
                 No migration path. Re-collect against the raw files in {raw} \
                 (or delete the DB and re-run the originating command).",
                found = found,
                SCHEMA_VERSION = SCHEMA_VERSION,
                path = path.display(),
                raw = path
                    .parent()
                    .map(|p| p.join(path.file_stem().unwrap_or_default()).join("raw"))
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(no raw dir)".into()),
            );
        }

        validate_required_schema(&conn, path)?;
        let owner_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .with_context(|| format!("counting session owners in {}", path.display()))?;
        if owner_count != 1 {
            bail!(
                "session DB {} must contain exactly one session owner row, found {}",
                path.display(),
                owner_count
            );
        }

        // A well-formed DB has exactly one session row (the owning session).
        let (session_id, label, kind_s, class_s, target): (String, String, String, String, String) =
            conn.query_row(
                "SELECT id, label, kind, target_class, target FROM sessions LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .with_context(|| format!("reading session row from {}", path.display()))?;

        let kind = match kind_s.as_str() {
            "debug" => SessionKind::Debug,
            "profile" => SessionKind::Profile,
            other => bail!("unknown session kind in DB: {other}"),
        };
        let target_class: TargetClass = class_s
            .parse()
            .with_context(|| format!("parsing target_class {class_s}"))?;
        validate_class_schema(&conn, path, target_class)?;

        Ok(SessionDb {
            conn,
            session_id,
            label,
            kind,
            target_class,
            target,
            db_path: Some(path.to_path_buf()),
        })
    }

    /// Copy the current in-memory (or on-disk) DB to `path` using the SQLite
    /// online backup API. The source DB stays usable.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut dst = Connection::open(path)?;
        let backup = rusqlite::backup::Backup::new(&self.conn, &mut dst)?;
        backup.run_to_completion(128, Duration::from_millis(50), None)?;
        // The backup object cannot borrow `dst` after run_to_completion
        // returns, so set user_version on the freshly-populated file.
        drop(backup);
        dst.execute(&format!("PRAGMA user_version = {SCHEMA_VERSION}"), [])?;
        Ok(())
    }

    /// True iff the session has accumulated data worth keeping — either a
    /// breakpoint hit was captured (debug track) or a data layer landed
    /// (profile track). Used by the daemon shutdown path to decide whether
    /// to persist or discard an auto session.
    pub fn has_captured_data(&self) -> Result<bool> {
        let hits: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM breakpoint_hits WHERE session_id = ?1",
            params![self.session_id],
            |r| r.get(0),
        )?;
        if hits > 0 {
            return Ok(true);
        }
        let layers: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM layers WHERE session_id = ?1",
            params![self.session_id],
            |r| r.get(0),
        )?;
        if layers > 0 {
            return Ok(true);
        }
        // Profile-mode sessions (dotnet-trace, perf, callgrind, import)
        // stash their source content in meta.profile_raw rather than
        // creating a layers row. Treat that as captured data so
        // `persist_session_on_exit` actually saves the DB and
        // `dbg replay <label>` can rehydrate the profile.
        let profile_raw: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM meta WHERE session_id = ?1 AND key = 'profile_raw'",
            params![self.session_id],
            |r| r.get(0),
        )?;
        if profile_raw > 0 {
            return Ok(true);
        }
        let disassembly: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM disassembly WHERE session_id = ?1",
            params![self.session_id],
            |r| r.get(0),
        )?;
        Ok(disassembly > 0)
    }

    /// Promote this session so `prune` will never delete it.
    pub fn promote_to_user(&self) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET created_by = 'user' WHERE id = ?1",
            params![self.session_id],
        )?;
        Ok(())
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (session_id, key, value) VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id, key) DO UPDATE SET value = excluded.value",
            params![self.session_id, key, value],
        )?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn
            .query_row(
                "SELECT value FROM meta WHERE session_id = ?1 AND key = ?2",
                params![self.session_id, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn conn(&self) -> &Connection { &self.conn }
    pub fn session_id(&self) -> &str { &self.session_id }
    pub fn label(&self) -> &str { &self.label }
    pub fn kind(&self) -> SessionKind { self.kind }
    pub fn target_class(&self) -> TargetClass { self.target_class }
    pub fn target(&self) -> &str { &self.target }
    pub fn db_path(&self) -> Option<&Path> { self.db_path.as_deref() }
}

fn validate_required_schema(conn: &Connection, path: &Path) -> Result<()> {
    let required: &[(&str, &[&str])] = &[
        ("sessions", &["id", "kind", "target", "target_class", "target_hash", "started_at", "ended_at", "label", "created_by"]),
        ("layers", &["id", "session_id", "source", "file", "collected_at", "command_used", "collection_secs", "target_hash"]),
        ("symbols", &["id", "session_id", "lang", "fqn", "file", "line", "demangled", "raw", "is_synthetic"]),
        ("meta", &["session_id", "key", "value"]),
        ("failures", &["session_id", "phase", "error"]),
        ("regions", &["id", "session_id", "name", "start_us", "duration_us", "thread", "layer_id"]),
        ("allocations", &["id", "session_id", "op", "address", "bytes", "start_us", "heap", "thread", "stack_json", "layer_id"]),
        ("commands", &["seq", "session_id", "input", "output_head", "output_file", "output_bytes", "ts", "canonical_op"]),
        ("breakpoint_hits", &["id", "session_id", "location_key", "hit_seq", "thread", "ts", "locals_json", "stack_json"]),
        ("watch_evals", &["id", "session_id", "hit_id", "expr", "value", "type_name", "ts"]),
        ("disassembly", &["id", "session_id", "symbol_id", "source", "tier", "code_bytes", "asm_text", "asm_lines_json", "collected_at", "trigger", "layer_id"]),
        ("source_snapshots", &["id", "session_id", "symbol_id", "file", "line_start", "line_end", "text", "content_hash", "collected_at"]),
        ("alloc_sites", &["id", "session_id", "symbol_id", "bytes_total", "count", "largest_bytes", "collected_at", "layer_id"]),
        ("insn_hits", &["id", "session_id", "target", "hit_count", "sample_basis", "sample_period", "window_us", "backend", "collected_at", "detail_json"]),
        ("insn_hit_details", &["id", "insn_hit_id", "ts_us", "stack_json", "regs_json"]),
    ];
    for (table, columns) in required {
        validate_table_columns(conn, table, columns, path)?;
    }
    Ok(())
}

fn validate_class_schema(conn: &Connection, path: &Path, class: TargetClass) -> Result<()> {
    let required: &[(&str, &[&str])] = match class {
        TargetClass::Gpu => &[
            ("launches", &["id", "session_id", "kernel_name", "duration_us", "grid_x", "grid_y", "grid_z", "block_x", "block_y", "block_z", "stream_id", "start_us", "correlation_id", "layer_id"]),
            ("metrics", &["session_id", "kernel_name", "occupancy_pct", "compute_throughput_pct", "memory_throughput_pct", "registers_per_thread", "shared_mem_static_bytes", "shared_mem_dynamic_bytes", "l2_hit_rate_pct", "achieved_bandwidth_gb_s", "peak_bandwidth_gb_s", "boundedness", "layer_id"]),
            ("transfers", &["id", "session_id", "kind", "bytes", "duration_us", "start_us", "stream_id", "layer_id"]),
            ("ops", &["id", "session_id", "name", "module_path", "cpu_time_us", "gpu_time_us", "input_shapes", "layer_id"]),
            ("op_kernel_map", &["session_id", "op_id", "kernel_name"]),
        ],
        TargetClass::ManagedDotnet | TargetClass::Jvm => &[
            ("samples", &["id", "session_id", "symbol_id", "thread", "start_us", "duration_us", "cpu_ns", "weight", "stack_json", "layer_id"]),
            ("counters", &["id", "session_id", "name", "symbol_id", "value", "unit", "layer_id"]),
            ("gc_events", &["id", "session_id", "kind", "pause_us", "start_us", "heap_before_bytes", "heap_after_bytes", "reason", "layer_id"]),
            ("jit_events", &["id", "session_id", "symbol_id", "compile_us", "code_bytes", "tier", "start_us", "layer_id"]),
        ],
        TargetClass::Python => &[
            ("samples", &["id", "session_id", "symbol_id", "thread", "start_us", "duration_us", "cpu_ns", "weight", "stack_json", "layer_id"]),
            ("counters", &["id", "session_id", "name", "symbol_id", "value", "unit", "layer_id"]),
            ("gil_events", &["id", "session_id", "kind", "thread", "start_us", "duration_us", "layer_id"]),
        ],
        TargetClass::JsNode => &[
            ("samples", &["id", "session_id", "symbol_id", "thread", "start_us", "duration_us", "cpu_ns", "weight", "stack_json", "layer_id"]),
            ("counters", &["id", "session_id", "name", "symbol_id", "value", "unit", "layer_id"]),
            ("event_loop_lags", &["id", "session_id", "lag_us", "start_us", "phase", "layer_id"]),
        ],
        TargetClass::NativeCpu | TargetClass::Ruby | TargetClass::Php => &[
            ("samples", &["id", "session_id", "symbol_id", "thread", "start_us", "duration_us", "cpu_ns", "weight", "stack_json", "layer_id"]),
            ("counters", &["id", "session_id", "name", "symbol_id", "value", "unit", "layer_id"]),
        ],
    };
    for (table, columns) in required {
        validate_table_columns(conn, table, columns, path)?;
    }
    Ok(())
}

fn validate_table_columns(
    conn: &Connection,
    table: &str,
    required: &[&str],
    path: &Path,
) -> Result<()> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .with_context(|| format!("checking required table {table} in {}", path.display()))?;
    let actual: std::collections::HashSet<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    if actual.is_empty() {
        bail!("session DB {} is missing required table {table}", path.display());
    }
    for column in required {
        if !actual.contains(*column) {
            bail!(
                "session DB {} has malformed table {table}: missing column {column}",
                path.display()
            );
        }
    }
    Ok(())
}

/// `<cwd>/.dbg/sessions/` — where saved session DBs live.
pub fn sessions_dir(cwd: &Path) -> PathBuf {
    cwd.join(".dbg").join("sessions")
}

/// `<cwd>/.dbg/sessions/<label>/raw/` — where raw captures (`perf.data`,
/// `nsys-rep`, `.nettrace`, etc.) are preserved verbatim. Per principle 5
/// (adaptation layer), these files are the durable artifact.
pub fn raw_dir(cwd: &Path, label: &str) -> PathBuf {
    sessions_dir(cwd).join(label).join("raw")
}

/// Deterministic session-group id derived from `(cwd, target_hash)`.
/// Two sessions share a group iff they were launched against the same
/// target from the same working directory.
pub fn group_key(cwd: &Path, target_hash: &str) -> String {
    format!("{}|{}", cwd.display(), target_hash)
}

/// Cheap target fingerprint: `"<size>:<mtime_ns>"`. Good enough to detect
/// "target has been rebuilt since last session". Not a cryptographic hash.
pub fn compute_target_hash(target: &Path) -> Result<String> {
    let md = fs::metadata(target)
        .with_context(|| format!("stat {}", target.display()))?;
    let size = md.len();
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(format!("{size}:{mtime}"))
}

/// `"{basename}-{yyyymmdd-hhmmss}"` in the local-ish (system time) zone,
/// computed from `SystemTime::now()` without pulling in chrono.
pub fn auto_label(target: &str) -> Result<String> {
    let basename = Path::new(target)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("session")
        .to_string();
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(format!("{basename}-{}", format_timestamp(secs)))
}

/// UTC `yyyymmdd-hhmmss`. Proleptic Gregorian, no leap seconds —
/// sufficient for session labels.
fn format_timestamp(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = decompose_utc(secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

fn decompose_utc(mut secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = (secs % 60) as u32;
    secs /= 60;
    let mi = (secs % 60) as u32;
    secs /= 60;
    let h = (secs % 24) as u32;
    secs /= 24;
    let mut days = secs as i64;

    // Convert days-since-1970-01-01 to (year, month, day) — algorithm from
    // Howard Hinnant's date library, public-domain.
    days += 719_468;
    let era = if days >= 0 { days / 146_097 } else { (days - 146_096) / 146_097 };
    let doe = (days - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (y as u32, mo, d, h, mi, s)
}

fn random_id(conn: &Connection) -> Result<String> {
    // 16-byte randomblob → 32-hex-char id. SQLite's CSPRNG.
    let id: String =
        conn.query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))?;
    Ok(id)
}

/// Walk `sessions_dir`, delete `.db` files for auto sessions whose mtime
/// is older than `older_than`. Returns the list of deleted paths.
/// User-promoted sessions are never touched under `PrunePolicy::AutoOnly`.
pub fn prune(
    sessions_dir: &Path,
    older_than: Duration,
    policy: PrunePolicy,
) -> Result<Vec<PathBuf>> {
    if !sessions_dir.exists() {
        return Ok(vec![]);
    }
    let cutoff = SystemTime::now() - older_than;
    let mut deleted = vec![];

    for entry in fs::read_dir(sessions_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("db") {
            continue;
        }
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = md.modified().unwrap_or(SystemTime::now());
        if mtime > cutoff {
            continue;
        }

        // Inspect created_by without opening via SessionDb (which would
        // fail on version mismatch — intentional, but the prune path
        // should still be able to remove obsolete files).
        let should_delete = match policy {
            PrunePolicy::All => true,
            PrunePolicy::AutoOnly => is_auto_session(&path).unwrap_or(false),
        };
        if !should_delete {
            continue;
        }

        // Remove the DB plus any adjacent raw-capture directory named
        // after the label (file stem).
        fs::remove_file(&path).ok();
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            let raw = sessions_dir.join(stem);
            if raw.is_dir() {
                fs::remove_dir_all(&raw).ok();
            }
        }
        deleted.push(path);
    }

    Ok(deleted)
}

fn is_auto_session(path: &Path) -> Result<bool> {
    let conn = Connection::open(path)?;
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(-1);
    let created_by: String = conn
        .query_row("SELECT created_by FROM sessions LIMIT 1", [], |r| r.get(0))
        .unwrap_or_else(|_| "auto".to_string());
    if v != SCHEMA_VERSION {
        // A user-promoted DB must remain protected even when it needs a
        // migration. Unknown files without that marker remain pruneable.
        return Ok(created_by != "user");
    }
    Ok(created_by == "auto")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn opts<'a>(cwd: &'a Path, target: &'a str) -> CreateOptions<'a> {
        CreateOptions {
            kind: SessionKind::Debug,
            target,
            target_class: TargetClass::NativeCpu,
            cwd,
            db_path: None,
            label: Some(format!("t-{target}")),
            target_hash: Some("test".into()),
        }
    }

    #[test]
    fn create_inserts_session_row() {
        let tmp = TempDir::new().unwrap();
        let db = SessionDb::create(opts(tmp.path(), "/bin/ls")).unwrap();
        assert_eq!(db.kind(), SessionKind::Debug);
        assert_eq!(db.target(), "/bin/ls");
        let (label, class, created_by): (String, String, String) = db
            .conn
            .query_row(
                "SELECT label, target_class, created_by FROM sessions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(label, "t-/bin/ls");
        assert_eq!(class, "native-cpu");
        assert_eq!(created_by, "auto");
    }

    #[test]
    fn save_and_open_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("roundtrip.db");
        let src = SessionDb::create(opts(tmp.path(), "/bin/ls")).unwrap();
        src.save_to(&path).unwrap();

        let opened = SessionDb::open(&path).unwrap();
        assert_eq!(opened.session_id(), src.session_id());
        assert_eq!(opened.label(), src.label());
        assert_eq!(opened.target_class(), TargetClass::NativeCpu);
    }

    #[test]
    fn foreign_keys_are_enabled_for_created_and_opened_sessions() {
        let tmp = TempDir::new().unwrap();
        let db = SessionDb::create(opts(tmp.path(), "/bin/ls")).unwrap();
        let fk: i64 = db
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
        assert!(db
            .conn
            .execute(
                "INSERT INTO layers (session_id, source) VALUES ('missing', 'bad')",
                [],
            )
            .is_err());

        let path = tmp.path().join("foreign-keys.db");
        db.save_to(&path).unwrap();
        let opened = SessionDb::open(&path).unwrap();
        let fk: i64 = opened
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
        assert!(opened
            .conn
            .execute(
                "INSERT INTO layers (session_id, source) VALUES ('missing', 'bad')",
                [],
            )
            .is_err());
    }

    #[test]
    fn open_requires_exactly_one_owner_row_and_required_tables() {
        let tmp = TempDir::new().unwrap();
        let source = SessionDb::create(opts(tmp.path(), "/bin/ls")).unwrap();
        let path = tmp.path().join("malformed.db");
        source.save_to(&path).unwrap();
        let err = {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
            conn.execute("DELETE FROM sessions", []).unwrap();
            SessionDb::open(&path).unwrap_err().to_string()
        };
        assert!(err.contains("exactly one session owner row"), "{err}");

        let source = SessionDb::create(opts(tmp.path(), "/bin/ls")).unwrap();
        source.save_to(&path).unwrap();
        let err = {
            let conn = Connection::open(&path).unwrap();
            conn.execute("DROP TABLE samples", []).unwrap();
            SessionDb::open(&path).unwrap_err().to_string()
        };
        assert!(err.contains("missing required table samples"), "{err}");
    }

    #[test]
    fn open_refuses_mismatched_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.db");
        {
            let conn = Connection::open(&path).unwrap();
            // Pretend this DB was written by a far-future version.
            conn.execute("PRAGMA user_version = 9999", []).unwrap();
        }
        let err = SessionDb::open(&path).unwrap_err().to_string();
        assert!(
            err.contains("schema_version=9999") && err.contains("No migration path"),
            "error missing expected phrases: {err}"
        );
    }

    #[test]
    fn has_captured_data_false_on_fresh_session() {
        let tmp = TempDir::new().unwrap();
        let db = SessionDb::create(opts(tmp.path(), "/bin/ls")).unwrap();
        assert!(!db.has_captured_data().unwrap());
    }

    #[test]
    fn has_captured_data_true_after_breakpoint_hit() {
        let tmp = TempDir::new().unwrap();
        let db = SessionDb::create(opts(tmp.path(), "/bin/ls")).unwrap();
        db.conn
            .execute(
                "INSERT INTO breakpoint_hits
                    (session_id, location_key, hit_seq, ts)
                 VALUES (?1, 'main.c:42', 1, datetime('now'))",
                params![db.session_id()],
            )
            .unwrap();
        assert!(db.has_captured_data().unwrap());
    }

    #[test]
    fn has_captured_data_true_after_layer() {
        let tmp = TempDir::new().unwrap();
        let db = SessionDb::create(opts(tmp.path(), "/bin/ls")).unwrap();
        db.conn
            .execute(
                "INSERT INTO layers (session_id, source) VALUES (?1, 'perf')",
                params![db.session_id()],
            )
            .unwrap();
        assert!(db.has_captured_data().unwrap());
    }

    #[test]
    fn promote_to_user_sticks_through_save() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("user.db");
        let src = SessionDb::create(opts(tmp.path(), "/bin/ls")).unwrap();
        src.promote_to_user().unwrap();
        src.save_to(&path).unwrap();
        let opened = SessionDb::open(&path).unwrap();
        let cb: String = opened
            .conn
            .query_row("SELECT created_by FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cb, "user");
    }

    #[test]
    fn read_only_open_rejects_mutation() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("readonly.db");
        let src = SessionDb::create(opts(tmp.path(), "/bin/ls")).unwrap();
        src.save_to(&path).unwrap();
        let ro = SessionDb::open_read_only(&path).unwrap();
        assert!(ro.promote_to_user().is_err());
    }

    #[test]
    fn set_and_get_meta() {
        let tmp = TempDir::new().unwrap();
        let db = SessionDb::create(opts(tmp.path(), "/bin/ls")).unwrap();
        db.set_meta("custom", "hello").unwrap();
        assert_eq!(db.meta("custom").unwrap(), Some("hello".into()));
        db.set_meta("custom", "world").unwrap();
        assert_eq!(db.meta("custom").unwrap(), Some("world".into()));
    }

    #[test]
    fn session_group_key_persisted_at_create() {
        let tmp = TempDir::new().unwrap();
        let db = SessionDb::create(opts(tmp.path(), "/bin/ls")).unwrap();
        let g = db.meta("session_group_key").unwrap().unwrap();
        assert!(g.ends_with("|test"), "unexpected group key: {g}");
    }

    #[test]
    fn prune_auto_only_keeps_user_sessions() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();

        let auto_path = dir.join("auto.db");
        let user_path = dir.join("user.db");
        let a = SessionDb::create(CreateOptions {
            db_path: None,
            ..opts(&dir, "/bin/ls")
        })
        .unwrap();
        a.save_to(&auto_path).unwrap();

        let u = SessionDb::create(CreateOptions {
            db_path: None,
            ..opts(&dir, "/bin/ls")
        })
        .unwrap();
        u.promote_to_user().unwrap();
        u.save_to(&user_path).unwrap();

        // Give the fs a tick so our ZERO-age cutoff sits at-or-after the
        // file mtimes (required on coarse-grained timestamp filesystems).
        std::thread::sleep(Duration::from_millis(10));

        let deleted = prune(&dir, Duration::ZERO, PrunePolicy::AutoOnly).unwrap();
        assert_eq!(deleted.len(), 1, "deleted: {deleted:?}");
        assert!(deleted[0].ends_with("auto.db"));
        assert!(user_path.exists());
        assert!(!auto_path.exists());
    }

    #[test]
    fn prune_auto_only_keeps_promoted_unknown_schema() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("promoted.db");
        let db = SessionDb::create(opts(tmp.path(), "/bin/ls")).unwrap();
        db.promote_to_user().unwrap();
        db.save_to(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.execute("PRAGMA user_version = 9999", []).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        let deleted = prune(tmp.path(), Duration::ZERO, PrunePolicy::AutoOnly).unwrap();
        assert!(deleted.is_empty());
        assert!(path.exists());
    }

    #[test]
    fn prune_skips_files_newer_than_cutoff() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let p = dir.join("fresh.db");
        let s = SessionDb::create(CreateOptions {
            db_path: None,
            ..opts(&dir, "/bin/ls")
        })
        .unwrap();
        s.save_to(&p).unwrap();
        // One-day cutoff should keep a just-created file.
        let deleted = prune(&dir, Duration::from_secs(86_400), PrunePolicy::AutoOnly).unwrap();
        assert!(deleted.is_empty());
        assert!(p.exists());
    }

    #[test]
    fn auto_label_has_expected_shape() {
        let lbl = auto_label("/usr/bin/ls").unwrap();
        assert!(lbl.starts_with("ls-"), "label: {lbl}");
        let suffix = lbl.trim_start_matches("ls-");
        assert_eq!(suffix.len(), 15); // yyyymmdd-hhmmss
        let (date, time) = suffix.split_once('-').unwrap();
        assert_eq!(date.len(), 8);
        assert_eq!(time.len(), 6);
        assert!(date.chars().all(|c| c.is_ascii_digit()));
        assert!(time.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn raw_dir_is_sibling_of_label() {
        let d = raw_dir(Path::new("/tmp/proj"), "myapp-20260415-120000");
        assert_eq!(
            d,
            PathBuf::from("/tmp/proj/.dbg/sessions/myapp-20260415-120000/raw")
        );
    }

    #[test]
    fn group_key_stable_across_create_calls() {
        let tmp = TempDir::new().unwrap();
        let a = SessionDb::create(opts(tmp.path(), "/bin/ls")).unwrap();
        let b = SessionDb::create(opts(tmp.path(), "/bin/ls")).unwrap();
        assert_eq!(
            a.meta("session_group_key").unwrap(),
            b.meta("session_group_key").unwrap()
        );
    }
}
