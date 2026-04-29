//! `dbg insn-hits <symbol|addr>` — count how many times a specific
//! instruction (or symbol entry) executes, abstracted over hardware
//! tracers, statistical samplers, and trap-based probes.
//!
//! The verb is intentionally one user-facing concept ("how many hits")
//! over backends with very different semantics (PT and ETM are exact
//! and post-hoc, eBPF uprobe is exact and live, IBS/PEBS/SPE are
//! statistical, hwbp is exact but stops the thread). The capability
//! matrix below makes those differences first-class so the planner
//! can pick honestly and `--why` can explain the choice.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use dbg_cli::session_db::SessionDb;
use rusqlite::params;

/// Capability descriptor for a backend. Drives planner eligibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// `true` for backends that produce an exact hit count
    /// (PT/ETM/uprobe/hwbp); `false` for samplers (PEBS/IBS/SPE).
    pub exact: bool,
    /// `true` if the counter can be observed while the workload runs.
    /// PT and ETM are post-hoc (capture, stop, decode); everything
    /// else can stream incrementally.
    pub live: bool,
    pub stack_per_hit: bool,
    pub regs_per_hit: bool,
    /// Maximum number of simultaneous targets the host can carry
    /// (hwbp caps at 4 on x86, 6+ on ARM64; everything else is
    /// effectively unbounded so we report `u32::MAX`).
    pub max_simultaneous: u32,
    /// `true` if the backend modifies the target's text segment
    /// (uprobe int3 patch, hwbp DR registers do not patch text).
    pub trap_on_target: bool,
    /// Approximate cost ranking used to break ties between eligible
    /// backends. Lower is cheaper. The numeric value is advisory and
    /// only meaningful in comparison.
    pub overhead_score: u32,
}

/// Catalog of every backend the planner knows about. New rows go
/// here; the planner does not enumerate Rust types.
pub const BACKENDS: &[(BackendId, Capabilities)] = &[
    (
        BackendId::Pt,
        Capabilities {
            exact: true,
            live: false,
            stack_per_hit: true,
            regs_per_hit: false,
            max_simultaneous: u32::MAX,
            trap_on_target: false,
            overhead_score: 10,
        },
    ),
    (
        BackendId::Etm,
        Capabilities {
            exact: true,
            live: false,
            stack_per_hit: true,
            regs_per_hit: false,
            max_simultaneous: u32::MAX,
            trap_on_target: false,
            overhead_score: 12,
        },
    ),
    (
        BackendId::Spe,
        Capabilities {
            exact: false,
            live: true,
            stack_per_hit: false,
            regs_per_hit: false,
            max_simultaneous: u32::MAX,
            trap_on_target: false,
            overhead_score: 20,
        },
    ),
    (
        BackendId::Ibs,
        Capabilities {
            exact: false,
            live: true,
            stack_per_hit: false,
            regs_per_hit: true,
            max_simultaneous: u32::MAX,
            trap_on_target: false,
            overhead_score: 22,
        },
    ),
    (
        BackendId::Pebs,
        Capabilities {
            exact: false,
            live: true,
            stack_per_hit: false,
            regs_per_hit: false,
            max_simultaneous: u32::MAX,
            trap_on_target: false,
            overhead_score: 24,
        },
    ),
    (
        BackendId::Uprobe,
        Capabilities {
            exact: true,
            live: true,
            stack_per_hit: true,
            regs_per_hit: true,
            max_simultaneous: u32::MAX,
            trap_on_target: true,
            overhead_score: 30,
        },
    ),
    (
        BackendId::Hwbp,
        Capabilities {
            exact: true,
            live: true,
            stack_per_hit: true,
            regs_per_hit: true,
            max_simultaneous: 4,
            trap_on_target: false,
            overhead_score: 40,
        },
    ),
    (
        BackendId::Mock,
        Capabilities {
            exact: true,
            live: true,
            stack_per_hit: true,
            regs_per_hit: true,
            max_simultaneous: u32::MAX,
            trap_on_target: false,
            overhead_score: u32::MAX,
        },
    ),
];

/// Stable identifier for each backend. New backends append; existing
/// values must keep their position so persisted rows referencing
/// `backend` strings stay decodable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BackendId {
    Pt,
    Etm,
    Spe,
    Ibs,
    Pebs,
    Uprobe,
    Hwbp,
    Mock,
}

impl BackendId {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendId::Pt => "pt",
            BackendId::Etm => "etm",
            BackendId::Spe => "spe",
            BackendId::Ibs => "ibs",
            BackendId::Pebs => "pebs",
            BackendId::Uprobe => "uprobe",
            BackendId::Hwbp => "hwbp",
            BackendId::Mock => "mock",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "pt" => BackendId::Pt,
            "etm" => BackendId::Etm,
            "spe" => BackendId::Spe,
            "ibs" => BackendId::Ibs,
            "pebs" => BackendId::Pebs,
            "uprobe" => BackendId::Uprobe,
            "hwbp" => BackendId::Hwbp,
            "mock" => BackendId::Mock,
            _ => return None,
        })
    }
}

/// Parsed user request. Built from CLI flags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub target: String,
    pub mode: Mode,
    pub with_stack: bool,
    pub with_regs: bool,
    pub forced_backend: Option<BackendId>,
    pub explain: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Streaming counter readout while the workload runs.
    Live,
    /// Capture for a fixed window then summarize.
    Window(Duration),
}

/// What the planner picked, plus the trail it considered. The trail
/// is what `--why` prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub chosen: BackendId,
    pub trail: Vec<TrailEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrailEntry {
    pub backend: BackendId,
    pub verdict: Verdict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Chosen,
    Eligible,
    Rejected(String),
    NotAvailable,
}

/// Runtime detection: which BackendIds does this host actually carry?
/// Real backends (PT, ETM, uprobe, hwbp, …) override this; for now
/// the only one populated is `Mock`, which is always available.
pub trait HostProbe {
    fn is_available(&self, id: BackendId) -> bool;
}

/// Real-host probe used by the daemon. Each backend gets an honest
/// answer from a runtime check (binary on PATH, CPU feature flag, /
/// kernel capability). Mock is never visible here so the daemon does
/// not silently report synthetic counts when a real backend was not
/// installed.
pub struct HostProbeAuto;
impl HostProbe for HostProbeAuto {
    fn is_available(&self, id: BackendId) -> bool {
        match id {
            BackendId::Uprobe => uprobe::is_bpftrace_available(),
            BackendId::Mock => false,
            // PT, ETM, IBS, PEBS, SPE, hwbp probes land with their
            // collectors. Until then their availability is reported
            // false so the planner steers clear.
            _ => false,
        }
    }
}

/// Pick the cheapest backend that satisfies every flag the user set,
/// from the set the host actually carries. Returns `Err` with a
/// reason string when no backend is eligible (so the caller can
/// surface the cause to the user instead of falling back silently).
pub fn plan(req: &Request, probe: &dyn HostProbe) -> std::result::Result<Plan, String> {
    let mut trail: Vec<TrailEntry> = Vec::new();
    let mut eligible: Vec<(BackendId, &Capabilities)> = Vec::new();

    for (id, caps) in BACKENDS.iter() {
        if !probe.is_available(*id) {
            trail.push(TrailEntry {
                backend: *id,
                verdict: Verdict::NotAvailable,
            });
            continue;
        }
        if let Some(reason) = caps_reject(req, caps) {
            trail.push(TrailEntry {
                backend: *id,
                verdict: Verdict::Rejected(reason),
            });
            continue;
        }
        trail.push(TrailEntry {
            backend: *id,
            verdict: Verdict::Eligible,
        });
        eligible.push((*id, caps));
    }

    // Honor an explicit `--backend` choice when it survived eligibility.
    if let Some(forced) = req.forced_backend {
        if let Some(idx) = trail.iter().position(|e| e.backend == forced) {
            return match &trail[idx].verdict {
                Verdict::Eligible => {
                    let mut t = trail;
                    t[idx].verdict = Verdict::Chosen;
                    Ok(Plan { chosen: forced, trail: t })
                }
                Verdict::Rejected(r) => Err(format!(
                    "backend `{name}` is not eligible for this request: {r}",
                    name = forced.as_str()
                )),
                Verdict::NotAvailable => Err(format!(
                    "backend `{name}` is not available on this host",
                    name = forced.as_str()
                )),
                Verdict::Chosen => unreachable!("not yet marked"),
            };
        }
        return Err(format!("unknown backend `{}`", forced.as_str()));
    }

    if eligible.is_empty() {
        return Err(no_eligible_reason(req, &trail));
    }

    eligible.sort_by_key(|(_, c)| c.overhead_score);
    let chosen = eligible[0].0;
    if let Some(e) = trail.iter_mut().find(|e| e.backend == chosen) {
        e.verdict = Verdict::Chosen;
    }
    Ok(Plan { chosen, trail })
}

fn caps_reject(req: &Request, caps: &Capabilities) -> Option<String> {
    if matches!(req.mode, Mode::Live) && !caps.live {
        return Some("post-hoc only (no live readout)".into());
    }
    if req.with_stack && !caps.stack_per_hit {
        return Some("no per-hit stack capture".into());
    }
    if req.with_regs && !caps.regs_per_hit {
        return Some("no per-hit register capture".into());
    }
    None
}

fn no_eligible_reason(req: &Request, trail: &[TrailEntry]) -> String {
    let avail: Vec<_> = trail
        .iter()
        .filter(|e| !matches!(e.verdict, Verdict::NotAvailable))
        .collect();
    if avail.is_empty() {
        return "no instruction-hit backend is available on this host (PT/ETM/uprobe/hwbp \
                /IBS/PEBS/SPE all reported unavailable; install bpftrace or run on a host \
                with a supported PMU)"
            .into();
    }
    let mut buf = String::from("no available backend can satisfy this request:");
    if matches!(req.mode, Mode::Live) {
        buf.push_str(" --live ");
    }
    if req.with_stack {
        buf.push_str(" --with-stack");
    }
    if req.with_regs {
        buf.push_str(" --with-regs");
    }
    buf.push_str("\n");
    for e in avail {
        if let Verdict::Rejected(r) = &e.verdict {
            buf.push_str(&format!("  {:>7}: {}\n", e.backend.as_str(), r));
        }
    }
    buf
}

/// Render the planner trail in the form the user sees with `--why`.
pub fn format_why(plan: &Plan) -> String {
    let mut buf = format!("backend={} (chosen)\n", plan.chosen.as_str());
    for e in &plan.trail {
        match &e.verdict {
            Verdict::Chosen => continue,
            Verdict::Eligible => buf.push_str(&format!(
                "  {:>7}: eligible (not chosen, higher overhead)\n",
                e.backend.as_str()
            )),
            Verdict::Rejected(r) => buf.push_str(&format!(
                "  {:>7}: rejected -- {}\n",
                e.backend.as_str(),
                r
            )),
            Verdict::NotAvailable => buf.push_str(&format!(
                "  {:>7}: not available on this host\n",
                e.backend.as_str()
            )),
        }
    }
    buf
}

// ============================================================
// Parser
// ============================================================

/// Parse `insn-hits <target> [--live | --window <dur>] [--with-stack]
/// [--with-regs] [--backend <name>] [--why]`.
pub fn try_dispatch(input: &str) -> Option<super::Dispatched> {
    let input = input.trim();
    let (verb, rest) = match input.find(|c: char| c.is_ascii_whitespace()) {
        Some(i) => (&input[..i], input[i..].trim_start()),
        None => (input, ""),
    };
    if verb != "insn-hits" {
        return None;
    }
    let mut toks = rest.split_whitespace().peekable();
    let mut target: Option<String> = None;
    let mut mode = Mode::Window(Duration::from_secs(10));
    let mut explicit_mode = false;
    let mut with_stack = false;
    let mut with_regs = false;
    let mut forced_backend: Option<BackendId> = None;
    let mut explain = false;

    while let Some(t) = toks.next() {
        match t {
            "--live" => {
                mode = Mode::Live;
                explicit_mode = true;
            }
            "--window" => {
                let Some(v) = toks.next() else {
                    return Some(super::Dispatched::Immediate(
                        "--window needs a duration (e.g. 30s, 2m)".into(),
                    ));
                };
                let Some(d) = super::lifecycle::parse_duration(v) else {
                    return Some(super::Dispatched::Immediate(format!(
                        "could not parse `{v}` as a duration (try 30s, 2m, 1h)"
                    )));
                };
                mode = Mode::Window(d);
                explicit_mode = true;
            }
            "--with-stack" => with_stack = true,
            "--with-regs" => with_regs = true,
            "--backend" => {
                let Some(v) = toks.next() else {
                    return Some(super::Dispatched::Immediate(
                        "--backend needs a name (auto, pt, etm, uprobe, hwbp, ibs, pebs, spe)".into(),
                    ));
                };
                if v == "auto" {
                    forced_backend = None;
                } else {
                    let Some(id) = BackendId::from_str(v) else {
                        return Some(super::Dispatched::Immediate(format!(
                            "unknown backend `{v}` (known: auto, pt, etm, uprobe, hwbp, ibs, pebs, spe)"
                        )));
                    };
                    forced_backend = Some(id);
                }
            }
            "--why" => explain = true,
            "--help" | "-h" => return Some(super::Dispatched::Immediate(help_text())),
            _ if t.starts_with("--") => {
                return Some(super::Dispatched::Immediate(format!(
                    "unknown flag `{t}` -- run `dbg insn-hits --help` for the flag list"
                )));
            }
            _ if target.is_none() => target = Some(t.to_string()),
            _ => {
                return Some(super::Dispatched::Immediate(format!(
                    "insn-hits takes one target (got extra: `{t}`)"
                )));
            }
        }
    }
    let _ = explicit_mode;
    let Some(target) = target else {
        return Some(super::Dispatched::Immediate(help_text()));
    };
    Some(super::Dispatched::InsnHits(Request {
        target,
        mode,
        with_stack,
        with_regs,
        forced_backend,
        explain,
    }))
}

fn help_text() -> String {
    "\
usage: dbg insn-hits <symbol|0xADDR> [flags]

flags:
  --live                Stream a counter while the workload runs.
  --window <duration>   Capture for <duration> (default 10s) then summarize.
  --with-stack          Collect a stack per hit (backend permitting).
  --with-regs           Collect register values per hit.
  --backend <name>      auto (default), pt, etm, uprobe, hwbp, ibs, pebs, spe.
  --why                 Print the planner's backend choice and reasoning.
"
    .into()
}

// ============================================================
// Execution
// ============================================================

/// What every concrete backend implements. Keep the trait free of
/// session-DB or daemon types so the backends stay testable in
/// isolation.
pub trait Collector {
    fn id(&self) -> BackendId;
    fn collect(&self, req: &Request, ctx: &CollectCtx<'_>) -> Result<Outcome>;
}

/// Inputs the backend needs from the surrounding session that are
/// not part of the user's parsed `Request`. Lives separately so the
/// `Request` stays a pure CLI parse artifact.
pub struct CollectCtx<'a> {
    /// Path to the target binary the session is debugging. Used by
    /// uprobe and PT to scope probes to one binary.
    pub target_binary: &'a str,
    /// Working directory at session start. Used for relative path
    /// resolution and (for PT) where the raw trace lands.
    pub cwd: &'a std::path::Path,
}

/// Result of one collection. Multiple rows iff the request asked for
/// per-hit details that the backend provided.
pub struct Outcome {
    pub hit_count: u64,
    pub sample_basis: SampleBasis,
    pub sample_period: Option<u64>,
    pub window_us: Option<f64>,
    pub details: Vec<HitDetail>,
    pub detail_summary: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleBasis {
    Exact,
    Pebs,
    Ibs,
    Spe,
}

impl SampleBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            SampleBasis::Exact => "exact",
            SampleBasis::Pebs => "pebs",
            SampleBasis::Ibs => "ibs",
            SampleBasis::Spe => "spe",
        }
    }
}

#[derive(Clone, Debug)]
pub struct HitDetail {
    pub ts_us: Option<f64>,
    pub stack_json: Option<String>,
    pub regs_json: Option<String>,
}

/// Mock backend: returns a deterministic synthetic outcome. Lets the
/// dispatch path, schema round-trip, and `--why` rendering be tested
/// without a real PMU/eBPF/debugger present.
pub struct MockBackend;

impl Collector for MockBackend {
    fn id(&self) -> BackendId {
        BackendId::Mock
    }

    fn collect(&self, req: &Request, _ctx: &CollectCtx<'_>) -> Result<Outcome> {
        let window_us = match req.mode {
            Mode::Live => 1_000_000.0,
            Mode::Window(d) => d.as_secs_f64() * 1e6,
        };
        Ok(Outcome {
            hit_count: 42,
            sample_basis: SampleBasis::Exact,
            sample_period: None,
            window_us: Some(window_us),
            details: Vec::new(),
            detail_summary: Some("synthetic mock outcome".into()),
        })
    }
}

/// Top-level execution. Plans, picks a Collector by `BackendId`,
/// invokes it, and writes one `insn_hits` row plus zero or more
/// `insn_hit_details` rows. Returns the user-facing summary.
pub fn run(
    req: &Request,
    db: &SessionDb,
    probe: &dyn HostProbe,
    ctx: &CollectCtx<'_>,
) -> String {
    let plan = match plan(req, probe) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let mut buf = String::new();
    if req.explain {
        buf.push_str(&format_why(&plan));
        buf.push('\n');
    }

    let collector = collector_for(plan.chosen);
    let outcome = match collector.collect(req, ctx) {
        Ok(o) => o,
        Err(e) => {
            buf.push_str(&format!(
                "[{} backend failed: {e}]",
                plan.chosen.as_str()
            ));
            return buf;
        }
    };

    if let Err(e) = persist(db, req, &plan, &outcome) {
        // A persistence failure should not hide the count we already
        // measured. Surface both so the agent can react.
        buf.push_str(&format!(
            "[warn: writing insn_hits row failed: {e}]\n"
        ));
    }

    buf.push_str(&format_outcome(req, &plan, &outcome));
    buf
}

fn collector_for(id: BackendId) -> Box<dyn Collector> {
    // TODO(insn-hits): real backends fill in their slots one by one.
    // ETM, PEBS, IBS, SPE, hwbp still route through MockBackend; the
    // planner already advertises their capabilities so when their
    // collectors land the only change here is an extra match arm.
    match id {
        BackendId::Uprobe => Box::new(uprobe::UprobeBackend),
        BackendId::Mock => Box::new(MockBackend),
        _ => Box::new(MockBackend),
    }
}

fn persist(db: &SessionDb, req: &Request, plan: &Plan, outcome: &Outcome) -> Result<()> {
    db.conn().execute(
        "INSERT INTO insn_hits
            (session_id, target, hit_count, sample_basis, sample_period,
             window_us, backend, collected_at, detail_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'), ?8)",
        params![
            db.session_id(),
            req.target,
            outcome.hit_count as i64,
            outcome.sample_basis.as_str(),
            outcome.sample_period.map(|p| p as i64),
            outcome.window_us,
            plan.chosen.as_str(),
            outcome.detail_summary.as_deref(),
        ],
    )?;
    let parent_id = db.conn().last_insert_rowid();
    for d in &outcome.details {
        db.conn().execute(
            "INSERT INTO insn_hit_details (insn_hit_id, ts_us, stack_json, regs_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![parent_id, d.ts_us, d.stack_json, d.regs_json],
        )?;
    }
    Ok(())
}

fn format_outcome(req: &Request, plan: &Plan, outcome: &Outcome) -> String {
    let basis = match outcome.sample_basis {
        SampleBasis::Exact => "exact".to_string(),
        other => match outcome.sample_period {
            Some(p) => format!("{} (period={})", other.as_str(), p),
            None => other.as_str().to_string(),
        },
    };
    let window = outcome
        .window_us
        .map(|us| format!("{:.1}ms", us / 1000.0))
        .unwrap_or_else(|| "?".into());
    let details = outcome
        .detail_summary
        .as_deref()
        .map(|s| format!("\n  {s}"))
        .unwrap_or_default();
    format!(
        "insn-hits {target}\n  backend: {backend}\n  hits:    {hits}\n  basis:   {basis}\n  \
         window:  {window}{details}\n",
        target = req.target,
        backend = plan.chosen.as_str(),
        hits = outcome.hit_count,
    )
}

/// Public so the daemon's `pub fn is_repl_verb` can refer to a
/// stable list of verbs without each module re-encoding it.
pub fn verbs() -> &'static [&'static str] {
    &["insn-hits"]
}

// ============================================================
// Uprobe backend
// ============================================================

pub mod uprobe {
    //! eBPF uprobe collector. Shells out to `bpftrace` and parses its
    //! aggregation output. Cross-architecture (x86_64, ARM64,
    //! POWER, RISC-V on recent kernels) so this is the fallback path
    //! for hosts without Intel PT or ARM CoreSight.
    //!
    //! Trade-off the user pays: every hit traps via int3, kernel
    //! handler, BPF execution, return. Sub-microsecond per hit, but
    //! a hot inner loop with ~1e9 hits/sec spends seconds in the
    //! trap path, so the planner exposes this honestly via the
    //! `overhead_score` ranking.
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    pub struct UprobeBackend;

    impl Collector for UprobeBackend {
        fn id(&self) -> BackendId {
            BackendId::Uprobe
        }

        fn collect(&self, req: &Request, ctx: &CollectCtx<'_>) -> Result<Outcome> {
            if !is_bpftrace_available() {
                anyhow::bail!(
                    "uprobe backend needs `bpftrace` on PATH. Install via your package \
                     manager (apt install bpftrace, dnf install bpftrace, ...) or pick \
                     another backend with --backend"
                );
            }
            let window = match req.mode {
                Mode::Live => {
                    anyhow::bail!(
                        "uprobe live-mode streaming readout is not yet implemented. \
                         Pass --window <duration> for a one-shot count, or use --backend hwbp \
                         for an exact live counter when 4 simultaneous targets are enough."
                    );
                }
                Mode::Window(d) => d,
            };
            let prog = build_program(&req.target, ctx.target_binary, req.with_stack);
            let raw = run_bpftrace(&prog, window)?;
            let parsed = parse_output(&raw, req.with_stack)?;
            Ok(Outcome {
                hit_count: parsed.total,
                sample_basis: SampleBasis::Exact,
                sample_period: None,
                window_us: Some(window.as_secs_f64() * 1e6),
                details: parsed
                    .by_stack
                    .into_iter()
                    .map(|(stack, n)| HitDetail {
                        ts_us: None,
                        stack_json: Some(serialize_stack(&stack, n)),
                        regs_json: None,
                    })
                    .collect(),
                detail_summary: None,
            })
        }
    }

    pub(super) fn is_bpftrace_available() -> bool {
        which::which("bpftrace").is_ok()
    }

    /// Build the bpftrace program text. With-stack uses an aggregation
    /// keyed by `ustack`; without it, a single scalar counter. Target
    /// strings starting with `0x` are treated as raw addresses, anything
    /// else as a symbol name. bpftrace itself rejects malformed input.
    pub(super) fn build_program(target: &str, binary: &str, with_stack: bool) -> String {
        let probe_target = if target.starts_with("0x") || target.starts_with("0X") {
            target.to_string()
        } else {
            target.to_string()
        };
        if with_stack {
            format!(
                "uprobe:{binary}:{probe_target} {{ @[ustack] = count(); }}"
            )
        } else {
            format!(
                "uprobe:{binary}:{probe_target} {{ @ = count(); }}"
            )
        }
    }

    fn run_bpftrace(prog: &str, window: Duration) -> Result<String> {
        // `timeout` ships everywhere bpftrace does; bpftrace itself
        // has no built-in deadline that emits its aggregation on
        // expiry. SIGTERM (default for `timeout`) makes bpftrace
        // print its accumulated maps before exiting, which is the
        // only way to get the count out without a streaming reader.
        let secs = window.as_secs().max(1);
        let out = Command::new("timeout")
            .arg("--signal=TERM")
            .arg(secs.to_string())
            .arg("bpftrace")
            .arg("-q")
            .arg("-e")
            .arg(prog)
            .output()
            .map_err(|e| anyhow::anyhow!("failed to launch bpftrace: {e}"))?;
        // `timeout` exits 124 on SIGTERM; for bpftrace that is the
        // happy path (program printed its maps, then we killed it).
        if !out.status.success() && out.status.code() != Some(124) {
            anyhow::bail!(
                "bpftrace failed (exit {:?}): {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Aggregated parse result. `total` is the scalar count when
    /// no stacks were collected, or the sum across stacks when
    /// `--with-stack` was set.
    #[derive(Debug, Default, PartialEq, Eq)]
    pub(super) struct Parsed {
        pub total: u64,
        pub by_stack: Vec<(Vec<String>, u64)>,
    }

    /// Parse the textual output `bpftrace -q` emits when its
    /// aggregations dump. Two shapes:
    ///   `@: 42`                      (scalar count)
    ///   `@[\n  symbol+0x10\n  ...]: 5` (stack-keyed aggregation)
    pub(super) fn parse_output(raw: &str, with_stack: bool) -> Result<Parsed> {
        let mut parsed = Parsed::default();
        if with_stack {
            // Stack-keyed entries span multiple lines. Walk the text
            // splitting on `]: <n>` and recovering the stack body that
            // came before each closing bracket.
            let mut idx = 0;
            while let Some(open) = raw[idx..].find("@[") {
                let abs_open = idx + open + 2;
                let Some(close_rel) = raw[abs_open..].find("]:") else {
                    break;
                };
                let abs_close = abs_open + close_rel;
                let body = &raw[abs_open..abs_close];
                let after = &raw[abs_close + 2..];
                let count_str = after
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim();
                let n: u64 = count_str.parse().unwrap_or(0);
                let stack: Vec<String> = body
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .map(|l| l.to_string())
                    .collect();
                parsed.total = parsed.total.saturating_add(n);
                parsed.by_stack.push((stack, n));
                idx = abs_close + 2;
            }
        } else {
            for line in raw.lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("@:") {
                    let n: u64 = rest.trim().parse().unwrap_or(0);
                    parsed.total = parsed.total.saturating_add(n);
                }
            }
        }
        Ok(parsed)
    }

    fn serialize_stack(stack: &[String], hits: u64) -> String {
        let frames: Vec<String> = stack
            .iter()
            .map(|f| format!("    \"{}\"", f.replace('"', "\\\"")))
            .collect();
        format!(
            "{{\n  \"hits\": {hits},\n  \"frames\": [\n{}\n  ]\n}}",
            frames.join(",\n")
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn build_program_scalar() {
            let p = build_program("CosineDistanceSingles", "/usr/bin/raven", false);
            assert!(p.contains("uprobe:/usr/bin/raven:CosineDistanceSingles"));
            assert!(p.contains("@ = count();"));
            assert!(!p.contains("ustack"));
        }

        #[test]
        fn build_program_with_stack() {
            let p = build_program("foo", "./bin", true);
            assert!(p.contains("@[ustack] = count();"));
        }

        #[test]
        fn parse_scalar_output() {
            let raw = "Attaching 1 probe...\n\n@: 12345\n";
            let parsed = parse_output(raw, false).unwrap();
            assert_eq!(parsed.total, 12345);
            assert!(parsed.by_stack.is_empty());
        }

        #[test]
        fn parse_stack_keyed_output() {
            let raw = "\
Attaching 1 probe...

@[
    foo+0x10
    bar+0x20
    main+0x40
]: 5

@[
    foo+0x10
    other+0x8
]: 3
";
            let parsed = parse_output(raw, true).unwrap();
            assert_eq!(parsed.total, 8);
            assert_eq!(parsed.by_stack.len(), 2);
            let (stack0, n0) = &parsed.by_stack[0];
            assert_eq!(*n0, 5);
            assert_eq!(stack0.len(), 3);
            assert_eq!(stack0[0], "foo+0x10");
        }

        #[test]
        fn parse_handles_multiple_scalar_lines() {
            // A user could attach multiple uprobes; bpftrace prints
            // one `@: N` per. The parser sums.
            let raw = "@: 10\n@: 32\n";
            let parsed = parse_output(raw, false).unwrap();
            assert_eq!(parsed.total, 42);
        }

        #[test]
        fn parse_empty_output_is_zero_not_error() {
            // bpftrace exits cleanly with no probes fired — the
            // count must read as 0 rather than failing.
            let parsed = parse_output("Attaching 1 probe...\n", false).unwrap();
            assert_eq!(parsed.total, 0);
        }
    }
}

// ============================================================
// Aggregation helpers (replay surface)
// ============================================================

/// Read all `insn_hits` rows for one session, grouped by target. Used
/// by replay to render a one-shot summary without re-running the
/// collector. Returns `BTreeMap` so the order is deterministic.
pub fn aggregate_by_target(db: &SessionDb) -> BTreeMap<String, Vec<StoredRow>> {
    let mut out: BTreeMap<String, Vec<StoredRow>> = BTreeMap::new();
    let Ok(mut stmt) = db.conn().prepare(
        "SELECT target, hit_count, sample_basis, sample_period, window_us,
                backend, collected_at, detail_json
         FROM insn_hits
         WHERE session_id = ?1
         ORDER BY collected_at ASC, id ASC",
    ) else {
        return out;
    };
    let rows = stmt.query_map(params![db.session_id()], |r| {
        Ok(StoredRow {
            target: r.get(0)?,
            hit_count: r.get(1)?,
            sample_basis: r.get(2)?,
            sample_period: r.get(3)?,
            window_us: r.get(4)?,
            backend: r.get(5)?,
            collected_at: r.get(6)?,
            detail_summary: r.get(7)?,
        })
    });
    if let Ok(it) = rows {
        for r in it.flatten() {
            out.entry(r.target.clone()).or_default().push(r);
        }
    }
    out
}

#[derive(Clone, Debug)]
pub struct StoredRow {
    pub target: String,
    pub hit_count: i64,
    pub sample_basis: String,
    pub sample_period: Option<i64>,
    pub window_us: Option<f64>,
    pub backend: String,
    pub collected_at: String,
    pub detail_summary: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbg_cli::session_db::{CreateOptions, SessionKind, TargetClass};
    use tempfile::TempDir;

    struct FakeProbe(Vec<BackendId>);
    impl HostProbe for FakeProbe {
        fn is_available(&self, id: BackendId) -> bool {
            self.0.contains(&id)
        }
    }

    fn req(target: &str) -> Request {
        Request {
            target: target.into(),
            mode: Mode::Window(Duration::from_secs(10)),
            with_stack: false,
            with_regs: false,
            forced_backend: None,
            explain: false,
        }
    }

    fn ctx() -> CollectCtxOwned {
        CollectCtxOwned {
            target_binary: "/usr/bin/test".into(),
            cwd: std::env::temp_dir(),
        }
    }

    struct CollectCtxOwned {
        target_binary: String,
        cwd: std::path::PathBuf,
    }
    impl CollectCtxOwned {
        fn as_ref(&self) -> CollectCtx<'_> {
            CollectCtx { target_binary: &self.target_binary, cwd: &self.cwd }
        }
    }

    #[test]
    fn parser_basic_target_only() {
        let d = try_dispatch("insn-hits 0xDEADBEEF").unwrap();
        match d {
            super::super::Dispatched::InsnHits(r) => {
                assert_eq!(r.target, "0xDEADBEEF");
                assert!(matches!(r.mode, Mode::Window(d) if d == Duration::from_secs(10)));
                assert!(!r.with_stack && !r.with_regs && !r.explain);
                assert_eq!(r.forced_backend, None);
            }
            _ => panic!("expected InsnHits dispatch"),
        }
    }

    #[test]
    fn parser_full_flag_set() {
        let d = try_dispatch(
            "insn-hits foo --live --with-stack --with-regs --backend uprobe --why",
        )
        .unwrap();
        match d {
            super::super::Dispatched::InsnHits(r) => {
                assert_eq!(r.target, "foo");
                assert_eq!(r.mode, Mode::Live);
                assert!(r.with_stack && r.with_regs && r.explain);
                assert_eq!(r.forced_backend, Some(BackendId::Uprobe));
            }
            _ => panic!("expected InsnHits dispatch"),
        }
    }

    #[test]
    fn parser_window_duration() {
        let d = try_dispatch("insn-hits foo --window 30s").unwrap();
        match d {
            super::super::Dispatched::InsnHits(r) => {
                assert_eq!(r.mode, Mode::Window(Duration::from_secs(30)));
            }
            _ => panic!("expected InsnHits dispatch"),
        }
    }

    #[test]
    fn parser_unknown_backend_is_helpful() {
        let d = try_dispatch("insn-hits foo --backend bogus").unwrap();
        match d {
            super::super::Dispatched::Immediate(s) => {
                assert!(s.contains("unknown backend"), "{s}");
                assert!(s.contains("uprobe"), "must list known names: {s}");
            }
            _ => panic!("expected immediate error for unknown backend"),
        }
    }

    #[test]
    fn parser_no_target_returns_help() {
        let d = try_dispatch("insn-hits").unwrap();
        match d {
            super::super::Dispatched::Immediate(s) => {
                assert!(s.contains("usage: dbg insn-hits"), "{s}");
            }
            _ => panic!("expected help text"),
        }
    }

    #[test]
    fn planner_picks_lowest_overhead_when_unconstrained() {
        // PT and uprobe both available. PT has lower overhead score
        // and satisfies a default (window-mode, no stack/regs)
        // request -- it must win.
        let probe = FakeProbe(vec![BackendId::Pt, BackendId::Uprobe, BackendId::Mock]);
        let plan = plan(&req("foo"), &probe).unwrap();
        assert_eq!(plan.chosen, BackendId::Pt);
    }

    #[test]
    fn planner_drops_post_hoc_when_live_requested() {
        // PT and uprobe available; --live forces a live-capable
        // backend, so PT is rejected and uprobe wins.
        let mut r = req("foo");
        r.mode = Mode::Live;
        let probe = FakeProbe(vec![BackendId::Pt, BackendId::Uprobe, BackendId::Mock]);
        let plan = plan(&r, &probe).unwrap();
        assert_eq!(plan.chosen, BackendId::Uprobe);
        let pt_entry = plan
            .trail
            .iter()
            .find(|e| e.backend == BackendId::Pt)
            .unwrap();
        assert!(matches!(&pt_entry.verdict, Verdict::Rejected(_)));
    }

    #[test]
    fn planner_rejects_when_no_backend_can_do_regs() {
        // Only PT available; --with-regs is unsupported by PT, so
        // the planner must fail loudly.
        let mut r = req("foo");
        r.with_regs = true;
        let probe = FakeProbe(vec![BackendId::Pt]);
        let err = plan(&r, &probe).unwrap_err();
        assert!(
            err.contains("--with-regs") && err.contains("pt"),
            "expected reason mentioning the flag and rejected backend: {err}"
        );
    }

    #[test]
    fn planner_honors_force_backend() {
        // uprobe has higher overhead than PT but the user forced it
        // explicitly; planner must respect that.
        let mut r = req("foo");
        r.forced_backend = Some(BackendId::Uprobe);
        let probe = FakeProbe(vec![BackendId::Pt, BackendId::Uprobe]);
        let plan = plan(&r, &probe).unwrap();
        assert_eq!(plan.chosen, BackendId::Uprobe);
    }

    #[test]
    fn planner_force_backend_rejects_when_not_eligible() {
        // Forcing PT with --live must fail (PT cannot do live), and
        // the message must point at the reason rather than silently
        // falling back to uprobe.
        let mut r = req("foo");
        r.mode = Mode::Live;
        r.forced_backend = Some(BackendId::Pt);
        let probe = FakeProbe(vec![BackendId::Pt, BackendId::Uprobe]);
        let err = plan(&r, &probe).unwrap_err();
        assert!(err.contains("pt") && err.contains("not eligible"), "{err}");
    }

    #[test]
    fn planner_no_backends_available_explains_setup_gap() {
        let r = req("foo");
        let probe = FakeProbe(vec![]);
        let err = plan(&r, &probe).unwrap_err();
        assert!(
            err.contains("no instruction-hit backend") && err.contains("bpftrace"),
            "{err}"
        );
    }

    #[test]
    fn format_why_lists_chosen_eligible_and_rejected() {
        let mut r = req("foo");
        r.mode = Mode::Live;
        r.explain = true;
        let probe = FakeProbe(vec![BackendId::Pt, BackendId::Uprobe, BackendId::Mock]);
        let plan = plan(&r, &probe).unwrap();
        let why = format_why(&plan);
        assert!(why.contains("backend=uprobe (chosen)"), "{why}");
        assert!(why.contains("pt") && why.contains("post-hoc"), "{why}");
        assert!(why.contains("not available"), "{why}");
    }

    fn mk_db(tmp: &TempDir, label: &str) -> SessionDb {
        SessionDb::create(CreateOptions {
            kind: SessionKind::Debug,
            target: "./app",
            target_class: TargetClass::NativeCpu,
            cwd: tmp.path(),
            db_path: None,
            label: Some(label.into()),
            target_hash: None,
        })
        .unwrap()
    }

    #[test]
    fn run_persists_a_row_and_summary() {
        let tmp = TempDir::new().unwrap();
        let db = mk_db(&tmp, "s1");
        let r = req("foo");
        let probe = FakeProbe(vec![BackendId::Mock]);
        let c = ctx();
        let summary = run(&r, &db, &probe, &c.as_ref());
        assert!(summary.contains("insn-hits foo"), "{summary}");
        assert!(summary.contains("backend: mock"), "{summary}");
        assert!(summary.contains("hits:    42"), "{summary}");

        let stored = aggregate_by_target(&db);
        let rows = stored.get("foo").expect("missing target row");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hit_count, 42);
        assert_eq!(rows[0].backend, "mock");
        assert_eq!(rows[0].sample_basis, "exact");
    }

    #[test]
    fn run_explain_prints_why_before_summary() {
        let tmp = TempDir::new().unwrap();
        let db = mk_db(&tmp, "s2");
        let mut r = req("foo");
        r.explain = true;
        let probe = FakeProbe(vec![BackendId::Mock]);
        let c = ctx();
        let out = run(&r, &db, &probe, &c.as_ref());
        let why_pos = out.find("backend=mock").unwrap();
        let summary_pos = out.find("insn-hits foo").unwrap();
        assert!(why_pos < summary_pos, "--why should print before summary:\n{out}");
    }
}
