use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};
use crate::daemon::session_tmp;

pub struct GhcProfBackend;

impl Backend for GhcProfBackend {
    fn name(&self) -> &'static str {
        "ghc-profile"
    }

    fn types(&self) -> &'static [&'static str] {
        &["haskell-profile", "hs-profile"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let out_dir = session_tmp("ghcprof");
        let out_dir_str = out_dir.display().to_string();
        let prof_file = out_dir.join("ghc.prof");
        let prof_str = prof_file.display().to_string();
        let cg_file = out_dir.join("callgrind.out");
        let cg_str = cg_file.display().to_string();

        // Step 1: Compile with profiling if it's a source file
        let (binary, compile_cmd) = if target.ends_with(".hs") {
            let bin = out_dir.join("profiled");
            let bin_str = bin.display().to_string();
            let cmd = format!(
                "mkdir -p {} && ghc -prof -fprof-late -rtsopts -o {} {}",
                out_dir_str, bin_str, target
            );
            (bin_str, Some(cmd))
        } else {
            // Already compiled with -prof -rtsopts
            (target.to_string(), None)
        };

        // Step 2: Run with profiling RTS flags
        let mut run_cmd = format!(
            "cd {} && {} +RTS -p -RTS",
            out_dir_str, binary
        );
        if !args.is_empty() {
            // Insert program args before +RTS
            run_cmd = format!(
                "cd {} && {} {} +RTS -p -RTS",
                out_dir_str, binary, args.join(" ")
            );
        }
        // GHC writes .prof next to the binary or in cwd — rename to known location
        let rename_cmd = format!(
            "mv {}/*.prof {} 2>/dev/null || true",
            out_dir_str, prof_str
        );

        // Step 3: Convert to callgrind format
        let dbg_bin = std::env::current_exe()
            .unwrap_or_else(|_| "dbg".into())
            .display()
            .to_string();

        let convert_cmd = format!(
            "{} --ghcprof-convert {} {}",
            dbg_bin, prof_str, cg_str
        );

        // Step 4: Exec into the profile REPL
        let exec_repl = format!(
            "exec {} --phpprofile-repl {} --profile-prompt 'haskell-profile> '",
            dbg_bin, cg_str
        );

        let mut init_commands = Vec::new();
        if let Some(cmd) = compile_cmd {
            init_commands.push(cmd);
        }
        init_commands.push(run_cmd);
        init_commands.push(rename_cmd);
        init_commands.push(convert_cmd);
        init_commands.push(exec_repl);

        Ok(SpawnConfig {
            bin: "bash".into(),
            args: vec!["--norc".into(), "--noprofile".into()],
            env: vec![("PS1".into(), "haskell-profile> ".into())],
            init_commands,
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"haskell-profile> $"
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "ghc",
            check: DependencyCheck::Binary {
                name: "ghc",
                alternatives: &["ghc"],
                version_cmd: Some(("ghc", &["--version"])),
            },
            install: "curl --proto '=https' --tlsv1.2 -sSf https://get-ghcup.haskell.org | sh  # or: sudo apt install ghc",
        }]
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
        "haskell-profile: hotspots, flat, calls, callers, inspect, stats, memory, search, tree, hotpath, focus, ignore, reset, help".to_string()
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("haskell-profile.md", include_str!("../../skills/adapters/haskell-profile.md"))]
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
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
    fn spawn_config_compiles_source() {
        let cfg = GhcProfBackend.spawn_config("test.hs", &[]).unwrap();
        assert_eq!(cfg.bin, "bash");
        // First init command should compile
        assert!(cfg.init_commands[0].contains("ghc -prof"));
        assert!(cfg.init_commands[0].contains("test.hs"));
        // Should have convert step
        assert!(cfg.init_commands.iter().any(|c| c.contains("--ghcprof-convert")));
        // Should exec into REPL
        assert!(cfg.init_commands.last().unwrap().contains("--phpprofile-repl"));
    }

    #[test]
    fn spawn_config_precompiled_binary() {
        let cfg = GhcProfBackend.spawn_config("./myapp", &[]).unwrap();
        // No compile step — first command runs the binary
        assert!(cfg.init_commands[0].contains("./myapp"));
        assert!(cfg.init_commands[0].contains("+RTS -p -RTS"));
        // Should NOT contain ghc -prof
        assert!(!cfg.init_commands[0].contains("ghc -prof"));
    }

    #[test]
    fn spawn_config_includes_args() {
        let cfg = GhcProfBackend
            .spawn_config("test.hs", &["--input".into(), "data.txt".into()])
            .unwrap();
        let run_cmd = cfg.init_commands.iter().find(|c| c.contains("+RTS")).unwrap();
        assert!(run_cmd.contains("--input"));
        assert!(run_cmd.contains("data.txt"));
    }

    #[test]
    fn prompt_pattern_matches() {
        let re = regex::Regex::new(GhcProfBackend.prompt_pattern()).unwrap();
        assert!(re.is_match("haskell-profile> "));
    }

    #[test]
    fn format_breakpoint_empty() {
        assert_eq!(GhcProfBackend.format_breakpoint("anything"), "");
    }
}
