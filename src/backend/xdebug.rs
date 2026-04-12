use super::{Backend, Dependency, DependencyCheck, SpawnConfig, shell_escape};
use crate::daemon::session_tmp;

pub struct XdebugProfileBackend;

impl Backend for XdebugProfileBackend {
    fn name(&self) -> &'static str {
        "xdebug-profile"
    }

    fn description(&self) -> &'static str {
        "PHP profiler (Xdebug)"
    }

    fn types(&self) -> &'static [&'static str] {
        &["php-profile"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let out_dir = session_tmp("xdebug");
        let out_dir_str = out_dir.display().to_string();
        let out_file = out_dir.join("cachegrind.out");
        let out_file_str = out_file.display().to_string();

        let mut php_cmd = format!(
            "mkdir -p {} && php -d xdebug.mode=profile -d xdebug.output_dir={} -d xdebug.profiler_output_name=cachegrind.out {}",
            out_dir_str, out_dir_str, shell_escape(target)
        );
        for a in args {
            php_cmd.push(' ');
            php_cmd.push_str(&shell_escape(a));
        }

        let dbg_bin = super::self_exe();
        let exec_repl = format!("exec {} --phpprofile-repl {}", dbg_bin, out_file_str);

        Ok(SpawnConfig {
            bin: "bash".into(),
            args: vec!["--norc".into(), "--noprofile".into()],
            env: vec![
                ("PS1".into(), "php-profile> ".into()),
            ],
            init_commands: vec![
                php_cmd,
                exec_repl,
            ],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"php-profile> $"
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![
            Dependency {
                name: "php",
                check: DependencyCheck::Binary {
                    name: "php",
                    alternatives: &["php"],
                    version_cmd: None,
                },
                install: "sudo apt install php-cli  # or: brew install php",
            },
            Dependency {
                name: "xdebug",
                check: DependencyCheck::Command {
                    program: "php",
                    args: &["-r", "if (!extension_loaded('xdebug')) exit(1);"],
                },
                install: "sudo apt install php-xdebug  # or: pecl install xdebug",
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
        "php-profile: hotspots, flat, calls, callers, inspect, stats, memory, search, tree, hotpath, focus, ignore, reset, help".to_string()
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("php-profile.md", include_str!("../../skills/adapters/php-profile.md"))]
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_config_execs_repl() {
        let cfg = XdebugProfileBackend.spawn_config("test.php", &[]).unwrap();
        assert_eq!(cfg.bin, "bash");
        assert!(cfg.init_commands[0].contains("xdebug.mode=profile"));
        assert!(cfg.init_commands[0].contains("test.php"));
        assert!(cfg.init_commands[1].contains("--phpprofile-repl"));
        assert!(cfg.init_commands[1].contains("exec"));
    }

    #[test]
    fn spawn_config_includes_args() {
        let cfg = XdebugProfileBackend
            .spawn_config("test.php", &["--flag".into()])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("test.php"));
        assert!(cmd.contains("--flag"));
    }

    #[test]
    fn prompt_pattern_matches() {
        let re = regex::Regex::new(XdebugProfileBackend.prompt_pattern()).unwrap();
        assert!(re.is_match("php-profile> "));
    }

    #[test]
    fn format_breakpoint_empty() {
        assert_eq!(XdebugProfileBackend.format_breakpoint("anything"), "");
    }

    #[test]
    fn spawn_config_escapes_target_with_spaces() {
        let cfg = XdebugProfileBackend
            .spawn_config("./my script.php", &[])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("'./my script.php'"), "target not escaped: {cmd}");
    }

    #[test]
    fn dep_check_verifies_xdebug_loaded() {
        let deps = XdebugProfileBackend.dependencies();
        let xdebug_dep = deps.iter().find(|d| d.name == "xdebug").unwrap();
        match &xdebug_dep.check {
            DependencyCheck::Command { program, args } => {
                assert_eq!(*program, "php");
                // Must actually check for xdebug, not just run php -m
                let args_str = args.join(" ");
                assert!(args_str.contains("xdebug"), "dep check doesn't verify xdebug: {args_str}");
            }
            _ => panic!("xdebug dep should use Command check"),
        }
    }
}
