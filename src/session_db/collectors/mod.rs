//! On-demand collectors.
//!
//! These populate the cross-track shared tables (`disassembly`,
//! `source_snapshots`, `alloc_sites`) in response to a specific agent
//! question — `dbg disasm <sym>`, `dbg source <sym>`, etc. They're
//! lightweight: cheap enough to run mid-debug at a breakpoint.
//!
//! Wiring:
//!   * A collector may have access to the live debugger session via
//!     `LiveDebugger`. Collectors that can reuse the existing PTY do
//!     so (lldb's `disassemble` runs cleanly inside an active session);
//!     collectors that need a fresh process (.NET jitdasm, which must
//!     set `DOTNET_JitDisasm` before the runtime starts) always spawn
//!     one — never restarting the live debug session.
//!   * Results are deduplicated on `(symbol_id, source, tier)` unless
//!     `CollectCtx::refresh = true` is set.

pub mod disasm;

use std::path::Path;

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use super::canonicalizer::{CanonicalSymbol, for_lang};
use super::{SessionDb, TargetClass};

/// What drove this collection — informational, stored on the row so
/// agents can see whether disasm was requested at a stop point,
/// drilled into from a hotspot, or asked for explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectTrigger {
    BreakpointHit,
    HotspotDrill,
    Explicit,
}

impl CollectTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            CollectTrigger::BreakpointHit => "breakpoint-hit",
            CollectTrigger::HotspotDrill => "hotspot-drill",
            CollectTrigger::Explicit => "explicit",
        }
    }
}

/// Context passed to every collector invocation.
pub struct CollectCtx<'a> {
    pub target: &'a str,
    pub target_class: TargetClass,
    /// The symbol to collect for. May be a raw (pre-canonical) or
    /// canonical form; the collector canonicalizes internally.
    pub symbol: &'a str,
    /// If `true`, overwrite any existing row matching
    /// `(symbol_id, source, tier)` instead of returning the cached one.
    pub refresh: bool,
    pub trigger: CollectTrigger,
    pub cwd: &'a Path,
}

/// The output of a disasm collector, before it's written to the DB.
/// Separated so tests can exercise the shell-out + parse path without
/// an actual SessionDb attached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisasmOutput {
    pub source: &'static str,
    pub tier: Option<String>,
    pub code_bytes: Option<i64>,
    pub asm_text: String,
    pub asm_lines_json: Option<String>,
}

/// A minimal handle the daemon passes to collectors so they can issue
/// commands on the live debugger session when useful. None when the
/// collector is invoked from a profile-only context (no live PTY).
pub trait LiveDebugger: Send + Sync {
    /// Send a native-tool command to the active debugger and return
    /// its (cleaned) output.
    fn send(&self, cmd: &str) -> Result<String>;
    /// Tool name, for logging / error messages.
    fn tool_name(&self) -> &'static str;
}

/// The on-demand collector trait.
pub trait OnDemandCollector: Send + Sync {
    /// Stable identifier — also stored in `disassembly.source`.
    fn kind(&self) -> &'static str;

    /// Does this collector handle the given target class?
    fn supports(&self, class: TargetClass) -> bool;

    /// Collect disassembly (or equivalent) for `ctx.symbol`.
    fn collect(
        &self,
        ctx: &CollectCtx<'_>,
        live: Option<&dyn LiveDebugger>,
    ) -> Result<DisasmOutput>;
}

/// Upsert a symbols row for the given canonical form and return its id.
/// Called by `persist_disasm` before inserting the disassembly row so
/// `symbol_id` joins are always valid.
fn upsert_symbol(db: &SessionDb, sym: &CanonicalSymbol) -> Result<i64> {
    let session_id = current_session_id(db)?;
    // Try to find an existing row.
    let existing: Option<i64> = db.conn()
        .query_row(
            "SELECT id FROM symbols WHERE session_id=?1 AND lang=?2 AND fqn=?3",
            params![session_id, sym.lang, sym.fqn],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    db.conn().execute(
        "INSERT INTO symbols (session_id, lang, fqn, file, line, demangled, raw, is_synthetic)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            session_id,
            sym.lang,
            sym.fqn,
            sym.file,
            sym.line,
            sym.demangled,
            sym.raw,
            sym.is_synthetic as i64,
        ],
    )?;
    Ok(db.conn().last_insert_rowid())
}

fn current_session_id(db: &SessionDb) -> Result<String> {
    Ok(db.conn().query_row(
        "SELECT id FROM sessions LIMIT 1",
        [],
        |r| r.get::<_, String>(0),
    )?)
}

/// Write a `DisasmOutput` into the `disassembly` table, keyed to the
/// canonicalized symbol. Respects `ctx.refresh` — returns the existing
/// row's id when a match is found and refresh is off.
///
/// Returns the `disassembly.id` of the stored row.
pub fn persist_disasm(
    db: &SessionDb,
    ctx: &CollectCtx<'_>,
    output: &DisasmOutput,
) -> Result<i64> {
    let lang = lang_for_class(ctx.target_class);
    let canon = match for_lang(lang) {
        Some(c) => c.canonicalize(ctx.symbol),
        None => CanonicalSymbol {
            lang: "unknown",
            fqn: ctx.symbol.to_string(),
            file: None,
            line: None,
            demangled: None,
            raw: ctx.symbol.to_string(),
            is_synthetic: false,
        },
    };
    let symbol_id = upsert_symbol(db, &canon)?;
    let session_id = current_session_id(db)?;

    if !ctx.refresh {
        let existing: Option<i64> = db.conn()
            .query_row(
                "SELECT id FROM disassembly
                 WHERE session_id=?1 AND symbol_id=?2 AND source=?3
                       AND tier IS ?4",
                params![session_id, symbol_id, output.source, output.tier],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok(id);
        }
    } else {
        db.conn().execute(
            "DELETE FROM disassembly
             WHERE session_id=?1 AND symbol_id=?2 AND source=?3
                   AND (tier IS ?4 OR tier=?4)",
            params![session_id, symbol_id, output.source, output.tier],
        )?;
    }

    db.conn().execute(
        "INSERT INTO disassembly
            (session_id, symbol_id, source, tier, code_bytes,
             asm_text, asm_lines_json, collected_at, trigger)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'), ?8)",
        params![
            session_id,
            symbol_id,
            output.source,
            output.tier,
            output.code_bytes,
            output.asm_text,
            output.asm_lines_json,
            ctx.trigger.as_str(),
        ],
    )?;
    Ok(db.conn().last_insert_rowid())
}

/// Pick the canonicalizer language for a target class. For
/// `NativeCpu` we default to `cpp` — demangling covers C/C++/Rust
/// equivalently for disasm purposes. Callers with language-specific
/// knowledge may use the canonicalizer modules directly.
fn lang_for_class(class: TargetClass) -> &'static str {
    match class {
        TargetClass::Gpu => "cuda",
        TargetClass::NativeCpu => "cpp",
        TargetClass::ManagedDotnet => "dotnet",
        TargetClass::Jvm => "jvm",
        TargetClass::Python => "python",
        TargetClass::JsNode => "js",
        TargetClass::Ruby => "ruby",
        TargetClass::Php => "php",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_db::{CreateOptions, SessionKind};
    use tempfile::TempDir;

    fn test_db(tmp: &TempDir, class: TargetClass) -> SessionDb {
        SessionDb::create(CreateOptions {
            kind: SessionKind::Debug,
            target: "./t",
            target_class: class,
            cwd: tmp.path(),
            db_path: None,
            label: Some("t".into()),
            target_hash: Some("h".into()),
        })
        .unwrap()
    }

    fn ctx<'a>(tmp: &'a TempDir, symbol: &'a str, refresh: bool) -> CollectCtx<'a> {
        CollectCtx {
            target: "./t",
            target_class: TargetClass::NativeCpu,
            symbol,
            refresh,
            trigger: CollectTrigger::Explicit,
            cwd: tmp.path(),
        }
    }

    #[test]
    fn persist_inserts_symbol_and_disasm() {
        let tmp = TempDir::new().unwrap();
        let db = test_db(&tmp, TargetClass::NativeCpu);
        let out = DisasmOutput {
            source: "lldb-disassemble",
            tier: None,
            code_bytes: Some(128),
            asm_text: "mov rax, rbx\nret".into(),
            asm_lines_json: None,
        };
        let id = persist_disasm(&db, &ctx(&tmp, "main", false), &out).unwrap();
        assert!(id > 0);

        let count: i64 = db.conn()
            .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1);
        let dcount: i64 = db.conn()
            .query_row("SELECT COUNT(*) FROM disassembly", [], |r| r.get(0)).unwrap();
        assert_eq!(dcount, 1);
    }

    #[test]
    fn persist_dedups_without_refresh() {
        let tmp = TempDir::new().unwrap();
        let db = test_db(&tmp, TargetClass::NativeCpu);
        let out = DisasmOutput {
            source: "lldb-disassemble",
            tier: None,
            code_bytes: None,
            asm_text: "a".into(),
            asm_lines_json: None,
        };
        let a = persist_disasm(&db, &ctx(&tmp, "main", false), &out).unwrap();
        let b = persist_disasm(&db, &ctx(&tmp, "main", false), &out).unwrap();
        assert_eq!(a, b, "second call should return cached id");

        let count: i64 = db.conn()
            .query_row("SELECT COUNT(*) FROM disassembly", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn persist_refresh_replaces_row() {
        let tmp = TempDir::new().unwrap();
        let db = test_db(&tmp, TargetClass::NativeCpu);
        let v1 = DisasmOutput {
            source: "lldb-disassemble",
            tier: None,
            code_bytes: None,
            asm_text: "old asm".into(),
            asm_lines_json: None,
        };
        let v2 = DisasmOutput {
            source: "lldb-disassemble",
            tier: None,
            code_bytes: None,
            asm_text: "new asm".into(),
            asm_lines_json: None,
        };
        let _ = persist_disasm(&db, &ctx(&tmp, "main", false), &v1).unwrap();
        let _ = persist_disasm(&db, &ctx(&tmp, "main", true), &v2).unwrap();

        let count: i64 = db.conn()
            .query_row("SELECT COUNT(*) FROM disassembly", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1);
        let text: String = db.conn()
            .query_row("SELECT asm_text FROM disassembly", [], |r| r.get(0))
            .unwrap();
        assert_eq!(text, "new asm");
    }

    #[test]
    fn persist_distinguishes_by_tier() {
        let tmp = TempDir::new().unwrap();
        let db = test_db(&tmp, TargetClass::ManagedDotnet);
        let t0 = DisasmOutput {
            source: "jitdasm",
            tier: Some("tier0".into()),
            code_bytes: None,
            asm_text: "tier-0".into(),
            asm_lines_json: None,
        };
        let t1 = DisasmOutput {
            source: "jitdasm",
            tier: Some("tier1".into()),
            code_bytes: None,
            asm_text: "tier-1".into(),
            asm_lines_json: None,
        };
        let c = CollectCtx {
            target: "./t",
            target_class: TargetClass::ManagedDotnet,
            symbol: "MyApp.Foo",
            refresh: false,
            trigger: CollectTrigger::Explicit,
            cwd: tmp.path(),
        };
        persist_disasm(&db, &c, &t0).unwrap();
        persist_disasm(&db, &c, &t1).unwrap();

        let count: i64 = db.conn()
            .query_row("SELECT COUNT(*) FROM disassembly", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 2, "tier0 and tier1 are distinct rows");
    }

    #[test]
    fn lang_mapping_covers_every_class() {
        assert_eq!(lang_for_class(TargetClass::Gpu), "cuda");
        assert_eq!(lang_for_class(TargetClass::NativeCpu), "cpp");
        assert_eq!(lang_for_class(TargetClass::ManagedDotnet), "dotnet");
        assert_eq!(lang_for_class(TargetClass::Jvm), "jvm");
        assert_eq!(lang_for_class(TargetClass::Python), "python");
        assert_eq!(lang_for_class(TargetClass::JsNode), "js");
    }
}
