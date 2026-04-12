use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct PhpdbgBackend;

impl Backend for PhpdbgBackend {
    fn name(&self) -> &'static str {
        "phpdbg"
    }

    fn types(&self) -> &'static [&'static str] {
        &["php"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let mut spawn_args = vec!["-q".into(), "-e".into(), target.into()];
        spawn_args.extend(args.iter().cloned());

        Ok(SpawnConfig {
            bin: "phpdbg".into(),
            args: spawn_args,
            env: vec![],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"prompt>"
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "phpdbg",
            check: DependencyCheck::Binary {
                name: "phpdbg",
                alternatives: &["phpdbg"],
                version_cmd: None,
            },
            install: "sudo apt install php  # phpdbg is bundled with PHP since 5.6",
        }]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        format!("break {spec}")
    }

    fn run_command(&self) -> &'static str {
        "run"
    }

    fn quit_command(&self) -> &'static str {
        "quit"
    }

    fn parse_help(&self, raw: &str) -> String {
        let mut cmds: Vec<String> = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("phpdbg") || line.starts_with("To") {
                continue;
            }
            // phpdbg help lists commands as "  command   alias  description"
            if let Some(first) = line.split_whitespace().next() {
                if first.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && first.len() < 20
                    && first.len() > 1
                {
                    cmds.push(first.to_string());
                }
            }
        }
        cmds.sort();
        cmds.dedup();
        format!("phpdbg: {}", cmds.join(", "))
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("php.md", include_str!("../../skills/adapters/php.md"))]
    }

    fn clean(&self, cmd: &str, output: &str) -> CleanResult {
        let trimmed = cmd.trim();
        let mut events = Vec::new();
        let mut lines = Vec::new();

        for line in output.lines() {
            let l = line.trim();
            // Extract lifecycle events
            if l.starts_with("[Welcome to phpdbg") || l.starts_with("[Successful compilation") {
                events.push(l.to_string());
                continue;
            }
            // Extract stop/breakpoint hit events
            if l.starts_with("[Breakpoint") || l.starts_with("[Break") {
                events.push(l.to_string());
            }
            // Filter internal noise from backtraces
            if (trimmed == "back" || trimmed == "t") && l.contains("phpdbg_exec") {
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
        assert_eq!(PhpdbgBackend.format_breakpoint("test.php:10"), "break test.php:10");
        assert_eq!(PhpdbgBackend.format_breakpoint("my_function"), "break my_function");
    }

    #[test]
    fn clean_extracts_breakpoint_events() {
        let input = "[Breakpoint #0 at test.php:10]\nsome output\nmore output";
        let r = PhpdbgBackend.clean("run", input);
        assert!(r.events.iter().any(|e| e.contains("Breakpoint #0")));
        assert!(r.output.contains("some output"));
    }

    #[test]
    fn clean_filters_phpdbg_exec_from_backtrace() {
        let input = "frame #0: test.php:10\nframe #1: phpdbg_exec stuff\nframe #2: test.php:5";
        let r = PhpdbgBackend.clean("back", input);
        assert!(!r.output.contains("phpdbg_exec"));
        assert!(r.output.contains("frame #0"));
        assert!(r.output.contains("frame #2"));
    }

    #[test]
    fn clean_welcome_as_event() {
        let input = "[Welcome to phpdbg, the interactive PHP debugger]\nready";
        let r = PhpdbgBackend.clean("", input);
        assert!(r.events.iter().any(|e| e.contains("Welcome")));
        assert!(r.output.contains("ready"));
    }

    #[test]
    fn clean_passthrough_normal_commands() {
        let input = "$x = 42";
        let r = PhpdbgBackend.clean("ev $x", input);
        assert_eq!(r.output, "$x = 42");
        assert!(r.events.is_empty());
    }

    #[test]
    fn spawn_config_includes_target_and_args() {
        let cfg = PhpdbgBackend
            .spawn_config("test.php", &["--verbose".into()])
            .unwrap();
        assert_eq!(cfg.bin, "phpdbg");
        assert!(cfg.args.contains(&"-q".to_string()));
        assert!(cfg.args.contains(&"-e".to_string()));
        assert!(cfg.args.contains(&"test.php".to_string()));
        assert!(cfg.args.contains(&"--verbose".to_string()));
    }

    #[test]
    fn parse_help_extracts_commands() {
        let raw = "phpdbg help\nTo get help...\n  exec     e   set execution context\n  run      r   attempt execution\n  step     s   step through\n  break    b   set breakpoint";
        let result = PhpdbgBackend.parse_help(raw);
        assert!(result.contains("exec"));
        assert!(result.contains("run"));
        assert!(result.contains("step"));
        assert!(result.contains("break"));
    }
}
