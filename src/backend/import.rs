//! Import an existing profile snapshot into a dbg session.
//!
//! Bridges externally-collected traces (`dotnet-trace --output foo.nettrace`,
//! `perf script > foo.txt`, V8 `.cpuprofile`, raw speedscope JSON, …) into
//! the same profile-mode REPL used by `dbg start dotnet-trace` etc.
//! `top`, `callers`, `callees`, `traces`, `tree`, `hotpath`, `focus`,
//! `ignore` all become available against the imported data.
//!
//! Implementation reuses the existing pipeline: copy/convert the file
//! into `session_tmp("imported.speedscope.json")`, then let
//! `ProfileData::load(profile_output())` in the daemon do the parsing
//! exactly as it does for fresh collections.

use super::{Backend, Dependency, SpawnConfig, shell_escape};
use crate::check::find_bin;
use crate::daemon::session_tmp;

pub struct ImportBackend;

impl Backend for ImportBackend {
    fn name(&self) -> &'static str {
        "import"
    }

    fn description(&self) -> &'static str {
        "import an existing profile snapshot (.nettrace, .speedscope.json, .cpuprofile, perf-script, pprof-traces)"
    }

    fn types(&self) -> &'static [&'static str] {
        &["import"]
    }

    fn spawn_config(&self, target: &str, _args: &[String]) -> anyhow::Result<SpawnConfig> {
        let speedscope_out = session_tmp("imported.speedscope.json");
        let speedscope_str = speedscope_out.display().to_string();

        // .nettrace is binary, only `dotnet-trace convert` understands
        // it. Everything else is text-shaped (speedscope JSON, V8
        // cpuprofile, perf-script text, pprof traces text); copy as-is
        // and let `ProfileData::load_str` content-detect.
        let prep_cmd = if extension_matches(target, "nettrace") {
            // `dotnet-trace convert -o <base>` always appends
            // `.speedscope.json` to <base>, so pass the extensionless
            // sibling and the resulting file lands at exactly
            // `<speedscope_str>`. Passing `<speedscope_str>` directly
            // would produce `imported.speedscope.speedscope.json` and
            // `profile_output()` (which reports `<speedscope_str>`)
            // would never find it.
            let convert_base = session_tmp("imported");
            let trace_bin = find_bin("dotnet-trace");
            format!(
                "{} convert --format Speedscope {} -o {}",
                shell_escape(&trace_bin),
                shell_escape(target),
                shell_escape(&convert_base.display().to_string()),
            )
        } else {
            format!("cp {} {}", shell_escape(target), shell_escape(&speedscope_str))
        };

        Ok(SpawnConfig {
            bin: "bash".into(),
            args: vec!["--norc".into(), "--noprofile".into()],
            env: vec![("PS1".into(), "$ ".into())],
            init_commands: vec![prep_cmd, "echo '--- import ready ---'".into()],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"\$ $"
    }

    // Conversion can take a while on large traces (hundreds of MB
    // .nettrace files take 30-60s under dotnet-trace convert). The
    // 60s default would abort mid-convert.
    fn init_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(600)
    }

    fn dependencies(&self) -> Vec<Dependency> {
        // No unconditional deps — `dotnet-trace` is only required for
        // `.nettrace` inputs and is checked target-aware in `cmd_import`
        // before delegating to `cmd_start`.
        vec![]
    }

    fn run_command(&self) -> &'static str {
        "top"
    }

    fn quit_command(&self) -> &'static str {
        "exit"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "commands: top [N] [--no-idle], callers <func>, callees <func>, traces [N], tree [N], hotpath, threads, stats, search <pattern>, focus <func>, ignore <func>, window <t0> <t1> | window clear, reset".to_string()
    }

    fn profile_output(&self) -> Option<String> {
        Some(session_tmp("imported.speedscope.json").display().to_string())
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("import.md", include_str!("../../skills/adapters/import.md"))]
    }
}

/// Case-insensitive extension match. Handles `.NETTRACE` from copy-pasted
/// Windows paths and double-extensions like `.speedscope.json`.
pub(crate) fn extension_matches(path: &str, ext: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case(ext))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_nettrace_extension_case_insensitive() {
        assert!(extension_matches("/tmp/foo.nettrace", "nettrace"));
        assert!(extension_matches("/tmp/FOO.NETTRACE", "nettrace"));
        assert!(extension_matches("foo.nettrace", "nettrace"));
    }

    #[test]
    fn rejects_non_nettrace() {
        assert!(!extension_matches("/tmp/foo.speedscope.json", "nettrace"));
        assert!(!extension_matches("/tmp/foo.cpuprofile", "nettrace"));
        assert!(!extension_matches("/tmp/foo", "nettrace"));
        assert!(!extension_matches("/tmp/foo.txt", "nettrace"));
    }

    #[test]
    fn spawn_config_for_nettrace_runs_convert() {
        let cfg = ImportBackend
            .spawn_config("/tmp/snap.nettrace", &[])
            .unwrap();
        assert_eq!(cfg.init_commands.len(), 2);
        let cmd = &cfg.init_commands[0];
        assert!(
            cmd.contains("dotnet-trace"),
            "expected dotnet-trace convert step, got: {cmd}"
        );
        assert!(cmd.contains("convert"));
        assert!(cmd.contains("--format Speedscope"));
        assert!(cmd.contains("/tmp/snap.nettrace"));
        // dotnet-trace appends `.speedscope.json` to its `-o` arg —
        // we must pass the extensionless base or the output file
        // lands at `<base>.speedscope.json.speedscope.json` and
        // `profile_output()` can't find it.
        assert!(
            cmd.contains("-o ") && cmd.contains("imported"),
            "expected `-o <session>/imported`, got: {cmd}"
        );
        assert!(
            !cmd.contains("imported.speedscope.json"),
            "convert -o must not include the .speedscope.json suffix; dotnet-trace appends it. cmd: {cmd}"
        );
    }

    #[test]
    fn spawn_config_for_speedscope_just_copies() {
        let cfg = ImportBackend
            .spawn_config("/tmp/snap.speedscope.json", &[])
            .unwrap();
        // Non-binary inputs skip the converter — `cp` is enough because
        // `ProfileData::load_str` content-detects speedscope/cpuprofile/
        // perf-script/pprof-traces from the bytes themselves.
        assert!(
            cfg.init_commands[0].starts_with("cp "),
            "expected cp, got: {}",
            cfg.init_commands[0]
        );
        assert!(cfg.init_commands[0].contains("/tmp/snap.speedscope.json"));
        assert!(cfg.init_commands[0].contains("imported.speedscope.json"));
    }

    #[test]
    fn profile_output_points_at_session_tmp_target() {
        let out = ImportBackend.profile_output().unwrap();
        assert!(out.ends_with("imported.speedscope.json"));
    }
}
