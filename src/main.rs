mod backend;
mod check;
mod commands;
mod daemon;
mod dap;
mod ghcprof;
mod init;
mod inspector;
mod phpprofile;
mod profile;
mod pty;
mod resolve;
mod transport_common;

use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use nix::unistd::{ForkResult, fork};

use backend::Registry;

/// Subcommands are not modeled as clap subcommands because the client
/// forwards most verbs to a long-lived daemon. The top-level `--help`
/// therefore listed only the global flags, leaving agents with no way
/// to enumerate what the tool actually supports. This block is shown
/// under clap's `after_help` so `dbg --help` stays self-documenting.
const SUBCOMMAND_HELP: &str = "\
Common commands (forwarded to the session daemon):

  Session lifecycle:
    start <type> <target>   Launch a debugger/profiler session
    status                  Show active session details
    kill                    Stop the active session
    sessions [--group]      List saved / live sessions
    save [label]            Persist the active session to .dbg/sessions/
    replay <label>          Re-open a saved session read-only
    finalize                Stop a profile collector cleanly so its
                            trace flushes and the daemon can serve queries
    prune [--older-than D]  Delete auto-saved sessions past age D
    diff <other>            Compare active session against another
    cross <symbol>          Aggregate all captured evidence for a symbol

  Debugger control:
    break <loc> [if <cond>] [log <msg>]
    continue | step | next | finish | pause | restart
    run [args...]
    stack | frame <n> | locals | print <expr> | set <lval> <expr>
    threads | thread <n> | watch <expr> | list [loc] | catch <evt>

  Captured evidence (works live or in replay):
    hits <loc> [--group-by F] [--count-by F --top N]
    hit-diff <loc> <a> <b>
    hit-trend <loc> <field>
    source <symbol> [radius]
    disasm <symbol> [--refresh]
    disasm-diff <a> <b>

  Adapter escape hatch:
    raw <native-command>    Send a literal command to the underlying tool
    tool                    Print which underlying tool is driving the session

Run `dbg help <verb>` inside a session for backend-specific details.";

#[derive(Parser)]
#[command(
    name = "dbg",
    version,
    about = "AI can read your code. Now it can live debug it too.",
    after_help = SUBCOMMAND_HELP,
)]
struct Cli {
    /// Initialize skill integration for an AI agent (`claude`, `codex`, `forge`),
    /// or print the skill's YAML frontmatter to stdout (`agent-context`) for
    /// piping into a harness SessionStart hook.
    #[arg(long)]
    init: Option<String>,

    /// Check backend dependencies (comma-separated types)
    #[arg(long, alias = "language")]
    backend: Option<String>,

    /// Internal: run the JIT disassembly REPL on a captured .asm file
    #[arg(long, hide = true)]
    jitdasm_repl: Option<String>,

    /// Internal: default filter pattern for the jitdasm REPL —
    /// remembers the `DOTNET_JitDisasm` filter so summary commands
    /// (`stats`, `simd`, `hotspots`) narrow to the user's methods
    /// by default instead of the whole capture.
    #[arg(long, hide = true, default_value = "")]
    jitdasm_pattern: String,

    /// Internal: run the profile REPL on a captured cachegrind file
    #[arg(long, hide = true)]
    phpprofile_repl: Option<String>,

    /// Internal: convert GHC .prof to callgrind format
    #[arg(long, hide = true, num_args = 2, value_names = &["PROF", "OUT"])]
    ghcprof_convert: Option<Vec<String>>,

    /// Internal: custom prompt for the profile REPL
    #[arg(long, hide = true, default_value = "php-profile> ")]
    profile_prompt: String,

    /// All remaining arguments
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

/// Top-level verbs that end the active session. `kill` is the
/// canonical spelling; `stop`/`quit`/`exit` are the words every other
/// dev tool uses for the same action, and users/agents reach for them
/// first. Before the alias existed, `dbg stop` reached the debugger
/// as raw input and surfaced as a cryptic pdb/lldb parse error.
fn is_kill_alias(verb: &str) -> bool {
    matches!(verb, "kill" | "stop" | "quit" | "exit")
}

fn main() -> Result<()> {
    // Hidden self-test hook: when DBG_DETACH_SELF_TEST is set, perform
    // the same fork/setsid/detach_stdio dance as `dbg start` around a
    // no-op "daemon" (sleep 5s). Used by the integration test in
    // `tests/daemon_detach.rs` to prove that fd 1/2 are released before
    // `dbg start` returns to its caller. Keeping this inside the real
    // binary guarantees the test exercises production code, not a
    // re-implementation.
    if std::env::var_os("DBG_DETACH_SELF_TEST").is_some() {
        let log_path =
            std::env::temp_dir().join(format!("dbg-detach-selftest-{}.log", std::process::id()));
        // Safety: fork duplicates the process
        let fork_result = unsafe { fork() }?;
        match fork_result {
            ForkResult::Child => {
                let _ = nix::unistd::setsid();
                daemon::detach_stdio(&log_path);
                std::thread::sleep(Duration::from_secs(5));
                std::process::exit(0);
            }
            ForkResult::Parent { .. } => {
                // Parent returns immediately — caller's pipe should
                // EOF as soon as we exit *if* the child detached stdio.
                return Ok(());
            }
        }
    }

    let cli = Cli::parse();
    let mut registry = Registry::new();
    registry.register(Box::new(backend::lldb::LldbBackend));
    registry.register(Box::new(backend::lldb_dap_proto::LldbDapProtoBackend));
    registry.register(Box::new(backend::pdb::PdbBackend::new()));
    registry.register(Box::new(backend::debugpy_proto::DebugpyProtoBackend));
    registry.register(Box::new(backend::netcoredbg::NetCoreDbgBackend));
    registry.register(Box::new(backend::netcoredbg_proto::NetCoreDbgProtoBackend));
    registry.register(Box::new(backend::delve::DelveBackend));
    registry.register(Box::new(backend::delve_proto::DelveProtoBackend));
    registry.register(Box::new(backend::jdb::JdbBackend));
    registry.register(Box::new(backend::pprof::PprofBackend));
    registry.register(Box::new(backend::perf::PerfBackend));
    registry.register(Box::new(backend::callgrind::CallgrindBackend));
    registry.register(Box::new(backend::pstats::PstatsBackend));
    registry.register(Box::new(backend::memcheck::MemcheckBackend));
    registry.register(Box::new(backend::massif::MassifBackend));
    registry.register(Box::new(backend::dotnettrace::DotnetTraceBackend));
    registry.register(Box::new(backend::import::ImportBackend));
    registry.register(Box::new(backend::jitdasm::JitDasmBackend));
    registry.register(Box::new(backend::phpdbg::PhpdbgBackend));
    registry.register(Box::new(backend::xdebug::XdebugProfileBackend));
    registry.register(Box::new(backend::rdbg::RdbgBackend));
    registry.register(Box::new(backend::stackprof::StackprofBackend));
    registry.register(Box::new(backend::ghci::GhciBackend));
    registry.register(Box::new(backend::ghcprof::GhcProfBackend));
    registry.register(Box::new(backend::ocamldebug::OcamlDebugBackend));
    registry.register(Box::new(backend::node_proto::NodeProtoBackend));
    registry.register(Box::new(backend::nodeprof::NodeProfBackend));

    // Auto-update installed skills if binary version changed
    init::auto_update(&registry);

    // --jitdasm-repl (internal: launched by the jitdasm backend)
    if let Some(asm_path) = &cli.jitdasm_repl {
        return dbg_cli::jitdasm::run_repl(asm_path, &cli.jitdasm_pattern).map_err(Into::into);
    }

    // --phpprofile-repl (internal: launched by profile backends)
    if let Some(cg_path) = &cli.phpprofile_repl {
        return phpprofile::run_repl(cg_path, &cli.profile_prompt).map_err(Into::into);
    }

    // --ghcprof-convert (internal: convert GHC .prof to callgrind format)
    if let Some(paths) = &cli.ghcprof_convert {
        return ghcprof::convert(&paths[0], &paths[1]);
    }

    // --init
    if let Some(target) = &cli.init {
        if target == "agent-context" {
            init::emit_skill_yaml();
            return Ok(());
        }
        return init::run_init(target, &registry);
    }

    // --backend
    if let Some(types_str) = &cli.backend {
        let types: Vec<&str> = types_str.split(',').map(|s| s.trim()).collect();
        let (results, unknown) = check::check_backends(&registry, &types);
        if !unknown.is_empty() {
            bail!(
                "unknown type(s): {} (available: {})",
                unknown.join(", "),
                registry.available_types().join(", ")
            );
        }
        print!("{}", check::format_results(&results));

        let all_ok = results.iter().all(|(_, deps)| deps.iter().all(|d| d.ok));
        if !all_ok {
            std::process::exit(1);
        }
        return Ok(());
    }

    // No subcommand args — show usage and backend status
    if cli.args.is_empty() {
        println!("dbg — AI can read your code. Now it can live debug it too.\n");
        println!("  dbg start <type> <target> [--break spec] [--args ...] [--run]");
        println!("  dbg <any debugger command>");
        println!("  dbg help            list available commands");
        println!("  dbg help <command>   ask the debugger what a command does");
        println!("  dbg kill\n");

        println!("backends:");
        for backend in registry.all_backends() {
            let (results, _) = check::check_backends(&registry, &[backend.name()]);
            let missing: Vec<&str> = results
                .iter()
                .flat_map(|(_, statuses)| statuses.iter().filter(|s| !s.ok).map(|s| s.name))
                .collect();
            let status = if missing.is_empty() {
                "ready".to_string()
            } else {
                format!("missing: {}", missing.join(", "))
            };
            println!(
                "  {:<14} {} [{}]",
                backend.name(),
                backend.description(),
                status
            );
        }
        return Ok(());
    }

    let first = cli.args[0].as_str();

    // `dbg <verb> --help` / `-h` — serve the static verb help before
    // any daemon check, so it works whether or not a session is live.
    // `dbg help <verb>` already does this; users also naturally reach
    // for `--help`, and that used to bail with "no session running".
    if cli.args.iter().any(|a| a == "--help" || a == "-h") {
        if let Some(text) = daemon::dbg_verb_help(first) {
            println!("{text}");
            return Ok(());
        }
    }

    match first {
        "start" => cmd_start(&registry, &cli.args[1..]),
        "attach" => {
            // Intercept client-side: `dbg attach` is not a verb. Without
            // this the arg falls through to the debugger backend (pdb
            // etc.) and surfaces as a cryptic `*** SyntaxError: invalid
            // syntax`, because pdb tries to parse `attach <label>` as
            // Python.
            eprintln!(
                "`dbg attach` is not a verb. Did you mean:\n  \
                 dbg start <type> <target> --attach-pid <PID>   (attach live to a process)\n  \
                 dbg replay <label>                             (re-open a saved session; see `dbg sessions`)"
            );
            std::process::exit(2);
        }
        // `stop` is the verb every other dev tool uses to end a
        // session — users reach for it before `kill`. Previously it
        // was forwarded to the debugger, where pdb/lldb/jdb report it
        // as an unknown command with no hint that `dbg kill` exists.
        v if is_kill_alias(v) => {
            let msg = daemon::kill_daemon()?;
            println!("{msg}");
            Ok(())
        }
        "status" if !daemon::is_running() => {
            println!("no session");
            Ok(())
        }
        "sessions" if !daemon::is_running() => {
            // Allow listing without a live daemon — same output as
            // when live except no "* currently live" marker. Show
            // peer daemons first (the bare cwd slot may be empty but
            // other pid-suffixed daemons can still be running).
            print_live_daemon_peers();
            let cwd = std::env::current_dir()?;
            let ctx = commands::lifecycle::LifeCtx {
                cwd: &cwd,
                active: None,
            };
            let l = commands::lifecycle::Lifecycle::Sessions { group_only: false };
            println!("{}", commands::lifecycle::run(&l, &ctx));
            Ok(())
        }
        "sessions" => {
            print_live_daemon_peers();
            let cmd = cli.args.join(" ");
            let resp = daemon::send_command(&cmd)?;
            println!("{resp}");
            Ok(())
        }
        "replay" => cmd_replay(&cli.args[1..]),
        "import" => cmd_import(&registry, &cli.args[1..]),
        "finalize" => cmd_finalize(),
        "diff" if cli.args.len() == 3 || (!daemon::is_running() && cli.args.len() == 2) => {
            // Client-side diff between two saved sessions (or between
            // one saved session and... well, the parser handles a
            // single label as "active vs other", which fails cleanly
            // when no daemon is running). The two-arg form is the
            // regression-hunt path: open both DBs read-only,
            // dispatch to lifecycle::run.
            let cwd = std::env::current_dir()?;
            let l = match commands::lifecycle::try_dispatch(&cli.args.join(" ")) {
                Some(commands::Dispatched::Lifecycle(l)) => l,
                Some(commands::Dispatched::Immediate(s)) => {
                    println!("{s}");
                    return Ok(());
                }
                _ => bail!("internal: diff parser returned wrong variant"),
            };
            let ctx = commands::lifecycle::LifeCtx {
                cwd: &cwd,
                active: None,
            };
            println!("{}", commands::lifecycle::run(&l, &ctx));
            Ok(())
        }
        "help" => {
            if cli.args.len() > 1 {
                // dbg help <topic> — serve dbg-level verbs client-side
                // so they work without a running daemon. Only fall
                // through to the daemon (for backend-specific help)
                // when the topic is *not* a known dbg verb.
                let topic = cli.args[1..].join(" ");
                if let Some(text) = daemon::dbg_verb_help(topic.trim()) {
                    println!("{text}");
                    return Ok(());
                }
                ensure_running()?;
                let resp = daemon::send_command(&format!("help {topic}"))?;
                println!("{resp}");
                Ok(())
            } else if daemon::is_running() {
                let resp = daemon::send_command("help")?;
                println!("{resp}");
                Ok(())
            } else {
                println!("dbg — unified debug CLI\n");
                println!("  dbg start <type> <target> [--break spec] [--args ...] [--run]");
                println!("  dbg <any debugger command>");
                println!("  dbg help            list available commands");
                println!("  dbg help <command>   help for a specific verb\n");
                println!(
                    "session lifecycle:  start, run, continue, step, next, finish, kill, status, cancel, finalize"
                );
                println!("inspection:         break, locals, stack, print");
                println!("crosstrack (DB):    hits, hit-diff, hit-trend, cross, disasm, source");
                println!("persistence:        sessions, save, replay");
                println!("timeline:           events\n");
                println!("types: {}", registry.available_types().join(", "));
                Ok(())
            }
        }
        _ => {
            // Passthrough to running daemon
            ensure_running()?;
            let cmd = cli.args.join(" ");
            let resp = daemon::send_command(&cmd)?;
            println!("{resp}");
            Ok(())
        }
    }
}

/// Client-side `dbg replay <label>`: opens a persisted SessionDb
/// read-only and runs a minimal crosstrack REPL against it. No live
/// debugger, so only DB-backed verbs (hits, hit-diff, hit-trend,
/// cross, disasm, source) are honored. All other verbs return a
/// clear "live debugger not attached to a replay" error.
fn cmd_replay(args: &[String]) -> Result<()> {
    use std::io::{BufRead, Write};
    if args.is_empty() {
        bail!("usage: dbg replay <label>  (see `dbg sessions` for labels)");
    }
    // Reap stale pid/socket files from a crashed previous daemon so
    // replay doesn't false-positive on "live session running".
    daemon::clean_stale_runtime_files();
    if daemon::is_running() {
        bail!(
            "a live session is running in this cwd — `dbg kill` it first, then \
             `dbg replay {}`",
            args[0]
        );
    }
    let cwd = std::env::current_dir()?;
    let sessions_dir = dbg_cli::session_db::sessions_dir(&cwd);
    let label = &args[0];
    let path = if std::path::Path::new(label).exists() {
        std::path::PathBuf::from(label)
    } else {
        sessions_dir.join(format!("{label}.db"))
    };
    // Fall back to matching on the DB's stored `sessions.label` column
    // when the filename-stem lookup misses. `dbg sessions` prints the
    // stored label (e.g. `broken-20260419-154358`) but files are named
    // by their filename stem (e.g. `session-10.db`); copy-pasting what
    // `dbg sessions` showed would otherwise fail with "no session at …".
    let path = if path.exists() {
        path
    } else if let Some(by_label) = find_session_by_label(&sessions_dir, label) {
        by_label
    } else {
        bail!(
            "no session matching `{label}` — got neither a file `{}` nor any \
             saved DB whose `label` column matches. `dbg sessions` lists \
             what's available.",
            path.display()
        );
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(-1);
    if v != dbg_cli::session_db::SCHEMA_VERSION {
        bail!(
            "session `{}` has schema_version={v}, expected {} — re-collect to replay",
            path.display(),
            dbg_cli::session_db::SCHEMA_VERSION
        );
    }
    let db = dbg_cli::session_db::SessionDb::open_read_only(&path)?;

    // Dump high-level info, then either execute a one-shot query from
    // any trailing args or drop into a minimal REPL.
    let (target, target_class): (String, String) = db
        .conn()
        .query_row(
            "SELECT target, target_class FROM sessions LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or_else(|_| ("?".into(), "?".into()));

    use std::str::FromStr;
    let target_class_enum = dbg_cli::session_db::TargetClass::from_str(&target_class)
        .unwrap_or(dbg_cli::session_db::TargetClass::NativeCpu);

    // Profile-mode replay: rehydrate the in-memory `ProfileData` from
    // the source content stashed in `meta.profile_raw` at session start.
    // Without this, `dbg replay` on a profile session would be
    // write-only — top/callers/callees would all bail because no live
    // backend is attached. With it, every profile REPL verb works
    // identically against a saved DB.
    let mut profile = load_profile_from_db(&db);

    let is_profile = matches!(db.kind(), dbg_cli::session_db::SessionKind::Profile);
    eprintln!("replay `{label}` (target={target}, class={target_class}) — read-only REPL");
    if is_profile && profile.is_some() {
        eprintln!(
            "supported: top, callers, callees, traces, tree, hotpath, threads, stats, \
             search, focus, ignore, reset, plus crosstrack verbs (cross, disasm, source)"
        );
    } else if is_profile {
        eprintln!(
            "[warn] profile session has no persisted source — profile verbs unavailable. \
             Re-collect with a recent dbg to enable replay queries."
        );
        eprintln!("supported: hits, hit-diff, hit-trend, cross, disasm, source, sessions");
    } else {
        eprintln!("supported: hits, hit-diff, hit-trend, cross, disasm, source, sessions");
    }
    eprintln!("type `quit` or EOF to exit");

    // One-shot mode: `dbg replay <label> hits foo:42`
    if args.len() > 1 {
        let cmd = args[1..].join(" ");
        let out = replay_eval(
            &cmd,
            &db,
            &cwd,
            &target,
            target_class_enum,
            profile.as_mut(),
        );
        println!("{out}");
        return Ok(());
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    loop {
        write!(stdout, "replay> ")?;
        stdout.flush()?;
        line.clear();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let cmd = line.trim();
        if cmd.is_empty() {
            continue;
        }
        if matches!(cmd, "quit" | "exit" | "q") {
            break;
        }
        let out = replay_eval(cmd, &db, &cwd, &target, target_class_enum, profile.as_mut());
        println!("{out}");
    }
    Ok(())
}

/// Rebuild a `ProfileData` from the source content stashed in the DB
/// at session start. Returns `None` when the meta keys are missing
/// (debug session, or profile session captured before persistence
/// landed) or when parsing fails.
pub(crate) fn load_profile_from_db(
    db: &dbg_cli::session_db::SessionDb,
) -> Option<profile::ProfileData> {
    let content = db.meta("profile_raw").ok().flatten()?;
    let ext = db.meta("profile_raw_ext").ok().flatten();
    profile::ProfileData::load_str(&content, ext.as_deref()).ok()
}

fn replay_eval(
    cmd: &str,
    db: &dbg_cli::session_db::SessionDb,
    cwd: &std::path::Path,
    target: &str,
    target_class: dbg_cli::session_db::TargetClass,
    profile: Option<&mut profile::ProfileData>,
) -> String {
    // Profile REPL verbs (top/callers/callees/…) are owned by the
    // in-memory ProfileData — short-circuit before the dispatcher,
    // matching how the live daemon routes them in handle_command.
    if daemon::is_profile_repl_verb(cmd) {
        return match profile {
            Some(p) => p.handle_command(cmd),
            None => "no profile data available in this session — profile verbs require \
                     a profile-kind session captured with dbg ≥ schema_version 1 (try \
                     `dbg sessions` to check the kind)"
                .to_string(),
        };
    }
    match commands::dispatch_no_backend(cmd) {
        Some(commands::Dispatched::Immediate(s)) => s,
        Some(commands::Dispatched::Query(q)) => {
            if matches!(
                q,
                commands::crosstrack::Query::Disasm { .. }
                    | commands::crosstrack::Query::AtHitDisasm
            ) {
                return "replay is read-only: disassembly collection is unavailable; use cached disasm rows or run `dbg disasm` in a live session".into();
            }
            let ctx = commands::crosstrack::RunCtx {
                target,
                target_class,
                cwd,
                live: None,
            };
            commands::crosstrack::run(&q, db, &ctx)
        }
        Some(commands::Dispatched::Lifecycle(l)) => {
            if matches!(
                l,
                commands::lifecycle::Lifecycle::Save { .. }
                    | commands::lifecycle::Lifecycle::Prune { .. }
                    | commands::lifecycle::Lifecycle::Replay { .. }
            ) {
                return "replay is read-only: save, prune, and nested replay are unavailable"
                    .into();
            }
            let ctx = commands::lifecycle::LifeCtx {
                cwd,
                active: Some(db),
            };
            commands::lifecycle::run(&l, &ctx)
        }
        _ => "replay only supports crosstrack + lifecycle verbs (hits, hit-diff, \
             hit-trend, cross, disasm, source, sessions, status) plus profile verbs \
             (top, callers, callees, …) on profile-kind sessions. Live debugger \
             verbs (step, continue, break, …) aren't available — start a new \
             session with `dbg start` for those."
            .to_string(),
    }
}

/// Search `.dbg/sessions/` for a DB whose `sessions.label` column
/// matches `label`, returning its file path. Falls back for `dbg replay`
/// when the user copied a label from `dbg sessions` (which shows the
/// stored label, not the filename stem).
fn find_session_by_label(dir: &std::path::Path, label: &str) -> Option<std::path::PathBuf> {
    if !dir.exists() {
        return None;
    }
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("db") {
            continue;
        }
        let conn = match rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let got: Result<String, _> =
            conn.query_row("SELECT label FROM sessions LIMIT 1", [], |r| r.get(0));
        if got.ok().as_deref() == Some(label) {
            return Some(path);
        }
    }
    None
}

fn ensure_running() -> Result<()> {
    if !daemon::is_running() {
        bail!("no session running — use: dbg start <type> <target>");
    }
    Ok(())
}

/// Client-side `dbg finalize`: stop a profile/sample collector cleanly
/// so its on-disk trace flushes and the daemon can transition into
/// query mode.
///
/// This is a *client-side* operation — it does NOT go through the
/// Import a previously-collected profile snapshot into a fresh
/// session. Bridges externally-collected traces (`dotnet-trace
/// --output foo.nettrace`, `perf script > foo.txt`, V8 `.cpuprofile`,
/// raw speedscope JSON, …) into the same profile-mode REPL that fresh
/// `dbg start dotnet-trace` would expose.
///
/// `--label <name>` overrides the auto-generated session slug, so the
/// imported snapshot shows up in `dbg sessions` under a memorable name
/// and can be reopened later with `dbg replay <name>`.
fn cmd_import(registry: &Registry, args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!(
            "usage: dbg import <profile-file> [--label <name>]\n  \
             accepts: .nettrace, .speedscope.json, .cpuprofile, perf-script text, pprof-traces text"
        );
    }

    // Parse cmd_import-specific flags. `--label <name>` is consumed
    // here; any other token is the profile file. Once we delegate to
    // `cmd_start` below, no more flag parsing happens at this layer.
    let mut file: Option<String> = None;
    let mut label: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--label" => {
                i += 1;
                if i >= args.len() {
                    bail!("--label requires a name");
                }
                label = Some(args[i].clone());
            }
            other if other.starts_with("--label=") => {
                let v = other.trim_start_matches("--label=");
                if v.is_empty() {
                    bail!("--label requires a name");
                }
                label = Some(v.to_string());
            }
            other if other.starts_with("--") => {
                bail!("unknown flag for `dbg import`: {other}");
            }
            other if file.is_none() => {
                file = Some(other.to_string());
            }
            other => {
                bail!("unexpected positional argument: {other}");
            }
        }
        i += 1;
    }
    let file = file.ok_or_else(|| anyhow::anyhow!("missing <profile-file>"))?;

    // Verify the file exists and resolve to absolute. The daemon's
    // cwd may differ (and after fork its perspective is the daemon's
    // cwd snapshot at start), so a relative path could mis-resolve
    // inside `bash -c "cp <file> ..."`. Anchor it now.
    let path = std::path::Path::new(&file);
    if !path.exists() {
        bail!("profile file does not exist: {file}");
    }
    let abs = std::fs::canonicalize(path)
        .with_context(|| format!("failed to canonicalize profile path: {file}"))?;

    // Target-aware preflight: `.nettrace` is binary and requires
    // `dotnet-trace convert`. We don't list this as an unconditional
    // ImportBackend dependency because users importing speedscope JSON
    // shouldn't be forced to install dotnet-trace.
    if backend::import::extension_matches(&abs.display().to_string(), "nettrace")
        && which::which("dotnet-trace").is_err()
    {
        bail!(
            "importing a `.nettrace` requires `dotnet-trace` on PATH (install: \
             `dotnet tool install -g dotnet-trace` and add `~/.dotnet/tools` to PATH)"
        );
    }

    // Apply the user-chosen label by setting `DBG_SESSION` before
    // delegating into `cmd_start` — `allocate_slug` reads it and
    // routes the resulting daemon/session DB filename through it.
    if let Some(name) = label {
        // Mirror `daemon::sanitize_slug` (private): keep alphanumerics,
        // `-`, and `_`; replace anything else with `_`. Reject inputs
        // that produce an empty / all-underscore slug because those
        // collide with the auto-generated cwd slug.
        let sanitized: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if sanitized.is_empty() || sanitized.chars().all(|c| c == '_') {
            bail!("--label `{name}` sanitizes to empty/all-underscore (allowed: [A-Za-z0-9-_])");
        }
        // SAFETY: cmd_import runs on the single client thread before
        // any daemon fork; matching the contract used by cmd_start.
        // Set both: DBG_SESSION drives the runtime slug (socket / pid
        // file naming, peer discovery), DBG_LABEL drives the
        // persisted session-DB label used by `dbg sessions` and
        // `dbg replay <name>`.
        unsafe {
            std::env::set_var("DBG_SESSION", &sanitized);
            std::env::set_var("DBG_LABEL", &sanitized);
        }
    }

    // Reuse the regular start pipeline. `import` is a registered
    // backend; the only thing that makes this verb its own front door
    // (vs. typing `dbg start import <file>`) is the convenient label
    // flag and the file existence preflight above.
    let start_args: Vec<String> = vec!["import".into(), abs.display().to_string()];
    cmd_start(registry, &start_args)
}

/// daemon socket. The daemon's accept loop only starts after init
/// completes, but profile backends (dotnet-trace, perf, callgrind)
/// block init for the entire lifetime of the target process. So while
/// init is running there's no listener to receive a `dbg cancel`,
/// even though the kernel happily queues the connection. `finalize`
/// sidesteps that by reading the daemon's pid file and walking its
/// child tree directly.
///
/// All profile backends spawn `bash` as their direct child (PTY-driven
/// init script), and bash spawns the actual collector. We send SIGINT
/// to bash's foreground process group (i.e. the collector) — this
/// matches what Ctrl-C from a real terminal would do via the PTY's
/// tty discipline, which is exactly what dotnet-trace / perf-record
/// expect to receive in order to flush their trace and exit.
fn cmd_finalize() -> Result<()> {
    let report = daemon::finalize_collector()?;

    println!(
        "finalize: SIGINT sent to {} collector process(es): {:?}",
        report.signalled.len(),
        report.signalled
    );
    if !report.failed.is_empty() {
        eprintln!(
            "finalize: {} child(ren) could not be signalled: {:?}",
            report.failed.len(),
            report.failed
        );
    }
    println!(
        "the collector should now flush its trace and exit. Conversion may still \
         be running; poll with `dbg status` or run `dbg top` once the daemon \
         starts serving profile queries."
    );
    Ok(())
}

/// Emit a header listing every live daemon in the current cwd, with a
/// `*` next to the one the current process resolves to. Suppressed
/// entirely when only one (or zero) live daemons exist — the normal
/// case. Used at the top of `dbg sessions`.
fn print_live_daemon_peers() {
    let peers = daemon::live_slugs_in_cwd();
    if peers.len() <= 1 {
        return;
    }
    let active = std::env::var("DBG_SESSION").ok().or_else(|| {
        std::fs::read_to_string(daemon::latest_pointer_path())
            .ok()
            .map(|s| s.trim().to_string())
    });
    println!("live daemons in this cwd:");
    for slug in &peers {
        let marker = if active.as_deref() == Some(slug.as_str()) {
            "*"
        } else {
            " "
        };
        println!("  {marker} {slug}");
    }
    println!("  (set DBG_SESSION=<slug> to target a specific one)\n");
}

/// Pick a backend from a target filename when the user omits the type.
/// Unambiguous extensions only — binaries (no extension) and shared
/// types (.cs can be script or project) still require an explicit type.
fn autodetect_backend(target: &str) -> Option<&'static str> {
    let lower = target.to_ascii_lowercase();
    if lower.ends_with(".py") {
        Some("pdb")
    } else if lower.ends_with(".go") {
        // delve-proto (DAP) is the headless variant — delve without
        // DAP needs an interactive TTY and doesn't work under our
        // PTY transport when driven non-interactively.
        Some("delve-proto")
    } else if lower.ends_with(".java") {
        Some("jdb")
    } else if lower.ends_with(".rb") {
        Some("rdbg")
    } else if lower.ends_with(".php") {
        Some("phpdbg")
    } else if lower.ends_with(".csproj") {
        Some("netcoredbg")
    } else if lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".ts") {
        Some("node-proto")
    } else if lower.ends_with(".hs") {
        Some("ghci")
    } else if lower.ends_with(".ml") {
        Some("ocamldebug")
    } else {
        None
    }
}

/// Output of [`parse_start_flags`]. Pure data so the parser can be
/// tested without the surrounding daemon/registry machinery.
#[derive(Debug)]
struct ParsedStartFlags {
    breakpoints: Vec<String>,
    run_args: Vec<String>,
    do_run: bool,
    attach_pid: Option<u32>,
}

/// Parse the trailing flag tail of `dbg start <type> <target> …`.
/// `args` is the slice **after** type+target. Behaviour:
///   - `--break SPEC` / `-b SPEC`: append to `breakpoints`.
///   - `--run` / `-r`: set `do_run`.
///   - `--attach-pid N` / `--attach-pid=N`: set `attach_pid`.
///   - `--args …`: forward everything that follows to the debuggee.
///     Bails if a known dbg flag (`--break`, `--run`, `--attach-pid`,
///     `--attach-port`) appears after `--args` — that misroute used
///     to silently pass the flag to the debuggee.
///   - any other token: appended to `run_args`.
fn parse_start_flags(args: &[String]) -> Result<ParsedStartFlags> {
    let mut breakpoints = Vec::new();
    let mut run_args = Vec::new();
    let mut do_run = false;
    let mut attach_pid: Option<u32> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--break" | "-b" => {
                i += 1;
                if i < args.len() {
                    breakpoints.push(args[i].clone());
                }
            }
            "--args" | "-a" => {
                // Everything after `--args` belongs to the debuggee,
                // including hyphenated flags like `--commit-every`.
                // Stopping at the next `--` token used to drop those
                // silently into the catch-all below.
                //
                // Catch dbg's own flags *before* we forward — putting
                // `--break`/`--run`/`--attach-pid` after `--args` is a
                // common mistake and used to silently misroute the
                // flag to the debuggee, which then ignored or rejected
                // it. Better to fail fast with a fix-instruction.
                i += 1;
                while i < args.len() {
                    let tok = args[i].as_str();
                    let is_dbg_flag = matches!(
                        tok,
                        "--break" | "-b" | "--run" | "-r" | "--attach-pid" | "--attach-port"
                    ) || tok.starts_with("--attach-pid=")
                        || tok.starts_with("--attach-port=");
                    if is_dbg_flag {
                        bail!(
                            "dbg flag `{tok}` appeared after `--args`. `--args` forwards everything that follows to the debuggee, so dbg flags must come before it. Move `{tok}` (and any `--break`/`--run`/`--attach-*` flags) before `--args`."
                        );
                    }
                    run_args.push(args[i].clone());
                    i += 1;
                }
                continue;
            }
            "--run" | "-r" => do_run = true,
            "--attach-pid" => {
                i += 1;
                if i < args.len() {
                    attach_pid = Some(
                        args[i]
                            .parse()
                            .with_context(|| format!("invalid --attach-pid `{}`", args[i]))?,
                    );
                } else {
                    bail!("--attach-pid requires a PID");
                }
            }
            other if other.starts_with("--attach-pid=") => {
                let pid = other.trim_start_matches("--attach-pid=");
                if pid.is_empty() {
                    bail!("--attach-pid requires a PID");
                }
                attach_pid = Some(
                    pid.parse()
                        .with_context(|| format!("invalid --attach-pid `{pid}`"))?,
                );
            }
            other => {
                // Bare positionals and unknown `--*` flags both go to
                // the debuggee. dbg's own flags are a closed set above
                // (`--break`, `--args`, `--run`, `--attach-pid`), so any
                // long option that lands here is meant for the program.
                // Silently dropping those used to break invocations like
                // `dbg start <t> <target> ./bench --commit-every 1000`.
                run_args.push(other.to_string());
            }
        }
        i += 1;
    }
    Ok(ParsedStartFlags {
        breakpoints,
        run_args,
        do_run,
        attach_pid,
    })
}

fn normalize_start_args(registry: &Registry, args: &[String]) -> Result<Vec<String>> {
    if args.is_empty() {
        bail!("usage: dbg start <type> <target> [--break spec] [--args ...] [--run]");
    }
    if args
        .first()
        .is_some_and(|a| a == "--attach-pid" || a.starts_with("--attach-pid="))
    {
        bail!(
            "usage: dbg start <type> <target> --attach-pid <PID>\n       --attach-pid must come after <type> <target>; attach mode still needs an explicit DAP backend type and target hint"
        );
    }
    if args
        .get(1)
        .is_some_and(|a| a == "--attach-pid" || a.starts_with("--attach-pid="))
    {
        bail!(
            "usage: dbg start <type> <target> --attach-pid <PID>\n       attach mode still needs a target hint before --attach-pid; use an absolute target/path hint when source paths matter"
        );
    }
    // Single-arg form: `dbg start <target>` — infer backend from the
    // target's extension. Unambiguous only; unknown extensions bail
    // with the standard usage. `dbg start <type> <target>` (two args)
    // still takes the explicit path.
    let normalized: Vec<String> = if args.len() == 1 {
        match autodetect_backend(&args[0]) {
            Some(t) => {
                let mut v = vec![t.to_string()];
                v.extend_from_slice(args);
                v
            }
            None => bail!(
                "usage: dbg start <type> <target> [--break spec] [--args ...] [--run]\n\
                 (no type given and couldn't infer one from `{}` — supported extensions: \
                 .py .go .java .rb .php .csproj .js .ts .hs .ml)",
                args[0]
            ),
        }
    } else if args.len() >= 2 && registry.get(&args[0]).is_none() {
        // First token isn't a known backend — maybe the user omitted
        // the type entirely and args[0] is the target path.
        if let Some(t) = autodetect_backend(&args[0]) {
            let mut v = vec![t.to_string()];
            v.extend_from_slice(args);
            v
        } else {
            args.to_vec()
        }
    } else {
        args.to_vec()
    };
    if normalized.len() < 2 {
        bail!("usage: dbg start <type> <target> [--break spec] [--args ...] [--run]");
    }
    Ok(normalized)
}

fn cmd_start(registry: &Registry, args: &[String]) -> Result<()> {
    let args = normalize_start_args(registry, args)?;
    let args = args.as_slice();

    // Reap orphaned pid/socket files from a crashed previous daemon
    // so allocate_slug doesn't treat a dead socket as "live".
    daemon::clean_stale_runtime_files();

    // Allocate a slug for this session. If another daemon already
    // owns the bare cwd slot, we coexist by appending our pid rather
    // than evicting the existing daemon. Explicit DBG_SESSION names
    // that collide fail loudly so named-slot semantics stay honest.
    let slug = daemon::allocate_slug()?;
    // SAFETY: set_var is unsafe in threaded contexts. cmd_start is
    // still single-threaded at this point (fork hasn't happened).
    unsafe {
        std::env::set_var("DBG_SESSION", &slug);
    }
    // Publish this as the newest daemon in the cwd so env-less
    // clients in other shells find it by default.
    daemon::write_latest_pointer(&slug);
    let peers = daemon::live_slugs_in_cwd();
    if !peers.is_empty() {
        eprintln!(
            "session: {slug}  (coexisting with: {})",
            peers
                .iter()
                .filter(|s| *s != &slug)
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        );
        eprintln!("  other shells: set DBG_SESSION={slug} to target this session");
    } else {
        eprintln!("session: {slug}");
    }

    let backend_type = &args[0];
    let target_raw = &args[1];

    // Intercept GPU-related types — the agent should use gdbg, not dbg
    match backend_type.as_str() {
        "gdbg" | "gpu" | "cuda" | "pytorch" | "triton" | "tensorflow" | "tf" | "jax" | "mxnet"
        | "cupy" => {
            eprintln!("GPU profiling uses gdbg, not dbg.");
            eprintln!();
            eprintln!("  gdbg {target_raw}          # collect + analyze");
            eprintln!("  gdbg --from <name>        # reload saved session");
            eprintln!("  gdbg check                # verify nsys/ncu installed");
            eprintln!();
            eprintln!("gdbg auto-detects the target type (CUDA, PyTorch, Triton).");
            eprintln!("It collects GPU timeline (nsys), hardware metrics (ncu),");
            eprintln!("and op mapping (torch.profiler) into a single session,");
            eprintln!("then opens an interactive REPL with 30+ analysis commands.");
            bail!("use gdbg instead of dbg for GPU profiling");
        }
        _ => {}
    }

    let backend = registry.get(backend_type).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown type: {backend_type} (available: {})",
            registry.available_types().join(", ")
        )
    })?;

    // Runtime preflight (kernel settings etc.) — separate from binary
    // dependency checks. Surfaces clear, actionable errors before we
    // fork the daemon; a silent daemon crash post-fork leaves the
    // agent with an empty capture and no diagnostic.
    if let Err(e) = backend.preflight() {
        bail!(e);
    }

    // Check dependencies before attempting to spawn
    let (results, _) = check::check_backends(registry, &[backend_type]);
    let missing: Vec<_> = results
        .iter()
        .flat_map(|(_, deps)| deps.iter().filter(|d| !d.ok))
        .collect();
    if !missing.is_empty() {
        eprintln!("missing dependencies:");
        for d in &missing {
            eprintln!("  {}: {}", d.name, d.install);
        }
        bail!("install missing dependencies and retry");
    }

    // Parse flags. Positional tokens that don't match a known flag
    // are collected into run_args — this is what `dbg start jitdasm
    // Broken.csproj 'Program:SumFast' --run` needs so the backend
    // sees the pattern. Previously those tokens were silently
    // dropped, so jitdasm's filter never reached the runtime.
    let parsed = parse_start_flags(&args[2..])?;
    let breakpoints = parsed.breakpoints;
    let run_args = parsed.run_args;
    let do_run = parsed.do_run;
    let attach_pid = parsed.attach_pid;
    if attach_pid.is_some() && !backend.uses_dap() {
        bail!(
            "--attach-pid requires a DAP backend such as netcoredbg-proto, debugpy-proto, delve-proto, or lldb-dap-proto; `{backend_type}` is not DAP-based"
        );
    }
    let attach = attach_pid.map(|pid| backend::AttachSpec { pid: Some(pid) });

    // Resolve target. Attach mode doesn't need a local target file —
    // the debuggee is already running — so skip resolution and pass
    // the raw value through for logging.
    let resolved = if attach.is_some() {
        target_raw.clone()
    } else {
        resolve::resolve(backend_type, target_raw)?
    };
    eprintln!("target: {resolved}");

    // Fork daemon. Redirect the child's stderr to a per-session log
    // file so that when the daemon dies before publishing the socket
    // (common when the backend spawn fails — silent until now) the
    // parent can surface the captured message instead of just
    // "daemon failed to start".
    let log_path = daemon::startup_log_path();
    let _ = std::fs::remove_file(&log_path);
    // Safety: fork duplicates the process
    let fork_result = unsafe { fork() }?;
    match fork_result {
        ForkResult::Child => {
            // Daemon process
            let _ = nix::unistd::setsid();
            daemon::detach_stdio(&log_path);
            if let Err(e) = daemon::run_daemon(backend, &resolved, &run_args, attach.as_ref()) {
                eprintln!("daemon error: {e:#}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        ForkResult::Parent { .. } => {
            // Wait for socket
            if !daemon::wait_for_socket(Duration::from_secs(30)) {
                let log = std::fs::read_to_string(&log_path).unwrap_or_default();
                if log.trim().is_empty() {
                    bail!("daemon failed to start");
                } else {
                    bail!("daemon failed to start:\n{}", log.trim());
                }
            }

            // The socket may be bound *before* the backend itself has
            // Wait for the daemon to be ready, then confirm it's
            // alive before returning. "Ready" here means: pid file
            // exists, pid is alive, and the listener socket is
            // bound. We poll those over a 5s window — long enough
            // for slow backends like dotnet-trace, where the
            // collector forks `dotnet` which then JITs and loads
            // the target assembly before the command channel
            // accepts traffic, and a sub-second probe consistently
            // races the bind. We do NOT round-trip a command
            // through the backend here: a backend whose dispatcher
            // hasn't finished initializing answers EAGAIN, which
            // looks indistinguishable from a dead daemon to the
            // client and produces the bogus "daemon exited" bail
            // even though `ps` shows pid+collector+debuggee all
            // alive. Genuine backend exec failures still surface:
            // the daemon process itself exits when its child fails
            // to launch, so `is_running()` returns false within the
            // 5s window and we bail with whatever the daemon
            // wrote to its startup log.
            let probe_deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut healthy = false;
            loop {
                std::thread::sleep(Duration::from_millis(100));
                if daemon::is_running() {
                    healthy = true;
                    break;
                }
                if std::time::Instant::now() >= probe_deadline {
                    break;
                }
            }
            if !healthy {
                let log = std::fs::read_to_string(&log_path).unwrap_or_default();
                let log = log.trim();
                if log.is_empty() {
                    bail!("daemon started but exited before the debugger was ready");
                } else {
                    bail!("daemon started but exited before the debugger was ready:\n{log}");
                }
            }

            // Set breakpoints FIRST — some adapters (delve, DAP) need
            // every breakpoint registered before the program starts,
            // otherwise they never fire. If any `--break` fails we
            // refuse to auto-run to avoid the silent "ran past the
            // breakpoint" failure mode.
            let mut bp_ok = true;
            for bp in &breakpoints {
                let cmd = if backend.canonical_ops().is_some() {
                    format!("break {bp}")
                } else {
                    backend.format_breakpoint(bp)
                };
                let resp = daemon::send_command(&cmd)?;
                println!("{resp}");
                let lc = resp.to_lowercase();
                if lc.contains("[error")
                    || lc.contains("could not")
                    || lc.contains("cannot find")
                    || lc.contains("no source")
                    || lc.contains("unable to set")
                    || lc.contains("blank or comment")
                {
                    bp_ok = false;
                    if lc.contains("blank or comment") {
                        eprintln!(
                            "dbg: `{bp}` points at a blank/comment line — pdb won't stop there. \
                             Pick an executable line (or use `--break <function_name>`)."
                        );
                    }
                }
            }

            // Auto-run — but only when every breakpoint stuck. --run
            // means "start the debuggee (and let it stop at your
            // breakpoints)", not "run past all breakpoints". See
            // `dbg help start`.
            if do_run {
                if !bp_ok && !breakpoints.is_empty() {
                    eprintln!(
                        "dbg: skipping --run because a breakpoint failed to register. \
                         Fix the breakpoint or omit --break and drive with `dbg run` manually."
                    );
                } else {
                    let cmd = if backend.canonical_ops().is_some() {
                        "run".to_string()
                    } else {
                        backend.run_command().to_string()
                    };
                    let resp = daemon::send_command(&cmd)?;
                    println!("{resp}");
                }
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autodetect_go_prefers_dap() {
        // delve (PTY) needs a TTY and hangs under non-interactive
        // drivers — auto-detect must route .go to the DAP variant.
        assert_eq!(autodetect_backend("main.go"), Some("delve-proto"));
        assert_eq!(autodetect_backend("MAIN.GO"), Some("delve-proto"));
    }

    #[test]
    fn autodetect_unambiguous_extensions() {
        assert_eq!(autodetect_backend("broken.py"), Some("pdb"));
        assert_eq!(autodetect_backend("App.java"), Some("jdb"));
        assert_eq!(autodetect_backend("script.rb"), Some("rdbg"));
        assert_eq!(autodetect_backend("site.php"), Some("phpdbg"));
        assert_eq!(autodetect_backend("proj.csproj"), Some("netcoredbg"));
        assert_eq!(autodetect_backend("app.js"), Some("node-proto"));
        assert_eq!(autodetect_backend("app.ts"), Some("node-proto"));
        assert_eq!(autodetect_backend("foo.hs"), Some("ghci"));
        assert_eq!(autodetect_backend("main.ml"), Some("ocamldebug"));
        assert_eq!(autodetect_backend("bin/no-ext"), None);
    }

    #[test]
    fn kill_aliases_cover_common_stop_verbs() {
        // Regression: `dbg stop` used to reach the debugger as raw
        // input (pdb/lldb reported "*** NameError: name 'stop' is
        // not defined"). Every dev tool uses `stop`/`quit`/`exit` as
        // end-session verbs; the dispatcher must treat all four as
        // aliases for `kill` so the agent never has to guess.
        for alias in ["kill", "stop", "quit", "exit"] {
            assert!(is_kill_alias(alias), "`{alias}` must end the session");
        }
        for non_alias in ["start", "break", "sessions", "hits", "continue"] {
            assert!(
                !is_kill_alias(non_alias),
                "`{non_alias}` must not be treated as kill"
            );
        }
    }

    /// Regression: `dbg sessions` prints the DB's stored label
    /// (e.g. `broken-20260419-154358`), but on disk sessions are named
    /// after a filename stem (e.g. `session-10.db`). Users copying the
    /// label into `dbg replay` hit "no session at …/broken-…-.db".
    /// `find_session_by_label` walks the sessions dir and matches on
    /// the DB's `sessions.label` column so the copy-paste workflow
    /// works.
    #[test]
    fn find_session_by_label_matches_stored_label_not_filename_stem() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        // Create a DB whose filename stem (`session-10`) differs from
        // its stored `sessions.label` value (`broken-20260419-154358`).
        let path = dir.join("session-10.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE sessions (label TEXT)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO sessions (label) VALUES (?1)",
            ["broken-20260419-154358"],
        )
        .unwrap();
        drop(conn);

        let got = find_session_by_label(dir, "broken-20260419-154358");
        assert_eq!(got.as_deref(), Some(path.as_path()));

        // A non-existent label returns None, not a bogus match.
        assert!(find_session_by_label(dir, "nope").is_none());
    }

    #[test]
    fn top_level_help_lists_subcommand_vocabulary() {
        // Regression: `dbg --help` used to show only the global flags
        // (`--init`, `--backend`, `-h`, `-V`), leaving agents and new
        // users no way to enumerate the 30+ verbs forwarded to the
        // daemon. The after_help block must name the core verbs so
        // cold-start discovery works.
        use clap::CommandFactory;
        let rendered = Cli::command().render_help().to_string();
        for verb in [
            "start",
            "kill",
            "sessions",
            "replay",
            "break",
            "continue",
            "stack",
            "locals",
            "hits",
            "hit-diff",
            "hit-trend",
            "cross",
            "disasm",
            "raw",
            "forge",
        ] {
            assert!(
                rendered.contains(verb),
                "`dbg --help` is missing `{verb}` — the after_help \
                 subcommand listing regressed:\n{rendered}"
            );
        }
    }

    #[test]
    fn help_flag_short_circuits_on_known_verb() {
        // Regression: `dbg hits --help` without a live session used
        // to bail with "no session running" before reaching the help
        // intercept. The static dispatch table must be reachable for
        // every dbg-level verb so `--help`/`-h` always work.
        for verb in ["hits", "start", "replay", "save"] {
            assert!(
                daemon::dbg_verb_help(verb).is_some(),
                "dbg_verb_help missing entry for `{verb}`"
            );
        }
    }

    #[test]
    fn replay_rehydrates_profile_from_meta() {
        // Regression: `dbg replay <profile-session>` used to leave the
        // saved DB write-only — top/callers/callees would all bail
        // because the in-memory ProfileData never got rebuilt. Now
        // session start stashes the profile source into meta and
        // replay reconstructs ProfileData from it.
        use dbg_cli::session_db::{CreateOptions, SessionDb, SessionKind, TargetClass};
        let speedscope = r#"{
            "shared": {"frames": [{"name":"a"},{"name":"b"}]},
            "profiles": [{
                "events": [
                    {"type":"O","at":0.0,"frame":0},
                    {"type":"O","at":1.0,"frame":1},
                    {"type":"C","at":3.0,"frame":1},
                    {"type":"C","at":4.0,"frame":0}
                ]
            }]
        }"#;
        let tmp = tempfile::TempDir::new().unwrap();
        let db = SessionDb::create(CreateOptions {
            kind: SessionKind::Profile,
            target: "./app",
            target_class: TargetClass::NativeCpu,
            cwd: tmp.path(),
            db_path: None,
            label: Some("p1".into()),
            target_hash: None,
        })
        .unwrap();
        db.set_meta("profile_raw", speedscope).unwrap();

        let mut p = load_profile_from_db(&db).expect("rehydrate failed");
        let top = p.handle_command("top 5");
        // Both frames should appear with non-zero inclusive %.
        assert!(top.contains("a"), "top output missing frame `a`:\n{top}");
        assert!(top.contains("b"), "top output missing frame `b`:\n{top}");
    }

    #[test]
    fn replay_returns_none_when_meta_absent() {
        // Debug sessions (or older profile sessions captured before
        // persistence landed) lack `profile_raw` — the rehydrate path
        // must surface that as `None` so replay can fall back to the
        // crosstrack-only message instead of panicking.
        use dbg_cli::session_db::{CreateOptions, SessionDb, SessionKind, TargetClass};
        let tmp = tempfile::TempDir::new().unwrap();
        let db = SessionDb::create(CreateOptions {
            kind: SessionKind::Debug,
            target: "./app",
            target_class: TargetClass::NativeCpu,
            cwd: tmp.path(),
            db_path: None,
            label: Some("d1".into()),
            target_hash: None,
        })
        .unwrap();
        assert!(load_profile_from_db(&db).is_none());
    }

    #[test]
    fn start_rejects_attach_pid_before_type_or_target() {
        let registry = Registry::new();
        let err = normalize_start_args(
            &registry,
            &[
                "--attach-pid".into(),
                "1234".into(),
                "netcoredbg-proto".into(),
            ],
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("must come after <type> <target>"),
            "unexpected error: {err}"
        );

        let err = normalize_start_args(
            &registry,
            &[
                "--attach-pid=1234".into(),
                "netcoredbg-proto".into(),
                "app.dll".into(),
            ],
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("must come after <type> <target>"),
            "unexpected error: {err}"
        );

        let err = normalize_start_args(
            &registry,
            &["netcoredbg-proto".into(), "--attach-pid=1234".into()],
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("needs a target hint"),
            "unexpected error: {err}"
        );

        let err = normalize_start_args(
            &registry,
            &[
                "netcoredbg-proto".into(),
                "--attach-pid".into(),
                "1234".into(),
            ],
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("needs a target hint"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn start_accepts_space_and_equals_attach_pid_after_target() {
        let mut registry = Registry::new();
        registry.register(Box::new(backend::netcoredbg_proto::NetCoreDbgProtoBackend));

        let args = normalize_start_args(
            &registry,
            &[
                "netcoredbg-proto".into(),
                "app.dll".into(),
                "--attach-pid".into(),
                "1234".into(),
            ],
        )
        .unwrap();
        assert_eq!(
            args,
            vec!["netcoredbg-proto", "app.dll", "--attach-pid", "1234"]
        );

        let args = normalize_start_args(
            &registry,
            &[
                "netcoredbg-proto".into(),
                "app.dll".into(),
                "--attach-pid=1234".into(),
            ],
        )
        .unwrap();
        assert_eq!(
            args,
            vec!["netcoredbg-proto", "app.dll", "--attach-pid=1234"]
        );
    }

    #[test]
    fn dap_attach_configs_remember_debuggee_pid() {
        use crate::backend::Backend;

        let spec = backend::AttachSpec { pid: Some(4321) };
        let cfg = backend::netcoredbg_proto::NetCoreDbgProtoBackend
            .dap_attach(&spec)
            .unwrap();
        assert_eq!(cfg.launch_verb, "attach");
        assert_eq!(cfg.debuggee_pid, Some(4321));
        assert_eq!(cfg.launch_args["processId"], 4321);
    }

    #[test]
    fn netcoredbg_proto_unbreak_maps_to_delete() {
        use crate::backend::canonical::CanonicalOps;

        let cmd = backend::netcoredbg_proto::NetCoreDbgProtoBackend
            .op_unbreak(backend::BreakId(7))
            .unwrap();
        assert_eq!(cmd, "delete 7");
    }

    #[test]
    fn dbg_verb_help_is_publicly_callable() {
        // Regression: client-side `dbg help start` used to bail with
        // "no session running" because the static help lived behind
        // ensure_running(). Make sure the dispatch table is reachable
        // from main via the public daemon re-export.
        assert!(daemon::dbg_verb_help("start").is_some());
        assert!(daemon::dbg_verb_help("replay").is_some());
        assert!(daemon::dbg_verb_help("not-a-verb").is_none());
    }

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn parse_start_flags_collects_breakpoints() {
        let p = parse_start_flags(&s(&["--break", "main.rs:42", "-b", "lib.rs:7"])).unwrap();
        assert_eq!(p.breakpoints, vec!["main.rs:42", "lib.rs:7"]);
        assert!(p.run_args.is_empty());
    }

    #[test]
    fn parse_start_flags_run_and_attach_pid() {
        let p = parse_start_flags(&s(&["--run", "--attach-pid", "12345"])).unwrap();
        assert!(p.do_run);
        assert_eq!(p.attach_pid, Some(12345));

        let p = parse_start_flags(&s(&["--attach-pid=999"])).unwrap();
        assert_eq!(p.attach_pid, Some(999));
    }

    #[test]
    fn parse_start_flags_args_forwards_everything() {
        // Hyphenated debuggee flags must reach run_args verbatim,
        // including ones that look like other tools' flags.
        let p = parse_start_flags(&s(&[
            "--break",
            "x.rs:1",
            "--args",
            "--ServerUrl=http://127.0.0.1:8083",
            "--commit-every",
            "1000",
            "positional",
        ]))
        .unwrap();
        assert_eq!(p.breakpoints, vec!["x.rs:1"]);
        assert_eq!(
            p.run_args,
            vec![
                "--ServerUrl=http://127.0.0.1:8083",
                "--commit-every",
                "1000",
                "positional"
            ]
        );
    }

    #[test]
    fn parse_start_flags_rejects_dbg_flags_after_args() {
        // Regression: putting `--break` after `--args` used to
        // silently misroute the flag to the debuggee (which would
        // ignore or reject it). Now bails fast with a fix-instruction.
        for misordered in [
            vec!["--args", "foo", "--break", "x.rs:1"],
            vec!["--args", "foo", "-b", "x.rs:1"],
            vec!["--args", "foo", "--run"],
            vec!["--args", "foo", "-r"],
            vec!["--args", "foo", "--attach-pid", "1"],
            vec!["--args", "foo", "--attach-pid=1"],
            vec!["--args", "foo", "--attach-port=:9999"],
        ] {
            let err = parse_start_flags(&s(&misordered))
                .expect_err(&format!("expected error for misordered `{misordered:?}`"));
            let msg = format!("{err:#}");
            assert!(
                msg.contains("appeared after `--args`"),
                "error did not name the misorder: {msg}"
            );
            assert!(
                msg.contains("must come before"),
                "error did not give a fix instruction: {msg}"
            );
        }
    }

    #[test]
    fn parse_start_flags_unknown_long_flags_route_to_debuggee() {
        // Adjacent design rule: any `--whatever` we don't recognize is
        // assumed to be for the debuggee, so `dbg start cargo run --release`
        // works without manual --args.
        let p = parse_start_flags(&s(&["positional", "--release", "--features", "x"])).unwrap();
        assert_eq!(
            p.run_args,
            vec!["positional", "--release", "--features", "x"]
        );
        assert!(p.breakpoints.is_empty());
        assert!(!p.do_run);
        assert!(p.attach_pid.is_none());
    }

    #[test]
    fn parse_start_flags_attach_pid_validates() {
        let err = parse_start_flags(&s(&["--attach-pid", "not-a-number"])).unwrap_err();
        assert!(format!("{err:#}").contains("invalid --attach-pid"));

        let err = parse_start_flags(&s(&["--attach-pid="])).unwrap_err();
        assert!(format!("{err:#}").contains("requires a PID"));

        let err = parse_start_flags(&s(&["--attach-pid"])).unwrap_err();
        assert!(format!("{err:#}").contains("requires a PID"));
    }
}
