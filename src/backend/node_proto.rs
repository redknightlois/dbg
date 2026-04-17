//! node-proto backend — V8 Inspector transport.
//!
//! Same target surface as `node-inspect` but uses the Inspector
//! WebSocket protocol directly (see `crate::inspector`). Coexists
//! with the PTY-based node-inspect until the validation matrix
//! confirms parity; we only retire the PTY version once this one
//! passes all 5/5 cases across example projects.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use super::canonical::{BreakLoc, CanonicalOps, HitEvent};
use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct NodeProtoBackend;

impl Backend for NodeProtoBackend {
    fn name(&self) -> &'static str {
        "node-proto"
    }

    fn description(&self) -> &'static str {
        "Node.js via V8 Inspector protocol (structured events, separate stdout)"
    }

    fn types(&self) -> &'static [&'static str] {
        &["node-proto", "node", "nodejs", "js", "javascript", "ts", "typescript"]
    }

    fn spawn_config(&self, target: &str, _args: &[String]) -> anyhow::Result<SpawnConfig> {
        // The real spawn is done by the inspector transport (spawns
        // node --inspect-brk directly). This config is unused but the
        // trait requires one; we fill in plausible values so anything
        // that inspects it reads sensibly.
        Ok(SpawnConfig {
            bin: "node".into(),
            args: vec!["--inspect-brk=127.0.0.1:0".into(), target.into()],
            env: vec![],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        // Unused — the inspector transport doesn't read a prompt.
        r"debug> "
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "node",
            check: DependencyCheck::Binary {
                name: "node",
                alternatives: &["node"],
                version_cmd: Some(("node", &["--version"])),
            },
            install: "https://nodejs.org  # or: nvm install --lts",
        }]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        if let Some((file, line)) = spec.rsplit_once(':') {
            format!("sb('{file}', {line})")
        } else if spec.chars().all(|c| c.is_ascii_digit()) {
            format!("sb({spec})")
        } else {
            format!("sb('{spec}')")
        }
    }

    fn run_command(&self) -> &'static str {
        "cont"
    }

    fn quit_command(&self) -> &'static str {
        ".exit"
    }

    fn help_command(&self) -> &'static str {
        // Static because the protocol transport doesn't proxy a help
        // command from the target.
        "help"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "node-proto: cont, step, next, out, backtrace, breakpoints, \
         sb(file, line), print <expr>, .exit"
            .into()
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("javascript.md", include_str!("../../skills/adapters/javascript.md"))]
    }

    fn canonical_ops(&self) -> Option<&dyn CanonicalOps> {
        Some(self)
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        // The inspector transport produces structured text already —
        // no banner noise to strip.
        CleanResult {
            output: output.to_string(),
            events: vec![],
        }
    }

    /// Hook: tells the daemon that this backend wants a protocol
    /// transport rather than the default PTY. The daemon branches
    /// on this in `run_daemon`.
    fn uses_inspector(&self) -> bool {
        true
    }
}

impl CanonicalOps for NodeProtoBackend {
    fn tool_name(&self) -> &'static str {
        "node-proto"
    }
    fn auto_capture_locals(&self) -> bool {
        // The inspector transport implements `locals` natively via
        // Runtime.getProperties. The daemon's auto-capture path after
        // each hit is safe here — there's no PTY state to disturb,
        // and the roundtrip is a single JSON-RPC call per scope.
        true
    }

    fn op_breaks(&self) -> anyhow::Result<String> {
        Ok("breakpoints".into())
    }

    fn op_break(&self, loc: &BreakLoc) -> anyhow::Result<String> {
        Ok(match loc {
            BreakLoc::FileLine { file, line } => format!("sb('{file}', {line})"),
            BreakLoc::Fqn(name) => format!("sb('{name}')"),
            BreakLoc::ModuleMethod { module, method } => format!("sb('{module}:{method}')"),
        })
    }
    fn op_break_conditional(&self, loc: &BreakLoc, cond: &str) -> anyhow::Result<String> {
        let base = self.op_break(loc)?;
        // The inspector transport sniffs the trailing ` if <expr>` and
        // feeds it to `Debugger.setBreakpointByUrl.condition`.
        Ok(format!("{base} if {cond}"))
    }
    fn op_run(&self, _args: &[String]) -> anyhow::Result<String> {
        Ok("cont".into())
    }
    fn op_continue(&self) -> anyhow::Result<String> {
        Ok("cont".into())
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
        Ok(format!("frame({n})"))
    }
    fn op_locals(&self) -> anyhow::Result<String> {
        Ok("locals".into())
    }
    fn op_print(&self, expr: &str) -> anyhow::Result<String> {
        Ok(format!("print {expr}"))
    }
    fn op_list(&self, loc: Option<&str>) -> anyhow::Result<String> {
        // The transport's `list` reads the top frame's current
        // line by default. An optional `file:line` argument retargets
        // to a specific location — passed through verbatim.
        Ok(match loc {
            Some(s) => format!("list {s}"),
            None => "list".into(),
        })
    }
    fn op_watch(&self, expr: &str) -> anyhow::Result<String> {
        Ok(format!("watch('{expr}')"))
    }

    /// Hit parsing over text output is never exercised for this
    /// backend — the inspector transport delivers structured
    /// `Debugger.paused` events that the daemon consumes via
    /// `pending_hit()`. Provided only so the trait is complete; a
    /// best-effort regex matches the text we emit on failure paths.
    fn parse_hit(&self, output: &str) -> Option<HitEvent> {
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| Regex::new(r"paused at (\S+):(\d+)").unwrap());
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
        // The inspector transport already emits a JSON object as the
        // `locals` response — just round-trip it.
        serde_json::from_str(output.trim()).ok()
    }
}
