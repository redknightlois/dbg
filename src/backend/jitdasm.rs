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

/// Extract `--capture-duration <val>` (or `--capture-duration=<val>`)
/// from the backend args, returning the duration string (as accepted by
/// coreutils `timeout`, e.g. `30s`, `2m`, `1h`, or a bare number) and
/// the remaining args. Unknown values are passed through to `timeout`
/// verbatim — it rejects garbage at run time with a clear message.
fn extract_capture_duration(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut out = Vec::with_capacity(args.len());
    let mut duration: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(val) = a.strip_prefix("--capture-duration=") {
            duration = Some(val.to_string());
            i += 1;
            continue;
        }
        if a == "--capture-duration"
            && let Some(val) = args.get(i + 1)
        {
            duration = Some(val.clone());
            i += 2;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    (duration, out)
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
        // jitdasm drives `dotnet run --project <target>`, which only
        // accepts MSBuild project files. A bare `.dll` or `.exe`
        // reaches dotnet as an invalid --project argument; the error
        // lands in the capture file and the REPL happily loads the
        // host runtime's JIT noise instead. Catch the misuse up front.
        let lower = target.to_ascii_lowercase();
        if !(lower.ends_with(".csproj")
            || lower.ends_with(".fsproj")
            || lower.ends_with(".vbproj"))
        {
            anyhow::bail!(
                "jitdasm needs a project file (.csproj/.fsproj/.vbproj), got `{target}` — \
                 point at the source project, not a built binary"
            );
        }
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
        // Extract `--capture-duration <val>` before any positional
        // parsing. Long-running targets (QPS benches, servers, REPLs)
        // never exit under `dotnet run`, so `dbg start jitdasm` would
        // hang forever waiting for the child to produce EOF. When the
        // flag is set we wrap the child with `timeout --preserve-status`
        // so a graceful time-kill still looks like success. See
        // skills/adapters/jitdasm.md "Long-running targets".
        let (capture_duration, rest_args) = extract_capture_duration(args);

        let raw_pattern = if rest_args.is_empty() {
            "*".to_string()
        } else {
            rest_args[0].clone()
        };
        let pattern = qualify_pattern(&raw_pattern, &project);

        let extra_args: Vec<String> = if rest_args.len() > 1 {
            rest_args[1..].to_vec()
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

        let timeout_prefix = match capture_duration.as_deref() {
            Some(d) => format!("timeout --preserve-status {} ", shell_escape(d)),
            None => String::new(),
        };

        // Why not `dotnet run --project <csproj>`? On recent SDKs, the
        // `dotnet run` host launches the user program via an
        // intermediate process that drops `DOTNET_JitDisasm` (and the
        // other DOTNET_* JIT knobs) before reaching the process whose
        // JIT we actually want to inspect. Result: `capture.asm` ends
        // up empty except for the program's own stdout. Running
        // `dotnet exec <dll>` directly skips the intermediate process
        // and the env vars reach the JIT. We recover the dll path by
        // reading the `<AssemblyName>.runtimeconfig.json` sibling that
        // the build drops into `bin/Release/net*/` — only executables
        // emit one, so it reliably names the entry-point dll.
        let proj_dir_expr = format!("$(dirname {})", shell_escape(&project));
        let locate_dll = format!(
            "dll=$(ls -t {proj_dir}/bin/Release/net*/*.runtimeconfig.json 2>/dev/null | \
             head -1 | sed 's/\\.runtimeconfig\\.json$/.dll/'); \
             if [ -z \"$dll\" ] || [ ! -f \"$dll\" ]; then \
               echo 'jitdasm: could not locate built dll under bin/Release/net*/; did build succeed?' >&2; \
               exit 1; \
             fi",
            proj_dir = proj_dir_expr,
        );
        let run_cmd = format!(
            "echo 'Disassembling: {pattern}' && {locate_dll} && \
             DOTNET_TieredCompilation=0 DOTNET_JitDisasm='{pattern}' DOTNET_JitDiffableDasm=1 \
             {timeout_prefix}dotnet exec \"$dll\"{extra} > {out_file} 2>&1",
            out_file = out_file_str,
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
    fn capture_duration_wraps_with_timeout() {
        let cfg = JitDasmBackend
            .spawn_config(
                "myapp.csproj",
                &["--capture-duration".into(), "30s".into(), "Foo:Bar".into()],
            )
            .unwrap();
        let run = &cfg.init_commands[2];
        assert!(
            run.contains("timeout --preserve-status 30s dotnet exec"),
            "expected timeout prefix in: {run}"
        );
        // Flag must not leak through to the project's own args.
        assert!(!run.contains("--capture-duration"), "flag leaked: {run}");
    }

    #[test]
    fn capture_duration_equals_form_also_supported() {
        let cfg = JitDasmBackend
            .spawn_config(
                "myapp.csproj",
                &["--capture-duration=2m".into(), "Foo:Bar".into()],
            )
            .unwrap();
        assert!(cfg.init_commands[2].contains("timeout --preserve-status 2m dotnet exec"));
    }

    #[test]
    fn capture_duration_absent_means_no_timeout() {
        let cfg = JitDasmBackend
            .spawn_config("myapp.csproj", &["Foo:Bar".into()])
            .unwrap();
        assert!(!cfg.init_commands[2].contains("timeout "));
    }

    #[test]
    fn extract_capture_duration_preserves_order_of_remaining_args() {
        let (d, rest) = extract_capture_duration(&[
            "Foo:Bar".into(),
            "--capture-duration".into(),
            "45s".into(),
            "--iterations".into(),
            "1".into(),
        ]);
        assert_eq!(d.as_deref(), Some("45s"));
        assert_eq!(rest, vec!["Foo:Bar", "--iterations", "1"]);
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
    fn spawn_config_rejects_dll_target() {
        // Regression: `dbg start jitdasm broken.dll` used to proceed
        // silently — the generated `dotnet run --project broken.dll`
        // fails (dll is not a project file) but the error landed in
        // the capture file; `dbg methods` then returned host-runtime
        // methods only, with no indication that the user-supplied
        // target was rejected. jitdasm must refuse non-project files
        // up front so the misuse is caught at session start.
        let err = match JitDasmBackend.spawn_config("bin/Release/net8.0/broken.dll", &[]) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("jitdasm accepted a .dll target"),
        };
        assert!(
            err.contains(".csproj") || err.contains(".fsproj"),
            "expected project-file hint, got: {err}"
        );
        assert!(
            err.to_lowercase().contains("jitdasm"),
            "error should mention the jitdasm backend, got: {err}"
        );
    }

    #[test]
    fn spawn_config_accepts_project_targets() {
        // Sanity: project files must still spawn cleanly.
        JitDasmBackend
            .spawn_config("app.csproj", &[])
            .expect("csproj must be accepted");
        JitDasmBackend
            .spawn_config("app.fsproj", &[])
            .expect("fsproj must be accepted");
    }

    #[test]
    fn qualify_pattern_leaves_wildcard_prefix() {
        assert_eq!(
            qualify_pattern("*:DoWork", "/tmp/Broken.csproj"),
            "*:DoWork"
        );
    }
}
