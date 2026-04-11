use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};
use crate::daemon::session_tmp;

pub struct DotnetTraceBackend;

impl Backend for DotnetTraceBackend {
    fn name(&self) -> &'static str {
        "dotnet-trace"
    }

    fn types(&self) -> &'static [&'static str] {
        &["dotnet-trace"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let trace_file = session_tmp("trace.nettrace");
        let speedscope_base = session_tmp("trace");
        let trace_str = trace_file.display().to_string();
        let speedscope_str = speedscope_base.display().to_string();

        let collect_cmd = if args.is_empty() {
            format!("dotnet-trace collect --output {} -- {}", trace_str, target)
        } else {
            format!(
                "dotnet-trace collect --output {} -- {} {}",
                trace_str,
                target,
                args.join(" ")
            )
        };

        Ok(SpawnConfig {
            bin: "bash".into(),
            args: vec!["--norc".into(), "--noprofile".into()],
            env: vec![
                ("PS1".into(), "$ ".into()),
                (
                    "DOTNET_ROOT".into(),
                    find_dotnet_root().unwrap_or_default(),
                ),
                ("DOTNET_ROLL_FORWARD".into(), "LatestMajor".into()),
                (
                    "PATH".into(),
                    format!(
                        "{}:{}",
                        std::env::var("PATH").unwrap_or_default(),
                        dirs_home().join(".dotnet/tools").display()
                    ),
                ),
            ],
            init_commands: vec![
                collect_cmd,
                format!(
                    "dotnet-trace convert --format Speedscope {} -o {}",
                    trace_str, speedscope_str
                ),
                "echo '--- trace data ready ---'".into(),
            ],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"\$ $"
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![
            Dependency {
                name: "dotnet",
                check: DependencyCheck::Binary {
                    name: "dotnet",
                    alternatives: &["dotnet"],
                    version_cmd: None,
                },
                install: "https://dot.net/install",
            },
            Dependency {
                name: "dotnet-trace",
                check: DependencyCheck::Binary {
                    name: "dotnet-trace",
                    alternatives: &["dotnet-trace"],
                    version_cmd: None,
                },
                install: "dotnet tool install -g dotnet-trace",
            },
        ]
    }

    fn format_breakpoint(&self, _spec: &str) -> String {
        String::new()
    }

    fn run_command(&self) -> &'static str {
        "top"
    }

    fn quit_command(&self) -> &'static str {
        "exit"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "commands: top [N], callers <func>, callees <func>, traces [N], tree [N], hotpath, threads, stats, search <pattern>, focus <func>, ignore <func>, reset".to_string()
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        CleanResult {
            output: output.to_string(),
            events: vec![],
        }
    }

    fn profile_output(&self) -> Option<String> {
        Some(session_tmp("trace.speedscope.json").display().to_string())
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("dotnet-trace.md", include_str!("../../skills/adapters/dotnet-trace.md"))]
    }
}

fn find_dotnet_root() -> Option<String> {
    let dotnet = which::which("dotnet").ok()?;
    let canonical = std::fs::canonicalize(dotnet).ok()?;
    let bin_dir = canonical.parent()?;
    let libexec = bin_dir.parent()?.join("libexec");
    if libexec.join("shared").exists() {
        return Some(libexec.display().to_string());
    }
    Some(bin_dir.display().to_string())
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("~"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_passthrough() {
        let r = DotnetTraceBackend.clean("top", "profile output");
        assert_eq!(r.output, "profile output");
        assert!(r.events.is_empty());
    }

    #[test]
    fn format_breakpoint_empty() {
        assert_eq!(DotnetTraceBackend.format_breakpoint("anything"), "");
    }

    #[test]
    fn spawn_config_includes_collect_and_convert() {
        let cfg = DotnetTraceBackend
            .spawn_config("./myapp", &[])
            .unwrap();
        assert!(cfg.init_commands.len() >= 3);
        assert!(cfg.init_commands[0].contains("dotnet-trace collect"));
        assert!(cfg.init_commands[0].contains("./myapp"));
        assert!(cfg.init_commands[1].contains("dotnet-trace convert"));
        assert!(cfg.init_commands[1].contains("Speedscope"));
    }

    #[test]
    fn spawn_config_with_args() {
        let cfg = DotnetTraceBackend
            .spawn_config("./myapp", &["--port".into(), "8080".into()])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("./myapp --port 8080"));
    }

    #[test]
    fn spawn_config_sets_dotnet_env() {
        let cfg = DotnetTraceBackend
            .spawn_config("./myapp", &[])
            .unwrap();
        assert!(cfg.env.iter().any(|(k, _)| k == "DOTNET_ROLL_FORWARD"));
        assert!(cfg.env.iter().any(|(k, _)| k == "PATH"));
    }

    #[test]
    fn profile_output_returns_speedscope_path() {
        let path = DotnetTraceBackend.profile_output().unwrap();
        assert!(path.contains("trace.speedscope.json"));
    }
}
