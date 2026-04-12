use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig, shell_escape};
use crate::daemon::session_tmp;

pub struct MassifBackend;

impl Backend for MassifBackend {
    fn name(&self) -> &'static str {
        "massif"
    }

    fn types(&self) -> &'static [&'static str] {
        &["massif"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let out_file = session_tmp("massif.out");
        let out_str = out_file.display().to_string();

        let mut valgrind_cmd = format!(
            "valgrind --tool=massif --massif-out-file={} {}",
            out_str, shell_escape(target)
        );
        for a in args {
            valgrind_cmd.push(' ');
            valgrind_cmd.push_str(&shell_escape(a));
        }

        Ok(SpawnConfig {
            bin: "bash".into(),
            args: vec!["--norc".into(), "--noprofile".into()],
            env: vec![
                ("PS1".into(), "$ ".into()),
                ("MASSIF_OUT".into(), out_str),
            ],
            init_commands: vec![
                valgrind_cmd,
                "echo '--- massif data ready ---'".into(),
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
                name: "ms_print",
                check: DependencyCheck::Binary {
                    name: "ms_print",
                    alternatives: &["ms_print"],
                    version_cmd: None,
                },
                install: "sudo apt install valgrind  # included with valgrind",
            },
        ]
    }

    fn run_command(&self) -> &'static str {
        "ms_print $MASSIF_OUT"
    }

    fn quit_command(&self) -> &'static str {
        "exit"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "massif: ms_print [--threshold=N] $MASSIF_OUT".to_string()
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        let mut lines = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("==") && (trimmed.contains("Massif") || trimmed.contains("Copyright")) {
                continue;
            }
            lines.push(line);
        }
        CleanResult {
            output: lines.join("\n"),
            events: vec![],
        }
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("massif.md", include_str!("../../skills/adapters/massif.md"))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_filters_massif_header() {
        let input = "  == Massif, a heap profiler\n  == Copyright (C) 2022\nactual data\nmore data";
        let r = MassifBackend.clean("ms_print", input);
        assert!(!r.output.contains("Massif"));
        assert!(!r.output.contains("Copyright"));
        assert!(r.output.contains("actual data"));
    }

    #[test]
    fn clean_keeps_non_header_lines() {
        let input = "  == Some other valgrind line\ndata";
        let r = MassifBackend.clean("ms_print", input);
        assert!(r.output.contains("Some other valgrind line"));
    }

    #[test]
    fn spawn_config_sets_massif_out_env() {
        let cfg = MassifBackend.spawn_config("./app", &[]).unwrap();
        assert!(cfg.env.iter().any(|(k, _)| k == "MASSIF_OUT"));
    }

    #[test]
    fn spawn_config_valgrind_cmd_with_args() {
        let cfg = MassifBackend
            .spawn_config("./app", &["--size".into(), "100".into()])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("--tool=massif"));
        assert!(cmd.contains("./app"));
        assert!(cmd.contains("--size"));
        assert!(cmd.contains("100"));
    }

    #[test]
    fn spawn_config_escapes_spaces() {
        let cfg = MassifBackend
            .spawn_config("./my app", &[])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("'./my app'"), "target not escaped: {cmd}");
    }
}
