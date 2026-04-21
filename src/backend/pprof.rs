use super::{Backend, Dependency, DependencyCheck, SpawnConfig, shell_escape};
use crate::daemon::session_tmp;

fn is_source_extension(lower: &str) -> bool {
    matches!(
        std::path::Path::new(lower)
            .extension()
            .and_then(|s| s.to_str()),
        Some("go" | "py" | "rs" | "ts" | "js" | "cs" | "java" | "cpp" | "c" | "rb" | "php")
    )
}

/// Return true if the path starts with the ELF magic bytes `\x7fELF`.
fn is_elf_binary(path: &str) -> bool {
    use std::io::Read;
    let mut buf = [0u8; 4];
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    if f.read_exact(&mut buf).is_err() { return false }
    buf == [0x7f, b'E', b'L', b'F']
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

        // pprof only ingests profile files — source files and ELF
        // binaries both cause `go tool pprof` to exit before printing
        // a prompt, surfacing an opaque "debugger did not produce
        // prompt" error with no actionable hint.
        for p in &positional {
            let lower = p.to_ascii_lowercase();
            if is_source_extension(&lower) {
                anyhow::bail!(
                    "pprof needs a profile file (.prof / cpu.prof / mem.prof), \
                     got `{p}` — generate one with `go test -cpuprofile` or \
                     `runtime/pprof` and pass that instead"
                );
            }
            if is_elf_binary(p) {
                anyhow::bail!(
                    "pprof needs a recorded profile (.prof), not an ELF binary — \
                     profile first with `go test -cpuprofile cpu.prof ./...` or \
                     `perf record` and pass the resulting .prof file instead"
                );
            }
        }

        // Pre-run `go tool pprof -traces` to a side file so ProfileData
        // can load the profile as if it were evented speedscope; then
        // `exec` into the interactive REPL so users still get pprof's
        // native commands (top, list, web, peek). Running the conversion
        // and the exec in a single `bash -c` keeps the whole thing as
        // one PTY-controlled process — the daemon sees only the pprof
        // prompt, never bash.
        let traces_out = session_tmp("pprof.traces.txt");
        let traces_str = traces_out.display().to_string();
        let pprof_args: Vec<String> = [vec!["tool".into(), "pprof".into()], positional.clone()]
            .concat();
        let quoted_args = pprof_args
            .iter()
            .map(|a| shell_escape(a))
            .collect::<Vec<_>>()
            .join(" ");
        let shell_cmd = format!(
            "go {} -traces > {} 2>/dev/null || true; exec go {}",
            quoted_args.clone(),
            shell_escape(&traces_str),
            quoted_args,
        );

        Ok(SpawnConfig {
            bin: "bash".into(),
            args: vec!["-c".into(), shell_cmd],
            env: vec![],
            init_commands: vec![],
        })
    }

    fn profile_output(&self) -> Option<String> {
        Some(session_tmp("pprof.traces.txt").display().to_string())
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

    fn shell_script(cfg: &SpawnConfig) -> &str {
        assert_eq!(cfg.bin, "bash");
        assert_eq!(cfg.args.first().map(|s| s.as_str()), Some("-c"));
        cfg.args.get(1).expect("shell script arg").as_str()
    }

    #[test]
    fn spawn_config_single_profile() {
        let cfg = PprofBackend.spawn_config("cpu.prof", &[]).unwrap();
        let script = shell_script(&cfg);
        assert!(script.contains("go tool pprof cpu.prof"), "{script}");
        assert!(script.contains("-traces"), "must produce traces file: {script}");
        assert!(script.contains("exec go tool pprof cpu.prof"), "{script}");
    }

    #[test]
    fn spawn_config_binary_and_profile() {
        let cfg = PprofBackend
            .spawn_config("./mybin cpu.prof", &[])
            .unwrap();
        let script = shell_script(&cfg);
        assert!(script.contains("./mybin") && script.contains("cpu.prof"), "{script}");
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
        let script = shell_script(&cfg);
        assert!(
            script.contains("./mybin") && script.contains("cpu.prof"),
            "two-arg form was not forwarded: {script}"
        );
    }

    #[test]
    fn spawn_config_emits_traces_side_file() {
        let cfg = PprofBackend.spawn_config("cpu.prof", &[]).unwrap();
        let script = shell_script(&cfg);
        // -traces conversion must be in the shell script
        assert!(script.contains("-traces"), "script missing -traces: {script}");
        // profile_output() must point to the traces file
        let out = PprofBackend.profile_output().unwrap();
        assert!(out.ends_with("pprof.traces.txt"), "unexpected profile_output: {out}");
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
    fn spawn_config_rejects_elf_binary() {
        // Regression: `dbg start pprof ./broken` (an ELF binary, not a
        // profile) used to pass the binary straight to `go tool pprof`,
        // which exited before printing a prompt, surfacing an opaque
        // "debugger did not produce prompt" error with no hint.
        // ELF magic (\x7fELF) must be detected and a clear message
        // shown naming the required profile format.
        use std::io::Write;
        let tmp = tempfile::TempDir::new().unwrap();
        let elf_path = tmp.path().join("broken");
        {
            let mut f = std::fs::File::create(&elf_path).unwrap();
            f.write_all(&[0x7f, b'E', b'L', b'F', 0, 0, 0, 0]).unwrap();
        }
        let path_str = elf_path.to_str().unwrap();
        let err = match PprofBackend.spawn_config(path_str, &[]) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("pprof accepted ELF binary `{path_str}`"),
        };
        assert!(
            err.to_lowercase().contains(".prof")
                || err.to_lowercase().contains("profile"),
            "error should mention profile format, got: {err}"
        );
        assert!(
            err.to_lowercase().contains("elf") || err.to_lowercase().contains("binary"),
            "error should mention ELF or binary, got: {err}"
        );
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
