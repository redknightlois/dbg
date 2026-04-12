use super::{Backend, Dependency, DependencyCheck, SpawnConfig, shell_escape};
use crate::daemon::session_tmp;

pub struct StackprofBackend;

impl Backend for StackprofBackend {
    fn name(&self) -> &'static str {
        "stackprof"
    }

    fn types(&self) -> &'static [&'static str] {
        &["ruby-profile"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let out_dir = session_tmp("stackprof");
        let dump_file = out_dir.join("stackprof.dump");
        let dump_str = dump_file.display().to_string();
        let cg_file = out_dir.join("callgrind.out");
        let cg_str = cg_file.display().to_string();
        let out_dir_str = out_dir.display().to_string();

        // Use environment variables to pass target/args safely into Ruby,
        // avoiding multi-layer shell+Ruby escaping issues.
        let ruby_script = format!(
            "require 'stackprof'; StackProf.run(mode: :cpu, out: '{}', raw: true) {{ load ENV['DBG_TARGET'] }}",
            dump_str
        );
        let ruby_script_with_args = format!(
            "ARGV.replace(ENV['DBG_ARGS'].split('\\x00')); require 'stackprof'; StackProf.run(mode: :cpu, out: '{}', raw: true) {{ load ENV['DBG_TARGET'] }}",
            dump_str
        );

        let ruby_cmd = if args.is_empty() {
            format!(
                "mkdir -p {} && DBG_TARGET={} ruby -e {}",
                shell_escape(&out_dir_str), shell_escape(target), shell_escape(&ruby_script)
            )
        } else {
            let joined_args = args.join("\x00");
            format!(
                "mkdir -p {} && DBG_TARGET={} DBG_ARGS={} ruby -e {}",
                shell_escape(&out_dir_str), shell_escape(target), shell_escape(&joined_args), shell_escape(&ruby_script_with_args)
            )
        };

        // Convert stackprof dump to callgrind format, fail if output is empty
        let convert_cmd = format!(
            "stackprof {} --callgrind > {} && test -s {}",
            shell_escape(&dump_str), shell_escape(&cg_str), shell_escape(&cg_str)
        );

        let dbg_bin = super::self_exe();
        let exec_repl = format!(
            "exec {} --phpprofile-repl {} --profile-prompt 'ruby-profile> '",
            dbg_bin, cg_str
        );

        Ok(SpawnConfig {
            bin: "bash".into(),
            args: vec!["--norc".into(), "--noprofile".into()],
            env: vec![
                ("PS1".into(), "ruby-profile> ".into()),
            ],
            init_commands: vec![
                ruby_cmd,
                convert_cmd,
                exec_repl,
            ],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"ruby-profile> $"
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![
            Dependency {
                name: "ruby",
                check: DependencyCheck::Binary {
                    name: "ruby",
                    alternatives: &["ruby"],
                    version_cmd: None,
                },
                install: "sudo apt install ruby  # or: brew install ruby",
            },
            Dependency {
                name: "stackprof",
                check: DependencyCheck::Command {
                    program: "ruby",
                    args: &["-e", "require 'stackprof'"],
                },
                install: "gem install stackprof",
            },
        ]
    }

    fn run_command(&self) -> &'static str {
        "hotspots"
    }

    fn quit_command(&self) -> &'static str {
        "exit"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "ruby-profile: hotspots, flat, calls, callers, inspect, stats, memory, search, tree, hotpath, focus, ignore, reset, help".to_string()
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("ruby-profile.md", include_str!("../../skills/adapters/ruby-profile.md"))]
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_config_runs_stackprof_and_converts() {
        let cfg = StackprofBackend.spawn_config("test.rb", &[]).unwrap();
        assert_eq!(cfg.bin, "bash");
        // First command uses env var for target, runs ruby with stackprof
        assert!(cfg.init_commands[0].contains("DBG_TARGET=test.rb"));
        assert!(cfg.init_commands[0].contains("ruby -e"));
        assert!(cfg.init_commands[0].contains("mode: :cpu"));
        // Second command converts to callgrind with validation
        assert!(cfg.init_commands[1].contains("--callgrind"));
        assert!(cfg.init_commands[1].contains("test -s"));
        // Third command execs into the REPL
        assert!(cfg.init_commands[2].contains("--phpprofile-repl"));
        assert!(cfg.init_commands[2].contains("exec"));
    }

    #[test]
    fn spawn_config_includes_args() {
        let cfg = StackprofBackend
            .spawn_config("test.rb", &["--flag".into()])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("DBG_TARGET=test.rb"));
        assert!(cmd.contains("DBG_ARGS=--flag"));
    }

    #[test]
    fn prompt_pattern_matches() {
        let re = regex::Regex::new(StackprofBackend.prompt_pattern()).unwrap();
        assert!(re.is_match("ruby-profile> "));
    }

    #[test]
    fn format_breakpoint_empty() {
        assert_eq!(StackprofBackend.format_breakpoint("anything"), "");
    }

    #[test]
    fn spawn_config_escapes_special_chars_in_target() {
        let cfg = StackprofBackend
            .spawn_config("it's_a_test.rb", &[])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        // Target with single quote is shell-escaped via env var
        assert!(cmd.contains("DBG_TARGET="), "target not passed via env: {cmd}");
        assert!(cmd.contains("it"), "target not present: {cmd}");
    }

    #[test]
    fn spawn_config_escapes_shell_metacharacters() {
        let cfg = StackprofBackend
            .spawn_config("$(evil).rb", &[])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        // Shell metacharacters must be escaped in the DBG_TARGET value
        assert!(cmd.contains("'$(evil).rb'"), "shell metacharacter not escaped: {cmd}");
    }
}
