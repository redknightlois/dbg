use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::canonical::{BreakLoc, CanonicalOps, HitEvent};
use super::{Backend, Dependency, DependencyCheck, SpawnConfig};

pub struct RdbgBackend;

impl Backend for RdbgBackend {
    fn name(&self) -> &'static str {
        "rdbg"
    }

    fn description(&self) -> &'static str {
        "Ruby debugger"
    }

    fn types(&self) -> &'static [&'static str] {
        &["ruby", "rb"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let mut spawn_args = vec!["--no-color".into(), target.into()];
        if !args.is_empty() {
            spawn_args.push("--".into());
            spawn_args.extend(args.iter().cloned());
        }

        Ok(SpawnConfig {
            bin: "rdbg".into(),
            args: spawn_args,
            env: vec![
                ("RUBY_DEBUG_NO_COLOR".into(), "1".into()),
                ("RUBY_DEBUG_NO_RELINE".into(), "1".into()),
            ],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"\(rdbg[^)]*\) "
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![
            Dependency {
                name: "ruby",
                check: DependencyCheck::Binary {
                    name: "ruby",
                    alternatives: &["ruby"],
                    version_cmd: None,
                },
                install: "sudo apt install ruby  # or: brew install ruby",
            },
            Dependency {
                name: "rdbg",
                check: DependencyCheck::Binary {
                    name: "rdbg",
                    alternatives: &["rdbg"],
                    version_cmd: None,
                },
                install: "gem install debug  # Ruby 3.1+ includes it; older versions need the gem",
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
        "quit!"
    }

    fn parse_help(&self, raw: &str) -> String {
        // rdbg help lists commands as "  command (alias) -- description"
        // or "  command    -- description"
        super::parse_help_first_token(raw, "rdbg", true, |tok| {
            tok.len() > 1
                && tok.len() < 20
                && !tok.starts_with('-')
                && tok.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '!')
        })
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("ruby.md", include_str!("../../skills/adapters/ruby.md"))]
    }

    fn canonical_ops(&self) -> Option<&dyn CanonicalOps> { Some(self) }

    fn clean(&self, cmd: &str, output: &str) -> String {
        let trimmed = cmd.trim();
        let mut lines = Vec::new();

        for line in output.lines() {
            let l = line.trim();

            // Drop lifecycle / stop banners — agents query session state.
            if l.starts_with("DEBUGGER: ")
                || l.starts_with("Stop by ")
                || l.starts_with("Catch ")
            {
                continue;
            }

            // Filter internal debug gem frames from backtrace
            if (trimmed == "bt" || trimmed == "backtrace")
                && (l.contains("/debug/") || l.contains("<internal:"))
            {
                continue;
            }

            lines.push(line);
        }

        lines.join("\n")
    }
}

impl CanonicalOps for RdbgBackend {
    fn tool_name(&self) -> &'static str { "rdbg" }

    fn op_break(&self, loc: &BreakLoc) -> anyhow::Result<String> {
        Ok(match loc {
            BreakLoc::FileLine { file, line } => format!("break {file}:{line}"),
            BreakLoc::Fqn(name) => format!("break {name}"),
            BreakLoc::ModuleMethod { module, method } => format!("break {module}#{method}"),
        })
    }
    fn op_run(&self, _args: &[String]) -> anyhow::Result<String> { Ok("continue".into()) }
    fn op_continue(&self) -> anyhow::Result<String> { Ok("continue".into()) }
    fn op_step(&self) -> anyhow::Result<String> { Ok("step".into()) }
    fn op_next(&self) -> anyhow::Result<String> { Ok("next".into()) }
    fn op_finish(&self) -> anyhow::Result<String> { Ok("finish".into()) }
    fn op_stack(&self, _n: Option<u32>) -> anyhow::Result<String> { Ok("bt".into()) }
    fn op_frame(&self, n: u32) -> anyhow::Result<String> { Ok(format!("frame {n}")) }
    fn op_locals(&self) -> anyhow::Result<String> { Ok("info".into()) }
    fn op_print(&self, expr: &str) -> anyhow::Result<String> { Ok(format!("p {expr}")) }
    fn op_list(&self, _loc: Option<&str>) -> anyhow::Result<String> { Ok("list".into()) }
    fn op_breaks(&self) -> anyhow::Result<String> {
        // rdbg's native list-breakpoints verb is `info breakpoint` (alias
        // `info b`); the default trait string `breakpoint list` raises
        // `undefined local variable or method 'list'` because rdbg
        // evaluates unknown commands as Ruby expressions in the debuggee.
        Ok("info breakpoint".into())
    }

    fn parse_hit(&self, output: &str) -> Option<HitEvent> {
        // rdbg raw: `Stop by #0  BP - Line  /path/to/file.rb:10`
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| {
            Regex::new(r"Stop by #\d+\s+BP - Line\s+(\S+):(\d+)").unwrap()
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
        // rdbg `info` prints `%self = #<...>`, `a = 0`, `b = 1`, etc.
        let mut obj = Map::new();
        for line in output.lines() {
            let line = line.trim();
            if let Some((name, val)) = line.split_once(" = ") {
                let name = name.trim().trim_start_matches('%').to_string();
                if name.is_empty() || name == "self" { continue; }
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
    fn format_breakpoint() {
        assert_eq!(RdbgBackend.format_breakpoint("test.rb:10"), "break test.rb:10");
        assert_eq!(
            RdbgBackend.format_breakpoint("MyClass#method"),
            "break MyClass#method"
        );
    }

    #[test]
    fn clean_extracts_debugger_events() {
        let input = "DEBUGGER: Session start (pid: 12345)\nsome output\nmore output";
        let r = RdbgBackend.clean("continue", input);
        assert!(!r.contains("DEBUGGER:"));
        assert!(r.contains("some output"));
    }

    #[test]
    fn clean_extracts_stop_events() {
        let input = "Stop by #0 BP - Line /path/test.rb:10\nlocal_var = 42";
        let r = RdbgBackend.clean("continue", input);
        assert!(!r.contains("Stop by"), "stop event should not be duplicated in output");
        assert!(r.contains("local_var = 42"));
    }

    #[test]
    fn clean_filters_internal_frames_from_backtrace() {
        let input = "=>#0 test.rb:10:in 'main'\n  #1 /usr/lib/ruby/debug/session.rb:100\n  #2 <internal:kernel>:100\n  #3 test.rb:5:in 'setup'";
        let r = RdbgBackend.clean("bt", input);
        assert!(!r.contains("/debug/"));
        assert!(!r.contains("<internal:"));
        assert!(r.contains("test.rb:10"));
        assert!(r.contains("test.rb:5"));
    }

    #[test]
    fn clean_passthrough_normal_commands() {
        let input = "=> 42";
        let r = RdbgBackend.clean("p 6 * 7", input);
        assert_eq!(r, "=> 42");
    }

    #[test]
    fn spawn_config_includes_target_and_args() {
        let cfg = RdbgBackend
            .spawn_config("test.rb", &["--verbose".into()])
            .unwrap();
        assert_eq!(cfg.bin, "rdbg");
        assert!(cfg.args.contains(&"--no-color".to_string()));
        assert!(cfg.args.contains(&"test.rb".to_string()));
        assert!(cfg.args.contains(&"--verbose".to_string()));
    }

    #[test]
    fn spawn_config_no_args() {
        let cfg = RdbgBackend.spawn_config("test.rb", &[]).unwrap();
        assert_eq!(cfg.bin, "rdbg");
        assert_eq!(cfg.args, vec!["--no-color", "test.rb"]);
    }

    #[test]
    fn parse_help_extracts_commands() {
        let raw = "  break (b)    -- set breakpoint\n  continue (c) -- continue execution\n  step (s)     -- step in\n  next (n)     -- step over\n  finish (fin) -- step out";
        let result = RdbgBackend.parse_help(raw);
        assert!(result.contains("break"));
        assert!(result.contains("continue"));
        assert!(result.contains("step"));
        assert!(result.contains("next"));
        assert!(result.contains("finish"));
    }

    #[test]
    fn prompt_pattern_matches() {
        let re = regex::Regex::new(RdbgBackend.prompt_pattern()).unwrap();
        assert!(re.is_match("(rdbg) "));
        assert!(re.is_match("(rdbg#main) "));
        assert!(re.is_match("(rdbg:irb) "));
    }
}
