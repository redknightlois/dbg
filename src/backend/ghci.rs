use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::canonical::{BreakLoc, CanonicalOps, HitEvent, unsupported};
use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};
use crate::check::find_bin;

pub struct GhciBackend;

impl Backend for GhciBackend {
    fn name(&self) -> &'static str {
        "ghci"
    }

    fn description(&self) -> &'static str {
        "Haskell debugger (GHCi)"
    }

    fn types(&self) -> &'static [&'static str] {
        &["haskell", "hs"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let mut spawn_args = vec![
            "-v0".into(),               // suppress GHC version/loading noise
            "-fbreak-on-exception".into(), // break on exceptions (useful for debugging)
            "-ignore-dot-ghci".into(),  // don't load user .ghci (predictable behaviour)
            target.into(),
        ];
        spawn_args.extend(args.iter().cloned());

        Ok(SpawnConfig {
            bin: find_bin("ghci"),
            args: spawn_args,
            env: vec![],
            init_commands: vec![
                // Enable useful debugging defaults
                ":set -fghci-hist-size=50".into(),
            ],
        })
    }

    fn prompt_pattern(&self) -> &str {
        // Matches "ghci> " and "[file:line:col] ghci> " (at breakpoint stop)
        r"(\[.*\] )?ghci> "
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "ghci",
            check: DependencyCheck::Binary {
                name: "ghci",
                alternatives: &["ghci"],
                version_cmd: Some(("ghc", &["--version"])),
            },
            install: "curl --proto '=https' --tlsv1.2 -sSf https://get-ghcup.haskell.org | sh  # or: sudo apt install ghc",
        }]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        format!(":break {spec}")
    }

    fn run_command(&self) -> &'static str {
        ":trace main"
    }

    fn quit_command(&self) -> &'static str {
        ":quit"
    }

    fn help_command(&self) -> &'static str {
        ":?"
    }

    fn parse_help(&self, raw: &str) -> String {
        let mut cmds: Vec<String> = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // GHCi help lines look like: ":command  description" or ":cmd1 :cmd2  description"
            for tok in line.split_whitespace() {
                if tok.starts_with(':')
                    && tok.len() > 1
                    && tok.len() < 25
                    && tok[1..].chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '!')
                {
                    cmds.push(tok.to_string());
                }
            }
        }
        cmds.sort();
        cmds.dedup();
        format!("ghci: {}", cmds.join(", "))
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("haskell.md", include_str!("../../skills/adapters/haskell.md"))]
    }

    fn clean(&self, cmd: &str, output: &str) -> CleanResult {
        let trimmed = cmd.trim();
        let mut events = Vec::new();
        let mut lines: Vec<&str> = Vec::new();

        for line in output.lines() {
            let l = line.trim();

            // Extract stop events (breakpoint hits)
            if l.starts_with("Stopped at ") {
                events.push(l.to_string());
            }

            // Extract trace events
            if l.starts_with("Logged breakpoint at ") {
                events.push(l.to_string());
                continue; // noise during :trace
            }

            // Filter GHCi internal noise
            if l.starts_with("Some flags have not been recognized:")
                || l.starts_with("GHCi, version")
                || l.starts_with("type :? for help")
            {
                continue;
            }

            // Filter redundant module-loading messages
            if (l.starts_with("[") && l.contains("Compiling") && l.contains("]"))
                || l.starts_with("Ok, ")
                || l.starts_with("Ok, modules loaded:")
            {
                continue;
            }

            // For :history, keep as-is (already compact)
            // For :show bindings, keep as-is
            // For :back/:forward, the stop event + bindings are useful
            lines.push(line);
        }

        // For backtrace (:history), strip the "logged events" preamble noise
        let output = if trimmed == ":history" || trimmed.starts_with(":history ") {
            lines
                .iter()
                .filter(|l| !l.trim().starts_with("Empty history"))
                .copied()
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            lines.join("\n")
        };

        CleanResult { output, events }
    }

    fn canonical_ops(&self) -> Option<&dyn CanonicalOps> { Some(self) }
}

impl CanonicalOps for GhciBackend {
    fn tool_name(&self) -> &'static str { "ghci" }
    fn auto_capture_locals(&self) -> bool { false }

    fn op_break(&self, loc: &BreakLoc) -> anyhow::Result<String> {
        Ok(match loc {
            BreakLoc::FileLine { file: _, line } => {
                // `:break <line>` sets a breakpoint in the most recently
                // loaded module. We can't reliably infer the Haskell module
                // name from the filename (algos.hs → Main, not Algos).
                format!(":break {line}")
            }
            BreakLoc::Fqn(name) => format!(":break {name}"),
            BreakLoc::ModuleMethod { module, method } => format!(":break {module}.{method}"),
        })
    }
    fn op_run(&self, _args: &[String]) -> anyhow::Result<String> { Ok(":trace main".into()) }
    fn op_continue(&self) -> anyhow::Result<String> { Ok(":continue".into()) }
    fn op_step(&self) -> anyhow::Result<String> { Ok(":step".into()) }
    fn op_next(&self) -> anyhow::Result<String> { Ok(":steplocal".into()) }
    fn op_finish(&self) -> anyhow::Result<String> {
        Err(unsupported("ghci", "step-out (Haskell uses :back for time-travel)"))
    }
    fn op_stack(&self, n: Option<u32>) -> anyhow::Result<String> {
        Ok(match n {
            Some(k) => format!(":history {k}"),
            None => ":history".into(),
        })
    }
    fn op_frame(&self, _n: u32) -> anyhow::Result<String> { Ok(":back".into()) }
    fn op_locals(&self) -> anyhow::Result<String> { Ok(":show bindings".into()) }
    fn op_print(&self, expr: &str) -> anyhow::Result<String> { Ok(expr.to_string()) }
    fn op_list(&self, _loc: Option<&str>) -> anyhow::Result<String> { Ok(":list".into()) }

    fn parse_hit(&self, output: &str) -> Option<HitEvent> {
        // GHCi raw: `Stopped at Main.hs:5:3-39` (stays in output even
        // though clean() also pushes it to events — we parse raw).
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| {
            Regex::new(r"Stopped at (\S+):(\d+)").unwrap()
        });
        for line in output.lines() {
            if let Some(c) = re.captures(line) {
                let file = c[1].to_string();
                let line_no: u32 = c[2].parse().ok()?;
                return Some(HitEvent {
                    location_key: format!("{file}:{line_no}"),
                    thread: None,
                    frame_symbol: None,
                    file: Some(file),
                    line: Some(line_no),
                });
            }
        }
        None
    }

    fn parse_locals(&self, output: &str) -> Option<Value> {
        // `:show bindings` prints `name :: Type = value` lines.
        let mut obj = Map::new();
        for line in output.lines() {
            let line = line.trim();
            if let Some((before_eq, val)) = line.split_once(" = ") {
                let name = before_eq
                    .split("::")
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if name.is_empty() || name.starts_with("it") { continue; }
                let mut entry = Map::new();
                entry.insert("value".into(), Value::String(val.trim().to_string()));
                obj.insert(name, Value::Object(entry));
            }
        }
        if obj.is_empty() { None } else { Some(Value::Object(obj)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_breakpoint_function() {
        assert_eq!(GhciBackend.format_breakpoint("main"), ":break main");
    }

    #[test]
    fn format_breakpoint_module_line() {
        assert_eq!(GhciBackend.format_breakpoint("Main 42"), ":break Main 42");
    }

    #[test]
    fn clean_extracts_stop_events() {
        let input = "Stopped at Main.hs:5:3-39\n_result :: [Integer]\nn :: Integer = 12";
        let r = GhciBackend.clean(":continue", input);
        assert!(r.events.iter().any(|e| e.contains("Stopped at")));
        assert!(r.output.contains("_result"));
        assert!(r.output.contains("n :: Integer = 12"));
    }

    #[test]
    fn clean_filters_loading_noise() {
        let input = "[1 of 2] Compiling Lib\nOk, modules loaded: Main, Lib.\nactual output here";
        let r = GhciBackend.clean(":load Main.hs", input);
        assert!(!r.output.contains("Compiling"));
        assert!(!r.output.contains("Ok, modules loaded"));
        assert!(r.output.contains("actual output here"));
    }

    #[test]
    fn clean_filters_version_banner() {
        let input = "GHCi, version 9.8.1\ntype :? for help\nλ> ";
        let r = GhciBackend.clean("", input);
        assert!(!r.output.contains("GHCi, version"));
        assert!(!r.output.contains("type :? for help"));
    }

    #[test]
    fn clean_passthrough_normal_output() {
        let input = "42";
        let r = GhciBackend.clean("2 + 40", input);
        assert_eq!(r.output.trim(), "42");
        assert!(r.events.is_empty());
    }

    #[test]
    fn clean_filters_logged_breakpoint_noise() {
        let input = "Logged breakpoint at Main.hs:3:5\nLogged breakpoint at Main.hs:4:8\nStopped at Main.hs:5:1";
        let r = GhciBackend.clean(":trace main", input);
        assert!(!r.output.contains("Logged breakpoint"));
        assert!(r.output.contains("Stopped at"));
        assert_eq!(r.events.len(), 3); // 2 logged + 1 stopped
    }

    #[test]
    fn spawn_config_flags() {
        let cfg = GhciBackend.spawn_config("Main.hs", &[]).unwrap();
        assert!(cfg.bin.contains("ghci"), "bin should contain ghci: {}", cfg.bin);
        assert!(cfg.args.contains(&"-v0".to_string()));
        assert!(cfg.args.contains(&"-fbreak-on-exception".to_string()));
        assert!(cfg.args.contains(&"Main.hs".to_string()));
    }

    #[test]
    fn parse_help_extracts_colon_commands() {
        let raw = "  :break      set a breakpoint\n  :continue   resume execution\n  :step       single-step\n  :type       show type\n  some non-command line";
        let result = GhciBackend.parse_help(raw);
        assert!(result.contains(":break"));
        assert!(result.contains(":continue"));
        assert!(result.contains(":step"));
        assert!(result.contains(":type"));
        assert!(!result.contains("some"));
    }

    #[test]
    fn prompt_pattern_matches() {
        let re = regex::Regex::new(GhciBackend.prompt_pattern()).unwrap();
        assert!(re.is_match("ghci> "));
        assert!(re.is_match("[/tmp/test.hs:3:15-35] ghci> "));
    }
}
