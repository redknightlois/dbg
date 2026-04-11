use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct PerfBackend;

impl Backend for PerfBackend {
    fn name(&self) -> &'static str {
        "perf"
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
            // Existing perf data — go straight to report
            Ok(SpawnConfig {
                bin: "perf".into(),
                args: vec![
                    "report".into(),
                    "--stdio".into(),
                    "-i".into(),
                    target.into(),
                ],
                env: vec![],
                init_commands: vec![],
            })
        } else {
            // Record then report — use init_commands to record first
            let mut record_args = vec![
                "record".into(),
                "-g".into(),
                "--".into(),
                target.into(),
            ];
            record_args.extend(args.iter().cloned());

            Ok(SpawnConfig {
                bin: "perf".into(),
                args: vec!["report".into(), "--tui".into()],
                env: vec![],
                init_commands: vec![],
            })
        }
    }

    fn prompt_pattern(&self) -> &str {
        // perf report --tui doesn't have a traditional prompt.
        // We use --stdio mode which outputs and exits.
        // For interactive use, perf script piped to flamegraph is better.
        r"#"
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

    fn format_breakpoint(&self, _spec: &str) -> String {
        String::new()
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
        "q"
    }

    fn parse_help(&self, raw: &str) -> String {
        let _ = raw;
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
        assert_eq!(cfg.bin, "perf");
        assert!(cfg.args.contains(&"report".to_string()));
        assert!(cfg.args.contains(&"--stdio".to_string()));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn spawn_config_binary_target() {
        let cfg = PerfBackend.spawn_config("./myapp", &[]).unwrap();
        assert_eq!(cfg.bin, "perf");
        assert!(cfg.args.contains(&"report".to_string()));
    }

    #[test]
    fn format_breakpoint_empty() {
        assert_eq!(PerfBackend.format_breakpoint("anything"), "");
    }
}
