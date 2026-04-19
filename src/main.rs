mod backend;
mod check;
mod commands;
mod daemon;
mod dap;
mod ghcprof;
mod init;
mod inspector;
mod jitdasm;
mod phpprofile;
mod profile;
mod pty;
mod resolve;
mod transport_common;

use std::time::Duration;

use anyhow::{Result, bail};
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
    /// Initialize for an AI agent: claude, codex
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
    let cli = Cli::parse();
    let mut registry = Registry::new();
    registry.register(Box::new(backend::lldb::LldbBackend));
    registry.register(Box::new(backend::lldb_dap_proto::LldbDapProtoBackend));
    registry.register(Box::new(backend::pdb::PdbBackend));
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
        return jitdasm::run_repl(asm_path, &cli.jitdasm_pattern).map_err(Into::into);
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
            println!("  {:<14} {} [{}]", backend.name(), backend.description(), status);
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
            let ctx = commands::lifecycle::LifeCtx { cwd: &cwd, active: None };
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
                println!("session lifecycle:  start, run, continue, step, next, finish, kill, status, cancel");
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
    if !path.exists() {
        bail!("no session at {}", path.display());
    }
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(-1);
    if v != dbg_cli::session_db::SCHEMA_VERSION {
        bail!(
            "session `{}` has schema_version={v}, expected {} — re-collect to replay",
            path.display(),
            dbg_cli::session_db::SCHEMA_VERSION
        );
    }
    let db = dbg_cli::session_db::SessionDb::open(&path)?;

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
    eprintln!(
        "replay `{label}` (target={target}, class={target_class}) — read-only crosstrack REPL"
    );
    eprintln!("supported: hits, hit-diff, hit-trend, cross, disasm, source, sessions");
    eprintln!("type `quit` or EOF to exit");

    use std::str::FromStr;
    let target_class_enum = dbg_cli::session_db::TargetClass::from_str(&target_class)
        .unwrap_or(dbg_cli::session_db::TargetClass::NativeCpu);

    // One-shot mode: `dbg replay <label> hits foo:42`
    if args.len() > 1 {
        let cmd = args[1..].join(" ");
        let out = replay_eval(&cmd, &db, &cwd, &target, target_class_enum);
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
        let out = replay_eval(cmd, &db, &cwd, &target, target_class_enum);
        println!("{out}");
    }
    Ok(())
}

fn replay_eval(
    cmd: &str,
    db: &dbg_cli::session_db::SessionDb,
    cwd: &std::path::Path,
    target: &str,
    target_class: dbg_cli::session_db::TargetClass,
) -> String {
    match commands::dispatch_no_backend(cmd) {
        Some(commands::Dispatched::Immediate(s)) => s,
        Some(commands::Dispatched::Query(q)) => {
            let ctx = commands::crosstrack::RunCtx {
                target,
                target_class,
                cwd,
                live: None,
            };
            commands::crosstrack::run(&q, db, &ctx)
        }
        Some(commands::Dispatched::Lifecycle(l)) => {
            let ctx = commands::lifecycle::LifeCtx { cwd, active: Some(db) };
            commands::lifecycle::run(&l, &ctx)
        }
        _ => {
            "replay only supports crosstrack + lifecycle verbs (hits, hit-diff, \
             hit-trend, cross, disasm, source, sessions, status). Live debugger \
             verbs (step, continue, break, …) aren't available — start a new \
             session with `dbg start` for those."
                .to_string()
        }
    }
}

fn ensure_running() -> Result<()> {
    if !daemon::is_running() {
        bail!("no session running — use: dbg start <type> <target>");
    }
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
    let active = std::env::var("DBG_SESSION")
        .ok()
        .or_else(|| {
            std::fs::read_to_string(daemon::latest_pointer_path())
                .ok()
                .map(|s| s.trim().to_string())
        });
    println!("live daemons in this cwd:");
    for slug in &peers {
        let marker = if active.as_deref() == Some(slug.as_str()) { "*" } else { " " };
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

fn cmd_start(registry: &Registry, args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!("usage: dbg start <type> <target> [--break spec] [--args ...] [--run]");
    }

    // Single-arg form: `dbg start <target>` — infer backend from the
    // target's extension. Unambiguous only; unknown extensions bail
    // with the standard usage. `dbg start <type> <target>` (two args)
    // still takes the explicit path.
    let args: Vec<String> = if args.len() == 1 {
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
    let args = args.as_slice();

    if args.len() < 2 {
        bail!("usage: dbg start <type> <target> [--break spec] [--args ...] [--run]");
    }

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
    unsafe { std::env::set_var("DBG_SESSION", &slug); }
    // Publish this as the newest daemon in the cwd so env-less
    // clients in other shells find it by default.
    daemon::write_latest_pointer(&slug);
    let peers = daemon::live_slugs_in_cwd();
    if !peers.is_empty() {
        eprintln!(
            "session: {slug}  (coexisting with: {})",
            peers.iter().filter(|s| *s != &slug).cloned().collect::<Vec<_>>().join(", "),
        );
        eprintln!("  other shells: set DBG_SESSION={slug} to target this session");
    } else {
        eprintln!("session: {slug}");
    }

    let backend_type = &args[0];
    let target_raw = &args[1];

    // Intercept GPU-related types — the agent should use gdbg, not dbg
    match backend_type.as_str() {
        "gdbg" | "gpu" | "cuda" | "pytorch" | "triton"
        | "tensorflow" | "tf" | "jax" | "mxnet" | "cupy" => {
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

    let backend = registry
        .get(backend_type)
        .ok_or_else(|| {
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
    let mut breakpoints = Vec::new();
    let mut run_args = Vec::new();
    let mut do_run = false;
    let mut attach_pid: Option<u32> = None;
    let mut attach_host_port: Option<String> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--break" | "-b" => {
                i += 1;
                if i < args.len() {
                    breakpoints.push(args[i].clone());
                }
            }
            "--args" | "-a" => {
                i += 1;
                while i < args.len() && !args[i].starts_with("--") {
                    run_args.push(args[i].clone());
                    i += 1;
                }
                continue;
            }
            "--run" | "-r" => do_run = true,
            "--attach-pid" => {
                i += 1;
                if i < args.len() {
                    attach_pid = args[i].parse().ok();
                }
            }
            "--attach-port" => {
                i += 1;
                if i < args.len() {
                    attach_host_port = Some(args[i].clone());
                }
            }
            other if !other.starts_with("--") => {
                // Bare positional — forward to the backend.
                run_args.push(other.to_string());
            }
            _ => {}
        }
        i += 1;
    }
    let attach = if attach_pid.is_some() || attach_host_port.is_some() {
        Some(backend::AttachSpec {
            pid: attach_pid,
            host_port: attach_host_port,
        })
    } else {
        None
    };

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
            // Redirect stderr to the startup log so the parent can read
            // it back if the daemon dies before binding the socket.
            if let Ok(f) = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&log_path)
            {
                use std::os::unix::io::AsRawFd;
                let _ = nix::unistd::dup2(f.as_raw_fd(), 2);
            }
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
            // produced a prompt — if the backend dies (delve bails on
            // a bad exec target, dotnet build fails, …) we'd previously
            // fall through and the agent would see a healthy-looking
            // "target: foo" with no session actually listening. Give
            // the daemon a short grace window and then ping it; if
            // the ping fails (or the daemon process is gone), surface
            // the captured startup log.
            std::thread::sleep(Duration::from_millis(150));
            let healthy = daemon::is_running()
                && daemon::send_command("status").is_ok();
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
            "start", "kill", "sessions", "replay",
            "break", "continue", "stack", "locals",
            "hits", "hit-diff", "hit-trend",
            "cross", "disasm", "raw",
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
    fn dbg_verb_help_is_publicly_callable() {
        // Regression: client-side `dbg help start` used to bail with
        // "no session running" because the static help lived behind
        // ensure_running(). Make sure the dispatch table is reachable
        // from main via the public daemon re-export.
        assert!(daemon::dbg_verb_help("start").is_some());
        assert!(daemon::dbg_verb_help("replay").is_some());
        assert!(daemon::dbg_verb_help("not-a-verb").is_none());
    }
}
