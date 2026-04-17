//! delve-proto — Go debugging via Delve's DAP server.
//!
//! Pilot backend for the generic DAP transport. Spawns
//! `dlv dap -l 127.0.0.1:0`, scrapes the announced listen port,
//! then the shared `DapTransport` handles the rest of the handshake
//! (initialize / launch / configurationDone).

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Value, json};

use super::canonical::{BreakLoc, CanonicalOps, HitEvent};
use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct DelveProtoBackend;

impl Backend for DelveProtoBackend {
    fn name(&self) -> &'static str {
        "delve-proto"
    }

    fn description(&self) -> &'static str {
        "Go via Delve DAP (structured events, separate stdout)"
    }

    fn types(&self) -> &'static [&'static str] {
        &["delve-proto"]
    }

    fn spawn_config(&self, _target: &str, _args: &[String]) -> anyhow::Result<SpawnConfig> {
        // Unused — DAP path goes through `dap_launch`.
        Ok(SpawnConfig {
            bin: "dlv".into(),
            args: vec![],
            env: vec![],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        // Unused.
        r"\(dlv\) "
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![
            Dependency {
                name: "dlv",
                check: DependencyCheck::Binary {
                    name: "dlv",
                    alternatives: &["dlv"],
                    version_cmd: Some(("dlv", &["version"])),
                },
                install: "go install github.com/go-delve/delve/cmd/dlv@latest",
            },
            Dependency {
                name: "go",
                check: DependencyCheck::Binary {
                    name: "go",
                    alternatives: &["go"],
                    version_cmd: Some(("go", &["version"])),
                },
                install: "https://go.dev/dl/",
            },
        ]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        // Canonical DAP break verb — the transport's parse_break
        // expects `break <file>:<line>`.
        format!("break {spec}")
    }

    fn run_command(&self) -> &'static str {
        "continue"
    }

    fn quit_command(&self) -> &'static str {
        "quit"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "delve-proto: continue, next, step, out, backtrace, break <file>:<line>, \
         breakpoints, locals, print <expr>, quit"
            .into()
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("go.md", include_str!("../../skills/adapters/go.md"))]
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

    fn dap_launch(&self, target: &str, args: &[String]) -> anyhow::Result<crate::dap::DapLaunchConfig> {
        // `dlv dap -l 127.0.0.1:0` — lets the kernel pick a free port;
        // delve prints `DAP server listening at: 127.0.0.1:PORT` on
        // stderr. The transport scrapes that.
        Ok(crate::dap::DapLaunchConfig {
            bin: "dlv".into(),
            args: vec!["dap".into(), "-l".into(), "127.0.0.1:0".into()],
            listen_marker: "DAP server listening at:".into(),
            launch_verb: "launch".into(),
            launch_args: json!({
                // Delve launch config mirrors VSCode's go.debug settings.
                "request": "launch",
                "mode": "debug",
                "program": target,
                "args": args,
                "cwd": std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                "stopOnEntry": true,
                "showLog": false,
            }),
            preassigned_addr: None,
        })
    }
}

impl CanonicalOps for DelveProtoBackend {
    fn tool_name(&self) -> &'static str {
        "delve-proto"
    }
    fn auto_capture_locals(&self) -> bool {
        // DAP locals is a handful of structured roundtrips (scopes +
        // variables per scope) — no PTY state hazards.
        true
    }

    fn op_breaks(&self) -> anyhow::Result<String> {
        Ok("breakpoints".into())
    }
    fn op_break(&self, loc: &BreakLoc) -> anyhow::Result<String> {
        Ok(match loc {
            BreakLoc::FileLine { file, line } => format!("break {file}:{line}"),
            BreakLoc::Fqn(name) => format!("bfn {name}"),
            // Go doesn't have a shared-library concept; treat module.method as a fqn.
            BreakLoc::ModuleMethod { module, method } => format!("bfn {module}.{method}"),
        })
    }
    fn op_break_conditional(&self, loc: &BreakLoc, cond: &str) -> anyhow::Result<String> {
        let base = self.op_break(loc)?;
        Ok(format!("{base} if {cond}"))
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
        // DAP delivers structured hits via pending_hit(); this is a
        // best-effort fallback that matches delve's text stop format
        // in case an agent routes us a raw text response.
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
