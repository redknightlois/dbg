use super::{Backend, Dependency, DependencyCheck, SpawnConfig, shell_escape};
use crate::check::find_bin;
use crate::daemon::session_tmp;

pub struct DotnetTraceBackend;

impl Backend for DotnetTraceBackend {
    fn name(&self) -> &'static str {
        "dotnet-trace"
    }

    fn description(&self) -> &'static str {
        ".NET performance profiler"
    }

    fn types(&self) -> &'static [&'static str] {
        &["dotnet-trace"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let trace_file = session_tmp("trace.nettrace");
        let speedscope_base = session_tmp("trace");
        let trace_str = trace_file.display().to_string();
        let speedscope_str = speedscope_base.display().to_string();

        let trace_bin = find_bin("dotnet-trace");
        let mut collect_cmd = format!(
            "{} collect --output {} -- {}",
            shell_escape(&trace_bin), trace_str, shell_escape(target)
        );
        for a in args {
            collect_cmd.push(' ');
            collect_cmd.push_str(&shell_escape(a));
        }

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
                    "{} convert --format Speedscope {} -o {}",
                    shell_escape(&trace_bin), trace_str, speedscope_str
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

    fn run_command(&self) -> &'static str {
        "top"
    }

    fn quit_command(&self) -> &'static str {
        "exit"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "commands: top [N], callers <func>, callees <func>, traces [N], tree [N], hotpath, threads, stats, search <pattern>, focus <func>, ignore <func>, reset".to_string()
    }

    fn profile_output(&self) -> Option<String> {
        Some(session_tmp("trace.speedscope.json").display().to_string())
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("dotnet-trace.md", include_str!("../../skills/adapters/dotnet-trace.md"))]
    }
}

fn find_dotnet_root() -> Option<String> {
    // Homebrew layout: .../dotnet/<ver>/bin/dotnet → sibling libexec/shared.
    // Standard layout: dotnet binary lives directly in the root.
    // Require `libexec/shared` to avoid false-positives on bare libexec dirs.
    dbg_cli::deps::find_tool_root("dotnet", Some("libexec"), Some("shared"), 2)
        .map(|p| p.display().to_string())
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"))
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
        assert!(cmd.contains("./myapp"));
        assert!(cmd.contains("--port"));
        assert!(cmd.contains("8080"));
    }

    #[test]
    fn spawn_config_escapes_spaces() {
        let cfg = DotnetTraceBackend
            .spawn_config("./my app", &[])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("'./my app'"), "target not escaped: {cmd}");
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
