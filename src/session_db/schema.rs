//! SessionDb schema DDL.
//!
//! Every minor version can break schema. Old DBs that don't match
//! `SCHEMA_VERSION` fail to load with a clear "re-collect" message.
//! There is no migration path — the raw native files under
//! `.dbg/sessions/<label>/raw/` are the durable artifact and `dbg profile-*`
//! can always regenerate an index from them.

use anyhow::Result;
use rusqlite::Connection;

use super::TargetClass;

/// Bump this on every schema-breaking change. No migrations.
pub const SCHEMA_VERSION: i64 = 2;

/// Shared meta tables — always created regardless of track or target class.
pub const CORE_DDL: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL CHECK (kind IN ('debug','profile')),
    target          TEXT NOT NULL,
    target_class    TEXT NOT NULL,
    target_hash     TEXT,
    started_at      TEXT NOT NULL,
    ended_at        TEXT,
    label           TEXT NOT NULL,
    created_by      TEXT NOT NULL DEFAULT 'auto'
);

CREATE TABLE IF NOT EXISTS layers (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    source          TEXT NOT NULL,
    file            TEXT,
    collected_at    TEXT,
    command_used    TEXT,
    collection_secs REAL,
    target_hash     TEXT
);
CREATE INDEX IF NOT EXISTS idx_layers_session ON layers(session_id);

CREATE TABLE IF NOT EXISTS symbols (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    lang            TEXT NOT NULL,
    fqn             TEXT NOT NULL,
    file            TEXT,
    line            INTEGER,
    demangled       TEXT,
    raw             TEXT NOT NULL,
    is_synthetic    INTEGER NOT NULL DEFAULT 0,
    UNIQUE(session_id, lang, fqn)
);
CREATE INDEX IF NOT EXISTS idx_symbols_fqn ON symbols(lang, fqn);

CREATE TABLE IF NOT EXISTS meta (
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    key             TEXT NOT NULL,
    value           TEXT,
    PRIMARY KEY (session_id, key)
);

CREATE TABLE IF NOT EXISTS failures (
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    phase           TEXT,
    error           TEXT
);

CREATE TABLE IF NOT EXISTS regions (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    name            TEXT NOT NULL,
    start_us        REAL,
    duration_us     REAL,
    thread          TEXT,
    layer_id        INTEGER REFERENCES layers(id)
);
CREATE INDEX IF NOT EXISTS idx_regions_session ON regions(session_id);

CREATE TABLE IF NOT EXISTS allocations (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    op              TEXT NOT NULL,
    address         INTEGER NOT NULL,
    bytes           INTEGER NOT NULL,
    start_us        REAL,
    heap            TEXT NOT NULL DEFAULT 'default',
    thread          TEXT,
    stack_json      TEXT,
    layer_id        INTEGER REFERENCES layers(id)
);
CREATE INDEX IF NOT EXISTS idx_alloc_addr ON allocations(session_id, address);
CREATE INDEX IF NOT EXISTS idx_alloc_time ON allocations(session_id, start_us);
";

/// Debug track tables — created for every session regardless of kind.
/// A profile session simply never writes to them.
pub const DEBUG_DDL: &str = "
CREATE TABLE IF NOT EXISTS commands (
    seq             INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    input           TEXT NOT NULL,
    output_head     TEXT,
    output_file     TEXT,
    output_bytes    INTEGER,
    ts              TEXT NOT NULL,
    canonical_op    TEXT
);
CREATE INDEX IF NOT EXISTS idx_commands_session ON commands(session_id);

CREATE TABLE IF NOT EXISTS breakpoint_hits (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    location_key    TEXT NOT NULL,
    hit_seq         INTEGER NOT NULL,
    thread          TEXT,
    ts              TEXT NOT NULL,
    locals_json     TEXT,
    stack_json      TEXT
);
CREATE INDEX IF NOT EXISTS idx_bp_location ON breakpoint_hits(session_id, location_key);

CREATE TABLE IF NOT EXISTS watch_evals (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    hit_id          INTEGER REFERENCES breakpoint_hits(id),
    expr            TEXT NOT NULL,
    value           TEXT,
    type_name       TEXT,
    ts              TEXT NOT NULL
);
";

/// Cross-track shared tables — joinable from either debug or profile
/// tracks via `symbol_id`. Populated by on-demand collectors.
pub const CROSSTRACK_DDL: &str = "
CREATE TABLE IF NOT EXISTS disassembly (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    symbol_id       INTEGER REFERENCES symbols(id),
    source          TEXT NOT NULL,
    tier            TEXT,
    code_bytes      INTEGER,
    asm_text        TEXT NOT NULL,
    asm_lines_json  TEXT,
    collected_at    TEXT NOT NULL,
    trigger         TEXT,
    layer_id        INTEGER REFERENCES layers(id)
);
CREATE INDEX IF NOT EXISTS idx_disasm_sym ON disassembly(symbol_id);

CREATE TABLE IF NOT EXISTS source_snapshots (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    symbol_id       INTEGER REFERENCES symbols(id),
    file            TEXT,
    line_start      INTEGER,
    line_end        INTEGER,
    text            TEXT,
    content_hash    TEXT,
    collected_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_src_sym ON source_snapshots(symbol_id);

CREATE TABLE IF NOT EXISTS alloc_sites (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    symbol_id       INTEGER REFERENCES symbols(id),
    bytes_total     INTEGER,
    count           INTEGER,
    largest_bytes   INTEGER,
    collected_at    TEXT NOT NULL,
    layer_id        INTEGER REFERENCES layers(id)
);
CREATE INDEX IF NOT EXISTS idx_alloc_site_sym ON alloc_sites(symbol_id);

CREATE TABLE IF NOT EXISTS insn_hits (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    target          TEXT NOT NULL,
    hit_count       INTEGER NOT NULL,
    sample_basis    TEXT NOT NULL,
    sample_period   INTEGER,
    window_us       REAL,
    backend         TEXT NOT NULL,
    collected_at    TEXT NOT NULL,
    detail_json     TEXT
);
CREATE INDEX IF NOT EXISTS idx_insn_hits_target ON insn_hits(session_id, target);

CREATE TABLE IF NOT EXISTS insn_hit_details (
    id              INTEGER PRIMARY KEY,
    insn_hit_id     INTEGER NOT NULL REFERENCES insn_hits(id),
    ts_us           REAL,
    stack_json      TEXT,
    regs_json       TEXT
);
CREATE INDEX IF NOT EXISTS idx_insn_hit_details ON insn_hit_details(insn_hit_id);
";

/// GPU domain tables (CUDA launches, ncu metrics, memcpy transfers,
/// framework ops). From gdbg's current schema, with `session_id` added.
pub const GPU_DDL: &str = "
CREATE TABLE IF NOT EXISTS launches (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    kernel_name     TEXT NOT NULL,
    duration_us     REAL NOT NULL,
    grid_x          INTEGER, grid_y INTEGER, grid_z INTEGER,
    block_x         INTEGER, block_y INTEGER, block_z INTEGER,
    stream_id       INTEGER,
    start_us        REAL,
    correlation_id  INTEGER,
    layer_id        INTEGER REFERENCES layers(id)
);
CREATE INDEX IF NOT EXISTS idx_launches_kernel ON launches(session_id, kernel_name);
CREATE INDEX IF NOT EXISTS idx_launches_start  ON launches(session_id, start_us);
CREATE INDEX IF NOT EXISTS idx_launches_stream ON launches(session_id, stream_id);

CREATE TABLE IF NOT EXISTS metrics (
    session_id                TEXT NOT NULL REFERENCES sessions(id),
    kernel_name               TEXT NOT NULL,
    occupancy_pct             REAL,
    compute_throughput_pct    REAL,
    memory_throughput_pct     REAL,
    registers_per_thread      INTEGER,
    shared_mem_static_bytes   INTEGER,
    shared_mem_dynamic_bytes  INTEGER,
    l2_hit_rate_pct           REAL,
    achieved_bandwidth_gb_s   REAL,
    peak_bandwidth_gb_s       REAL,
    boundedness               TEXT,
    layer_id                  INTEGER REFERENCES layers(id),
    PRIMARY KEY (session_id, kernel_name)
);

CREATE TABLE IF NOT EXISTS transfers (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    kind            TEXT NOT NULL,
    bytes           INTEGER,
    duration_us     REAL,
    start_us        REAL,
    stream_id       INTEGER,
    layer_id        INTEGER REFERENCES layers(id)
);

CREATE TABLE IF NOT EXISTS ops (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    name            TEXT NOT NULL,
    module_path     TEXT,
    cpu_time_us     REAL,
    gpu_time_us     REAL,
    input_shapes    TEXT,
    layer_id        INTEGER REFERENCES layers(id)
);

CREATE TABLE IF NOT EXISTS op_kernel_map (
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    op_id           INTEGER NOT NULL REFERENCES ops(id),
    kernel_name     TEXT NOT NULL,
    PRIMARY KEY (op_id, kernel_name)
);
";

/// CPU-unified profile tables — sampling + EAV counters. Used by
/// native-cpu, managed-dotnet, jvm, python, js/node target classes.
pub const CPU_DDL: &str = "
CREATE TABLE IF NOT EXISTS samples (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    symbol_id       INTEGER REFERENCES symbols(id),
    thread          TEXT,
    start_us        REAL,
    duration_us     REAL,
    cpu_ns          INTEGER,
    weight          REAL NOT NULL DEFAULT 1.0,
    stack_json      TEXT,
    layer_id        INTEGER REFERENCES layers(id)
);
CREATE INDEX IF NOT EXISTS idx_samples_sym   ON samples(session_id, symbol_id);
CREATE INDEX IF NOT EXISTS idx_samples_start ON samples(session_id, start_us);

CREATE TABLE IF NOT EXISTS counters (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    name            TEXT NOT NULL,
    symbol_id       INTEGER REFERENCES symbols(id),
    value           REAL NOT NULL,
    unit            TEXT,
    layer_id        INTEGER REFERENCES layers(id)
);
CREATE INDEX IF NOT EXISTS idx_counters_name ON counters(session_id, name);
";

/// Managed-runtime tables (.NET, JVM).
pub const MANAGED_DDL: &str = "
CREATE TABLE IF NOT EXISTS gc_events (
    id                INTEGER PRIMARY KEY,
    session_id        TEXT NOT NULL REFERENCES sessions(id),
    kind              TEXT NOT NULL,
    pause_us          REAL,
    start_us          REAL,
    heap_before_bytes INTEGER,
    heap_after_bytes  INTEGER,
    reason            TEXT,
    layer_id          INTEGER REFERENCES layers(id)
);
CREATE INDEX IF NOT EXISTS idx_gc_start ON gc_events(session_id, start_us);

CREATE TABLE IF NOT EXISTS jit_events (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    symbol_id       INTEGER REFERENCES symbols(id),
    compile_us      REAL,
    code_bytes      INTEGER,
    tier            TEXT,
    start_us        REAL,
    layer_id        INTEGER REFERENCES layers(id)
);
";

/// Python-specific tables (GIL contention).
pub const PYTHON_DDL: &str = "
CREATE TABLE IF NOT EXISTS gil_events (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    kind            TEXT NOT NULL,
    thread          TEXT,
    start_us        REAL,
    duration_us     REAL,
    layer_id        INTEGER REFERENCES layers(id)
);
";

/// Node/JS-specific tables (event-loop lag).
pub const NODE_DDL: &str = "
CREATE TABLE IF NOT EXISTS event_loop_lags (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    lag_us          REAL NOT NULL,
    start_us        REAL,
    phase           TEXT,
    layer_id        INTEGER REFERENCES layers(id)
);
";

/// Apply the shared meta + debug + cross-track DDL plus the per-class
/// domain tables. Safe to call on a fresh or existing DB; uses
/// `CREATE TABLE IF NOT EXISTS` everywhere. Schema-version enforcement
/// happens in `SessionDb::open`, not here.
pub fn apply(conn: &Connection, class: TargetClass) -> Result<()> {
    conn.execute_batch(CORE_DDL)?;
    conn.execute_batch(DEBUG_DDL)?;
    conn.execute_batch(CROSSTRACK_DDL)?;
    match class {
        TargetClass::Gpu => conn.execute_batch(GPU_DDL)?,
        TargetClass::NativeCpu => conn.execute_batch(CPU_DDL)?,
        TargetClass::ManagedDotnet | TargetClass::Jvm => {
            conn.execute_batch(CPU_DDL)?;
            conn.execute_batch(MANAGED_DDL)?;
        }
        TargetClass::Python => {
            conn.execute_batch(CPU_DDL)?;
            conn.execute_batch(PYTHON_DDL)?;
        }
        TargetClass::JsNode => {
            conn.execute_batch(CPU_DDL)?;
            conn.execute_batch(NODE_DDL)?;
        }
        // Ruby and PHP share the CPU-unified profile shape for now.
        // They get their own TargetClass so the `sessions` listing shows
        // a meaningful runtime tag instead of `native-cpu`.
        TargetClass::Ruby | TargetClass::Php => {
            conn.execute_batch(CPU_DDL)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_mem() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn applies_cleanly_per_class() {
        for class in [
            TargetClass::Gpu,
            TargetClass::NativeCpu,
            TargetClass::ManagedDotnet,
            TargetClass::Jvm,
            TargetClass::Python,
            TargetClass::JsNode,
            TargetClass::Ruby,
            TargetClass::Php,
        ] {
            let c = in_mem();
            apply(&c, class).unwrap_or_else(|e| panic!("apply({class:?}): {e}"));
        }
    }

    #[test]
    fn apply_is_idempotent() {
        let c = in_mem();
        apply(&c, TargetClass::Gpu).unwrap();
        apply(&c, TargetClass::Gpu).unwrap();
        apply(&c, TargetClass::Gpu).unwrap();
    }

    #[test]
    fn core_tables_exist_after_apply() {
        let c = in_mem();
        apply(&c, TargetClass::NativeCpu).unwrap();
        for t in ["sessions", "layers", "symbols", "meta", "failures",
                  "regions", "allocations",
                  "commands", "breakpoint_hits", "watch_evals",
                  "disassembly", "source_snapshots", "alloc_sites",
                  "insn_hits", "insn_hit_details",
                  "samples", "counters"] {
            let exists: i64 = c.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [t],
                |r| r.get(0),
            ).unwrap();
            assert_eq!(exists, 1, "missing table: {t}");
        }
    }

    #[test]
    fn gpu_class_has_launches_but_not_samples() {
        let c = in_mem();
        apply(&c, TargetClass::Gpu).unwrap();
        let launches: i64 = c.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='launches'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(launches, 1);
        let samples: i64 = c.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='samples'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(samples, 0, "samples table should not exist for GPU class");
    }
}
