use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct PdbBackend;

impl Backend for PdbBackend {
    fn name(&self) -> &'static str {
        "pdb"
    }

    fn types(&self) -> &'static [&'static str] {
        &["python", "py"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let python = std::env::var("PYTHON").unwrap_or_else(|_| "python3".into());
        let mut spawn_args = vec!["-u".into(), "-m".into(), "pdb".into(), target.into()];
        spawn_args.extend(args.iter().cloned());

        Ok(SpawnConfig {
            bin: python,
            args: spawn_args,
            env: vec![("PYTHONDONTWRITEBYTECODE".into(), "1".into())],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"\(Pdb\) "
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "python3",
            check: DependencyCheck::Binary {
                name: "python3",
                alternatives: &["python3"],
                version_cmd: None,
            },
            install: "sudo apt install python3  # or: brew install python",
        }]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        format!("break {spec}")
    }

    fn run_command(&self) -> &'static str {
        "continue"
    }

    fn parse_help(&self, raw: &str) -> String {
        let mut cmds: Vec<String> = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty()
                || line.starts_with('=')
                || line.starts_with("Documented")
                || line.starts_with("Undocumented")
                || line.starts_with("Miscellaneous")
            {
                continue;
            }
            for tok in line.split_whitespace() {
                if tok.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && tok.len() < 20
                    && !tok.is_empty()
                {
                    cmds.push(tok.to_string());
                }
            }
        }
        cmds.sort();
        cmds.dedup();
        format!("pdb: {}", cmds.join(", "))
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("python.md", include_str!("../../skills/adapters/python.md"))]
    }

    fn clean(&self, cmd: &str, output: &str) -> CleanResult {
        let trimmed = cmd.trim();
        let output = if trimmed == "where" || trimmed == "bt" {
            output
                .lines()
                .filter(|l| !l.contains("bdb.py") && !l.contains("<string>(1)"))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            output.to_string()
        };
        CleanResult {
            output,
            events: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_breakpoint() {
        assert_eq!(PdbBackend.format_breakpoint("test.py:10"), "break test.py:10");
    }

    #[test]
    fn clean_where_filters_bdb() {
        let input = "> script.py(5)main()\n  bdb.py(123)dispatch()\n> script.py(10)<module>()\n  <string>(1)<module>()";
        let r = PdbBackend.clean("where", input);
        assert!(!r.output.contains("bdb.py"));
        assert!(!r.output.contains("<string>(1)"));
        assert!(r.output.contains("script.py(5)"));
        assert!(r.output.contains("script.py(10)"));
    }

    #[test]
    fn clean_other_cmd_passthrough() {
        let input = "bdb.py line should stay";
        let r = PdbBackend.clean("p x", input);
        assert!(r.output.contains("bdb.py"));
    }

    #[test]
    fn spawn_config_includes_target_and_args() {
        let cfg = PdbBackend
            .spawn_config("test.py", &["--verbose".into()])
            .unwrap();
        assert!(cfg.args.contains(&"test.py".to_string()));
        assert!(cfg.args.contains(&"--verbose".to_string()));
        assert!(cfg.args.contains(&"-m".to_string()));
    }

    #[test]
    fn parse_help_extracts_and_deduplicates() {
        let raw = "Documented commands:\n========\nbreak  continue  help\nbreak  next  step";
        let result = PdbBackend.parse_help(raw);
        assert!(result.contains("break"));
        assert!(result.contains("continue"));
        let count = result.matches("break").count();
        assert_eq!(count, 1);
    }
}
