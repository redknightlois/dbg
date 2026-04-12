use super::{Backend, Dependency, DependencyCheck, SpawnConfig, shell_escape};
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
            out_str, shell_escape(target)
        );
        for a in args {
            valgrind_cmd.push(' ');
            valgrind_cmd.push_str(&shell_escape(a));
        }

        let dbg_bin = super::self_exe();

        let exec_repl = format!(
            "exec {} --phpprofile-repl {} --profile-prompt 'callgrind> '",
            dbg_bin, out_str
        );

        Ok(SpawnConfig {
            bin: "bash".into(),
            args: vec!["--norc".into(), "--noprofile".into()],
            env: vec![
                ("PS1".into(), "callgrind> ".into()),
            ],
            init_commands: vec![
                valgrind_cmd,
                exec_repl,
            ],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"callgrind> $"
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
        ]
    }

    fn run_command(&self) -> &'static str {
        "stats"
    }

    fn quit_command(&self) -> &'static str {
        "exit"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "callgrind: hotspots, flat, calls, callers, inspect, stats, memory, search, tree, hotpath, focus, ignore, reset, help".to_string()
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("callgrind.md", include_str!("../../skills/adapters/callgrind.md"))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_config_execs_repl() {
        let cfg = CallgrindBackend.spawn_config("./app", &[]).unwrap();
        assert_eq!(cfg.bin, "bash");
        assert!(cfg.init_commands[0].contains("valgrind --tool=callgrind"));
        assert!(cfg.init_commands[0].contains("./app"));
        assert!(cfg.init_commands[1].contains("--phpprofile-repl"));
        assert!(cfg.init_commands[1].contains("exec"));
    }

    #[test]
    fn spawn_config_includes_args() {
        let cfg = CallgrindBackend
            .spawn_config("./app", &["--flag".into()])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("./app"));
        assert!(cmd.contains("--flag"));
    }

    #[test]
    fn prompt_pattern_matches() {
        let re = regex::Regex::new(CallgrindBackend.prompt_pattern()).unwrap();
        assert!(re.is_match("callgrind> "));
    }

    #[test]
    fn format_breakpoint_empty() {
        assert_eq!(CallgrindBackend.format_breakpoint("anything"), "");
    }

    #[test]
    fn spawn_config_escapes_spaces_in_target() {
        let cfg = CallgrindBackend
            .spawn_config("./my app", &[])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("'./my app'"), "target with space not escaped: {cmd}");
    }

    #[test]
    fn spawn_config_escapes_args_with_spaces() {
        let cfg = CallgrindBackend
            .spawn_config("./app", &["--dir=/my path".into()])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("'--dir=/my path'"), "arg with space not escaped: {cmd}");
    }

    #[test]
    fn spawn_config_escapes_shell_metacharacters() {
        let cfg = CallgrindBackend
            .spawn_config("./app$(evil)", &[])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("'./app$(evil)'"), "shell metacharacter not escaped: {cmd}");
    }
}
