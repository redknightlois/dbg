use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct JdbBackend;

impl Backend for JdbBackend {
    fn name(&self) -> &'static str {
        "jdb"
    }

    fn types(&self) -> &'static [&'static str] {
        &["java", "kotlin"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let mut spawn_args = vec![target.into()];
        spawn_args.extend(args.iter().cloned());

        Ok(SpawnConfig {
            bin: "jdb".into(),
            args: spawn_args,
            env: vec![],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"(\n> \z|\n\w+\[\d+\] \z)"
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "jdb",
            check: DependencyCheck::Binary {
                name: "jdb",
                alternatives: &["jdb"],
                version_cmd: None,
            },
            install: "sudo apt install default-jdk  # or: brew install openjdk",
        }]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        format!("stop at {spec}")
    }

    fn run_command(&self) -> &'static str {
        "run"
    }

    fn parse_help(&self, raw: &str) -> String {
        let mut cmds = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if let Some(tok) = line.split_whitespace().next() {
                if tok.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
                    && tok.len() < 20
                    && !tok.is_empty()
                    && tok.len() > 1
                {
                    cmds.push(tok.to_string());
                }
            }
        }
        cmds.dedup();
        format!("jdb: {}", cmds.join(", "))
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        let mut events = Vec::new();
        let mut lines = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("Set breakpoint") || trimmed.starts_with("Deferring breakpoint") {
                events.push(trimmed.to_string());
                continue;
            }
            if trimmed.contains("thread") && (trimmed.contains("started") || trimmed.contains("died")) {
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
        vec![("java.md", include_str!("../../skills/adapters/java.md"))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_breakpoint() {
        assert_eq!(JdbBackend.format_breakpoint("Main:10"), "stop at Main:10");
    }

    #[test]
    fn clean_extracts_breakpoint_events() {
        let input = "Set breakpoint at Main:10\nnormal output\nDeferring breakpoint Main:20";
        let r = JdbBackend.clean("stop at Main:10", input);
        assert_eq!(r.output, "normal output");
        assert_eq!(r.events.len(), 2);
    }

    #[test]
    fn clean_extracts_thread_events() {
        let input = "thread \"main\" started\noutput\nthread \"worker\" died";
        let r = JdbBackend.clean("run", input);
        assert_eq!(r.output, "output");
        assert_eq!(r.events.len(), 2);
    }

    #[test]
    fn parse_help_allows_hyphens() {
        let raw = "stop-in  Set breakpoint\ncont     Continue execution\nx single-char excluded";
        let result = JdbBackend.parse_help(raw);
        assert!(result.contains("stop-in"));
        assert!(result.contains("cont"));
        // single-char "x" excluded (len <= 1)
        assert!(!result.contains(", x,"));
    }
}
