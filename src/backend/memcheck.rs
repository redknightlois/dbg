use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct MemcheckBackend;

impl Backend for MemcheckBackend {
    fn name(&self) -> &'static str {
        "memcheck"
    }

    fn types(&self) -> &'static [&'static str] {
        &["memcheck", "valgrind"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let mut valgrind_cmd = format!(
            "valgrind --tool=memcheck --leak-check=full --show-leak-kinds=all --track-origins=yes {} {}",
            target,
            args.join(" ")
        );
        if args.is_empty() {
            valgrind_cmd = format!(
                "valgrind --tool=memcheck --leak-check=full --show-leak-kinds=all --track-origins=yes {}",
                target
            );
        }

        Ok(SpawnConfig {
            bin: "bash".into(),
            args: vec!["--norc".into(), "--noprofile".into()],
            env: vec![("PS1".into(), "$ ".into())],
            init_commands: vec![
                valgrind_cmd,
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

    fn format_breakpoint(&self, _spec: &str) -> String {
        String::new()
    }

    fn run_command(&self) -> &'static str {
        ""
    }

    fn quit_command(&self) -> &'static str {
        "exit"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "memcheck: memory error detector — reports use-after-free, uninitialized reads, leaks, buffer overflows".to_string()
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        let mut events = Vec::new();
        let mut lines = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            // Valgrind prefix lines (==PID==) with summary info → events
            if trimmed.starts_with("==") && trimmed.contains("== HEAP SUMMARY:") {
                events.push("heap summary available".to_string());
            }
            if trimmed.starts_with("==") && trimmed.contains("== LEAK SUMMARY:") {
                events.push("leak summary available".to_string());
            }
            lines.push(line);
        }
        CleanResult {
            output: lines.join("\n"),
            events,
        }
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("memcheck.md", include_str!("../../skills/adapters/memcheck.md"))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_emits_heap_summary_event() {
        let input = "==123== HEAP SUMMARY:\n==123==     in use at exit: 400 bytes";
        let r = MemcheckBackend.clean("valgrind", input);
        assert!(r.events.iter().any(|e| e == "heap summary available"));
        assert!(r.output.contains("HEAP SUMMARY"));
    }

    #[test]
    fn clean_emits_leak_summary_event() {
        let input = "==123== LEAK SUMMARY:\n==123==    definitely lost: 400 bytes";
        let r = MemcheckBackend.clean("valgrind", input);
        assert!(r.events.iter().any(|e| e == "leak summary available"));
    }

    #[test]
    fn clean_no_events_on_clean_output() {
        let r = MemcheckBackend.clean("echo", "no valgrind output here");
        assert!(r.events.is_empty());
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
        assert!(cmd.contains("./app arg1"));
    }
}
