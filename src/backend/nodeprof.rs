use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig, shell_escape};
use crate::daemon::session_tmp;

pub struct NodeProfBackend;

impl Backend for NodeProfBackend {
    fn name(&self) -> &'static str {
        "nodeprof"
    }

    fn description(&self) -> &'static str {
        "Node.js CPU profiler (V8 --cpu-prof)"
    }

    fn types(&self) -> &'static [&'static str] {
        &["nodeprof", "js-profile"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let path = std::path::Path::new(target);

        // Existing .cpuprofile or .speedscope.json → copy to known location
        if path.extension().is_some_and(|e| e == "cpuprofile")
            || target.ends_with(".speedscope.json")
        {
            let dest = session_tmp("profile.cpuprofile");
            let copy_cmd = format!(
                "cp {} {}",
                shell_escape(target),
                shell_escape(&dest.display().to_string()),
            );
            return Ok(SpawnConfig {
                bin: "bash".into(),
                args: vec!["--norc".into(), "--noprofile".into()],
                env: vec![("PS1".into(), "% ".into())],
                init_commands: vec![
                    copy_cmd,
                    "echo '--- profile data ready ---'".into(),
                ],
            });
        }

        // JS/TS script → profile it with --cpu-prof
        let prof_dir = session_tmp("cpuprof");
        let prof_dir_str = prof_dir.display().to_string();
        let dest = session_tmp("profile.cpuprofile");
        let dest_str = dest.display().to_string();
        let escaped_target = shell_escape(target);

        let mut profile_cmd = format!(
            "node --cpu-prof --cpu-prof-dir={} {}",
            shell_escape(&prof_dir_str),
            escaped_target,
        );
        for a in args {
            profile_cmd.push(' ');
            profile_cmd.push_str(&shell_escape(a));
        }

        // Copy the most recent .cpuprofile to the known location
        let copy_cmd = format!(
            "cp $(ls -t {}/*.cpuprofile 2>/dev/null | head -1) {}",
            shell_escape(&prof_dir_str),
            shell_escape(&dest_str),
        );

        Ok(SpawnConfig {
            bin: "bash".into(),
            args: vec!["--norc".into(), "--noprofile".into()],
            env: vec![("PS1".into(), "% ".into())],
            init_commands: vec![
                profile_cmd,
                copy_cmd,
                "echo '--- profile data ready ---'".into(),
            ],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"% $"
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "node",
            check: DependencyCheck::Binary {
                name: "node",
                alternatives: &["node"],
                version_cmd: Some(("node", &["--version"])),
            },
            install: "https://nodejs.org  # or: nvm install --lts",
        }]
    }

    fn run_command(&self) -> &'static str {
        "top 20"
    }

    fn quit_command(&self) -> &'static str {
        "exit"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "nodeprof: top, callers, callees, traces, tree, hotpath, threads, stats, search, focus, ignore, reset, help".to_string()
    }

    fn profile_output(&self) -> Option<String> {
        Some(session_tmp("profile.cpuprofile").display().to_string())
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        let mut lines = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("Waiting for the debugger")
                || trimmed.starts_with("Debugger attached")
            {
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
        vec![("js-profile.md", include_str!("../../skills/adapters/js-profile.md"))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_config_existing_cpuprofile() {
        let cfg = NodeProfBackend
            .spawn_config("profile.cpuprofile", &[])
            .unwrap();
        assert_eq!(cfg.bin, "bash");
        assert!(cfg.init_commands[0].contains("cp"));
        assert!(cfg.init_commands[0].contains("profile.cpuprofile"));
    }

    #[test]
    fn spawn_config_existing_speedscope() {
        let cfg = NodeProfBackend
            .spawn_config("data.speedscope.json", &[])
            .unwrap();
        assert_eq!(cfg.bin, "bash");
        assert!(cfg.init_commands[0].contains("cp"));
        assert!(cfg.init_commands[0].contains("data.speedscope.json"));
    }

    #[test]
    fn spawn_config_js_script() {
        let cfg = NodeProfBackend.spawn_config("app.js", &[]).unwrap();
        assert_eq!(cfg.bin, "bash");
        assert!(cfg.init_commands[0].contains("--cpu-prof"));
        assert!(cfg.init_commands[0].contains("app.js"));
        assert!(cfg.init_commands[1].contains("cp"));
        assert!(cfg.init_commands[2].contains("profile data ready"));
    }

    #[test]
    fn spawn_config_with_args() {
        let cfg = NodeProfBackend
            .spawn_config("server.js", &["--port".into(), "8080".into()])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("server.js"));
        assert!(cmd.contains("--port"));
        assert!(cmd.contains("8080"));
    }

    #[test]
    fn clean_filters_debugger_noise() {
        let input = "Waiting for the debugger to disconnect\nactual output";
        let r = NodeProfBackend.clean("top", input);
        assert!(!r.output.contains("Waiting for the debugger"));
        assert!(r.output.contains("actual output"));
    }

    #[test]
    fn profile_output_returns_cpuprofile_path() {
        let path = NodeProfBackend.profile_output().unwrap();
        assert!(path.contains("profile.cpuprofile"));
    }
}
