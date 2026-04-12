use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig, shell_escape};

pub struct PerfBackend;

impl Backend for PerfBackend {
    fn name(&self) -> &'static str {
        "perf"
    }

    fn description(&self) -> &'static str {
        "Linux performance profiler"
    }

    fn types(&self) -> &'static [&'static str] {
        &["perf"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        // Two modes:
        // 1. "perf.data" or path to existing perf data → open perf report
        // 2. binary path → perf record -g ./binary, then perf report
        let path = std::path::Path::new(target);

        if path.is_file() && (target.contains("perf") || target.ends_with(".data")) {
            // Existing perf data — go straight to report in bash
            Ok(SpawnConfig {
                bin: "bash".into(),
                args: vec!["--norc".into(), "--noprofile".into()],
                env: vec![("PS1".into(), "perf> ".into())],
                init_commands: vec![
                    format!("perf report --stdio -i {}", shell_escape(target)),
                ],
            })
        } else {
            // Record then report
            let mut record_cmd = format!(
                "perf record -g -- {}",
                shell_escape(target)
            );
            for a in args {
                record_cmd.push(' ');
                record_cmd.push_str(&shell_escape(a));
            }

            Ok(SpawnConfig {
                bin: "bash".into(),
                args: vec!["--norc".into(), "--noprofile".into()],
                env: vec![("PS1".into(), "perf> ".into())],
                init_commands: vec![
                    record_cmd,
                    "perf report --stdio".into(),
                ],
            })
        }
    }

    fn prompt_pattern(&self) -> &str {
        r"perf> $"
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "perf",
            check: DependencyCheck::Binary {
                name: "perf",
                alternatives: &["perf"],
                version_cmd: None,
            },
            install: "sudo apt install linux-tools-$(uname -r)  # or: linux-tools-generic",
        }]
    }

    fn run_command(&self) -> &'static str {
        ""
    }

    fn preflight(&self) -> anyhow::Result<()> {
        // perf record requires kernel.perf_event_paranoid <= 2 (or the
        // caller has CAP_PERFMON/CAP_SYS_ADMIN). Paranoid level 3 is the
        // Ubuntu/Debian default since 22.04 — without this check the
        // `perf record` inside the daemon fails silently and the agent
        // sees an empty perf.data with no clue why.
        const PARANOID: &str = "/proc/sys/kernel/perf_event_paranoid";
        if let Ok(contents) = std::fs::read_to_string(PARANOID) {
            if let Ok(level) = contents.trim().parse::<i32>() {
                if level >= 3 {
                    anyhow::bail!(
                        "kernel.perf_event_paranoid={level} blocks `perf record` for \
                         unprivileged users.\n  \
                         Lower it once:  sudo sysctl kernel.perf_event_paranoid=1\n  \
                         Or persist:     echo 'kernel.perf_event_paranoid=1' | \
                         sudo tee /etc/sysctl.d/99-perf.conf && sudo sysctl --system\n  \
                         Or run with:    sudo dbg start perf <target>"
                    );
                }
            }
        }
        Ok(())
    }

    fn quit_command(&self) -> &'static str {
        "exit"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "perf: record, report, stat, annotate, script, top".to_string()
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        let mut lines = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            // Skip perf noise
            if trimmed.starts_with('#') && trimmed.contains("was taken at") {
                continue;
            }
            if trimmed.starts_with("# Total Lost") {
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
        vec![("perf.md", include_str!("../../skills/adapters/perf.md"))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_filters_perf_noise() {
        let input = "# Samples: 1234\n# was taken at 2024-01-01\n# Total Lost Samples: 5\nactual report data";
        let r = PerfBackend.clean("report", input);
        assert!(!r.output.contains("was taken at"));
        assert!(!r.output.contains("Total Lost"));
        assert!(r.output.contains("# Samples: 1234"));
        assert!(r.output.contains("actual report data"));
    }

    #[test]
    fn clean_passthrough_normal() {
        let r = PerfBackend.clean("report", "overhead  symbol\n  50%  main");
        assert!(r.output.contains("50%"));
    }

    #[test]
    fn spawn_config_existing_data_file() {
        let tmp = std::env::temp_dir().join("dbg-test-perf.data");
        std::fs::write(&tmp, "fake").unwrap();
        let cfg = PerfBackend
            .spawn_config(tmp.to_str().unwrap(), &[])
            .unwrap();
        assert_eq!(cfg.bin, "bash");
        assert!(cfg.init_commands[0].contains("perf report --stdio"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn spawn_config_binary_target() {
        let cfg = PerfBackend.spawn_config("./myapp", &[]).unwrap();
        assert_eq!(cfg.bin, "bash");
        assert!(cfg.init_commands[0].contains("perf record -g"));
        assert!(cfg.init_commands[0].contains("./myapp"));
        assert!(cfg.init_commands[1].contains("perf report --stdio"));
    }

    #[test]
    fn spawn_config_binary_with_args() {
        let cfg = PerfBackend
            .spawn_config("./myapp", &["--port".into(), "8080".into()])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("./myapp"));
        assert!(cmd.contains("--port"));
        assert!(cmd.contains("8080"));
    }

    #[test]
    fn format_breakpoint_empty() {
        assert_eq!(PerfBackend.format_breakpoint("anything"), "");
    }

    #[test]
    fn spawn_config_record_has_separator() {
        let cfg = PerfBackend.spawn_config("./myapp", &[]).unwrap();
        let cmd = &cfg.init_commands[0];
        // perf record must have -- separator before the target
        assert!(cmd.contains("-- "), "missing -- separator: {cmd}");
    }

    #[test]
    fn spawn_config_escapes_target_with_spaces() {
        let cfg = PerfBackend
            .spawn_config("./my app", &[])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("'./my app'"), "target not escaped: {cmd}");
    }

    #[test]
    fn spawn_config_uses_bash_with_perf_prompt() {
        let cfg = PerfBackend.spawn_config("./myapp", &[]).unwrap();
        assert_eq!(cfg.bin, "bash");
        assert!(cfg.env.iter().any(|(k, v)| k == "PS1" && v == "perf> "));
    }

    #[test]
    fn quit_command_exits_bash() {
        assert_eq!(PerfBackend.quit_command(), "exit");
    }
}
