use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};
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

        let escaped_target = target.replace('\\', "\\\\").replace('\'', "\\'");
        let ruby_cmd = if args.is_empty() {
            format!(
                "mkdir -p {} && ruby -e \"require 'stackprof'; StackProf.run(mode: :cpu, out: '{}', raw: true) {{ load '{}' }}\"",
                out_dir_str, dump_str, escaped_target
            )
        } else {
            let escaped_args: Vec<String> = args.iter().map(|a| {
                let escaped = a.replace('\\', "\\\\").replace('\'', "\\'");
                format!("'{}'", escaped)
            }).collect();
            format!(
                "mkdir -p {} && ruby -e \"ARGV.replace([{}]); require 'stackprof'; StackProf.run(mode: :cpu, out: '{}', raw: true) {{ load '{}' }}\"",
                out_dir_str,
                escaped_args.join(", "),
                dump_str,
                escaped_target
            )
        };

        // Convert stackprof dump to callgrind format
        let convert_cmd = format!(
            "stackprof {} --callgrind > {}",
            dump_str, cg_str
        );

        // Find our own binary for exec-ing into the REPL
        let dbg_bin = std::env::current_exe()
            .unwrap_or_else(|_| "dbg".into())
            .display()
            .to_string();

        // Replace the bash shell with the profile REPL
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

    fn format_breakpoint(&self, _spec: &str) -> String {
        String::new()
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

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        // The REPL returns clean output — minimal cleaning needed
        CleanResult {
            output: output.to_string(),
            events: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_config_runs_stackprof_and_converts() {
        let cfg = StackprofBackend.spawn_config("test.rb", &[]).unwrap();
        assert_eq!(cfg.bin, "bash");
        assert!(cfg.init_commands[0].contains("stackprof"));
        assert!(cfg.init_commands[0].contains("test.rb"));
        assert!(cfg.init_commands[0].contains("mode: :cpu"));
        // Second command converts to callgrind
        assert!(cfg.init_commands[1].contains("--callgrind"));
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
        assert!(cmd.contains("test.rb"));
        assert!(cmd.contains("--flag"));
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
    fn spawn_config_escapes_single_quotes_in_target() {
        let cfg = StackprofBackend
            .spawn_config("it's_a_test.rb", &[])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        // Single quote in target must be escaped for Ruby string
        assert!(cmd.contains("it\\'s_a_test.rb"), "single quote not escaped in target: {cmd}");
    }

    #[test]
    fn spawn_config_escapes_single_quotes_in_args() {
        let cfg = StackprofBackend
            .spawn_config("test.rb", &["it's".into()])
            .unwrap();
        let cmd = &cfg.init_commands[0];
        assert!(cmd.contains("it\\'s"), "single quote not escaped in args: {cmd}");
    }
}
