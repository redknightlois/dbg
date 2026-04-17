//! lldb-dap-proto — native C/C++/Rust/Swift debugging via LLDB's DAP
//! adapter. Layers over the shared `DapTransport`; this backend just
//! supplies the spawn command and launch config.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Value, json};

use super::canonical::{BreakLoc, CanonicalOps, HitEvent};
use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct LldbDapProtoBackend;

impl Backend for LldbDapProtoBackend {
    fn name(&self) -> &'static str {
        "lldb-dap-proto"
    }

    fn description(&self) -> &'static str {
        "Native (C/C++/Rust/Swift) via LLDB DAP (structured events, separate stdout)"
    }

    fn types(&self) -> &'static [&'static str] {
        &["lldb-dap-proto"]
    }

    fn spawn_config(&self, _target: &str, _args: &[String]) -> anyhow::Result<SpawnConfig> {
        Ok(SpawnConfig {
            bin: "lldb-dap".into(),
            args: vec![],
            env: vec![],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"\(lldb\) "
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "lldb-dap",
            check: DependencyCheck::Binary {
                name: "lldb-dap",
                alternatives: &["lldb-dap"],
                version_cmd: Some(("lldb-dap", &["--help"])),
            },
            install: "brew install llvm  # or apt install lldb-dap",
        }]
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
        "lldb-dap-proto: continue, next, step, out, backtrace, \
         break <file>:<line>, breakpoints, locals, print <expr>, quit"
            .into()
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        // Shares skill adapters with the PTY lldb backend — native
        // debugging vocabulary doesn't differ at the agent layer.
        vec![
            ("rust.md", include_str!("../../skills/adapters/rust.md")),
            ("c.md", include_str!("../../skills/adapters/c.md")),
            ("cpp.md", include_str!("../../skills/adapters/cpp.md")),
            ("zig.md", include_str!("../../skills/adapters/zig.md")),
        ]
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
        Ok(crate::dap::DapLaunchConfig {
            bin: "lldb-dap".into(),
            args: vec![
                "--connection".into(),
                "listen://127.0.0.1:0".into(),
            ],
            listen_marker: "Listening for:".into(),
            launch_verb: "launch".into(),
            launch_args: json!({
                // lldb-dap requires an absolute path to the program.
                "program": std::fs::canonicalize(target)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| target.to_string()),
                "args": args,
                "cwd": std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                "stopOnEntry": true,
                "runInTerminal": false,
            }),
            preassigned_addr: None,
        })
    }

    fn dap_attach(
        &self,
        spec: &super::AttachSpec,
    ) -> anyhow::Result<crate::dap::DapLaunchConfig> {
        let pid = spec
            .pid
            .ok_or_else(|| anyhow::anyhow!("lldb-dap-proto attach needs --attach-pid"))?;
        Ok(crate::dap::DapLaunchConfig {
            bin: "lldb-dap".into(),
            args: vec!["--connection".into(), "listen://127.0.0.1:0".into()],
            listen_marker: "Listening for:".into(),
            launch_verb: "attach".into(),
            launch_args: json!({
                "request": "attach",
                "pid": pid,
            }),
            preassigned_addr: None,
        })
    }
}

impl CanonicalOps for LldbDapProtoBackend {
    fn tool_name(&self) -> &'static str {
        "lldb-dap-proto"
    }
    fn auto_capture_locals(&self) -> bool {
        true
    }

    fn op_breaks(&self) -> anyhow::Result<String> {
        Ok("breakpoints".into())
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
        // lldb-dap filters: "cpp_throw", "cpp_catch", "swift_throw",
        // "objc_throw", etc. Empty clears.
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
