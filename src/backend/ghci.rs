use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::canonical::{BreakLoc, CanonicalOps, HitEvent, unsupported};
use super::{Backend, Dependency, DependencyCheck, SpawnConfig};
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
                // `ghci --version` uses a compiled path that can succeed
                // even when the REPL runtime is broken (e.g. Homebrew
                // GHC with a missing /lib64/libc.so.6 symlink).
                // `-e '()'` forces the interpreter to start, catching
                // linker/runtime failures that --version would miss.
                version_cmd: Some(("ghci", &["-v0", "-e", "()"])),
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

    fn clean(&self, cmd: &str, output: &str) -> String {
        let trimmed = cmd.trim();
        let mut lines: Vec<&str> = Vec::new();

        for line in output.lines() {
            let l = line.trim();

            // Filter trace noise
            if l.starts_with("Logged breakpoint at ") {
                continue;
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
        if trimmed == ":history" || trimmed.starts_with(":history ") {
            lines
                .iter()
                .filter(|l| !l.trim().starts_with("Empty history"))
                .copied()
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            lines.join("\n")
        }
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
        // GHC <9.6: `Stopped at Main.hs:5:3-39`
        // GHC ≥9.6: `Stopped in Main.fibonacci.go, /path/Main.hs:20:16-35`
        // Match both forms with a single regex.
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| {
            // GHC <9.6: `Stopped at file:line:col`
            // GHC ≥9.6: `Stopped in symbol, file:line:col`
            // `[^,]+` for the symbol avoids eating the comma separator.
            Regex::new(r"Stopped (?:at|in [^,]+,)\s+(.+?):(\d+):\d+").unwrap()
        });
        for line in output.lines() {
            if let Some(c) = re.captures(line) {
                let file = c[1].to_string();
                let line_no: u32 = c[2].parse().ok()?;
                // Extract frame symbol from the "Stopped in <sym>," form.
                let frame_symbol = line
                    .strip_prefix("Stopped in ")
                    .and_then(|rest| rest.split(',').next())
                    .map(|s| s.trim().to_string());
                return Some(HitEvent {
                    location_key: format!("{file}:{line_no}"),
                    thread: None,
                    frame_symbol,
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
        assert!(r.contains("_result"));
        assert!(r.contains("n :: Integer = 12"));
    }

    #[test]
    fn clean_filters_loading_noise() {
        let input = "[1 of 2] Compiling Lib\nOk, modules loaded: Main, Lib.\nactual output here";
        let r = GhciBackend.clean(":load Main.hs", input);
        assert!(!r.contains("Compiling"));
        assert!(!r.contains("Ok, modules loaded"));
        assert!(r.contains("actual output here"));
    }

    #[test]
    fn clean_filters_version_banner() {
        let input = "GHCi, version 9.8.1\ntype :? for help\nλ> ";
        let r = GhciBackend.clean("", input);
        assert!(!r.contains("GHCi, version"));
        assert!(!r.contains("type :? for help"));
    }

    #[test]
    fn clean_passthrough_normal_output() {
        let input = "42";
        let r = GhciBackend.clean("2 + 40", input);
        assert_eq!(r.trim(), "42");
    }

    #[test]
    fn clean_filters_logged_breakpoint_noise() {
        let input = "Logged breakpoint at Main.hs:3:5\nLogged breakpoint at Main.hs:4:8\nStopped at Main.hs:5:1";
        let r = GhciBackend.clean(":trace main", input);
        assert!(!r.contains("Logged breakpoint"));
        assert!(r.contains("Stopped at"));
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
    fn parse_hit_stopped_at() {
        let raw = "Stopped at Main.hs:5:3-39\n_result :: [Integer]";
        let hit = GhciBackend.parse_hit(raw).expect("should match Stopped at");
        assert_eq!(hit.location_key, "Main.hs:5");
        assert_eq!(hit.line, Some(5));
    }

    #[test]
    fn parse_hit_stopped_in_ghc96() {
        let raw = "Stopped in Main.fibonacci.go, /path/algos.hs:20:16-35\n_result :: Integer";
        let hit = GhciBackend.parse_hit(raw).expect("should match Stopped in");
        assert_eq!(hit.location_key, "/path/algos.hs:20");
        assert_eq!(hit.line, Some(20));
        assert_eq!(hit.frame_symbol.as_deref(), Some("Main.fibonacci.go"));
    }

    #[test]
    fn prompt_pattern_matches() {
        let re = regex::Regex::new(GhciBackend.prompt_pattern()).unwrap();
        assert!(re.is_match("ghci> "));
        assert!(re.is_match("[/tmp/test.hs:3:15-35] ghci> "));
    }
}
