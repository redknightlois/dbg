//! Session-lifecycle commands: `sessions`, `save`, `prune`, `diff`.
//!
//! These operate on SessionDb files — the active one (save, diff
//! current) and the ones under `.dbg/sessions/` (sessions, prune,
//! diff against-other). Shape mirrors `crosstrack` — parse to an
//! enum, execute against a `RunCtx`-style input, return a string.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use dbg_cli::session_db::{PrunePolicy, SessionDb, SessionKind, prune, sessions_dir};
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
    /// `dbg diff <other>` — current session vs <other>. `dbg diff <a>
    /// <b>` — two saved sessions, no live session needed (regression
    /// hunts on saved profiles). For debug-kind sessions, FULL OUTER
    /// JOIN on breakpoint hits; for profile-kind, per-frame
    /// inclusive/exclusive ms delta.
    Diff { a: Option<String>, b: String },
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
            let toks: Vec<&str> = rest.split_whitespace().collect();
            match toks.len() {
                0 => {
                    return Some(super::Dispatched::Immediate(
                        "usage:\n  dbg diff <other>          (active vs other)\n  \
                         dbg diff <a> <b>          (two saved sessions)".into(),
                    ));
                }
                1 => Lifecycle::Diff { a: None, b: toks[0].to_string() },
                2 => Lifecycle::Diff {
                    a: Some(toks[0].to_string()),
                    b: toks[1].to_string(),
                },
                _ => {
                    return Some(super::Dispatched::Immediate(
                        format!("dbg diff takes at most two arguments, got {}", toks.len()),
                    ));
                }
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
        Lifecycle::Diff { a, b } => cmd_diff(ctx, a.as_deref(), b),
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
    // When the directory doesn't exist yet, still synthesize a row
    // for the live session — otherwise `dbg sessions` during an
    // active session before the first `dbg save` returns "no saved
    // sessions" and hides the running session entirely.
    if !dir.exists() && ctx.active.is_none() {
        return "no saved sessions (nothing under .dbg/sessions/)".into();
    }
    if group_only && ctx.active.is_none() {
        // Without a live session we have no group key to match on —
        // falling through would silently drop the filter and list
        // every saved DB, which is indistinguishable from plain
        // `dbg sessions` and misleads the caller.
        return "no active session — cannot filter by group (run `dbg sessions` for the full list)".into();
    }
    let group_key = if group_only {
        ctx.active.and_then(|db| db.meta("session_group_key").ok().flatten())
    } else {
        None
    };

    let mut rows: Vec<SessionListing> = Vec::new();
    if dir.exists() {
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
    }

    let active_label: Option<String> = ctx.active.map(|db| db.label().to_string());
    let active_id: Option<String> = ctx.active.map(|db| db.session_id().to_string());

    // The live session's DB isn't in .dbg/sessions/ until `dbg save`
    // (or the daemon's exit handler) persists it. Synthesize a row
    // for it so `dbg sessions` during a live session shows it,
    // marked `*`, instead of omitting it entirely. Dedupe on
    // session_id (stable), not label (which a later `save --label`
    // can change out from under us and break the `*` marker).
    if let (Some(db), Some(id)) = (ctx.active, active_id.as_deref()) {
        if !rows.iter().any(|r| r.session_id == id) {
            rows.push(SessionListing {
                label: db.label().to_string(),
                session_id: id.to_string(),
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

    // Sort newest first: smaller age_secs first.
    rows.sort_by_key(|r| r.age_secs);
    let mut out = String::new();
    out.push_str("  label                                  kind     class           by    age\n");
    let mut marked_any = false;
    for r in &rows {
        let is_live = match &active_id {
            Some(id) if *id == r.session_id => true,
            _ => false,
        };
        let live_mark = if is_live {
            marked_any = true;
            "*"
        } else {
            " "
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
    if marked_any {
        out.push_str("\n* = currently live session\n");
    } else if active_label.is_some() {
        // A live session exists but its row didn't match any listed
        // entry — surface that state instead of a misleading legend.
        out.push_str(&format!(
            "\n(live session `{}` is running but not listed here)\n",
            active_label.unwrap()
        ));
    }
    out
}

struct SessionListing {
    label: String,
    session_id: String,
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
    let (label, session_id, kind, tc, created_by): (String, String, String, String, String) = conn
        .query_row(
            "SELECT label, id, kind, target_class, created_by FROM sessions LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
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
        session_id,
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
    // Both `dbg save` and `dbg save <label>` mean the same thing:
    // persist the current live session DB into .dbg/sessions/. The
    // only difference is the filename — `save` uses the session's
    // auto-label, `save foo` uses `foo.db`. This used to be a
    // "promote existing saved DB" verb, which nobody actually wanted
    // (promotion alone never wrote the DB to disk, so replay never
    // worked after a kill).
    let Some(db) = ctx.active else {
        return "no active session to save (start one with `dbg start`)".into();
    };
    let lbl = label.unwrap_or_else(|| db.label());
    let path = sessions_dir(ctx.cwd).join(format!("{lbl}.db"));
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return format!("[error creating {}: {e}]", parent.display());
        }
    }
    if let Err(e) = db.save_to(&path) {
        return format!("[error writing {}: {e}]", path.display());
    }
    // Also promote in-memory so subsequent `save` calls don't
    // surprise the user by re-marking as auto.
    let _ = db.promote_to_user();
    // Stamp created_by=user on the persisted DB too so a later
    // `prune` won't reap it.
    if let Ok(conn) = rusqlite::Connection::open(&path) {
        let _ = conn.execute(
            "UPDATE sessions SET created_by='user'",
            [],
        );
    }
    format!("saved `{lbl}` to {}", path.display())
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

fn cmd_diff(ctx: &LifeCtx<'_>, a_label: Option<&str>, b_label: &str) -> String {
    // Two-arg form: open both DBs read-only and diff them. Used for
    // regression hunts on saved profiles where neither side is the
    // active live session.
    if let Some(a_label) = a_label {
        let a_path = resolve_session_path(ctx.cwd, a_label);
        if !a_path.exists() {
            return format!("no session file at {}", a_path.display());
        }
        if let Some(msg) = check_schema_version(&a_path) {
            return msg;
        }
        let a_db = match SessionDb::open(&a_path) {
            Ok(db) => db,
            Err(e) => return format!("[error opening {}: {e}]", a_path.display()),
        };
        return diff_two_dbs(ctx, &a_db, a_label, b_label);
    }

    // One-arg form: active session vs other.
    let active = match ctx.active {
        Some(a) => a,
        None => {
            return "no active session — start one with `dbg start`, or use \
                    `dbg diff <a> <b>` to compare two saved sessions"
                .into();
        }
    };
    diff_two_dbs(ctx, active, active.label(), b_label)
}

fn check_schema_version(path: &Path) -> Option<String> {
    // We refuse to diff a DB the daemon can't read (matches the
    // no-migration policy). Profile-mode rehydration also depends on
    // a current schema, so this guard is shared with the replay path.
    let conn = rusqlite::Connection::open(path).ok()?;
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(-1);
    if v != dbg_cli::session_db::SCHEMA_VERSION {
        return Some(format!(
            "`{}` has schema_version={v}, expected {} — re-collect to diff",
            path.display(),
            dbg_cli::session_db::SCHEMA_VERSION,
        ));
    }
    None
}

fn diff_two_dbs(ctx: &LifeCtx<'_>, a_db: &SessionDb, a_label: &str, b_label: &str) -> String {
    let b_path = resolve_session_path(ctx.cwd, b_label);
    if !b_path.exists() {
        return format!("no session file at {}", b_path.display());
    }
    if let Some(msg) = check_schema_version(&b_path) {
        return msg;
    }

    // Profile-vs-profile diff: per-frame inclusive/exclusive ms delta
    // sorted by |ΔInclusive|. This is the regression-hunt path the
    // SQL hits-diff doesn't serve — profile sessions have no
    // breakpoint_hits, so the old `cmd_diff` returned "no symbols to
    // compare" and forced manual %×total_ms math.
    let a_is_profile = a_db.kind() == SessionKind::Profile;
    if a_is_profile {
        let b_db = match SessionDb::open(&b_path) {
            Ok(db) => db,
            Err(e) => return format!("[error opening {}: {e}]", b_path.display()),
        };
        if b_db.kind() != SessionKind::Profile {
            return format!(
                "diff {a_label} ↔ {b_label}  — kind mismatch (a=profile, b={:?}): \
                 cannot diff profile against debug",
                b_db.kind(),
            );
        }
        return profile_diff(a_db, &b_db, a_label, b_label);
    }

    diff_hits(a_db, &b_path, a_label, b_label)
}

/// Per-frame inclusive/exclusive ms delta between two profile
/// sessions. Frames present in only one side appear with 0.0 on the
/// missing side (the "appeared/disappeared" case is itself signal).
/// Output is capped at 40 rows by default.
fn profile_diff(a_db: &SessionDb, b_db: &SessionDb, a_label: &str, b_label: &str) -> String {
    use std::collections::HashMap;
    let a = match (load_profile(a_db), load_profile(b_db)) {
        (Some(a), Some(b)) => (a, b),
        (None, _) => return format!(
            "session `{a_label}` has no persisted profile source \
             (collected before replay support landed; re-collect to enable diff)"
        ),
        (_, None) => return format!(
            "session `{b_label}` has no persisted profile source \
             (collected before replay support landed; re-collect to enable diff)"
        ),
    };
    let (a, b) = a;
    let total_a = a.total_ms();
    let total_b = b.total_ms();

    let mut combined: HashMap<String, [f64; 4]> = HashMap::new();
    for (name, inc, exc) in a.frame_metrics() {
        let entry = combined.entry(name).or_default();
        entry[0] = inc;
        entry[1] = exc;
    }
    for (name, inc, exc) in b.frame_metrics() {
        let entry = combined.entry(name).or_default();
        entry[2] = inc;
        entry[3] = exc;
    }

    let mut rows: Vec<(String, f64, f64, f64, f64)> = combined
        .into_iter()
        .map(|(name, [ia, ea, ib, eb])| (name, ia, ea, ib, eb))
        .collect();

    if rows.is_empty() {
        return format!("diff {a_label} ↔ {b_label}  — both profiles are empty");
    }

    rows.sort_by(|x, y| {
        let dx = (x.3 - x.1).abs();
        let dy = (y.3 - y.1).abs();
        dy.partial_cmp(&dx).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut out = format!(
        "diff {a_label} ↔ {b_label}  (totals: a={total_a:.1}ms, b={total_b:.1}ms, \
         Δ={:+.1}ms)\n",
        total_b - total_a,
    );
    out.push_str(&format!(
        "{:<58}  {:>10} {:>10} {:>10}   {:>10} {:>10} {:>10}\n",
        "Function", "Inc_A", "Inc_B", "ΔInc", "Exc_A", "Exc_B", "ΔExc",
    ));
    for (name, ia, ea, ib, eb) in rows.iter().take(40) {
        let dinc = ib - ia;
        let dexc = eb - ea;
        out.push_str(&format!(
            "{:<58}  {:>9.1} {:>9.1} {:>+9.1}   {:>9.1} {:>9.1} {:>+9.1}\n",
            truncate(name, 58),
            ia, ib, dinc,
            ea, eb, dexc,
        ));
    }
    if rows.len() > 40 {
        out.push_str(&format!(
            "… {} more rows (use `dbg replay <session>` for full per-session views)\n",
            rows.len() - 40,
        ));
    }
    out
}

fn load_profile(db: &SessionDb) -> Option<crate::profile::ProfileData> {
    let content = db.meta("profile_raw").ok().flatten()?;
    let ext = db.meta("profile_raw_ext").ok().flatten();
    crate::profile::ProfileData::load_str(&content, ext.as_deref()).ok()
}

/// Hits-mode diff: FULL OUTER JOIN of `a_db` against the DB at
/// `b_path` on (lang, fqn) with breakpoint-hit counts. Schema mirrors
/// the original `cmd_diff` body — only the entry shape changed.
fn diff_hits(a_db: &SessionDb, b_path: &Path, a_label: &str, b_label: &str) -> String {
    let attach_sql = "ATTACH DATABASE ? AS other_db";
    if let Err(e) = a_db
        .conn()
        .execute(attach_sql, params![b_path.to_string_lossy().as_ref()])
    {
        return format!("[error attaching {}: {e}]", b_path.display());
    }
    let _detach_guard = DetachGuard(a_db);

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

    let rows: Result<Vec<(String, String, i64, i64)>, rusqlite::Error> = a_db
        .conn()
        .prepare(sql)
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .and_then(|it| it.collect())
        });
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return format!("[error running diff: {e}]"),
    };

    if rows.is_empty() {
        return format!("diff {a_label} ↔ {b_label}  — no symbols to compare");
    }

    let mut out = format!("diff {a_label} ↔ {b_label}\n");
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
    fn sessions_sorted_newest_first() {
        // Regression: `cmd_sessions` used to apply two sort calls back
        // to back — the first one commented "newest first" actually
        // sorted oldest-first, and a trailing `sort_by_key(age_secs)`
        // overwrote it. A refactor that removes the trailing sort must
        // not silently flip the displayed order.
        let tmp = TempDir::new().unwrap();
        let older = mk_db(&tmp, "older");
        older
            .save_to(&sessions_dir(tmp.path()).join("older.db"))
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let newer = mk_db(&tmp, "newer");
        newer
            .save_to(&sessions_dir(tmp.path()).join("newer.db"))
            .unwrap();

        let ctx = LifeCtx { cwd: tmp.path(), active: None };
        let out = cmd_sessions(&ctx, false);
        let newer_pos = out.find("newer").expect("newer missing");
        let older_pos = out.find("older").expect("older missing");
        assert!(
            newer_pos < older_pos,
            "expected newer listed before older:\n{out}"
        );
    }

    #[test]
    fn sessions_group_without_active_errors() {
        // Regression: `dbg sessions --group` with no active session
        // used to silently become a no-op filter and return *all*
        // saved sessions, which is what `dbg sessions` (no flag)
        // already does and hides the fact that grouping is impossible
        // without a live session to read the group key from.
        let tmp = TempDir::new().unwrap();
        // Save one DB so the empty-dir early return can't mask the bug.
        let db = mk_db(&tmp, "peer");
        db.save_to(&sessions_dir(tmp.path()).join("peer.db"))
            .unwrap();
        let ctx = LifeCtx { cwd: tmp.path(), active: None };
        let out = cmd_sessions(&ctx, true);
        assert!(
            !out.contains("peer"),
            "--group leaked unrelated session when no active session exists:\n{out}"
        );
        assert!(
            out.to_lowercase().contains("no active session")
                || out.to_lowercase().contains("no peers"),
            "expected a clear no-active / no-peers message, got:\n{out}"
        );
    }

    #[test]
    fn save_writes_active_session_to_disk() {
        // Regression: `dbg save` used to only flip created_by=user in
        // memory and never write the DB, so `dbg replay` after a kill
        // found nothing.
        let tmp = TempDir::new().unwrap();
        let db = mk_db(&tmp, "active");
        let ctx = LifeCtx { cwd: tmp.path(), active: Some(&db) };
        let out = cmd_save(&ctx, None);
        assert!(out.contains("saved"), "{out}");
        let expected = sessions_dir(tmp.path()).join("active.db");
        assert!(expected.exists(), "missing {}", expected.display());
        let conn = rusqlite::Connection::open(&expected).unwrap();
        let cb: String = conn
            .query_row("SELECT created_by FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cb, "user");
    }

    #[test]
    fn save_writes_labeled_copy() {
        // Regression: `dbg save mylabel` looked up an existing DB
        // instead of copying the live session. Now it always
        // produces `.dbg/sessions/mylabel.db`.
        let tmp = TempDir::new().unwrap();
        let db = mk_db(&tmp, "active");
        let ctx = LifeCtx { cwd: tmp.path(), active: Some(&db) };
        let out = cmd_save(&ctx, Some("mybug"));
        assert!(out.contains("saved `mybug`"), "{out}");
        let expected = sessions_dir(tmp.path()).join("mybug.db");
        assert!(expected.exists(), "missing {}", expected.display());
    }

    #[test]
    fn save_without_active_session_errors() {
        let tmp = TempDir::new().unwrap();
        let ctx = LifeCtx { cwd: tmp.path(), active: None };
        let out = cmd_save(&ctx, Some("anything"));
        assert!(out.contains("no active session"), "{out}");
    }

    #[test]
    fn sessions_shows_live_when_sessions_dir_missing() {
        // Regression: `dbg sessions` with an active session but no
        // .dbg/sessions/ directory on disk returned "no saved
        // sessions", hiding the running session entirely.
        let tmp = TempDir::new().unwrap();
        let db = mk_db(&tmp, "live");
        let ctx = LifeCtx { cwd: tmp.path(), active: Some(&db) };
        // Don't create .dbg/sessions/ — the synthesize-live branch
        // should still fire.
        assert!(!sessions_dir(tmp.path()).exists());
        let out = cmd_sessions(&ctx, false);
        assert!(out.contains("live"), "missing live entry:\n{out}");
        assert!(out.contains("* = currently live session"), "missing * marker:\n{out}");
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
        let out = cmd_diff(&ctx, None, "anything");
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
        let out = cmd_diff(&ctx, None, other_path.to_string_lossy().as_ref());
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
        let out = cmd_diff(&ctx, None, "other");
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
        let out = cmd_diff(&ctx, None, "b");
        assert!(out.contains("foo"), "{out}");
        assert!(out.contains("bar"), "{out}");
        // foo: hits_a=3, hits_b=0 (δ=+3). bar: hits_a=0, hits_b=5 (δ=-5).
        assert!(out.contains("+3") || out.contains("+5"));
        assert!(out.contains("-5") || out.contains("-3"));
    }

    fn mk_profile_db(tmp: &TempDir, label: &str, speedscope: &str) -> SessionDb {
        use dbg_cli::session_db::CreateOptions;
        let db = SessionDb::create(CreateOptions {
            kind: SessionKind::Profile,
            target: "./app",
            target_class: dbg_cli::session_db::TargetClass::NativeCpu,
            cwd: tmp.path(),
            db_path: None,
            label: Some(label.into()),
            target_hash: Some("h".into()),
        })
        .unwrap();
        db.set_meta("profile_raw", speedscope).unwrap();
        db.save_to(&sessions_dir(tmp.path()).join(format!("{label}.db"))).unwrap();
        db
    }

    /// `slow` runs in 4 ms in profile A, 10 ms in profile B → +6 ms
    /// inclusive delta. `fast` runs 1 ms in both → 0 delta. The diff
    /// must surface `slow` first (sorted by |Δinc|) with the +6.0 ms
    /// regression marker, and must work without any active session.
    #[test]
    fn profile_diff_two_saved_sessions_surfaces_regression() {
        let tmp = TempDir::new().unwrap();
        let prof_a = r#"{"shared":{"frames":[{"name":"slow"},{"name":"fast"}]},
            "profiles":[{"events":[
                {"type":"O","at":0.0,"frame":0},{"type":"C","at":4.0,"frame":0},
                {"type":"O","at":4.0,"frame":1},{"type":"C","at":5.0,"frame":1}
            ]}]}"#;
        let prof_b = r#"{"shared":{"frames":[{"name":"slow"},{"name":"fast"}]},
            "profiles":[{"events":[
                {"type":"O","at":0.0,"frame":0},{"type":"C","at":10.0,"frame":0},
                {"type":"O","at":10.0,"frame":1},{"type":"C","at":11.0,"frame":1}
            ]}]}"#;
        mk_profile_db(&tmp, "a", prof_a);
        mk_profile_db(&tmp, "b", prof_b);

        let ctx = LifeCtx { cwd: tmp.path(), active: None };
        let out = cmd_diff(&ctx, Some("a"), "b");

        assert!(out.contains("slow"), "diff missing `slow`:\n{out}");
        assert!(out.contains("fast"), "diff missing `fast`:\n{out}");
        // `slow`: ΔInc = 10 - 4 = +6.0 ms.
        assert!(out.contains("+6.0"), "expected ΔInc +6.0 in:\n{out}");
        // `slow` has the larger |Δ| so it should appear before `fast`
        // in the sort.
        let slow_pos = out.find("slow").unwrap();
        let fast_pos = out.find("fast").unwrap();
        assert!(slow_pos < fast_pos, "rows not sorted by |ΔInc|:\n{out}");
    }

    #[test]
    fn profile_diff_reports_when_one_side_lacks_persisted_source() {
        // Old profile sessions captured before persistence landed have
        // no `profile_raw` meta. The diff must fail loudly with a
        // re-collect hint, not panic or silently produce empty output.
        let tmp = TempDir::new().unwrap();
        let prof_b = r#"{"shared":{"frames":[{"name":"x"}]},
            "profiles":[{"events":[
                {"type":"O","at":0.0,"frame":0},{"type":"C","at":1.0,"frame":0}
            ]}]}"#;

        // a: profile-kind but no profile_raw meta.
        use dbg_cli::session_db::CreateOptions;
        let a = SessionDb::create(CreateOptions {
            kind: SessionKind::Profile,
            target: "./app",
            target_class: dbg_cli::session_db::TargetClass::NativeCpu,
            cwd: tmp.path(),
            db_path: None,
            label: Some("a".into()),
            target_hash: Some("h".into()),
        })
        .unwrap();
        a.save_to(&sessions_dir(tmp.path()).join("a.db")).unwrap();

        mk_profile_db(&tmp, "b", prof_b);

        let ctx = LifeCtx { cwd: tmp.path(), active: None };
        let out = cmd_diff(&ctx, Some("a"), "b");
        assert!(
            out.contains("re-collect") && out.contains("a"),
            "expected re-collect hint mentioning `a`:\n{out}"
        );
    }

    #[test]
    fn diff_two_arg_form_parses() {
        match try_dispatch("diff foo bar").unwrap() {
            super::super::Dispatched::Lifecycle(Lifecycle::Diff { a, b }) => {
                assert_eq!(a.as_deref(), Some("foo"));
                assert_eq!(b, "bar");
            }
            _ => panic!("expected two-arg Diff"),
        }
    }
}
