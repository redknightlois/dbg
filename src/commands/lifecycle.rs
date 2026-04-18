//! Session-lifecycle commands: `sessions`, `save`, `prune`, `diff`.
//!
//! These operate on SessionDb files — the active one (save, diff
//! current) and the ones under `.dbg/sessions/` (sessions, prune,
//! diff against-other). Shape mirrors `crosstrack` — parse to an
//! enum, execute against a `RunCtx`-style input, return a string.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use dbg_cli::session_db::{PrunePolicy, SessionDb, prune, sessions_dir};
use rusqlite::{OptionalExtension, params};

/// Parsed lifecycle command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    /// `dbg sessions [--group]` — list sessions in `.dbg/sessions/`.
    Sessions { group_only: bool },
    /// `dbg save [<label>]` — promote the active session (or a named
    /// one under `.dbg/sessions/`) from auto to user.
    Save { label: Option<String> },
    /// `dbg prune [--older-than <duration>] [--all]`.
    Prune {
        older_than: Duration,
        policy: PrunePolicy,
    },
    /// `dbg diff <other-session>` — FULL OUTER JOIN current vs <other>
    /// on breakpoint hits (debug) or sample totals (profile).
    Diff { other: String },
    /// `dbg status` — one-line summary of the currently-live session.
    Status,
    /// `dbg replay <label>` — open a persisted session read-only and
    /// run crosstrack queries against it. When invoked inside the
    /// daemon this just reports the path the client should re-exec
    /// against; the actual replay is handled by the client short-
    /// circuiting the start path.
    Replay { label: String },
}

impl Lifecycle {
    pub fn canonical_op(&self) -> &'static str {
        match self {
            Lifecycle::Sessions { .. } => "sessions",
            Lifecycle::Save { .. } => "save",
            Lifecycle::Prune { .. } => "prune",
            Lifecycle::Diff { .. } => "diff",
            Lifecycle::Status => "status",
            Lifecycle::Replay { .. } => "replay",
        }
    }
}

/// Try to parse `input` as a lifecycle command.
pub fn try_dispatch(input: &str) -> Option<super::Dispatched> {
    let input = input.trim();
    let (verb, rest) = match input.find(|c: char| c.is_ascii_whitespace()) {
        Some(i) => (&input[..i], input[i..].trim_start()),
        None => (input, ""),
    };
    let l = match verb {
        "sessions" => {
            let group_only = rest.split_whitespace().any(|a| a == "--group" || a == "-g");
            Lifecycle::Sessions { group_only }
        }
        "save" => {
            // Accept both positional (`save mylabel`) and flagged
            // (`save --label mylabel`) forms. Without this, the flag
            // token was stored verbatim as the label ("--label mylabel").
            let mut label: Option<String> = None;
            let mut toks = rest.split_whitespace().peekable();
            while let Some(t) = toks.next() {
                match t {
                    "--label" | "-l" => {
                        if let Some(v) = toks.next() {
                            label = Some(v.to_string());
                        } else {
                            return Some(super::Dispatched::Immediate(
                                "--label needs a value".into(),
                            ));
                        }
                    }
                    "--help" | "-h" => {
                        return Some(super::Dispatched::Immediate(
                            "usage: dbg save [<label> | --label <label>]".into(),
                        ));
                    }
                    _ if t.starts_with("--") => {
                        return Some(super::Dispatched::Immediate(
                            format!("unknown flag `{t}` — supported: --label"),
                        ));
                    }
                    _ if label.is_none() => label = Some(t.to_string()),
                    _ => {}
                }
            }
            Lifecycle::Save { label }
        }
        "prune" => {
            let (older_than, policy) = parse_prune_args(rest);
            Lifecycle::Prune { older_than, policy }
        }
        "diff" => {
            if rest.is_empty() {
                return Some(super::Dispatched::Immediate(
                    "usage: dbg diff <other-session-label-or-path>".into(),
                ));
            }
            Lifecycle::Diff {
                other: rest.to_string(),
            }
        }
        "status" => Lifecycle::Status,
        "replay" => {
            if rest.is_empty() {
                return Some(super::Dispatched::Immediate(
                    "usage: dbg replay <label>  (see `dbg sessions` for labels)".into(),
                ));
            }
            Lifecycle::Replay {
                label: rest.split_whitespace().next().unwrap_or(rest).to_string(),
            }
        }
        _ => return None,
    };
    Some(super::Dispatched::Lifecycle(l))
}

fn parse_prune_args(rest: &str) -> (Duration, PrunePolicy) {
    let mut older_than = Duration::from_secs(7 * 86_400);
    let mut policy = PrunePolicy::AutoOnly;
    let toks: Vec<&str> = rest.split_whitespace().collect();
    let mut i = 0;
    while i < toks.len() {
        match toks[i] {
            "--all" => policy = PrunePolicy::All,
            "--older-than" => {
                if let Some(v) = toks.get(i + 1) {
                    if let Some(d) = parse_duration(v) {
                        older_than = d;
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    (older_than, policy)
}

/// Parse `1h`, `2d`, `30m`, `45s`, `604800` (seconds).
pub(crate) fn parse_duration(s: &str) -> Option<Duration> {
    if s.is_empty() {
        return None;
    }
    let last = s.chars().last().unwrap();
    let (num_str, unit) = if last.is_ascii_digit() {
        (s, 's')
    } else {
        (&s[..s.len() - last.len_utf8()], last)
    };
    let n: u64 = num_str.parse().ok()?;
    let secs = match unit {
        's' => n,
        'm' => n * 60,
        'h' => n * 3600,
        'd' => n * 86_400,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

// ============================================================
// Execution
// ============================================================

pub struct LifeCtx<'a> {
    pub cwd: &'a Path,
    /// The active session — needed for `save` (no label) and for
    /// reading the group key so `sessions --group` shows peers.
    pub active: Option<&'a SessionDb>,
}

pub fn run(l: &Lifecycle, ctx: &LifeCtx<'_>) -> String {
    match l {
        Lifecycle::Sessions { group_only } => cmd_sessions(ctx, *group_only),
        Lifecycle::Save { label } => cmd_save(ctx, label.as_deref()),
        Lifecycle::Prune { older_than, policy } => cmd_prune(ctx, *older_than, *policy),
        Lifecycle::Diff { other } => cmd_diff(ctx, other),
        Lifecycle::Status => cmd_status(ctx),
        Lifecycle::Replay { label } => cmd_replay_info(ctx, label),
    }
}

fn cmd_status(ctx: &LifeCtx<'_>) -> String {
    match ctx.active {
        None => "no session".into(),
        Some(db) => {
            let (target, target_class, kind): (String, String, String) = db
                .conn()
                .query_row(
                    "SELECT target, target_class, kind FROM sessions LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap_or_else(|_| ("?".into(), "?".into(), "?".into()));
            let pid = std::process::id();
            format!(
                "active session `{label}`\n  target:  {target}\n  class:   {class}\n  \
                 kind:    {kind}\n  daemon:  pid={pid}\n",
                label = db.label(),
                class = target_class,
            )
        }
    }
}

/// `dbg replay <label>` inside the daemon just reports that replay is
/// a client-level operation — the client short-circuits `dbg replay`
/// before hitting the daemon. If we end up here it's because the
/// agent ran `dbg replay ...` while a live session is active.
fn cmd_replay_info(_ctx: &LifeCtx<'_>, label: &str) -> String {
    format!(
        "cannot open replay `{label}` while a live session is running — \
         `dbg kill` the current session first, then `dbg replay {label}`"
    )
}

fn cmd_sessions(ctx: &LifeCtx<'_>, group_only: bool) -> String {
    let dir = sessions_dir(ctx.cwd);
    if !dir.exists() {
        return "no saved sessions (nothing under .dbg/sessions/)".into();
    }
    let group_key = if group_only {
        ctx.active.and_then(|db| db.meta("session_group_key").ok().flatten())
    } else {
        None
    };

    let mut rows: Vec<SessionListing> = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => return format!("[error reading {}: {e}]", dir.display()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("db") {
            continue;
        }
        if let Some(listing) = read_listing(&path) {
            if let Some(ref needed) = group_key {
                if listing.group_key.as_ref() != Some(needed) {
                    continue;
                }
            }
            rows.push(listing);
        }
    }

    let active_label: Option<String> = ctx.active.map(|db| db.label().to_string());

    // The live session's DB isn't in .dbg/sessions/ until `dbg save`
    // (or the daemon's exit handler) persists it. Synthesize a row
    // for it so `dbg sessions` during a live session shows it,
    // marked `*`, instead of omitting it entirely.
    if let (Some(db), Some(label)) = (ctx.active, active_label.as_deref()) {
        if !rows.iter().any(|r| r.label == label) {
            rows.push(SessionListing {
                label: label.to_string(),
                kind: format!("{:?}", db.kind()).to_lowercase(),
                target_class: db.target_class().as_str().to_string(),
                created_by: "live".into(),
                group_key: db.meta("session_group_key").ok().flatten(),
                age_secs: 0,
            });
        }
    }

    if rows.is_empty() {
        if group_only {
            return "no peers in the current session group".into();
        }
        return format!("no sessions under {}", dir.display());
    }

    rows.sort_by(|a, b| b.age_secs.cmp(&a.age_secs)); // newest first — smaller age
    rows.sort_by_key(|r| r.age_secs);
    let mut out = String::new();
    out.push_str("  label                                  kind     class           by    age\n");
    for r in &rows {
        let live_mark = match &active_label {
            Some(l) if *l == r.label => "*",
            _ => " ",
        };
        out.push_str(&format!(
            "{live_mark} {:<38} {:<8} {:<15} {:<5} {}\n",
            truncate(&r.label, 38),
            r.kind,
            r.target_class,
            r.created_by,
            humanize_secs(r.age_secs),
        ));
    }
    if active_label.is_some() {
        out.push_str("\n* = currently live session\n");
    }
    out
}

struct SessionListing {
    label: String,
    kind: String,
    target_class: String,
    created_by: String,
    group_key: Option<String>,
    age_secs: u64,
}

fn read_listing(path: &Path) -> Option<SessionListing> {
    let conn = rusqlite::Connection::open(path).ok()?;
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .ok()?;
    // Tolerate old-format DBs: we just don't list them rather than
    // erroring — the prune path cleans them up.
    if v != dbg_cli::session_db::SCHEMA_VERSION {
        return None;
    }
    let (label, kind, tc, created_by): (String, String, String, String) = conn
        .query_row(
            "SELECT label, kind, target_class, created_by FROM sessions LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok()?;
    let group_key: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key='session_group_key' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    let age_secs = path
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some(SessionListing {
        label,
        kind,
        target_class: tc,
        created_by,
        group_key,
        age_secs,
    })
}

fn humanize_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut cut = max.saturating_sub(1);
        while !s.is_char_boundary(cut) && cut > 0 {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    }
}

fn cmd_save(ctx: &LifeCtx<'_>, label: Option<&str>) -> String {
    match label {
        None => match ctx.active {
            Some(db) => match db.promote_to_user() {
                Ok(_) => format!(
                    "promoted active session `{}` to created_by=user (won't be pruned)",
                    db.label()
                ),
                Err(e) => format!("[error: {e}]"),
            },
            None => "no active session to save (start one with `dbg start`)".into(),
        },
        Some(lbl) => {
            let path = sessions_dir(ctx.cwd).join(format!("{lbl}.db"));
            if !path.exists() {
                return format!("no saved session named `{lbl}` under {}", path.display());
            }
            let conn = match rusqlite::Connection::open(&path) {
                Ok(c) => c,
                Err(e) => return format!("[error opening {}: {e}]", path.display()),
            };
            match conn.execute(
                "UPDATE sessions SET created_by='user' WHERE created_by='auto'",
                [],
            ) {
                Ok(n) if n > 0 => format!("promoted `{lbl}` to created_by=user"),
                Ok(_) => format!("`{lbl}` was already user-owned"),
                Err(e) => format!("[error: {e}]"),
            }
        }
    }
}

fn cmd_prune(ctx: &LifeCtx<'_>, older_than: Duration, policy: PrunePolicy) -> String {
    let dir = sessions_dir(ctx.cwd);
    let deleted = match prune(&dir, older_than, policy) {
        Ok(d) => d,
        Err(e) => return format!("[error: {e}]"),
    };
    if deleted.is_empty() {
        return format!(
            "no sessions matched (policy={:?}, older_than≥{}s)",
            policy,
            older_than.as_secs()
        );
    }
    let mut out = format!("deleted {} session(s):\n", deleted.len());
    for p in deleted {
        if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
            out.push_str(&format!("  {name}\n"));
        }
    }
    out
}

fn cmd_diff(ctx: &LifeCtx<'_>, other: &str) -> String {
    let active = match ctx.active {
        Some(a) => a,
        None => return "no active session — start one before diffing".into(),
    };
    let other_path = resolve_session_path(ctx.cwd, other);
    if !other_path.exists() {
        return format!("no session file at {}", other_path.display());
    }

    // Guard against version skew: we refuse to diff a DB the daemon
    // can't read (matches the no-migration policy).
    if let Ok(conn) = rusqlite::Connection::open(&other_path) {
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(-1);
        if v != dbg_cli::session_db::SCHEMA_VERSION {
            return format!(
                "`{}` has schema_version={v}, expected {} — re-collect to diff",
                other_path.display(),
                dbg_cli::session_db::SCHEMA_VERSION,
            );
        }
    }

    // ATTACH the other DB into the active conn and run a FULL OUTER
    // JOIN on symbols (lang, fqn) with LEFT/RIGHT OUTER unioned —
    // SQLite doesn't support FULL OUTER JOIN natively so we emulate.
    let attach_sql = format!(
        "ATTACH DATABASE ? AS other_db",
    );
    if let Err(e) = active.conn().execute(&attach_sql, params![other_path.to_string_lossy().as_ref()]) {
        return format!("[error attaching {}: {e}]", other_path.display());
    }
    // Best-effort detach on exit — leaking is benign within one
    // daemon lifetime but tidy is better.
    let _detach_guard = DetachGuard(active);

    // SQLite has no native FULL OUTER JOIN; emulate with a LEFT JOIN
    // ∪ a swapped LEFT JOIN and aggregate in a wrapper so ORDER BY
    // can see the computed column names.
    let sql = "
        WITH a AS (
            SELECT s.lang, s.fqn, COUNT(bh.id) AS hits_a
            FROM symbols s
            LEFT JOIN breakpoint_hits bh
                ON bh.session_id = s.session_id AND bh.location_key = s.fqn
            GROUP BY s.lang, s.fqn
        ),
        b AS (
            SELECT s.lang, s.fqn, COUNT(bh.id) AS hits_b
            FROM other_db.symbols s
            LEFT JOIN other_db.breakpoint_hits bh
                ON bh.session_id = s.session_id AND bh.location_key = s.fqn
            GROUP BY s.lang, s.fqn
        ),
        combined AS (
            SELECT a.lang, a.fqn,
                   a.hits_a AS hits_a,
                   COALESCE(b.hits_b, 0) AS hits_b
            FROM a LEFT JOIN b USING (lang, fqn)
            UNION
            SELECT b.lang, b.fqn,
                   COALESCE(a.hits_a, 0) AS hits_a,
                   b.hits_b AS hits_b
            FROM b LEFT JOIN a USING (lang, fqn)
        )
        SELECT lang, fqn, hits_a, hits_b
        FROM combined
        ORDER BY ABS(hits_a - hits_b) DESC, lang, fqn";

    let rows: Result<Vec<(String, String, i64, i64)>, rusqlite::Error> = active
        .conn()
        .prepare(sql)
        .and_then(|mut s| {
            s.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .and_then(|it| it.collect())
        });
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return format!("[error running diff: {e}]"),
    };

    if rows.is_empty() {
        return format!(
            "diff {} ↔ {}  — no symbols to compare",
            active.label(),
            other,
        );
    }

    let mut out = format!("diff {} ↔ {}\n", active.label(), other);
    out.push_str("lang     fqn                                     hits_a  hits_b  Δ\n");
    for (lang, fqn, a, b) in rows.iter().take(40) {
        let delta = a - b;
        out.push_str(&format!(
            "{:<8} {:<40} {a:>6}  {b:>6}  {delta:+}\n",
            lang,
            truncate(fqn, 40),
        ));
    }
    if rows.len() > 40 {
        out.push_str(&format!("… {} more rows (use `dbg export` for the full set)\n", rows.len() - 40));
    }
    out
}

struct DetachGuard<'a>(&'a SessionDb);
impl Drop for DetachGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.conn().execute("DETACH DATABASE other_db", []);
    }
}

fn resolve_session_path(cwd: &Path, other: &str) -> PathBuf {
    // Explicit path wins; else treat as a label under .dbg/sessions/.
    let p = PathBuf::from(other);
    if p.is_absolute() || other.contains('/') {
        return p;
    }
    sessions_dir(cwd).join(format!("{other}.db"))
}

// Suppress `UNIX_EPOCH` unused warning in builds that short-circuit
// before a timestamp comparison runs.
#[allow(dead_code)]
fn _keep_epoch_import() -> SystemTime { UNIX_EPOCH }

#[cfg(test)]
mod tests {
    use super::*;
    use dbg_cli::session_db::{CreateOptions, SessionKind, TargetClass};
    use tempfile::TempDir;

    fn mk_db(tmp: &TempDir, label: &str) -> SessionDb {
        SessionDb::create(CreateOptions {
            kind: SessionKind::Debug,
            target: "./app",
            target_class: TargetClass::NativeCpu,
            cwd: tmp.path(),
            db_path: None,
            label: Some(label.into()),
            target_hash: Some("h".into()),
        })
        .unwrap()
    }

    #[test]
    fn parse_duration_variants() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("7d"), Some(Duration::from_secs(7 * 86_400)));
        assert_eq!(parse_duration("60"), Some(Duration::from_secs(60)));
        assert_eq!(parse_duration("bogus"), None);
        assert_eq!(parse_duration(""), None);
    }

    #[test]
    fn parse_prune_args_defaults_seven_days_auto_only() {
        let (d, p) = parse_prune_args("");
        assert_eq!(d.as_secs(), 7 * 86_400);
        assert_eq!(p, PrunePolicy::AutoOnly);
    }

    #[test]
    fn parse_prune_args_all_and_custom_age() {
        let (d, p) = parse_prune_args("--older-than 1h --all");
        assert_eq!(d.as_secs(), 3600);
        assert_eq!(p, PrunePolicy::All);
    }

    #[test]
    fn try_dispatch_parses_each_verb() {
        for (input, expect_op) in [
            ("sessions", "sessions"),
            ("sessions --group", "sessions"),
            ("save", "save"),
            ("save my-label", "save"),
            ("prune", "prune"),
            ("prune --older-than 1h", "prune"),
            ("diff my-other", "diff"),
        ] {
            match try_dispatch(input).unwrap() {
                super::super::Dispatched::Lifecycle(l) => assert_eq!(l.canonical_op(), expect_op, "{input}"),
                other => panic!("unexpected variant for {input}: {:?}", match other {
                    super::super::Dispatched::Immediate(s) => format!("Immediate({s:?})"),
                    _ => "other".into(),
                }),
            }
        }
    }

    #[test]
    fn try_dispatch_none_for_unrelated_verb() {
        assert!(try_dispatch("break main.c:42").is_none());
        assert!(try_dispatch("continue").is_none());
    }

    #[test]
    fn save_accepts_label_flag() {
        // Regression: `save --label foo` used to fold the flag into
        // the label, producing a file named "--label foo.db".
        match try_dispatch("save --label mylabel").unwrap() {
            super::super::Dispatched::Lifecycle(Lifecycle::Save { label }) => {
                assert_eq!(label.as_deref(), Some("mylabel"));
            }
            _ => panic!("expected Save"),
        }
    }

    #[test]
    fn save_rejects_unknown_flag() {
        match try_dispatch("save --bogus").unwrap() {
            super::super::Dispatched::Immediate(s) => assert!(s.contains("unknown flag"), "{s}"),
            _ => panic!("expected Immediate"),
        }
    }

    #[test]
    fn save_positional_label_still_works() {
        match try_dispatch("save mylabel").unwrap() {
            super::super::Dispatched::Lifecycle(Lifecycle::Save { label }) => {
                assert_eq!(label.as_deref(), Some("mylabel"));
            }
            _ => panic!("expected Save"),
        }
    }

    #[test]
    fn diff_needs_payload() {
        match try_dispatch("diff").unwrap() {
            super::super::Dispatched::Immediate(s) => assert!(s.contains("usage")),
            _ => panic!("expected usage hint"),
        }
    }

    #[test]
    fn sessions_reports_empty_when_no_sessions_dir() {
        let tmp = TempDir::new().unwrap();
        let ctx = LifeCtx { cwd: tmp.path(), active: None };
        let out = cmd_sessions(&ctx, false);
        assert!(out.contains("no saved sessions"));
    }

    #[test]
    fn sessions_lists_saved_dbs() {
        let tmp = TempDir::new().unwrap();
        let db = mk_db(&tmp, "first");
        db.conn().execute(
            "INSERT INTO layers (session_id, source) VALUES ((SELECT id FROM sessions LIMIT 1), 'perf')",
            [],
        ).unwrap();
        let p = sessions_dir(tmp.path()).join("first.db");
        db.save_to(&p).unwrap();

        let ctx = LifeCtx { cwd: tmp.path(), active: None };
        let out = cmd_sessions(&ctx, false);
        assert!(out.contains("first"));
        assert!(out.contains("native-cpu"));
    }

    #[test]
    fn save_promotes_active_session() {
        let tmp = TempDir::new().unwrap();
        let db = mk_db(&tmp, "active");
        let ctx = LifeCtx { cwd: tmp.path(), active: Some(&db) };
        let out = cmd_save(&ctx, None);
        assert!(out.contains("created_by=user"));
        let cb: String = db.conn().query_row(
            "SELECT created_by FROM sessions",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(cb, "user");
    }

    #[test]
    fn save_named_promotes_file() {
        let tmp = TempDir::new().unwrap();
        let db = mk_db(&tmp, "named");
        let p = sessions_dir(tmp.path()).join("named.db");
        db.save_to(&p).unwrap();

        let ctx = LifeCtx { cwd: tmp.path(), active: None };
        let out = cmd_save(&ctx, Some("named"));
        assert!(out.contains("promoted") || out.contains("already"));

        let conn = rusqlite::Connection::open(&p).unwrap();
        let cb: String = conn
            .query_row("SELECT created_by FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cb, "user");
    }

    #[test]
    fn save_missing_label_reports() {
        let tmp = TempDir::new().unwrap();
        let ctx = LifeCtx { cwd: tmp.path(), active: None };
        let out = cmd_save(&ctx, Some("does-not-exist"));
        assert!(out.contains("no saved session"));
    }

    #[test]
    fn prune_zero_age_removes_auto_sessions() {
        let tmp = TempDir::new().unwrap();
        let db = mk_db(&tmp, "auto1");
        db.save_to(&sessions_dir(tmp.path()).join("auto1.db")).unwrap();
        std::thread::sleep(Duration::from_millis(10));

        let ctx = LifeCtx { cwd: tmp.path(), active: None };
        let out = cmd_prune(&ctx, Duration::ZERO, PrunePolicy::AutoOnly);
        assert!(out.contains("deleted 1 session"));
        assert!(out.contains("auto1.db"));
    }

    #[test]
    fn diff_reports_no_active_session() {
        let tmp = TempDir::new().unwrap();
        let ctx = LifeCtx { cwd: tmp.path(), active: None };
        let out = cmd_diff(&ctx, "anything");
        assert!(out.contains("no active session"));
    }

    #[test]
    fn diff_refuses_version_skew() {
        let tmp = TempDir::new().unwrap();
        let active = mk_db(&tmp, "cur");
        let other_path = tmp.path().join("bad.db");
        {
            let conn = rusqlite::Connection::open(&other_path).unwrap();
            conn.execute("PRAGMA user_version = 9999", []).unwrap();
        }
        let ctx = LifeCtx { cwd: tmp.path(), active: Some(&active) };
        let out = cmd_diff(&ctx, other_path.to_string_lossy().as_ref());
        assert!(out.contains("9999"));
        assert!(out.contains("re-collect"));
    }

    #[test]
    fn diff_reports_empty_on_no_symbols() {
        let tmp = TempDir::new().unwrap();
        let active = mk_db(&tmp, "cur");
        let other = mk_db(&tmp, "other");
        let other_path = sessions_dir(tmp.path()).join("other.db");
        other.save_to(&other_path).unwrap();

        let ctx = LifeCtx { cwd: tmp.path(), active: Some(&active) };
        let out = cmd_diff(&ctx, "other");
        assert!(
            out.contains("no symbols") || out.contains("diff cur"),
            "unexpected output:\n{out}"
        );
    }

    #[test]
    fn diff_full_outer_join_counts() {
        let tmp = TempDir::new().unwrap();
        let active = mk_db(&tmp, "a");
        let other = mk_db(&tmp, "b");

        // `a` has foo hit 3 times; `b` has bar hit 5 times; they
        // share nothing.
        active.conn().execute(
            "INSERT INTO symbols (session_id, lang, fqn, raw)
             VALUES ((SELECT id FROM sessions LIMIT 1), 'cpp', 'foo', 'foo')",
            [],
        ).unwrap();
        for seq in 1..=3 {
            active.conn().execute(
                "INSERT INTO breakpoint_hits (session_id, location_key, hit_seq, ts)
                 VALUES ((SELECT id FROM sessions LIMIT 1), 'foo', ?1, datetime('now'))",
                params![seq],
            ).unwrap();
        }
        other.conn().execute(
            "INSERT INTO symbols (session_id, lang, fqn, raw)
             VALUES ((SELECT id FROM sessions LIMIT 1), 'cpp', 'bar', 'bar')",
            [],
        ).unwrap();
        for seq in 1..=5 {
            other.conn().execute(
                "INSERT INTO breakpoint_hits (session_id, location_key, hit_seq, ts)
                 VALUES ((SELECT id FROM sessions LIMIT 1), 'bar', ?1, datetime('now'))",
                params![seq],
            ).unwrap();
        }

        let other_path = sessions_dir(tmp.path()).join("b.db");
        other.save_to(&other_path).unwrap();

        let ctx = LifeCtx { cwd: tmp.path(), active: Some(&active) };
        let out = cmd_diff(&ctx, "b");
        assert!(out.contains("foo"), "{out}");
        assert!(out.contains("bar"), "{out}");
        // foo: hits_a=3, hits_b=0 (δ=+3). bar: hits_a=0, hits_b=5 (δ=-5).
        assert!(out.contains("+3") || out.contains("+5"));
        assert!(out.contains("-5") || out.contains("-3"));
    }
}
