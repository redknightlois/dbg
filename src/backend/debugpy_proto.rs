//! debugpy-proto — Python debugging via debugpy's DAP adapter.
//!
//! Spawns `debugpy-adapter --port 0 --log-stderr`, scrapes the
//! announced listen port from stderr, and hands the rest to the
//! shared `DapTransport`. Launch config tells the adapter to
//! start the target Python script in the adapter's own subprocess.

use serde_json::{Value, json};

use super::canonical::{BreakLoc, CanonicalOps};
use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct DebugpyProtoBackend;

impl Backend for DebugpyProtoBackend {
    fn name(&self) -> &'static str {
        "debugpy-proto"
    }

    fn description(&self) -> &'static str {
        "Python via debugpy DAP (structured events, separate stdout)"
    }

    fn types(&self) -> &'static [&'static str] {
        &["debugpy-proto"]
    }

    fn spawn_config(&self, _target: &str, _args: &[String]) -> anyhow::Result<SpawnConfig> {
        Ok(SpawnConfig {
            bin: "debugpy-adapter".into(),
            args: vec![],
            env: vec![],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"\(Pdb\) "
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "debugpy-adapter",
            check: DependencyCheck::Binary {
                name: "debugpy-adapter",
                alternatives: &["debugpy-adapter"],
                version_cmd: None,
            },
            install: "uv tool install debugpy  # or pip install debugpy",
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
        "debugpy-proto: continue, next, step, out, backtrace, \
         break <file>:<line>, breakpoints, locals, print <expr>, quit"
            .into()
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("python.md", include_str!("../../skills/adapters/python.md"))]
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
            bin: "debugpy-adapter".into(),
            // `--log-stderr` is required to see the announce line;
            // the adapter is silent by default.
            args: vec!["--port".into(), "0".into(), "--log-stderr".into()],
            listen_marker: "Listening for incoming Client connections on".into(),
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
                "console": "internalConsole",
                "stopOnEntry": true,
                // justMyCode=false lets breakpoints land in library
                // code too — agents set breakpoints by file:line and
                // shouldn't hit a silent skip.
                "justMyCode": false,
            }),
            preassigned_addr: None,
        })
    }

    fn dap_attach(
        &self,
        spec: &super::AttachSpec,
    ) -> anyhow::Result<crate::dap::DapLaunchConfig> {
        // debugpy attach requires a pid. `--log-stderr` still needed
        // for the listen-announce line.
        let pid = spec
            .pid
            .ok_or_else(|| anyhow::anyhow!("debugpy-proto attach needs --attach-pid"))?;
        Ok(crate::dap::DapLaunchConfig {
            bin: "debugpy-adapter".into(),
            args: vec!["--port".into(), "0".into(), "--log-stderr".into()],
            listen_marker: "Listening for incoming Client connections on".into(),
            launch_verb: "attach".into(),
            launch_args: json!({
                "request": "attach",
                "processId": pid,
                "justMyCode": false,
            }),
            preassigned_addr: None,
        })
    }
}

impl CanonicalOps for DebugpyProtoBackend {
    fn tool_name(&self) -> &'static str {
        "debugpy-proto"
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
            // Python idiom is dotted; override the DAP default (::).
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
        // debugpy filters: "raised", "uncaught", "userUnhandled".
        // Default (no filter) means "clear".
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
