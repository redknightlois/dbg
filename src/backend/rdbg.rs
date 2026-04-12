use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

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
        let mut cmds: Vec<String> = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // rdbg help lists commands as "  command (alias) -- description"
            // or "  command    -- description"
            if let Some(first) = line.split_whitespace().next() {
                if first.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '!')
                    && first.len() < 20
                    && first.len() > 1
                    && !first.starts_with('-')
                {
                    cmds.push(first.to_string());
                }
            }
        }
        cmds.sort();
        cmds.dedup();
        format!("rdbg: {}", cmds.join(", "))
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("ruby.md", include_str!("../../skills/adapters/ruby.md"))]
    }

    fn clean(&self, cmd: &str, output: &str) -> CleanResult {
        let trimmed = cmd.trim();
        let mut events = Vec::new();
        let mut lines = Vec::new();

        for line in output.lines() {
            let l = line.trim();

            // Extract lifecycle events
            if l.starts_with("DEBUGGER: ") {
                events.push(l.to_string());
                continue;
            }

            // Extract stop events (breakpoint hits, catchpoints)
            if l.starts_with("Stop by ") || l.starts_with("Catch ") {
                events.push(l.to_string());
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

        CleanResult {
            output: lines.join("\n"),
            events,
        }
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
        assert!(r.events.iter().any(|e| e.contains("Session start")));
        assert!(!r.output.contains("DEBUGGER:"));
        assert!(r.output.contains("some output"));
    }

    #[test]
    fn clean_extracts_stop_events() {
        let input = "Stop by #0 BP - Line /path/test.rb:10\nlocal_var = 42";
        let r = RdbgBackend.clean("continue", input);
        assert!(r.events.iter().any(|e| e.contains("Stop by")));
        assert!(!r.output.contains("Stop by"), "stop event should not be duplicated in output");
        assert!(r.output.contains("local_var = 42"));
    }

    #[test]
    fn clean_filters_internal_frames_from_backtrace() {
        let input = "=>#0 test.rb:10:in 'main'\n  #1 /usr/lib/ruby/debug/session.rb:100\n  #2 <internal:kernel>:100\n  #3 test.rb:5:in 'setup'";
        let r = RdbgBackend.clean("bt", input);
        assert!(!r.output.contains("/debug/"));
        assert!(!r.output.contains("<internal:"));
        assert!(r.output.contains("test.rb:10"));
        assert!(r.output.contains("test.rb:5"));
    }

    #[test]
    fn clean_passthrough_normal_commands() {
        let input = "=> 42";
        let r = RdbgBackend.clean("p 6 * 7", input);
        assert_eq!(r.output, "=> 42");
        assert!(r.events.is_empty());
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
