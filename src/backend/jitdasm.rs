use super::{Backend, Dependency, DependencyCheck, SpawnConfig, shell_escape};
use crate::daemon::session_tmp;

pub struct JitDasmBackend;

/// Extract the `<RootNamespace>` from a csproj file, falling back to
/// the project's filename stem when unset. Cheap text scan — good
/// enough for the common SDK-style csproj shape.
fn project_namespace(csproj_path: &str) -> Option<String> {
    let path = std::path::Path::new(csproj_path);
    if let Ok(text) = std::fs::read_to_string(path) {
        // Look for <RootNamespace>X</RootNamespace>
        if let Some(start) = text.find("<RootNamespace>") {
            let rest = &text[start + "<RootNamespace>".len()..];
            if let Some(end) = rest.find("</RootNamespace>") {
                let ns = rest[..end].trim().to_string();
                if !ns.is_empty() {
                    return Some(ns);
                }
            }
        }
    }
    // Fallback: project filename stem.
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// Derive a REPL-friendly substring from the user's JitDisasm
/// pattern. The REPL matches against method-listing names like
/// `Broken.Program:SumFast(int[])`, so we want the most specific
/// token the user typed:
///   * `Type:Method` → `:Method` (keeps the `:` as a strong anchor)
///   * `Type`        → `Type`
///   * `*` / ``      → no default filter
fn repl_default_pattern(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() || raw == "*" {
        return String::new();
    }
    if let Some((type_part, method)) = raw.split_once(':') {
        let m = method.trim_matches('*');
        if !m.is_empty() {
            return format!(":{m}");
        }
        let t = type_part.trim_matches('*');
        return t.to_string();
    }
    raw.trim_matches('*').to_string()
}

/// Qualify a JitDisasm pattern with the project's namespace when the
/// user supplied only `Type:Method` (no `.` in the type component).
/// Leaves `*`, `Foo.Bar:Baz`, and already-qualified patterns alone.
fn qualify_pattern(raw: &str, csproj_path: &str) -> String {
    if raw == "*" || raw.is_empty() {
        return raw.to_string();
    }
    // Split on ':' — type part before, method part after. If no ':',
    // the user is matching by type name alone; apply the same rule.
    let (type_part, method_part) = match raw.split_once(':') {
        Some((t, m)) => (t, Some(m)),
        None => (raw, None),
    };
    // Already qualified (contains '.') or wildcard-prefixed — leave alone.
    if type_part.contains('.') || type_part.starts_with('*') {
        return raw.to_string();
    }
    let ns = match project_namespace(csproj_path) {
        Some(n) => n,
        None => return raw.to_string(),
    };
    match method_part {
        Some(m) => format!("{ns}.{type_part}:{m}"),
        None => format!("{ns}.{type_part}"),
    }
}

impl Backend for JitDasmBackend {
    fn name(&self) -> &'static str {
        "jitdasm"
    }

    fn description(&self) -> &'static str {
        ".NET JIT disassembly analyzer"
    }

    fn types(&self) -> &'static [&'static str] {
        &["jitdasm"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let project = target.to_string();
        let out_dir = session_tmp("jitdasm");
        let out_dir_str = out_dir.display().to_string();
        let out_file = out_dir.join("capture.asm");
        let out_file_str = out_file.display().to_string();

        // .NET's JitDisasm matches patterns of the form
        // `<Namespace>.<Type>:<Method>`. Users and adapter docs often
        // type `Program:Main` or `MyClass:*`, omitting the namespace —
        // which silently matches nothing because the runtime includes
        // the project's RootNamespace in the method name. Auto-prepend
        // the project's namespace to any pattern whose type portion
        // (before `:`) has no `.` and no leading `*`.
        let raw_pattern = if args.is_empty() {
            "*".to_string()
        } else {
            args[0].clone()
        };
        let pattern = qualify_pattern(&raw_pattern, &project);

        let extra_args: Vec<String> = if args.len() > 1 {
            args[1..].to_vec()
        } else {
            vec![]
        };

        let extra = if extra_args.is_empty() {
            String::new()
        } else {
            let escaped: Vec<String> = extra_args.iter().map(|a| shell_escape(a)).collect();
            format!(" {}", escaped.join(" "))
        };

        let dbg_bin = super::self_exe();

        let mkdir_cmd = format!("mkdir -p {}", out_dir_str);

        let build_cmd = format!(
            "echo 'Building...' && dotnet build {} -c Release --nologo -v q 2>&1 | tail -1",
            shell_escape(&project)
        );

        let run_cmd = format!(
            "echo 'Disassembling: {}' && DOTNET_TieredCompilation=0 DOTNET_JitDisasm='{}' DOTNET_JitDiffableDasm=1 dotnet run --project {} -c Release --no-build{} > {} 2>&1",
            pattern, pattern, shell_escape(&project), extra, out_file_str
        );

        // Replace the bash shell with our Rust REPL. Pass the raw
        // (unqualified) pattern as the REPL's default filter so
        // `stats`/`simd`/`hotspots` without an arg narrow to the
        // user's methods instead of the whole capture. The env-var
        // pattern is CLR-syntax (`Broken.Program:SumFast`) but the
        // REPL does a substring match on the `; Assembly listing
        // for method <name>` token, so we want the method-name part
        // — strip any leading `Namespace.Type:` prefix and wildcards.
        let repl_default = repl_default_pattern(&raw_pattern);
        let exec_repl = if repl_default.is_empty() {
            format!("exec {} --jitdasm-repl {}", dbg_bin, out_file_str)
        } else {
            format!(
                "exec {} --jitdasm-repl {} --jitdasm-pattern {}",
                dbg_bin,
                out_file_str,
                shell_escape(&repl_default),
            )
        };

        Ok(SpawnConfig {
            bin: "bash".into(),
            args: vec!["--norc".into(), "--noprofile".into()],
            env: vec![("PS1".into(), "jitdasm> ".into())],
            init_commands: vec![mkdir_cmd, build_cmd, run_cmd, exec_repl],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"jitdasm> $"
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "dotnet",
            check: DependencyCheck::Binary {
                name: "dotnet",
                alternatives: &["dotnet"],
                version_cmd: None,
            },
            install: "https://dot.net/install",
        }]
    }

    fn run_command(&self) -> &'static str {
        "stats"
    }

    fn quit_command(&self) -> &'static str {
        "exit"
    }

    fn parse_help(&self, _raw: &str) -> String {
        "jitdasm: methods, disasm <pattern>, search <instr>, stats, hotspots [N], simd, help".to_string()
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("jitdasm.md", include_str!("../../skills/adapters/jitdasm.md"))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_config_with_pattern() {
        let cfg = JitDasmBackend
            .spawn_config("bench/tq-quick/tq-quick.csproj", &["SimdOps:DotProduct".into()])
            .unwrap();
        assert_eq!(cfg.bin, "bash");
        assert!(cfg.init_commands[0].contains("mkdir"));
        assert!(cfg.init_commands[1].contains("dotnet build"));
        assert!(cfg.init_commands[2].contains("DOTNET_JitDisasm"));
        assert!(cfg.init_commands[2].contains("SimdOps:DotProduct"));
    }

    #[test]
    fn spawn_config_default_captures_all() {
        let cfg = JitDasmBackend
            .spawn_config("myapp.csproj", &[])
            .unwrap();
        assert!(cfg.init_commands[2].contains("DOTNET_JitDisasm='*'"));
    }

    #[test]
    fn spawn_config_with_extra_args() {
        let cfg = JitDasmBackend
            .spawn_config(
                "myapp.csproj",
                &["MyMethod".into(), "--ef".into(), "64".into()],
            )
            .unwrap();
        assert!(cfg.init_commands[2].contains("--ef 64"));
    }

    #[test]
    fn spawn_config_execs_repl() {
        let cfg = JitDasmBackend
            .spawn_config("myapp.csproj", &["Foo:Bar".into()])
            .unwrap();
        assert!(cfg.init_commands[3].contains("--jitdasm-repl"));
        assert!(cfg.init_commands[3].contains("exec"));
    }

    #[test]
    fn prompt_pattern_matches() {
        let re = regex::Regex::new(JitDasmBackend.prompt_pattern()).unwrap();
        assert!(re.is_match("jitdasm> "));
    }

    #[test]
    fn format_breakpoint_empty() {
        assert_eq!(JitDasmBackend.format_breakpoint("anything"), "");
    }

    #[test]
    fn qualify_pattern_leaves_wildcard() {
        assert_eq!(qualify_pattern("*", "Foo.csproj"), "*");
    }

    #[test]
    fn qualify_pattern_leaves_already_qualified() {
        assert_eq!(
            qualify_pattern("Foo.Bar:Baz", "Foo.csproj"),
            "Foo.Bar:Baz"
        );
    }

    #[test]
    fn qualify_pattern_prepends_namespace_when_missing() {
        // With no csproj on disk, fallback uses the filename stem.
        let got = qualify_pattern("Program:Main", "/tmp/Broken.csproj");
        assert_eq!(got, "Broken.Program:Main");
    }

    #[test]
    fn qualify_pattern_handles_type_only() {
        let got = qualify_pattern("Program", "/tmp/Broken.csproj");
        assert_eq!(got, "Broken.Program");
    }

    #[test]
    fn repl_default_extracts_method_token() {
        // Regression: `dbg start jitdasm foo.csproj Program:SumFast`
        // left the REPL with no default filter, so `stats` (the
        // default run_command) aggregated ~790 methods instead of
        // the one the user asked about.
        assert_eq!(repl_default_pattern("Program:SumFast"), ":SumFast");
        assert_eq!(repl_default_pattern("SumFast"), "SumFast");
        assert_eq!(repl_default_pattern("Program:*"), "Program");
        assert_eq!(repl_default_pattern("*"), "");
        assert_eq!(repl_default_pattern(""), "");
    }

    #[test]
    fn spawn_config_forwards_pattern_to_repl() {
        let cfg = JitDasmBackend
            .spawn_config("myapp.csproj", &["Program:SumFast".into()])
            .unwrap();
        let exec_cmd = cfg.init_commands.last().expect("exec cmd");
        assert!(
            exec_cmd.contains("--jitdasm-pattern"),
            "missing --jitdasm-pattern:\n{exec_cmd}"
        );
        assert!(exec_cmd.contains(":SumFast"), "missing :SumFast:\n{exec_cmd}");
    }

    #[test]
    fn spawn_config_skips_pattern_for_wildcard() {
        let cfg = JitDasmBackend
            .spawn_config("myapp.csproj", &["*".into()])
            .unwrap();
        let exec_cmd = cfg.init_commands.last().expect("exec cmd");
        assert!(
            !exec_cmd.contains("--jitdasm-pattern"),
            "wildcard shouldn't produce a default filter:\n{exec_cmd}"
        );
    }

    #[test]
    fn qualify_pattern_leaves_wildcard_prefix() {
        assert_eq!(
            qualify_pattern("*:DoWork", "/tmp/Broken.csproj"),
            "*:DoWork"
        );
    }
}
