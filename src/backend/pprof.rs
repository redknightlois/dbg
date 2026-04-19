use super::{Backend, Dependency, DependencyCheck, SpawnConfig};

fn is_source_extension(lower: &str) -> bool {
    matches!(
        std::path::Path::new(lower)
            .extension()
            .and_then(|s| s.to_str()),
        Some("go" | "py" | "rs" | "ts" | "js" | "cs" | "java" | "cpp" | "c" | "rb" | "php")
    )
}

pub struct PprofBackend;

impl Backend for PprofBackend {
    fn name(&self) -> &'static str {
        "pprof"
    }

    fn description(&self) -> &'static str {
        "Go CPU/memory profiler"
    }

    fn types(&self) -> &'static [&'static str] {
        &["pprof"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        // Accept three invocation shapes:
        //   dbg start pprof cpu.prof                 → [cpu.prof]
        //   dbg start pprof ./mybin cpu.prof         → [./mybin, cpu.prof]
        //   dbg start pprof "./mybin cpu.prof"       → [./mybin, cpu.prof]  (legacy)
        //
        // The space-joined legacy form is what the CLI used to force
        // because `spawn_config` ignored `args`; real users and the
        // adapter doc both expect two separate positional arguments.
        let mut positional: Vec<String> = Vec::new();
        let split: Vec<&str> = target.splitn(2, ' ').collect();
        if split.len() == 2 {
            positional.push(split[0].into());
            positional.push(split[1].into());
        } else {
            positional.push(target.into());
        }
        positional.extend(args.iter().cloned());

        // pprof only ingests profile files — source files hit
        // `go tool pprof` as an unknown format and the PTY exits
        // before printing a prompt, which surfaces as
        // "debugger did not produce prompt" with no hint.
        for p in &positional {
            let lower = p.to_ascii_lowercase();
            if is_source_extension(&lower) {
                anyhow::bail!(
                    "pprof needs a profile file (.prof / cpu.prof / mem.prof), \
                     got `{p}` — generate one with `go test -cpuprofile` or \
                     `runtime/pprof` and pass that instead"
                );
            }
        }

        Ok(SpawnConfig {
            bin: "go".into(),
            args: [vec!["tool".into(), "pprof".into()], positional].concat(),
            env: vec![],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"\(pprof\) "
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "go",
            check: DependencyCheck::Binary {
                name: "go",
                alternatives: &["go"],
                version_cmd: None,
            },
            install: "https://go.dev/dl/",
        }]
    }

    fn run_command(&self) -> &'static str {
        "top"
    }

    fn quit_command(&self) -> &'static str {
        "quit"
    }

    fn parse_help(&self, raw: &str) -> String {
        let mut cmds = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if let Some(tok) = line.split_whitespace().next() {
                if tok.chars().all(|c| c.is_ascii_alphabetic())
                    && tok.len() > 1
                    && tok.len() < 20
                {
                    cmds.push(tok.to_string());
                }
            }
        }
        cmds.dedup();
        format!("pprof: {}", cmds.join(", "))
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("pprof.md", include_str!("../../skills/adapters/pprof.md"))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_config_single_profile() {
        let cfg = PprofBackend.spawn_config("cpu.prof", &[]).unwrap();
        assert_eq!(cfg.args, vec!["tool", "pprof", "cpu.prof"]);
    }

    #[test]
    fn spawn_config_binary_and_profile() {
        let cfg = PprofBackend
            .spawn_config("./mybin cpu.prof", &[])
            .unwrap();
        assert_eq!(cfg.args, vec!["tool", "pprof", "./mybin", "cpu.prof"]);
    }

    #[test]
    fn spawn_config_accepts_two_args_for_binary_and_profile() {
        // Regression: the pprof adapter doc shows
        // `dbg start pprof <binary> <profile>` but the backend used
        // to ignore `args` entirely — only the space-joined single
        // string form worked. The two-arg CLI form is the natural
        // invocation for agents and must reach `go tool pprof` as
        // two positional arguments.
        let cfg = PprofBackend
            .spawn_config("./mybin", &["cpu.prof".to_string()])
            .unwrap();
        assert_eq!(
            cfg.args,
            vec!["tool", "pprof", "./mybin", "cpu.prof"],
            "two-arg form was not forwarded"
        );
    }

    #[test]
    fn spawn_config_rejects_source_files() {
        // Regression: `dbg start pprof broken.go` used to pass the
        // source file straight to `go tool pprof`, which exited
        // immediately with an opaque "debugger did not produce
        // prompt" error. Source files are never valid pprof input —
        // reject them up front and name the expected format.
        for src in ["broken.go", "main.py", "lib.rs", "app.ts", "foo.js", "Program.cs"] {
            let err = match PprofBackend.spawn_config(src, &[]) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("pprof accepted source file `{src}`"),
            };
            assert!(
                err.to_lowercase().contains(".prof")
                    || err.to_lowercase().contains("profile file"),
                "error should name the expected profile-file format, got: {err}"
            );
        }
    }

    #[test]
    fn format_breakpoint_empty() {
        assert_eq!(PprofBackend.format_breakpoint("anything"), "");
    }

    #[test]
    fn clean_passthrough() {
        let r = PprofBackend.clean("top", "some output");
        assert_eq!(r.output, "some output");
        assert!(r.events.is_empty());
    }
}
