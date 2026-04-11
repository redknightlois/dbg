use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};
use crate::daemon::session_tmp;

pub struct CallgrindBackend;

impl Backend for CallgrindBackend {
    fn name(&self) -> &'static str {
        "callgrind"
    }

    fn types(&self) -> &'static [&'static str] {
        &["callgrind"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let out_file = session_tmp("callgrind.out");
        let out_str = out_file.display().to_string();

        let mut valgrind_cmd = format!(
            "valgrind --tool=callgrind --callgrind-out-file={} {}",
            out_str, target
        );
        if !args.is_empty() {
            valgrind_cmd.push(' ');
            valgrind_cmd.push_str(&args.join(" "));
        }

        // Export path so the user can reference it without knowing the session dir
        Ok(SpawnConfig {
            bin: "bash".into(),
            args: vec!["--norc".into(), "--noprofile".into()],
            env: vec![
                ("PS1".into(), "$ ".into()),
                ("CALLGRIND_OUT".into(), out_str),
            ],
            init_commands: vec![
                valgrind_cmd,
                "echo '--- callgrind data ready ---'".into(),
            ],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"\$ $"
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![
            Dependency {
                name: "valgrind",
                check: DependencyCheck::Binary {
                    name: "valgrind",
                    alternatives: &["valgrind"],
                    version_cmd: None,
                },
                install: "sudo apt install valgrind  # or: brew install valgrind",
            },
            Dependency {
                name: "callgrind_annotate",
                check: DependencyCheck::Binary {
                    name: "callgrind_annotate",
                    alternatives: &["callgrind_annotate"],
                    version_cmd: None,
                },
                install: "sudo apt install valgrind  # included with valgrind",
            },
        ]
    }

    fn format_breakpoint(&self, _spec: &str) -> String {
        String::new()
    }

    fn run_command(&self) -> &'static str {
        "callgrind_annotate $CALLGRIND_OUT"
    }

    fn quit_command(&self) -> &'static str {
        "exit"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "callgrind: callgrind_annotate [--auto=yes] [--tree=both] [--threshold=N] $CALLGRIND_OUT".to_string()
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        let mut events = Vec::new();
        let mut lines = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("==") && trimmed.ends_with("==") {
                events.push(trimmed.to_string());
                continue;
            }
            lines.push(line);
        }
        CleanResult {
            output: lines.join("\n"),
            events,
        }
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("callgrind.md", include_str!("../../skills/adapters/callgrind.md"))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_separates_valgrind_lines_as_events() {
        let input = "==12345== Callgrind info==\n==12345== Using Valgrind==\nactual output\nmore output";
        let r = CallgrindBackend.clean("callgrind_annotate", input);
        assert_eq!(r.output, "actual output\nmore output");
        assert_eq!(r.events.len(), 2);
    }

    #[test]
    fn clean_keeps_incomplete_prefix() {
        let input = "==12345== no trailing equals\nkept";
        let r = CallgrindBackend.clean("echo", input);
        assert!(r.output.contains("no trailing equals"));
        assert!(r.output.contains("kept"));
        assert!(r.events.is_empty());
    }

    #[test]
    fn spawn_config_sets_env() {
        let cfg = CallgrindBackend.spawn_config("./app", &[]).unwrap();
        assert!(cfg.env.iter().any(|(k, _)| k == "CALLGRIND_OUT"));
        assert!(cfg.env.iter().any(|(k, v)| k == "PS1" && v == "$ "));
    }

    #[test]
    fn spawn_config_includes_args_in_valgrind_cmd() {
        let cfg = CallgrindBackend
            .spawn_config("./app", &["--flag".into()])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("./app"));
        assert!(cmd.contains("--flag"));
    }

    #[test]
    fn format_breakpoint_empty() {
        assert_eq!(CallgrindBackend.format_breakpoint("anything"), "");
    }
}
