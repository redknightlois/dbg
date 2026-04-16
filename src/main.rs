mod backend;
mod check;
mod commands;
mod daemon;
mod ghcprof;
mod init;
mod inspector;
mod jitdasm;
mod phpprofile;
mod profile;
mod pty;
mod resolve;

use std::time::Duration;

use anyhow::{Result, bail};
use clap::Parser;
use nix::unistd::{ForkResult, fork};

use backend::Registry;

#[derive(Parser)]
#[command(name = "dbg", version, about = "AI can read your code. Now it can live debug it too.")]
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut registry = Registry::new();
    registry.register(Box::new(backend::lldb::LldbBackend));
    registry.register(Box::new(backend::pdb::PdbBackend));
    registry.register(Box::new(backend::netcoredbg::NetCoreDbgBackend));
    registry.register(Box::new(backend::delve::DelveBackend));
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
    registry.register(Box::new(backend::node_inspect::NodeInspectBackend));
    registry.register(Box::new(backend::node_proto::NodeProtoBackend));
    registry.register(Box::new(backend::nodeprof::NodeProfBackend));

    // Auto-update installed skills if binary version changed
    init::auto_update(&registry);

    // --jitdasm-repl (internal: launched by the jitdasm backend)
    if let Some(asm_path) = &cli.jitdasm_repl {
        return jitdasm::run_repl(asm_path).map_err(Into::into);
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

    match first {
        "start" => cmd_start(&registry, &cli.args[1..]),
        "kill" => {
            let msg = daemon::kill_daemon()?;
            println!("{msg}");
            Ok(())
        }
        "help" => {
            if cli.args.len() > 1 {
                // dbg help <topic>
                ensure_running()?;
                let topic = cli.args[1..].join(" ");
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
                println!("  dbg help <command>   ask the debugger what a command does");
                println!("  dbg kill\n");
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

fn ensure_running() -> Result<()> {
    if !daemon::is_running() {
        bail!("no session running — use: dbg start <type> <target>");
    }
    Ok(())
}

fn cmd_start(registry: &Registry, args: &[String]) -> Result<()> {
    if args.len() < 2 {
        bail!("usage: dbg start <type> <target> [--break spec] [--args ...] [--run]");
    }

    // Kill existing session
    if daemon::is_running() {
        eprintln!("stopping existing session...");
        daemon::kill_daemon()?;
        std::thread::sleep(Duration::from_millis(300));
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

    // Parse flags
    let mut breakpoints = Vec::new();
    let mut run_args = Vec::new();
    let mut do_run = false;
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
            _ => {}
        }
        i += 1;
    }

    // Resolve target
    let resolved = resolve::resolve(backend_type, target_raw)?;
    eprintln!("target: {resolved}");

    // Fork daemon
    // Safety: fork duplicates the process
    let fork_result = unsafe { fork() }?;
    match fork_result {
        ForkResult::Child => {
            // Daemon process
            let _ = nix::unistd::setsid();
            if let Err(e) = daemon::run_daemon(backend, &resolved, &run_args) {
                eprintln!("daemon error: {e}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        ForkResult::Parent { .. } => {
            // Wait for socket
            if !daemon::wait_for_socket(Duration::from_secs(120)) {
                bail!("daemon failed to start");
            }

            // Set breakpoints — send through the canonical `break <spec>`
            // vocabulary so the dispatcher routes via CanonicalOps (which
            // handles jdb class-name extraction, ghci module inference,
            // etc.). Falls back to format_breakpoint for backends without
            // CanonicalOps.
            for bp in &breakpoints {
                let cmd = if backend.canonical_ops().is_some() {
                    format!("break {bp}")
                } else {
                    backend.format_breakpoint(bp)
                };
                let resp = daemon::send_command(&cmd)?;
                println!("{resp}");
            }

            // Auto-run — use the canonical `run` verb when available.
            if do_run {
                let cmd = if backend.canonical_ops().is_some() {
                    "run".to_string()
                } else {
                    backend.run_command().to_string()
                };
                let resp = daemon::send_command(&cmd)?;
                println!("{resp}");
            }

            Ok(())
        }
    }
}
