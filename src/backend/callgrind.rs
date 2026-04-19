use super::{Backend, Dependency, DependencyCheck, SpawnConfig, shell_escape};
use crate::daemon::session_tmp;

pub struct CallgrindBackend;

fn is_existing_callgrind_profile(target: &str) -> bool {
    let name = std::path::Path::new(target)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // Well-known valgrind output names + the generic ".out" extension
    // agents and CI pipelines produce.
    name.starts_with("callgrind.out")
        || name.contains(".callgrind.out")
        || name.ends_with(".out")
}

impl Backend for CallgrindBackend {
    fn name(&self) -> &'static str {
        "callgrind"
    }

    fn description(&self) -> &'static str {
        "call-graph profiler (valgrind)"
    }

    fn types(&self) -> &'static [&'static str] {
        &["callgrind"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let dbg_bin = super::self_exe();
        let path = std::path::Path::new(target);

        // If the caller pointed at an existing callgrind profile
        // (`callgrind.out.<pid>`, `*.callgrind.out`, or any `*.out`)
        // we skip the re-profile step and open it directly in the
        // REPL. The previous behaviour was to hand the .out to
        // valgrind as a binary, which always failed and left the
        // session looking "live" but unusable.
        if path.is_file() && is_existing_callgrind_profile(target) {
            let exec_repl = format!(
                "exec {} --phpprofile-repl {} --profile-prompt 'callgrind> '",
                dbg_bin,
                shell_escape(target),
            );
            return Ok(SpawnConfig {
                bin: "bash".into(),
                args: vec!["--norc".into(), "--noprofile".into()],
                env: vec![("PS1".into(), "callgrind> ".into())],
                init_commands: vec![exec_repl],
            });
        }

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
    fn spawn_config_loads_existing_callgrind_out() {
        // Regression: pointing `dbg start callgrind` at an existing
        // profile file (`cg_out.1234`, `foo.callgrind.out`, or any
        // `*.out`) used to re-run valgrind on the .out file itself,
        // which always fails because it is not an executable. The
        // session then appears "live" but every REPL command returns
        // "debuggee has exited". Existing profile files must be loaded
        // directly into the REPL instead of being re-profiled.
        let tmp = tempfile::NamedTempFile::with_suffix(".out").unwrap();
        let path = tmp.path().to_string_lossy().to_string();
        std::fs::write(&path, b"version: 1\ncmd: fake\n").unwrap();
        let cfg = CallgrindBackend.spawn_config(&path, &[]).unwrap();
        let joined = cfg.init_commands.join(" ; ");
        assert!(
            !joined.contains("--tool=callgrind"),
            "existing .out must not be re-profiled:\n{joined}"
        );
        assert!(
            cfg.init_commands.iter().any(|c| c.contains("--phpprofile-repl")),
            "REPL must still open on the existing profile:\n{joined}"
        );
        assert!(
            joined.contains(&path),
            "REPL command must reference the supplied profile path:\n{joined}"
        );
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
