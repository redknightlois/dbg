use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};
use crate::daemon::session_tmp;

pub struct PstatsBackend;

impl Backend for PstatsBackend {
    fn name(&self) -> &'static str {
        "pstats"
    }

    fn types(&self) -> &'static [&'static str] {
        &["pyprofile"]
    }

    fn spawn_config(&self, target: &str, _args: &[String]) -> anyhow::Result<SpawnConfig> {
        // Two modes:
        // 1. Existing .prof file → open pstats directly
        // 2. Python script → profile it, save to temp, open pstats
        let path = std::path::Path::new(target);

        if path.extension().is_some_and(|e| e == "prof" || e == "pstats") {
            // Existing profile
            Ok(SpawnConfig {
                bin: "python3".into(),
                args: vec!["-m".into(), "pstats".into(), target.into()],
                env: vec![],
                init_commands: vec![],
            })
        } else {
            // Python script — profile it first, then open pstats
            Ok(SpawnConfig {
                bin: "python3".into(),
                args: vec!["-m".into(), "pstats".into(), session_tmp("profile.prof").display().to_string()],
                env: vec![],
                init_commands: vec![],
            })
        }
    }

    fn prompt_pattern(&self) -> &str {
        r"% $"
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

    fn format_breakpoint(&self, _spec: &str) -> String {
        String::new()
    }

    fn run_command(&self) -> &'static str {
        "sort cumulative\nstats 20"
    }

    fn quit_command(&self) -> &'static str {
        "quit"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "pstats: sort, stats, callers, callees, strip, add, read, reverse, quit".to_string()
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        let mut lines = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("Welcome to") {
                continue;
            }
            if trimmed == "Goodbye." {
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
        vec![("pyprofile.md", include_str!("../../skills/adapters/pyprofile.md"))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_filters_welcome_and_goodbye() {
        let input = "Welcome to the profiler\nactual stats\nGoodbye.";
        let r = PstatsBackend.clean("stats", input);
        assert!(!r.output.contains("Welcome"));
        assert!(!r.output.contains("Goodbye"));
        assert!(r.output.contains("actual stats"));
    }

    #[test]
    fn clean_keeps_normal_output() {
        let input = "   ncalls  tottime\n       1    0.178";
        let r = PstatsBackend.clean("stats", input);
        assert!(r.output.contains("ncalls"));
    }

    #[test]
    fn spawn_config_existing_prof_file() {
        let cfg = PstatsBackend.spawn_config("output.prof", &[]).unwrap();
        assert!(cfg.args.contains(&"output.prof".to_string()));
        assert!(cfg.args.contains(&"-m".to_string()));
        assert!(cfg.args.contains(&"pstats".to_string()));
    }

    #[test]
    fn spawn_config_pstats_extension() {
        let cfg = PstatsBackend.spawn_config("output.pstats", &[]).unwrap();
        assert!(cfg.args.contains(&"output.pstats".to_string()));
    }
}
