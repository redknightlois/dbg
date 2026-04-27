use super::{Backend, Dependency, DependencyCheck, SpawnConfig, shell_escape};

pub struct MemcheckBackend;

impl Backend for MemcheckBackend {
    fn name(&self) -> &'static str {
        "memcheck"
    }

    fn description(&self) -> &'static str {
        "memory error detector (valgrind)"
    }

    fn types(&self) -> &'static [&'static str] {
        &["memcheck", "valgrind"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        // Capture valgrind's stderr to a session-scoped log so that
        // `dbg run` / run_command() can replay the summary later —
        // previously the output streamed directly to the PTY and was
        // unrecoverable once the prompt returned.
        let log_path = crate::daemon::session_tmp("memcheck.log");
        let log_str = log_path.display().to_string();
        let mut valgrind_cmd = format!(
            "valgrind --tool=memcheck --leak-check=full --show-leak-kinds=all \
             --track-origins=yes --log-file={} {}",
            shell_escape(&log_str),
            shell_escape(target),
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
                ("DBG_MEMCHECK_LOG".into(), log_str),
            ],
            init_commands: vec![
                valgrind_cmd,
                "cat \"$DBG_MEMCHECK_LOG\"".into(),
                "echo '--- memcheck done ---'".into(),
            ],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"\$ $"
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "valgrind",
            check: DependencyCheck::Binary {
                name: "valgrind",
                alternatives: &["valgrind"],
                version_cmd: None,
            },
            install: "sudo apt install valgrind  # or: brew install valgrind",
        }]
    }

    fn run_command(&self) -> &'static str {
        // `dbg run` on a memcheck session re-prints the captured
        // valgrind log. Previously this was an empty string, which
        // sent a blank line to bash and produced no output at all —
        // so `dbg start memcheck ./app --run` silently returned
        // nothing with no hint that output was available elsewhere.
        "cat \"$DBG_MEMCHECK_LOG\""
    }

    fn quit_command(&self) -> &'static str {
        "exit"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "memcheck: memory error detector — reports use-after-free, uninitialized reads, leaks, buffer overflows".to_string()
    }

    fn clean(&self, _cmd: &str, output: &str) -> String {
        output.to_string()
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("memcheck.md", include_str!("../../skills/adapters/memcheck.md"))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_passes_summary_through() {
        // Memcheck no longer parses lifecycle events — the cleaned
        // output is the raw valgrind output verbatim.
        let input = "==123== HEAP SUMMARY:\n==123==     in use at exit: 400 bytes";
        let r = MemcheckBackend.clean("valgrind", input);
        assert!(r.contains("HEAP SUMMARY"));
        assert!(r.contains("in use at exit"));
    }

    #[test]
    fn run_command_replays_valgrind_log() {
        // Regression: `run_command()` used to return "" so that
        // `dbg run` sent a blank line to bash and produced no output.
        // It must now emit a non-empty shell command that surfaces
        // the captured memcheck log so agents have something to read.
        let cmd = MemcheckBackend.run_command();
        assert!(
            !cmd.trim().is_empty(),
            "run_command must not be empty — a blank line to bash yields no output"
        );
        assert!(
            cmd.contains("DBG_MEMCHECK_LOG"),
            "run_command should read the session log env var, got: {cmd}"
        );
    }

    #[test]
    fn spawn_config_exports_log_path_and_writes_to_it() {
        let cfg = MemcheckBackend.spawn_config("./app", &[]).unwrap();
        assert!(
            cfg.env.iter().any(|(k, _)| k == "DBG_MEMCHECK_LOG"),
            "spawn_config must export DBG_MEMCHECK_LOG"
        );
        let valgrind_cmd = &cfg.init_commands[0];
        assert!(
            valgrind_cmd.contains("--log-file="),
            "valgrind must write to a log file so `dbg run` can replay it, got: {valgrind_cmd}"
        );
    }

    #[test]
    fn spawn_config_includes_full_flags() {
        let cfg = MemcheckBackend.spawn_config("./app", &[]).unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("--leak-check=full"));
        assert!(cmd.contains("--track-origins=yes"));
        assert!(cmd.contains("--show-leak-kinds=all"));
    }

    #[test]
    fn spawn_config_appends_args() {
        let cfg = MemcheckBackend
            .spawn_config("./app", &["arg1".into()])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("./app"));
        assert!(cmd.contains("arg1"));
    }

    #[test]
    fn spawn_config_escapes_spaces() {
        let cfg = MemcheckBackend
            .spawn_config("./my app", &["--dir=/my path".into()])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("'./my app'"), "target not escaped: {cmd}");
        assert!(cmd.contains("'--dir=/my path'"), "arg not escaped: {cmd}");
    }

    #[test]
    fn spawn_config_escapes_shell_metacharacters() {
        let cfg = MemcheckBackend
            .spawn_config("./app;rm -rf /", &[])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("'./app;rm -rf /'"), "metachar not escaped: {cmd}");
    }
}
