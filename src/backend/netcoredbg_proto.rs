//! netcoredbg-proto — .NET debugging via netcoredbg's VSCode/DAP mode.
//!
//! netcoredbg doesn't announce its listen port (and refuses `:0`),
//! so we pre-allocate a free port with `DapLaunchConfig::pick_free_port`
//! and pass it via `--server=<port>`. The shared `DapTransport`
//! handles the rest.

use serde_json::{Value, json};

use super::canonical::{BreakLoc, CanonicalOps};
use super::{Backend, Dependency, DependencyCheck, SpawnConfig};

pub struct NetCoreDbgProtoBackend;

impl Backend for NetCoreDbgProtoBackend {
    fn name(&self) -> &'static str {
        "netcoredbg-proto"
    }

    fn description(&self) -> &'static str {
        ".NET (C#, F#) via netcoredbg DAP (structured events)"
    }

    fn types(&self) -> &'static [&'static str] {
        &["netcoredbg-proto"]
    }

    fn spawn_config(&self, _target: &str, _args: &[String]) -> anyhow::Result<SpawnConfig> {
        Ok(SpawnConfig {
            bin: "netcoredbg".into(),
            args: vec![],
            env: vec![],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"ncdb>"
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![
            Dependency {
                name: "dotnet",
                check: DependencyCheck::Binary {
                    name: "dotnet",
                    alternatives: &["dotnet"],
                    version_cmd: None,
                },
                install: "https://dot.net/install",
            },
            Dependency {
                name: "netcoredbg",
                check: DependencyCheck::Binary {
                    name: "netcoredbg",
                    alternatives: &["netcoredbg"],
                    version_cmd: None,
                },
                install: concat!(
                    "mkdir -p ~/.local/share/netcoredbg && ",
                    "curl -sL https://github.com/Samsung/netcoredbg/releases/latest/download/",
                    "netcoredbg-linux-amd64.tar.gz | tar xz -C ~/.local/share/netcoredbg && ",
                    "ln -sf ~/.local/share/netcoredbg/netcoredbg/netcoredbg ~/.local/bin/netcoredbg"
                ),
            },
        ]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        format!("break {spec}")
    }

    fn run_command(&self) -> &'static str {
        "continue"
    }

    fn quit_command(&self) -> &'static str {
        "quit"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "netcoredbg-proto: continue, next, step, out, backtrace, \
         break <file>:<line>, breakpoints, locals, print <expr>, quit"
            .into()
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("dotnet.md", include_str!("../../skills/adapters/dotnet.md"))]
    }

    fn canonical_ops(&self) -> Option<&dyn CanonicalOps> {
        Some(self)
    }

    fn clean(&self, _cmd: &str, output: &str) -> String {
        output.to_string()
    }

    fn uses_dap(&self) -> bool {
        true
    }

    fn dap_launch(
        &self,
        target: &str,
        args: &[String],
    ) -> anyhow::Result<crate::dap::DapLaunchConfig> {
        let port = crate::dap::DapLaunchConfig::pick_free_port()?;
        Ok(crate::dap::DapLaunchConfig {
            bin: "netcoredbg".into(),
            args: vec![
                "--interpreter=vscode".into(),
                format!("--server={port}"),
            ],
            listen_marker: String::new(),
            launch_verb: "launch".into(),
            launch_args: json!({
                "request": "launch",
                "program": std::fs::canonicalize(target)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| target.to_string()),
                "args": args,
                "cwd": std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                "stopAtEntry": true,
                "stopOnEntry": true,
                "justMyCode": false,
            }),
            preassigned_addr: Some(format!("127.0.0.1:{port}")),
        })
    }

    fn dap_attach(
        &self,
        spec: &super::AttachSpec,
    ) -> anyhow::Result<crate::dap::DapLaunchConfig> {
        let pid = spec
            .pid
            .ok_or_else(|| anyhow::anyhow!("netcoredbg-proto attach needs --attach-pid"))?;
        let port = crate::dap::DapLaunchConfig::pick_free_port()?;
        Ok(crate::dap::DapLaunchConfig {
            bin: "netcoredbg".into(),
            args: vec!["--interpreter=vscode".into(), format!("--server={port}")],
            listen_marker: String::new(),
            launch_verb: "attach".into(),
            launch_args: json!({
                "request": "attach",
                "processId": pid,
                "justMyCode": false,
            }),
            preassigned_addr: Some(format!("127.0.0.1:{port}")),
        })
    }
}

impl CanonicalOps for NetCoreDbgProtoBackend {
    fn tool_name(&self) -> &'static str {
        "netcoredbg-proto"
    }
    fn auto_capture_locals(&self) -> bool {
        true
    }

    fn op_breaks(&self) -> anyhow::Result<String> {
        Ok("breakpoints".into())
    }
    fn op_break(&self, loc: &BreakLoc) -> anyhow::Result<String> {
        Ok(match loc {
            BreakLoc::FileLine { file, line } => format!("break {file}:{line}"),
            BreakLoc::Fqn(name) => format!("bfn {name}"),
            // .NET idiom is dotted; override the DAP default (::).
            BreakLoc::ModuleMethod { module, method } => format!("bfn {module}.{method}"),
        })
    }
    fn op_break_conditional(&self, loc: &BreakLoc, cond: &str) -> anyhow::Result<String> {
        Ok(format!("{} if {cond}", self.op_break(loc)?))
    }
    fn op_break_log(&self, loc: &BreakLoc, msg: &str) -> anyhow::Result<String> {
        Ok(format!("{} log {msg}", self.op_break(loc)?))
    }
    fn op_pause(&self) -> anyhow::Result<String> {
        Ok("pause".into())
    }
    fn op_restart(&self) -> anyhow::Result<String> {
        Ok("restart".into())
    }
    fn op_catch(&self, filters: &[String]) -> anyhow::Result<String> {
        Ok(if filters.is_empty() {
            "catch off".into()
        } else {
            format!("catch {}", filters.join(" "))
        })
    }
    fn op_set(&self, lhs: &str, rhs: &str) -> anyhow::Result<String> {
        Ok(format!("set {lhs} = {rhs}"))
    }
    fn op_list(&self, loc: Option<&str>) -> anyhow::Result<String> {
        Ok(match loc {
            Some(s) => format!("list {s}"),
            None => "list".into(),
        })
    }
    fn op_threads(&self) -> anyhow::Result<String> {
        Ok("threads".into())
    }
    fn op_thread(&self, n: u32) -> anyhow::Result<String> {
        Ok(format!("thread {n}"))
    }
    fn op_watch(&self, expr: &str) -> anyhow::Result<String> {
        Ok(format!("watch {expr}"))
    }

    fn parse_locals(&self, output: &str) -> Option<Value> {
        serde_json::from_str(output.trim()).ok()
    }
}
