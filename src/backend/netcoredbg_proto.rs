//! netcoredbg-proto — .NET debugging via netcoredbg's VSCode/DAP mode.
//!
//! netcoredbg doesn't announce its listen port (and refuses `:0`),
//! so we pre-allocate a free port with `DapLaunchConfig::pick_free_port`
//! and pass it via `--server=<port>`. The shared `DapTransport`
//! handles the rest.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Value, json};

use super::canonical::{BreakLoc, CanonicalOps, HitEvent};
use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

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

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        CleanResult {
            output: output.to_string(),
            events: vec![],
        }
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
            BreakLoc::ModuleMethod { module, method } => format!("bfn {module}.{method}"),
        })
    }
    fn op_break_conditional(&self, loc: &BreakLoc, cond: &str) -> anyhow::Result<String> {
        let base = self.op_break(loc)?;
        Ok(format!("{base} if {cond}"))
    }
    fn op_break_log(&self, loc: &BreakLoc, msg: &str) -> anyhow::Result<String> {
        let base = self.op_break(loc)?;
        Ok(format!("{base} log {msg}"))
    }
    fn op_run(&self, _args: &[String]) -> anyhow::Result<String> {
        Ok("continue".into())
    }
    fn op_continue(&self) -> anyhow::Result<String> {
        Ok("continue".into())
    }
    fn op_step(&self) -> anyhow::Result<String> {
        Ok("step".into())
    }
    fn op_next(&self) -> anyhow::Result<String> {
        Ok("next".into())
    }
    fn op_finish(&self) -> anyhow::Result<String> {
        Ok("out".into())
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
    fn op_stack(&self, _n: Option<u32>) -> anyhow::Result<String> {
        Ok("backtrace".into())
    }
    fn op_frame(&self, n: u32) -> anyhow::Result<String> {
        Ok(format!("frame {n}"))
    }
    fn op_locals(&self) -> anyhow::Result<String> {
        Ok("locals".into())
    }
    fn op_print(&self, expr: &str) -> anyhow::Result<String> {
        Ok(format!("print {expr}"))
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

    fn parse_hit(&self, output: &str) -> Option<HitEvent> {
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| Regex::new(r"at (\S+):(\d+)").unwrap());
        for line in output.lines() {
            if let Some(c) = re.captures(line) {
                return Some(HitEvent {
                    location_key: format!("{}:{}", &c[1], &c[2]),
                    thread: None,
                    frame_symbol: None,
                    file: Some(c[1].to_string()),
                    line: c[2].parse().ok(),
                });
            }
        }
        None
    }

    fn parse_locals(&self, output: &str) -> Option<Value> {
        serde_json::from_str(output.trim()).ok()
    }
}
