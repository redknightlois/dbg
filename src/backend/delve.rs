use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct DelveBackend;

impl Backend for DelveBackend {
    fn name(&self) -> &'static str {
        "delve"
    }

    fn types(&self) -> &'static [&'static str] {
        &["go"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let mut spawn_args = vec!["exec".into(), target.into()];
        if !args.is_empty() {
            spawn_args.push("--".into());
            spawn_args.extend(args.iter().cloned());
        }

        Ok(SpawnConfig {
            bin: "dlv".into(),
            args: spawn_args,
            env: vec![],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"\(dlv\) "
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![
            Dependency {
                name: "go",
                check: DependencyCheck::Binary {
                    name: "go",
                    alternatives: &["go"],
                    version_cmd: None,
                },
                install: "https://go.dev/dl/",
            },
            Dependency {
                name: "dlv",
                check: DependencyCheck::Binary {
                    name: "dlv",
                    alternatives: &["dlv"],
                    version_cmd: None,
                },
                install: "go install github.com/go-delve/delve/cmd/dlv@latest",
            },
        ]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        format!("break {spec}")
    }

    fn run_command(&self) -> &'static str {
        "continue"
    }

    fn parse_help(&self, raw: &str) -> String {
        let mut cmds = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            // Delve help: "command (alias) description" or "command  description"
            if let Some(tok) = line.split_whitespace().next() {
                if tok.chars().all(|c| c.is_ascii_alphabetic())
                    && tok.len() < 20
                    && !tok.is_empty()
                    && tok != "Type"
                    && tok != "Aliases"
                {
                    cmds.push(tok.to_string());
                }
            }
        }
        cmds.dedup();
        format!("delve: {}", cmds.join(", "))
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        let mut events = Vec::new();
        let mut lines = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("Created breakpoint") {
                events.push(trimmed.to_string());
                continue;
            }
            if trimmed.contains("goroutine") && trimmed.contains("exited") {
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
        vec![("go.md", include_str!("../../skills/adapters/go.md"))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_breakpoint() {
        assert_eq!(DelveBackend.format_breakpoint("main.go:10"), "break main.go:10");
    }

    #[test]
    fn clean_extracts_breakpoint_events() {
        let input = "Created breakpoint 1 at main.go:10\nnormal output";
        let r = DelveBackend.clean("break main.go:10", input);
        assert_eq!(r.output, "normal output");
        assert_eq!(r.events.len(), 1);
        assert!(r.events[0].contains("Created breakpoint"));
    }

    #[test]
    fn clean_extracts_goroutine_exit() {
        let input = "some output\ngoroutine 1 exited\nmore output";
        let r = DelveBackend.clean("continue", input);
        assert!(r.output.contains("some output"));
        assert!(r.output.contains("more output"));
        assert!(!r.output.contains("goroutine"));
        assert_eq!(r.events.len(), 1);
    }

    #[test]
    fn spawn_config_exec_with_args() {
        let cfg = DelveBackend
            .spawn_config("./app", &["--port".into(), "8080".into()])
            .unwrap();
        assert_eq!(cfg.args[0], "exec");
        assert_eq!(cfg.args[1], "./app");
        assert_eq!(cfg.args[2], "--");
        assert_eq!(cfg.args[3], "--port");
    }

    #[test]
    fn spawn_config_exec_no_args() {
        let cfg = DelveBackend.spawn_config("./app", &[]).unwrap();
        assert_eq!(cfg.args, vec!["exec", "./app"]);
    }

    #[test]
    fn parse_help_filters_noise() {
        let raw = "The following commands are available:\n  break   Set breakpoint\n  continue Resume\n  Type help for more\n  Aliases for break: b";
        let result = DelveBackend.parse_help(raw);
        assert!(result.contains("break"));
        assert!(result.contains("continue"));
        assert!(!result.contains("Type"));
        assert!(!result.contains("Aliases"));
    }
}
