//! Cross-track commands — operate on the SessionDb, not on a live
//! debugger PTY. These implement the "what do I know about this
//! symbol / location?" layer of the plan, joining debug-track hits
//! with profile-track samples and on-demand disasm/source rows.
//!
//! Pure input → output: every function here takes a `&SessionDb` and
//! the parsed arguments, returning a formatted string. The daemon
//! does the plumbing; these functions stay testable with a tempdir DB.

use std::fs;
use std::path::Path;

use anyhow::Result;
use dbg_cli::session_db::{
    CollectCtx, CollectTrigger, LiveDebugger, OnDemandCollector, SessionDb, TargetClass,
    collectors::disasm::{GoDisassCollector, JitDasmCollector, LldbDisassembleCollector},
    persist_disasm,
};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

/// Parsed cross-track command. The dispatcher builds one of these;
/// `run` executes it against a SessionDb and returns the formatted
/// report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    Hits {
        loc: String,
        /// When set, aggregate rows by this locals field instead of
        /// listing them. `--group-by foo` shows count per distinct
        /// value of `foo`. `--count-by foo --top N` is the same with
        /// a ranked top-N truncation.
        group_by: Option<String>,
        top: Option<usize>,
    },
    HitDiff { loc: String, a: u32, b: u32 },
    HitTrend { loc: String, field: String },
    Disasm { symbol: Option<String>, refresh: bool },
    DisasmDiff { a: String, b: String },
    Source { symbol: String, radius: u32 },
    Cross { symbol: String },
    AtHitDisasm,
}

impl Query {
    /// Canonical-op name for the `commands.canonical_op` log column.
    pub fn canonical_op(&self) -> &'static str {
        match self {
            Query::Hits { .. } => "hits",
            Query::HitDiff { .. } => "hit-diff",
            Query::HitTrend { .. } => "hit-trend",
            Query::Disasm { .. } => "disasm",
            Query::DisasmDiff { .. } => "disasm-diff",
            Query::Source { .. } => "source",
            Query::Cross { .. } => "cross",
            Query::AtHitDisasm => "at-hit",
        }
    }
}

/// Parse `input` as a cross-track command. Returns `Some(Dispatched)`
/// when the verb is a crosstrack verb; returns `None` so callers can
/// defer to the debug dispatcher or fall through.
pub fn try_dispatch(input: &str) -> Option<super::Dispatched> {
    let input = input.trim();
    let (verb, rest) = match input.find(|c: char| c.is_ascii_whitespace()) {
        Some(i) => (&input[..i], input[i..].trim_start()),
        None => (input, ""),
    };
    let q = match verb {
        "hits" => {
            if rest.is_empty() {
                return Some(super::Dispatched::Immediate(
                    "usage: dbg hits <loc> [--group-by FIELD] [--count-by FIELD --top N]\n  \
                     <loc> is file:line (e.g. broken.py:26), not a function name".into(),
                ));
            }
            let mut loc: Option<String> = None;
            let mut group_by: Option<String> = None;
            let mut top: Option<usize> = None;
            let mut toks = rest.split_whitespace().peekable();
            while let Some(t) = toks.next() {
                match t {
                    "--group-by" | "--count-by" => {
                        if let Some(v) = toks.next() {
                            group_by = Some(v.to_string());
                        } else {
                            return Some(super::Dispatched::Immediate(
                                format!("{t} needs a field name").into(),
                            ));
                        }
                    }
                    "--top" => {
                        if let Some(v) = toks.next() {
                            match v.parse::<usize>() {
                                Ok(n) => top = Some(n),
                                Err(_) => {
                                    return Some(super::Dispatched::Immediate(
                                        format!("--top needs a number, got `{v}`"),
                                    ));
                                }
                            }
                        }
                    }
                    "--help" | "-h" => {
                        return Some(super::Dispatched::Immediate(
                            "usage: dbg hits <loc> [--group-by FIELD] [--count-by FIELD --top N]\n\
                             see `dbg help hits` for details".into(),
                        ));
                    }
                    _ => {
                        if t.starts_with("--") {
                            return Some(super::Dispatched::Immediate(
                                format!("unknown flag `{t}` — supported: --group-by, --count-by, --top"),
                            ));
                        }
                        if loc.is_none() {
                            loc = Some(t.to_string());
                        }
                    }
                }
            }
            let loc = match loc {
                Some(l) => l,
                None => return Some(super::Dispatched::Immediate(
                    "usage: dbg hits <loc> [--group-by FIELD] [--count-by FIELD --top N]\n  \
                     <loc> is file:line (e.g. broken.py:26), not a function name".into(),
                )),
            };
            if top.is_some() && group_by.is_none() {
                return Some(super::Dispatched::Immediate(
                    "--top only applies with --group-by / --count-by\n  \
                     example: dbg hits broken.py:26 --group-by page --top 5".into(),
                ));
            }
            Query::Hits { loc, group_by, top }
        }
        "hit-diff" => {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() != 3 {
                return Some(super::Dispatched::Immediate(
                    "usage: dbg hit-diff <loc> <seq_a> <seq_b>\n  \
                     <loc> is file:line (e.g. broken.py:26), not a function name\n  \
                     example: dbg hit-diff broken.py:26 1 3".into(),
                ));
            }
            match (parts[1].parse::<u32>(), parts[2].parse::<u32>()) {
                (Ok(a), Ok(b)) => Query::HitDiff { loc: parts[0].into(), a, b },
                _ => {
                    return Some(super::Dispatched::Immediate(
                        "hit-diff needs numeric seq_a and seq_b".into(),
                    ));
                }
            }
        }
        "hit-trend" => {
            let parts: Vec<&str> = rest.splitn(2, char::is_whitespace).collect();
            if parts.len() != 2 {
                return Some(super::Dispatched::Immediate(
                    "usage: dbg hit-trend <loc> <field>\n  \
                     <loc> is file:line (e.g. broken.py:26), not a function name\n  \
                     <field> is a locals name, optionally dotted (e.g. self.page)\n  \
                     example: dbg hit-trend broken.py:26 start".into(),
                ));
            }
            Query::HitTrend {
                loc: parts[0].into(),
                field: parts[1].into(),
            }
        }
        "disasm" => {
            let (symbol, refresh) = parse_disasm_args(rest);
            Query::Disasm { symbol, refresh }
        }
        "disasm-diff" => {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() != 2 {
                return Some(super::Dispatched::Immediate(
                    "usage: dbg disasm-diff <symbol_a> <symbol_b>".into(),
                ));
            }
            Query::DisasmDiff {
                a: parts[0].into(),
                b: parts[1].into(),
            }
        }
        "source" => {
            if rest.is_empty() {
                return Some(super::Dispatched::Immediate(
                    "usage: dbg source <symbol> [radius=5]".into(),
                ));
            }
            let parts: Vec<&str> = rest.split_whitespace().collect();
            let radius = parts
                .get(1)
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(5);
            Query::Source {
                symbol: parts[0].into(),
                radius,
            }
        }
        "cross" => {
            if rest.is_empty() {
                return Some(super::Dispatched::Immediate(
                    "usage: dbg cross <symbol>".into(),
                ));
            }
            Query::Cross {
                symbol: rest.to_string(),
            }
        }
        "at-hit" => {
            let sub = rest.split_whitespace().next().unwrap_or("");
            match sub {
                "disasm" => Query::AtHitDisasm,
                "" => {
                    return Some(super::Dispatched::Immediate(
                        "usage: dbg at-hit disasm".into(),
                    ));
                }
                other => {
                    return Some(super::Dispatched::Immediate(format!(
                        "unknown at-hit subcommand `{other}` — supported: disasm"
                    )));
                }
            }
        }
        _ => return None,
    };
    Some(super::Dispatched::Query(q))
}

fn parse_disasm_args(rest: &str) -> (Option<String>, bool) {
    let mut symbol: Option<String> = None;
    let mut refresh = false;
    for tok in rest.split_whitespace() {
        match tok {
            "--refresh" | "-r" => refresh = true,
            _ => {
                if symbol.is_none() {
                    symbol = Some(tok.to_string());
                }
            }
        }
    }
    (symbol, refresh)
}

/// Inputs a cross-track query needs beyond the DB.
pub struct RunCtx<'a> {
    pub target: &'a str,
    pub target_class: TargetClass,
    pub cwd: &'a Path,
    /// `None` in profile-only contexts; `Some` when a live debug
    /// session is attached (unlocks `at-hit` and lets the disasm
    /// collector reuse the existing PTY).
    pub live: Option<&'a dyn LiveDebugger>,
}

/// Dispatch one `Query` against the provided DB.
pub fn run(q: &Query, db: &SessionDb, ctx: &RunCtx<'_>) -> String {
    match q {
        Query::Hits { loc, group_by, top } => {
            if let Some(field) = group_by {
                cmd_hits_grouped(db, loc, field, *top)
            } else {
                cmd_hits(db, loc)
            }
        }
        Query::HitDiff { loc, a, b } => cmd_hit_diff(db, loc, *a, *b),
        Query::HitTrend { loc, field } => cmd_hit_trend(db, loc, field),
        Query::Disasm { symbol, refresh } => {
            cmd_disasm(db, ctx, symbol.as_deref(), *refresh)
        }
        Query::DisasmDiff { a, b } => cmd_disasm_diff(db, a, b),
        Query::Source { symbol, radius } => cmd_source(db, symbol, *radius),
        Query::Cross { symbol } => cmd_cross(db, symbol),
        Query::AtHitDisasm => cmd_at_hit_disasm(db, ctx),
    }
}

/// The `<stem>:<line>` prefix — strips directory AND extension so
/// `/a/b/Algos.java:17` → `Algos:17`. Matches jdb's `Algos.fibonacci:17`
/// via `LIKE 'Algos:' || '%'` (prefix match).
fn stem_line_key(loc: &str) -> String {
    let (file, line) = match loc.rsplit_once(':') {
        Some(x) => x,
        None => return loc.to_string(),
    };
    let base = match file.rsplit_once('/') {
        Some((_, b)) => b,
        None => file,
    };
    let stem = match base.rsplit_once('.') {
        Some((s, _)) => s,
        None => base,
    };
    format!("{stem}:{line}")
}

/// The `<basename>:<line>` suffix of a `file:line` key. When the agent
/// queries `/a/b/main.go:22` but the debugger stored `./main.go:22` (or
/// `main.go:22`), we accept either by matching on this suffix.
fn basename_line_key(loc: &str) -> String {
    let (file, line) = match loc.rsplit_once(':') {
        Some(x) => x,
        None => return loc.to_string(),
    };
    let base = match file.rsplit_once('/') {
        Some((_, b)) => b,
        None => file,
    };
    format!("{base}:{line}")
}

// ============================================================
// dbg hits <loc>
// ============================================================

fn cmd_hits(db: &SessionDb, loc: &str) -> String {
    // Query by exact match first, then fall back to basename:line so
    // `/abs/path/to/main.go:22` matches the `./main.go:22` form delve
    // stores, the `src/main.rs:42` form lldb stores, and so on.
    // Also try stem:line (strip file extension) so `Algos.java:17`
    // matches jdb's `Algos:17` or `Algos.fibonacci:17`.
    let (exact, tail, stem_tail) = (
        loc.to_string(),
        basename_line_key(loc),
        stem_line_key(loc),
    );
    let mut stmt = match db.conn().prepare(
        "SELECT hit_seq, thread, ts, locals_json
         FROM breakpoint_hits
         WHERE location_key = ?1
            OR location_key LIKE '%' || ?2
            OR location_key LIKE ?3 || '%'
         ORDER BY hit_seq ASC",
    ) {
        Ok(s) => s,
        Err(e) => return format!("[error: {e}]"),
    };
    let rows = stmt
        .query_map(params![exact, tail, stem_tail], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        });
    let rows = match rows {
        Ok(it) => it.collect::<Result<Vec<_>, _>>().unwrap_or_default(),
        Err(e) => return format!("[error: {e}]"),
    };
    if rows.is_empty() {
        return no_hits_message(db, loc, "hits");
    }
    let mut out = String::new();
    out.push_str(&format!("{loc} — {} hit(s)\n", rows.len()));
    out.push_str("  seq  thread  ts                    locals summary\n");
    for (seq, thread, ts, locals) in rows {
        let summary = locals
            .as_deref()
            .and_then(|s| locals_summary(s))
            .unwrap_or_else(|| "(none)".into());
        out.push_str(&format!(
            "  #{seq:<3} {th:<6}  {ts:<20}  {summary}\n",
            th = thread.unwrap_or_else(|| "-".into()),
        ));
    }
    out
}

/// Aggregate hits by a locals field. Groups rows whose parsed
/// `locals_json[field].value` matches; outputs `value -> count` sorted
/// descending. With `top=Some(n)`, truncates to the n most-frequent.
fn cmd_hits_grouped(db: &SessionDb, loc: &str, field: &str, top: Option<usize>) -> String {
    let tail = basename_line_key(loc);
    let stem_tail = stem_line_key(loc);
    let mut stmt = match db.conn().prepare(
        "SELECT locals_json FROM breakpoint_hits
         WHERE location_key = ?1
            OR location_key LIKE '%' || ?2
            OR location_key LIKE ?3 || '%'",
    ) {
        Ok(s) => s,
        Err(e) => return format!("[error: {e}]"),
    };
    let rows: Vec<Option<String>> = stmt
        .query_map(params![loc, tail, stem_tail], |r| r.get::<_, Option<String>>(0))
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default();
    if rows.is_empty() {
        return no_hits_message(db, loc, "hits --group-by");
    }

    let total = rows.len();
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut with_locals = 0;
    let mut matched = 0;
    let mut repr_hint: Option<String> = None;
    for locals in rows.into_iter().flatten() {
        with_locals += 1;
        let Ok(v) = serde_json::from_str::<Value>(&locals) else { continue };
        if let Some(val) = lookup_field(&v, field) {
            matched += 1;
            *counts.entry(val).or_insert(0) += 1;
        } else if repr_hint.is_none() {
            // Bug 3 fix: check whether the miss is because pdb stored
            // the parent object as a repr string, not a traversable dict.
            repr_hint = repr_traverse_hint(&v, field);
        }
    }
    if counts.is_empty() {
        if with_locals == 0 {
            return format!(
                "no locals captured at {loc} — run `dbg locals` at a hit to populate, \
                 or enable auto-capture for the backend"
            );
        }
        // Bug 3: emit repr-traverse hint when applicable.
        if let Some(hint) = repr_hint {
            return hint;
        }
        // Enumerate captured field names to help the agent pick a real one.
        let names = collect_captured_names(db, loc);
        return if names.is_empty() {
            format!("field `{field}` not present in any captured locals at {loc}")
        } else {
            format!(
                "field `{field}` not captured at {loc} (available: {})",
                names.join(", ")
            )
        };
    }
    let mut pairs: Vec<(String, usize)> = counts.into_iter().collect();
    // Sort by count descending, breaking ties numerically when every
    // key parses as a number (`0, 13, 100, 102` instead of the
    // lexicographic `0, 100, 102, 13`). Fall back to string order
    // for non-numeric fields.
    let all_numeric = pairs
        .iter()
        .all(|(v, _)| v.parse::<f64>().is_ok());
    pairs.sort_by(|a, b| {
        b.1.cmp(&a.1).then_with(|| {
            if all_numeric {
                let (na, nb) = (a.0.parse::<f64>().unwrap_or(0.0), b.0.parse::<f64>().unwrap_or(0.0));
                na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                a.0.cmp(&b.0)
            }
        })
    });
    if let Some(n) = top {
        pairs.truncate(n);
    }

    let mut out = format!(
        "{loc} — {total} hit(s), grouped by `{field}` ({matched} match)\n"
    );
    let max_val_len = pairs.iter().map(|(v, _)| v.chars().count()).max().unwrap_or(5);
    for (val, count) in &pairs {
        out.push_str(&format!(
            "  {val:<w$}  {count}\n",
            w = max_val_len.max(5)
        ));
    }
    out
}

/// Look up `field` in a locals object. Supports dotted paths
/// (`self.x`, `obj.nested.v`) and plain names. Value is the stringified
/// `"value"` sub-key when present, otherwise the node itself.
fn lookup_field(locals: &Value, field: &str) -> Option<String> {
    let mut cur = locals;
    for part in field.split('.') {
        cur = cur.get(part)?;
    }
    if let Some(val) = cur.get("value").and_then(|v| v.as_str()) {
        Some(val.to_string())
    } else if let Some(s) = cur.as_str() {
        Some(s.to_string())
    } else {
        Some(cur.to_string())
    }
}

/// Bug 3 fix: when a dotted path like `self.page` fails, detect whether
/// the first segment (`self`) exists but its value is a repr string
/// (pdb stores complex objects as `repr(obj)`, not as nested JSON).
/// Returns a targeted hint message, or `None` if this isn't the cause.
fn repr_traverse_hint(locals: &Value, field: &str) -> Option<String> {
    if !field.contains('.') {
        return None;
    }
    let (first, rest) = field.split_once('.')?;
    // Check whether the first segment exists and its captured value
    // looks like a Python repr string (starts with `<`, which is the
    // canonical marker for `<ClassName object at 0x…>` reprs).
    let parent = locals.get(first)?;
    let repr_val = parent.get("value").and_then(|v| v.as_str())?;
    if repr_val.starts_with('<') || !repr_val.starts_with('{') {
        Some(format!(
            "cannot traverse into `{field}`: pdb stores objects as repr strings \
             (`{first}` = `{repr_val}`). \
             Capture the field directly with `break … log {rest}` or add \
             `{rest} = self.{rest}` as a local variable instead.",
        ))
    } else {
        None
    }
}

/// Enumerate distinct `location_key` values in the DB — used to steer
/// users who passed a function name or a stale file:line toward a real
/// captured location. Bounded so the error stays readable.
fn available_locations(db: &SessionDb, limit: usize) -> Vec<String> {
    let Ok(mut stmt) = db.conn().prepare(
        "SELECT location_key, COUNT(*) AS n
         FROM breakpoint_hits
         GROUP BY location_key
         ORDER BY n DESC, location_key ASC",
    ) else {
        return Vec::new();
    };
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default();
    rows.into_iter().take(limit).collect()
}

/// Explain a miss on `loc` — distinguishes "you passed a function name,
/// not a file:line" from "no hits recorded yet at that file:line", and
/// lists what *is* captured so the user can fix the query.
fn no_hits_message(db: &SessionDb, loc: &str, verb: &str) -> String {
    let looks_like_symbol = !loc.contains(':');
    let available = available_locations(db, 8);
    let mut msg = if looks_like_symbol {
        format!(
            "no hits at `{loc}` — `{verb}` matches on file:line, not function names. \
             Set the breakpoint by symbol (e.g. `dbg break {loc}`) then query the \
             file:line it actually hit."
        )
    } else {
        format!("no hits at {loc}")
    };
    if !available.is_empty() {
        msg.push_str("\n  captured locations: ");
        msg.push_str(&available.join(", "));
    } else {
        msg.push_str("\n  (no breakpoint hits recorded yet — run the program under `dbg` first)");
    }
    msg
}

/// Collect the set of captured locals field names across all hits at
/// `loc`. Used to enumerate options in error messages.
fn collect_captured_names(db: &SessionDb, loc: &str) -> Vec<String> {
    let tail = basename_line_key(loc);
    let stem_tail = stem_line_key(loc);
    let Ok(mut stmt) = db.conn().prepare(
        "SELECT locals_json FROM breakpoint_hits
         WHERE (location_key = ?1
                OR location_key LIKE '%' || ?2
                OR location_key LIKE ?3 || '%')
           AND locals_json IS NOT NULL",
    ) else {
        return Vec::new();
    };
    let rows: Vec<String> = stmt
        .query_map(params![loc, tail, stem_tail], |r| r.get::<_, String>(0))
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default();
    let mut names: std::collections::BTreeSet<String> = Default::default();
    for s in rows {
        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&s) {
            for k in obj.keys() {
                names.insert(k.clone());
            }
        }
    }
    names.into_iter().collect()
}

fn locals_summary(locals_json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(locals_json).ok()?;
    let obj = v.as_object()?;
    if obj.is_empty() {
        return Some("(empty)".into());
    }
    let mut parts = Vec::new();
    for (k, v) in obj.iter().take(4) {
        let val = v
            .get("value")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .chars()
            .take(30)
            .collect::<String>();
        parts.push(format!("{k}={val}"));
    }
    if obj.len() > 4 {
        parts.push(format!("… +{} more", obj.len() - 4));
    }
    Some(parts.join(", "))
}

// ============================================================
// dbg hit-diff <loc> <a> <b>
// ============================================================

fn cmd_hit_diff(db: &SessionDb, loc: &str, a: u32, b: u32) -> String {
    let tail = basename_line_key(loc);
    let stem_tail = stem_line_key(loc);
    let fetch = |seq: u32| -> Option<(Option<String>, Option<String>)> {
        db.conn()
            .query_row(
                "SELECT locals_json, stack_json
                 FROM breakpoint_hits
                 WHERE (location_key = ?1 OR location_key LIKE '%' || ?2
                        OR location_key LIKE ?3 || '%')
                   AND hit_seq = ?4",
                params![loc, tail, stem_tail, seq as i64],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()
            .ok()
            .flatten()
    };
    let (la, _sa) = match fetch(a) {
        Some(x) => x,
        None => return format!("no hit #{a} at {loc}"),
    };
    let (lb, _sb) = match fetch(b) {
        Some(x) => x,
        None => return format!("no hit #{b} at {loc}"),
    };

    let va: Value = la
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Object(Default::default()));
    let vb: Value = lb
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Object(Default::default()));

    let mut out = format!("hit-diff {loc}  #{a} vs #{b}\n");
    let oa = va.as_object().cloned().unwrap_or_default();
    let ob = vb.as_object().cloned().unwrap_or_default();
    let mut keys: Vec<&String> = oa.keys().chain(ob.keys()).collect();
    keys.sort();
    keys.dedup();
    if keys.is_empty() {
        out.push_str("  (no locals captured on either hit)\n");
        return out;
    }
    out.push_str("  field            #a                    #b\n");
    for k in keys {
        let va = oa
            .get(k)
            .and_then(|v| v.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        let vb = ob
            .get(k)
            .and_then(|v| v.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        let mark = if va != vb { "≠" } else { " " };
        out.push_str(&format!(
            "  {mark} {k:<14}  {va:<20}  {vb:<20}\n"
        ));
    }
    out
}

// ============================================================
// dbg hit-trend <loc> <field>
// ============================================================

fn cmd_hit_trend(db: &SessionDb, loc: &str, field: &str) -> String {
    let tail = basename_line_key(loc);
    let stem_tail = stem_line_key(loc);
    let mut stmt = match db.conn().prepare(
        "SELECT hit_seq, locals_json FROM breakpoint_hits
         WHERE location_key = ?1
            OR location_key LIKE '%' || ?2
            OR location_key LIKE ?3 || '%'
         ORDER BY hit_seq ASC",
    ) {
        Ok(s) => s,
        Err(e) => return format!("[error: {e}]"),
    };
    let rows: Vec<(i64, Option<String>)> = stmt
        .query_map(params![loc, tail, stem_tail], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default();
    if rows.is_empty() {
        return no_hits_message(db, loc, "hit-trend");
    }

    let mut have_any_locals = false;
    // Bug 3 fix: keep first parsed locals that missed the field lookup,
    // so we can check for the repr-traverse case in the error path.
    let mut first_miss: Option<Value> = None;
    let mut values: Vec<(i64, String)> = Vec::new();
    for (seq, locals_opt) in rows {
        let Some(locals) = locals_opt else { continue };
        have_any_locals = true;
        let Ok(v) = serde_json::from_str::<Value>(&locals) else { continue };
        if let Some(raw) = lookup_field(&v, field) {
            values.push((seq, raw));
        } else if first_miss.is_none() {
            first_miss = Some(v);
        }
    }
    if values.is_empty() {
        if !have_any_locals {
            return format!(
                "no locals captured at {loc} — run `dbg locals` at a hit to populate, \
                 or enable auto-capture for the backend"
            );
        }
        // Bug 3: detect repr-traverse failure before falling back to
        // generic "not captured" message.
        if let Some(hint) = first_miss.as_ref().and_then(|v| repr_traverse_hint(v, field)) {
            return hint;
        }
        let names = collect_captured_names(db, loc);
        return if names.is_empty() {
            format!("field `{field}` not captured at {loc}")
        } else {
            // Suggest `self.<field>` only when the user didn't
            // already supply a dotted path — otherwise we'd emit
            // nonsense like `self.self.x`.
            let hint = if field.contains('.') {
                "dotted paths supported".to_string()
            } else {
                format!("dotted paths supported (e.g. self.{field})")
            };
            format!(
                "field `{field}` not captured at {loc} (available: {}). {hint}",
                names.join(", ")
            )
        };
    }

    let mut out = format!("hit-trend {loc} / {field}\n");
    let numeric: Vec<(i64, f64)> = values
        .iter()
        .filter_map(|(s, v)| v.parse::<f64>().ok().map(|f| (*s, f)))
        .collect();
    if numeric.len() == values.len() {
        // All numeric — render a sparkline alongside the table.
        out.push_str(&format!("  sparkline: {}\n", sparkline(&numeric)));
    }
    out.push_str("  seq   value\n");
    for (seq, v) in &values {
        out.push_str(&format!("  #{seq:<3}  {v}\n"));
    }
    out
}

fn sparkline(points: &[(i64, f64)]) -> String {
    if points.is_empty() {
        return String::new();
    }
    let bars = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let min = points.iter().map(|(_, v)| *v).fold(f64::INFINITY, f64::min);
    let max = points.iter().map(|(_, v)| *v).fold(f64::NEG_INFINITY, f64::max);
    let range = (max - min).max(1e-9);
    points
        .iter()
        .map(|(_, v)| {
            let ix = (((v - min) / range) * (bars.len() - 1) as f64).round() as usize;
            bars[ix.min(bars.len() - 1)]
        })
        .collect()
}

// ============================================================
// dbg disasm [<sym>]
// ============================================================

fn cmd_disasm(
    db: &SessionDb,
    ctx: &RunCtx<'_>,
    symbol: Option<&str>,
    refresh: bool,
) -> String {
    let sym = match symbol {
        Some(s) => s.to_string(),
        None => match resolve_current_symbol(db) {
            Some(s) => s,
            None => {
                return "no symbol given and no recent breakpoint hit to infer from — try `dbg disasm <symbol>`".into();
            }
        },
    };

    let collector: Box<dyn OnDemandCollector> = match ctx.target_class {
        TargetClass::ManagedDotnet => Box::new(JitDasmCollector),
        TargetClass::NativeCpu => {
            // Go vs C/Rust: prefer go-objdump when the target file
            // claims to be a Go binary (cheap heuristic: extension
            // unlikely, so rely on user explicitly passing `--tool=go`
            // in future). Default lldb for now.
            Box::new(LldbDisassembleCollector)
        }
        _ => {
            return format!(
                "disasm not implemented for target class `{}` yet",
                ctx.target_class
            );
        }
    };

    let collect_ctx = CollectCtx {
        target: ctx.target,
        target_class: ctx.target_class,
        symbol: &sym,
        refresh,
        trigger: CollectTrigger::Explicit,
        cwd: ctx.cwd,
    };

    let output = match collector.collect(&collect_ctx, ctx.live) {
        Ok(o) => o,
        Err(e) => return format!("[disasm {}: {e}]", collector.kind()),
    };
    if let Err(e) = persist_disasm(db, &collect_ctx, &output) {
        eprintln!("[dbg] warning: disasm persist failed: {e}");
    }

    let mut header = format!(
        "[via {tool}] disasm {sym}",
        tool = collector.kind(),
        sym = sym,
    );
    if let Some(tier) = output.tier.as_deref() {
        header.push_str(&format!(" ({tier})"));
    }
    if let Some(bytes) = output.code_bytes {
        header.push_str(&format!(" — {bytes} bytes"));
    }
    format!("{header}\n{}", output.asm_text)
}

/// Locate a canonical symbol from the most recent breakpoint hit —
/// used when `dbg disasm` is called without a symbol at a stop point.
fn resolve_current_symbol(db: &SessionDb) -> Option<String> {
    // Prefer a stack-captured frame symbol over the location key
    // because a frame symbol maps cleanly to a disasm target, while
    // `file:line` does not.
    let stack_json: Option<String> = db
        .conn()
        .query_row(
            "SELECT stack_json FROM breakpoint_hits
             WHERE stack_json IS NOT NULL
             ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    if let Some(s) = stack_json {
        if let Ok(v) = serde_json::from_str::<Value>(&s) {
            if let Some(sym) = v.get("frame_symbol").and_then(|s| s.as_str()) {
                return Some(sym.to_string());
            }
        }
    }
    None
}

// ============================================================
// dbg disasm-diff <sym_a> <sym_b>
// ============================================================

fn cmd_disasm_diff(db: &SessionDb, a: &str, b: &str) -> String {
    let fetch = |fqn: &str| -> Option<String> {
        db.conn()
            .query_row(
                "SELECT d.asm_text FROM disassembly d
                 JOIN symbols s ON s.id = d.symbol_id
                 WHERE s.fqn = ?1
                 ORDER BY d.id DESC LIMIT 1",
                params![fqn],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
    };
    let aa = fetch(a);
    let bb = fetch(b);
    let mut out = format!("disasm-diff  {a}  ↔  {b}\n");
    match (aa, bb) {
        (None, None) => {
            out.push_str("neither symbol has cached disassembly — run `dbg disasm` on each first");
        }
        (Some(_), None) => {
            out.push_str(&format!("only {a} has disassembly; run `dbg disasm {b}` first"));
        }
        (None, Some(_)) => {
            out.push_str(&format!("only {b} has disassembly; run `dbg disasm {a}` first"));
        }
        (Some(a_asm), Some(b_asm)) => {
            out.push_str(&side_by_side(&a_asm, &b_asm));
        }
    }
    out
}

fn side_by_side(a: &str, b: &str) -> String {
    let a_lines: Vec<&str> = a.lines().collect();
    let b_lines: Vec<&str> = b.lines().collect();
    let n = a_lines.len().max(b_lines.len());
    let mut out = String::new();
    for i in 0..n {
        let la = a_lines.get(i).copied().unwrap_or("");
        let lb = b_lines.get(i).copied().unwrap_or("");
        let mark = if la == lb { " " } else { "|" };
        out.push_str(&format!("{la:<50} {mark} {lb}\n"));
    }
    out
}

// ============================================================
// dbg source <sym>
// ============================================================

fn cmd_source(db: &SessionDb, symbol: &str, radius: u32) -> String {
    let row: Option<(Option<String>, Option<i64>)> = db
        .conn()
        .query_row(
            "SELECT file, line FROM symbols WHERE fqn = ?1 ORDER BY id DESC LIMIT 1",
            params![symbol],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten();
    let (file, line) = match row {
        Some((Some(f), Some(l))) => (f, l as u32),
        _ => {
            // Fall back to the most recent breakpoint hit at a
            // file:line location key matching this symbol somehow —
            // best-effort only.
            return format!("no source location known for {symbol} yet");
        }
    };
    let text = match fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) => return format!("source {symbol} ({file}): {e}"),
    };
    let target = line.saturating_sub(1) as usize;
    let start = target.saturating_sub(radius as usize);
    let end = (target + radius as usize + 1).min(text.lines().count());
    let mut out = format!("source {symbol} ({file}:{line})\n");
    for (i, l) in text.lines().enumerate().take(end).skip(start) {
        let marker = if i == target { "→" } else { " " };
        out.push_str(&format!("  {marker} {:>5}: {l}\n", i + 1));
    }
    out
}

// ============================================================
// dbg cross <sym>
// ============================================================

fn cmd_cross(db: &SessionDb, symbol: &str) -> String {
    let mut out = format!("cross {symbol}\n");

    // Symbol row
    if let Ok(Some((lang, file, line))) = db.conn().query_row(
        "SELECT lang, file, line FROM symbols WHERE fqn = ?1 ORDER BY id DESC LIMIT 1",
        params![symbol],
        |r| Ok::<_, rusqlite::Error>((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<i64>>(2)?,
        )),
    ).optional() {
        out.push_str(&format!("  lang={lang}"));
        if let Some(f) = file.as_deref() {
            out.push_str(&format!("  file={f}"));
        }
        if let Some(l) = line {
            out.push_str(&format!(":{l}"));
        }
        out.push('\n');
    }

    // Hits. `breakpoint_hits.location_key` is a `file:line` string
    // (what the debugger reported at the stop), not a symbol name. So
    // we resolve the symbol's file+line from `symbols` and match by
    // that. We accept an exact fqn match (`fqn = ?1`), a suffix match
    // (e.g. `Algos.fibonacci` ↔ `fibonacci`), and a case-insensitive
    // suffix — the agent often types the short name.
    //
    // For the file:line match we normalize to `basename:line` on both
    // sides so `/abs/path/foo.rb:17` stored by the debugger matches
    // the symbol row's `file=/other/path/foo.rb, line=17`.
    let hits: i64 = count_hits_for_symbol(db, symbol);
    out.push_str(&format!("  breakpoint hits: {hits}\n"));

    // Samples (profile track — likely empty in Phase 1 debug-only sessions)
    let samples: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM samples sa
             JOIN symbols s ON s.id = sa.symbol_id
             WHERE s.fqn = ?1",
            params![symbol],
            |r| r.get(0),
        )
        .unwrap_or(0);
    out.push_str(&format!("  profile samples: {samples}\n"));

    // JIT events
    let jits: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM jit_events je
             JOIN symbols s ON s.id = je.symbol_id
             WHERE s.fqn = ?1",
            params![symbol],
            |r| r.get(0),
        )
        .unwrap_or(0);
    out.push_str(&format!("  jit events:      {jits}\n"));

    // Disassembly
    let disasms: Vec<(String, Option<String>, Option<i64>)> = db
        .conn()
        .prepare(
            "SELECT d.source, d.tier, d.code_bytes
             FROM disassembly d
             JOIN symbols s ON s.id = d.symbol_id
             WHERE s.fqn = ?1
             ORDER BY d.id DESC",
        )
        .and_then(|mut s| {
            s.query_map(params![symbol], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    out.push_str(&format!("  disassembly:     {} row(s)\n", disasms.len()));
    for (src, tier, bytes) in disasms {
        let tier = tier.as_deref().unwrap_or("-");
        let bytes = bytes.map(|b| format!("{b} B")).unwrap_or_else(|| "?".into());
        out.push_str(&format!("    {src}  tier={tier}  size={bytes}\n"));
    }

    // Source snapshots
    let snaps: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM source_snapshots ss
             JOIN symbols s ON s.id = ss.symbol_id
             WHERE s.fqn = ?1",
            params![symbol],
            |r| r.get(0),
        )
        .unwrap_or(0);
    out.push_str(&format!("  source snapshots: {snaps}\n"));

    out
}

/// Count breakpoint_hits that plausibly correspond to the given symbol.
///
/// Strategy:
///   1. Collect every `(file, line)` in `symbols` whose `fqn` matches
///      the query — exact, as a suffix (`Algos.fibonacci` ends with
///      `.fibonacci`), or as an `::`-delimited suffix
///      (`algos::fibonacci` ends with `::fibonacci`). Many indexers
///      store the fully-qualified name; the agent usually types only
///      the short one.
///   2. For each `(file, line)`, count hits whose `location_key` ends
///      with `basename(file):line`. The debugger may store an absolute
///      path, a relative path, or just the basename — basename+line
///      uniquely identifies the stop line within the session.
fn count_hits_for_symbol(db: &SessionDb, symbol: &str) -> i64 {
    let conn = db.conn();
    let Ok(mut stmt) = conn.prepare(
        "SELECT file, line FROM symbols
         WHERE file IS NOT NULL AND line IS NOT NULL
           AND ( fqn = ?1
                 OR fqn LIKE '%.' || ?1
                 OR fqn LIKE '%::' || ?1
                 OR fqn LIKE '%/' || ?1 )",
    ) else {
        return 0;
    };
    let rows: Vec<(String, i64)> = stmt
        .query_map(params![symbol], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default();

    let mut total: i64 = 0;
    for (file, line) in rows {
        let base = match file.rsplit_once('/') {
            Some((_, b)) => b,
            None => &file,
        };
        let tail = format!("{base}:{line}");
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM breakpoint_hits
                 WHERE location_key = ?1
                    OR location_key LIKE '%/' || ?1
                    OR location_key LIKE '%' || ?1",
                params![tail],
                |r| r.get(0),
            )
            .unwrap_or(0);
        total += n;
    }
    total
}

// ============================================================
// dbg at-hit disasm
// ============================================================

fn cmd_at_hit_disasm(db: &SessionDb, ctx: &RunCtx<'_>) -> String {
    let Some(sym) = resolve_current_symbol(db) else {
        return "no recent breakpoint hit — `at-hit` requires the debugger to be stopped".into();
    };
    cmd_disasm(db, ctx, Some(&sym), false)
}

// Silence unused-import clippy when building with only some of the
// collectors in scope (all three stay referenced above but lints may
// false-positive across target classes).
#[allow(dead_code)]
fn _keep_types_linked() -> (
    LldbDisassembleCollector,
    JitDasmCollector,
    GoDisassCollector,
) {
    (LldbDisassembleCollector, JitDasmCollector, GoDisassCollector)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbg_cli::session_db::{CreateOptions, SessionKind};
    use rusqlite::params;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn db_and_ctx<'a>(tmp: &'a TempDir) -> (SessionDb, PathBuf) {
        let cwd = tmp.path().to_path_buf();
        let db = SessionDb::create(CreateOptions {
            kind: SessionKind::Debug,
            target: "./app",
            target_class: TargetClass::NativeCpu,
            cwd: &cwd,
            db_path: None,
            label: Some("t".into()),
            target_hash: Some("h".into()),
        })
        .unwrap();
        (db, cwd)
    }

    fn insert_hit(
        db: &SessionDb,
        loc: &str,
        seq: i64,
        locals_json: &str,
        stack_json: Option<&str>,
    ) {
        db.conn()
            .execute(
                "INSERT INTO breakpoint_hits
                    (session_id, location_key, hit_seq, thread, ts, locals_json, stack_json)
                 VALUES ((SELECT id FROM sessions LIMIT 1), ?1, ?2, '1',
                         datetime('now'), ?3, ?4)",
                params![loc, seq, locals_json, stack_json],
            )
            .unwrap();
    }

    // ---------- basename matching ----------

    #[test]
    fn basename_line_key_strips_dir() {
        assert_eq!(basename_line_key("/a/b/main.go:22"), "main.go:22");
        assert_eq!(basename_line_key("./main.go:22"),     "main.go:22");
        assert_eq!(basename_line_key("main.go:22"),       "main.go:22");
        assert_eq!(basename_line_key("foo"),              "foo");
    }

    #[test]
    fn hits_matches_on_basename_when_dir_differs() {
        let tmp = TempDir::new().unwrap();
        let (db, _) = db_and_ctx(&tmp);
        // Capture was stored with the debugger's relative form.
        insert_hit(&db, "./examples/go/main.go:22", 1, r#"{"a":{"value":"0"}}"#, None);
        insert_hit(&db, "./examples/go/main.go:22", 2, r#"{"a":{"value":"1"}}"#, None);
        // Agent queries the absolute form.
        let out = cmd_hits(&db, "/repo/examples/go/main.go:22");
        assert!(out.contains("2 hit(s)"), "{out}");
        assert!(out.contains("a=0"));
        assert!(out.contains("a=1"));
    }

    // ---------- hits ----------

    #[test]
    fn hits_none_when_empty() {
        let tmp = TempDir::new().unwrap();
        let (db, _) = db_and_ctx(&tmp);
        let out = cmd_hits(&db, "main.c:1");
        assert!(out.contains("no hits at main.c:1"), "{out}");
        assert!(
            out.contains("no breakpoint hits recorded yet"),
            "empty DB should surface the hint: {out}"
        );
    }

    #[test]
    fn hits_formats_locals_summary() {
        let tmp = TempDir::new().unwrap();
        let (db, _) = db_and_ctx(&tmp);
        insert_hit(
            &db,
            "main.c:42",
            1,
            r#"{"x":{"value":"42"},"y":{"value":"hello"}}"#,
            None,
        );
        insert_hit(
            &db,
            "main.c:42",
            2,
            r#"{"x":{"value":"43"},"y":{"value":"world"}}"#,
            None,
        );
        let out = cmd_hits(&db, "main.c:42");
        assert!(out.contains("2 hit(s)"));
        assert!(out.contains("#1"));
        assert!(out.contains("x=42"));
        assert!(out.contains("#2"));
        assert!(out.contains("x=43"));
    }

    // ---------- hit-diff ----------

    #[test]
    fn hit_diff_highlights_changed_fields() {
        let tmp = TempDir::new().unwrap();
        let (db, _) = db_and_ctx(&tmp);
        insert_hit(&db, "main.c:42", 1, r#"{"x":{"value":"1"},"y":{"value":"A"}}"#, None);
        insert_hit(&db, "main.c:42", 2, r#"{"x":{"value":"2"},"y":{"value":"A"}}"#, None);
        let out = cmd_hit_diff(&db, "main.c:42", 1, 2);
        assert!(out.contains("#1 vs #2"));
        // x changed — should have the ≠ marker
        let x_line = out.lines().find(|l| l.contains(" x ")).unwrap();
        assert!(x_line.starts_with("  ≠"), "{x_line}");
        // y unchanged — space marker
        let y_line = out.lines().find(|l| l.contains(" y ")).unwrap();
        assert!(!y_line.contains("≠"), "{y_line}");
    }

    #[test]
    fn hit_diff_missing_hit_reports_error() {
        let tmp = TempDir::new().unwrap();
        let (db, _) = db_and_ctx(&tmp);
        let out = cmd_hit_diff(&db, "main.c:42", 1, 2);
        assert!(out.contains("no hit #1"));
    }

    // ---------- hit-trend ----------

    #[test]
    fn hit_trend_renders_sparkline_for_numeric_series() {
        let tmp = TempDir::new().unwrap();
        let (db, _) = db_and_ctx(&tmp);
        for (i, v) in [1, 3, 2, 5, 4].iter().enumerate() {
            insert_hit(
                &db,
                "loop:1",
                (i + 1) as i64,
                &format!(r#"{{"i":{{"value":"{v}"}}}}"#),
                None,
            );
        }
        let out = cmd_hit_trend(&db, "loop:1", "i");
        assert!(out.contains("sparkline:"));
        assert!(out.contains("#1"));
        assert!(out.contains("#5"));
    }

    #[test]
    fn hit_trend_missing_field() {
        let tmp = TempDir::new().unwrap();
        let (db, _) = db_and_ctx(&tmp);
        insert_hit(&db, "loop:1", 1, r#"{"i":{"value":"1"}}"#, None);
        let out = cmd_hit_trend(&db, "loop:1", "other");
        assert!(out.contains("not captured"), "{out}");
        // Enumerates available field names so the agent can correct.
        assert!(out.contains("i"), "{out}");
    }

    // ---------- source ----------

    #[test]
    fn source_reads_file_around_line() {
        let tmp = TempDir::new().unwrap();
        let (db, cwd) = db_and_ctx(&tmp);
        let f = cwd.join("t.c");
        fs::write(&f, "int a = 1;\nint b = 2;\nint main(){ return 0; }\nint c = 3;\n").unwrap();
        db.conn()
            .execute(
                "INSERT INTO symbols (session_id, lang, fqn, file, line, raw)
                 VALUES ((SELECT id FROM sessions LIMIT 1), 'cpp', 'main',
                         ?1, 3, 'main')",
                params![f.to_string_lossy().as_ref()],
            )
            .unwrap();
        let out = cmd_source(&db, "main", 1);
        assert!(out.contains("int b = 2"));
        assert!(out.contains("int main"));
        assert!(out.contains("int c = 3"));
        assert!(out.contains("→"));
    }

    #[test]
    fn source_reports_missing_symbol() {
        let tmp = TempDir::new().unwrap();
        let (db, _) = db_and_ctx(&tmp);
        let out = cmd_source(&db, "unknown", 1);
        assert!(out.contains("no source location known"));
    }

    // ---------- cross ----------

    #[test]
    fn cross_aggregates_counts() {
        let tmp = TempDir::new().unwrap();
        let (db, _) = db_and_ctx(&tmp);
        db.conn()
            .execute(
                "INSERT INTO symbols (session_id, lang, fqn, file, line, raw)
                 VALUES ((SELECT id FROM sessions LIMIT 1), 'cpp', 'foo',
                         'main.c', 42, 'foo')",
                [],
            )
            .unwrap();
        insert_hit(&db, "main.c:42", 1, "{}", None);
        insert_hit(&db, "main.c:42", 2, "{}", None);
        let out = cmd_cross(&db, "foo");
        assert!(out.contains("cross foo"));
        assert!(out.contains("breakpoint hits: 2"), "{out}");
        assert!(out.contains("profile samples: 0"));
        assert!(out.contains("disassembly:     0 row(s)"));
        // Regression: the source-snapshots line used to render as
        // `source snapshots:0` with no space after the colon, breaking
        // the aligned layout used by every other row.
        assert!(
            out.contains("source snapshots: 0"),
            "missing space after 'source snapshots:' — got:\n{out}"
        );
        assert!(
            !out.contains("source snapshots:0"),
            "source-snapshots line regressed to the unspaced form:\n{out}"
        );
    }

    // Regression for X1: the indexer may store a fully-qualified name
    // (`Algos.fibonacci`, `algos::fibonacci`, `module/foo`) while the
    // agent types only the short symbol (`fibonacci`, `foo`). The hit
    // join must still find the hits — location_key is `file:line`, not
    // the function name, so we resolve file+line from the symbol row.
    #[test]
    fn cross_counts_hits_for_suffixed_fqn() {
        let tmp = TempDir::new().unwrap();
        let (db, _) = db_and_ctx(&tmp);
        // Indexer stored the fully-qualified name.
        db.conn()
            .execute(
                "INSERT INTO symbols (session_id, lang, fqn, file, line, raw)
                 VALUES ((SELECT id FROM sessions LIMIT 1), 'ruby',
                         'Algos.fibonacci', '/repo/algos.rb', 17,
                         'Algos.fibonacci')",
                [],
            )
            .unwrap();
        // Debugger stored hits by `file:line` with a relative path.
        for seq in 1..=5 {
            insert_hit(&db, "./algos.rb:17", seq, "{}", None);
        }
        // Agent types the short symbol name.
        let out = cmd_cross(&db, "fibonacci");
        assert!(
            out.contains("breakpoint hits: 5"),
            "expected 5 hits, got:\n{out}"
        );
    }

    // Same shape but with the `::` Rust/C++-style separator.
    #[test]
    fn cross_counts_hits_for_double_colon_fqn() {
        let tmp = TempDir::new().unwrap();
        let (db, _) = db_and_ctx(&tmp);
        db.conn()
            .execute(
                "INSERT INTO symbols (session_id, lang, fqn, file, line, raw)
                 VALUES ((SELECT id FROM sessions LIMIT 1), 'cpp',
                         'algos::fibonacci', 'src/main.rs', 22,
                         'algos::fibonacci')",
                [],
            )
            .unwrap();
        for seq in 1..=3 {
            insert_hit(&db, "/abs/src/main.rs:22", seq, "{}", None);
        }
        let out = cmd_cross(&db, "fibonacci");
        assert!(
            out.contains("breakpoint hits: 3"),
            "expected 3 hits, got:\n{out}"
        );
    }

    // ---------- query canonical-op names ----------

    #[test]
    fn canonical_op_names_are_stable() {
        assert_eq!(
            Query::Hits { loc: "x".into(), group_by: None, top: None }.canonical_op(),
            "hits"
        );
        assert_eq!(
            Query::Disasm { symbol: None, refresh: false }.canonical_op(),
            "disasm"
        );
        assert_eq!(Query::Cross { symbol: "x".into() }.canonical_op(), "cross");
        assert_eq!(Query::AtHitDisasm.canonical_op(), "at-hit");
    }

    // ---------- sparkline ----------

    #[test]
    fn sparkline_uses_all_bars_for_monotonic_series() {
        let points: Vec<(i64, f64)> = (0..8).map(|i| (i as i64, i as f64)).collect();
        let s = sparkline(&points);
        assert_eq!(s.chars().count(), 8);
        assert_eq!(s.chars().next(), Some('▁'));
        assert_eq!(s.chars().last(), Some('█'));
    }

    // ---------- hits flag parsing regressions ----------

    #[test]
    fn hits_grouped_sorts_numeric_fields_numerically() {
        // Regression: tie-broken alphabetic sort produced `0, 100,
        // 102, 13, 15` for depth values. Fields whose values all
        // parse as numbers should tie-break numerically.
        let tmp = TempDir::new().unwrap();
        let (db, _) = db_and_ctx(&tmp);
        for (seq, d) in [(1i64, 0), (2, 13), (3, 100), (4, 102), (5, 15)].iter() {
            insert_hit(&db, "f.py:1", *seq, &format!("{{\"d\":{d}}}"), None);
        }
        let out = cmd_hits_grouped(&db, "f.py:1", "d", None);
        let i0 = out.find(" 0 ").or_else(|| out.find("  0 ")).unwrap();
        let i13 = out.find("13").unwrap();
        let i100 = out.find("100").unwrap();
        assert!(i0 < i13 && i13 < i100, "not numeric-sorted:\n{out}");
    }

    #[test]
    fn hits_rejects_help_flag_as_loc() {
        // Regression: `--help` used to be stored as the location,
        // producing "no hits at --help" instead of usage.
        let d = try_dispatch("hits --help").expect("dispatch");
        match d {
            super::super::Dispatched::Immediate(s) => {
                assert!(s.starts_with("usage:"), "got: {s}");
            }
            _ => panic!("expected Immediate usage string"),
        }
    }

    #[test]
    fn hits_rejects_top_without_group_by() {
        // Regression: `--top N` with no `--group-by` / `--count-by`
        // used to be accepted and silently dropped at render time, so
        // agents would get the full ungrouped listing back without
        // any indication the flag did nothing.
        let d = try_dispatch("hits foo:10 --top 3").expect("dispatch");
        match d {
            super::super::Dispatched::Immediate(s) => {
                assert!(
                    s.contains("--top") && s.to_lowercase().contains("--group-by"),
                    "expected `--top requires --group-by` hint, got: {s}"
                );
            }
            _ => panic!("expected Immediate usage error for --top without --group-by"),
        }
    }

    #[test]
    fn hits_rejects_unknown_flag() {
        let d = try_dispatch("hits foo:10 --bogus").expect("dispatch");
        match d {
            super::super::Dispatched::Immediate(s) => {
                assert!(s.contains("unknown flag"), "got: {s}");
            }
            _ => panic!("expected Immediate"),
        }
    }

    #[test]
    fn hit_trend_dotted_path_hint_no_double_self() {
        // Regression: the error hint used to append `self.self.x`
        // when the user already supplied a dotted path.
        let tmp = TempDir::new().unwrap();
        let (db, _) = db_and_ctx(&tmp);
        insert_hit(&db, "foo.py:10", 1, r#"{"x":1}"#, None);
        let out = cmd_hit_trend(&db, "foo.py:10", "self.missing");
        assert!(!out.contains("self.self."), "double self in hint: {out}");
    }

    #[test]
    fn sparkline_flat_series_is_single_bar() {
        let points = vec![(1, 5.0), (2, 5.0), (3, 5.0)];
        let s = sparkline(&points);
        assert_eq!(s.chars().count(), 3);
        // All the same value — all the same bar.
        let first = s.chars().next().unwrap();
        assert!(s.chars().all(|c| c == first));
    }

    // ----------------------------------------------------------------
    // Bug 3: --group-by / hit-trend with dotted path into repr string
    // ----------------------------------------------------------------

    /// Regression: pdb stores complex objects (e.g. `self`) as their
    /// `repr()` string — `{"self": {"value": "<Obj object at 0x1a2b>"}}`.
    /// Traversing `self.page` fails silently because the JSON has no
    /// nested `page` key. The error message must explicitly tell the
    /// user that pdb stores objects as repr strings, not traversable
    /// JSON, and suggest capturing the field directly.
    #[test]
    fn hits_grouped_dotted_path_into_repr_gives_clear_error() {
        let tmp = TempDir::new().unwrap();
        let (db, _) = db_and_ctx(&tmp);
        // pdb stores `self` as its repr string, not as a nested dict.
        insert_hit(
            &db,
            "broken.py:26",
            1,
            r#"{"self": {"value": "<Paginator object at 0x7f1234>"},
                "page": {"value": "3"}}"#,
            None,
        );
        let out = cmd_hits_grouped(&db, "broken.py:26", "self.page", None);
        // Must NOT render as a grouped-result table (no `count` header, no
        // count lines). The repr hint has prose, not tabular output.
        assert!(
            !out.contains("count"),
            "must not produce a grouped-result table — got: {out}"
        );
        // Must include a helpful message about repr strings.
        assert!(
            out.to_lowercase().contains("repr")
                || out.contains("repr string")
                || out.contains("cannot traverse"),
            "expected repr-string hint in error, got: {out}"
        );
    }

    #[test]
    fn hit_trend_dotted_path_into_repr_gives_clear_error() {
        let tmp = TempDir::new().unwrap();
        let (db, _) = db_and_ctx(&tmp);
        insert_hit(
            &db,
            "broken.py:26",
            1,
            r#"{"self": {"value": "<Paginator object at 0x7f1234>"},
                "page": {"value": "3"}}"#,
            None,
        );
        let out = cmd_hit_trend(&db, "broken.py:26", "self.page");
        assert!(
            out.to_lowercase().contains("repr")
                || out.contains("repr string")
                || out.contains("cannot traverse"),
            "expected repr-string hint in error, got: {out}"
        );
    }
}
