use std::process::Command;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::canonical::{BreakId, BreakLoc, CanonicalOps, HitEvent, unsupported};
use super::{Backend, Dependency, DependencyCheck, SpawnConfig};

pub struct NetCoreDbgBackend;

impl Backend for NetCoreDbgBackend {
    fn name(&self) -> &'static str {
        "netcoredbg"
    }

    fn description(&self) -> &'static str {
        ".NET debugger (C#, F#)"
    }

    fn types(&self) -> &'static [&'static str] {
        &["dotnet", "csharp", "fsharp"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let netcoredbg = std::env::var("NETCOREDBG").unwrap_or_else(|_| "netcoredbg".into());

        let mut spawn_args = vec!["--interpreter=cli".into(), "--".into(), target.into()];
        spawn_args.extend(args.iter().cloned());

        let mut env = vec![];
        if std::env::var("DOTNET_ROOT").is_err() {
            if let Some(root) = detect_dotnet_root() {
                env.push(("DOTNET_ROOT".into(), root));
            }
        }

        Ok(SpawnConfig {
            bin: netcoredbg,
            args: spawn_args,
            env,
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"ncdb>"
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![
            Dependency {
                name: "dotnet",
                check: DependencyCheck::Binary {
                    name: "dotnet",
                    alternatives: &["dotnet"],
                    version_cmd: None,
                },
                install: "https://dot.net/install",
            },
            Dependency {
                name: "netcoredbg",
                check: DependencyCheck::Binary {
                    name: "netcoredbg",
                    alternatives: &["netcoredbg"],
                    version_cmd: None,
                },
                install: concat!(
                    "mkdir -p ~/.local/share/netcoredbg && ",
                    "curl -sL https://github.com/Samsung/netcoredbg/releases/latest/download/",
                    "netcoredbg-linux-amd64.tar.gz | tar xz -C ~/.local/share/netcoredbg && ",
                    "ln -sf ~/.local/share/netcoredbg/netcoredbg/netcoredbg ~/.local/bin/netcoredbg"
                ),
            },
        ]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        format!("break {spec}")
    }

    fn run_command(&self) -> &'static str {
        "run"
    }

    fn parse_help(&self, raw: &str) -> String {
        // The first token of the banner ("command list:") and dash-prefixed
        // option lines ("-h", "-v") would otherwise slip through the
        // alphabetic check; reject them by token shape.
        super::parse_help_first_token(raw, "netcoredbg", false, |tok| {
            tok.len() < 20
                && !tok.starts_with('-')
                && !tok.starts_with("command")
                && tok.chars().all(|c| c.is_ascii_alphabetic())
        })
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("dotnet.md", include_str!("../../skills/adapters/dotnet.md"))]
    }

    fn clean(&self, _cmd: &str, output: &str) -> String {
        let stop_re = Regex::new(r"reason: (.+?)(?:, thread|, stopped|$)").unwrap();
        let frame_re = Regex::new(r"frame=\{(.+?)\}").unwrap();

        let mut lines = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.contains("^running") {
                continue;
            }
            // Drop lifecycle noise — agents query session state, not banners.
            if trimmed.contains("library loaded:")
                || trimmed.contains("symbols loaded, base")
                || trimmed.contains("no symbols loaded")
                || trimmed.contains("thread created")
                || trimmed.contains("thread exited")
                || trimmed.contains("breakpoint modified")
            {
                continue;
            }
            if trimmed.starts_with("stopped,") {
                let reason = stop_re
                    .captures(trimmed)
                    .map(|c| c[1].to_string())
                    .unwrap_or_else(|| "unknown".into());
                let loc = frame_re
                    .captures(trimmed)
                    .map(|c| format!(" @ {}", &c[1]))
                    .unwrap_or_default();
                lines.push(format!("stopped: {reason}{loc}"));
                continue;
            }
            lines.push(trimmed.to_string());
        }

        lines.join("\n")
    }

    fn canonical_ops(&self) -> Option<&dyn CanonicalOps> {
        Some(self)
    }
}

impl CanonicalOps for NetCoreDbgBackend {
    fn tool_name(&self) -> &'static str { "netcoredbg" }
    fn auto_capture_locals(&self) -> bool { false }

    fn tool_version(&self) -> Option<String> {
        static V: OnceLock<Option<String>> = OnceLock::new();
        V.get_or_init(|| {
            let bin = std::env::var("NETCOREDBG").unwrap_or_else(|_| "netcoredbg".into());
            let out = Command::new(&bin).arg("--version").output().ok()?;
            let s = String::from_utf8_lossy(&out.stdout);
            let s = if s.trim().is_empty() {
                String::from_utf8_lossy(&out.stderr).to_string()
            } else {
                s.to_string()
            };
            s.lines().next().map(|l| l.trim().to_string())
        })
        .clone()
    }

    fn op_break(&self, loc: &BreakLoc) -> anyhow::Result<String> {
        Ok(match loc {
            BreakLoc::FileLine { file, line } => format!("break {file}:{line}"),
            BreakLoc::Fqn(name) => format!("break {name}"),
            BreakLoc::ModuleMethod { module, method } => {
                format!("break {module}!{method}")
            }
        })
    }

    fn op_unbreak(&self, id: BreakId) -> anyhow::Result<String> {
        Ok(format!("delete {}", id.0))
    }
    fn op_breaks(&self) -> anyhow::Result<String> { Ok("info breakpoints".into()) }

    fn op_run(&self, _args: &[String]) -> anyhow::Result<String> {
        // netcoredbg launches the target on startup; re-running is `run`.
        Ok("run".into())
    }
    fn op_continue(&self) -> anyhow::Result<String> { Ok("continue".into()) }
    fn op_step(&self) -> anyhow::Result<String> { Ok("step".into()) }
    fn op_next(&self) -> anyhow::Result<String> { Ok("next".into()) }
    fn op_finish(&self) -> anyhow::Result<String> { Ok("finish".into()) }

    fn op_stack(&self, n: Option<u32>) -> anyhow::Result<String> {
        Ok(match n {
            Some(k) => format!("backtrace {k}"),
            None => "backtrace".into(),
        })
    }
    fn op_frame(&self, n: u32) -> anyhow::Result<String> {
        Ok(format!("frame {n}"))
    }
    fn op_locals(&self) -> anyhow::Result<String> {
        // netcoredbg's CLI mode has no bulk `info locals` equivalent.
        // `print this` only shows the implicit receiver and doesn't
        // list method-local variables. Phase 2 should switch to MI
        // mode + `-stack-list-variables --all-values`. For now, agents
        // use `dbg print <varname>` for individual variables.
        Err(unsupported("netcoredbg", "bulk locals in CLI mode (use `dbg print <var>` for individual variables)"))
    }
    fn op_print(&self, expr: &str) -> anyhow::Result<String> {
        Ok(format!("print {expr}"))
    }
    fn op_threads(&self) -> anyhow::Result<String> { Ok("info threads".into()) }
    fn op_thread(&self, n: u32) -> anyhow::Result<String> {
        Ok(format!("thread {n}"))
    }
    fn op_list(&self, loc: Option<&str>) -> anyhow::Result<String> {
        Ok(match loc {
            Some(s) => format!("list {s}"),
            None => "list".into(),
        })
    }
    // op_watch inherits the default `unsupported` implementation:
    // netcoredbg's CLI mode does not expose watchpoints.

    /// netcoredbg emits two formats at stop:
    ///   * raw MI: `*stopped,reason="breakpoint-hit",thread-id="1",frame={...}`
    ///   * cleaned (via `Backend::clean`): `stopped: breakpoint N hit @ <frame> at <file>:<line>`
    /// `parse_hit` tries the cleaned form first (it's what the daemon
    /// feeds in after `clean()`); falls back to MI for completeness.
    fn parse_hit(&self, output: &str) -> Option<HitEvent> {
        // Try the cleaned `stopped: ... @ <frame> at <file>:<line>` form.
        let cleaned_re = stop_regex_cleaned();
        for line in output.lines() {
            if let Some(c) = cleaned_re.captures(line) {
                let file = c["file"].to_string();
                let line_no: u32 = c["line"].parse().ok()?;
                let func = c["frame"].to_string();
                return Some(HitEvent {
                    location_key: format!("{file}:{line_no}"),
                    thread: None,
                    frame_symbol: Some(func),
                    file: Some(file),
                    line: Some(line_no),
                });
            }
        }

        // Fall back to the MI form.
        let stop_re = stop_regex_mi();
        let frame_re = frame_regex_mi();
        let file_line_re = file_line_regex_mi();

        let stopped = output
            .lines()
            .any(|l| l.trim_start().starts_with("stopped,") || l.contains("*stopped"));
        if !stopped {
            return None;
        }
        let thread = output.lines().find_map(|l| {
            stop_re
                .captures(l)
                .and_then(|c| c.name("tid").map(|m| m.as_str().to_string()))
        });
        let frame = output.lines().find_map(|l| frame_re.captures(l));
        let (func, file, line) = match frame.as_ref() {
            Some(c) => {
                let frame_blob = &c[1];
                let func = find_mi_field(frame_blob, "func");
                let (file, line) = file_line_re
                    .captures(frame_blob)
                    .map(|m| (m["f"].to_string(), m["l"].parse::<u32>().ok()))
                    .unwrap_or_else(|| (String::new(), None));
                let file = if file.is_empty() {
                    find_mi_field(frame_blob, "file")
                } else {
                    Some(file)
                };
                let line = line.or_else(|| {
                    find_mi_field(frame_blob, "line").and_then(|s| s.parse().ok())
                });
                (func, file, line)
            }
            None => (None, None, None),
        };

        let location_key = match (&file, line, &func) {
            (Some(f), Some(l), _) => format!("{f}:{l}"),
            (_, _, Some(s)) => s.clone(),
            _ => return None,
        };

        Some(HitEvent {
            location_key,
            thread,
            frame_symbol: func,
            file,
            line,
        })
    }

    /// `info locals` emits `name = value` lines similar to gdb.
    fn parse_locals(&self, output: &str) -> Option<Value> {
        let re = locals_regex();
        let mut obj = Map::new();
        for line in output.lines() {
            if let Some(c) = re.captures(line.trim_end()) {
                let name = c[1].to_string();
                let val = c[2].trim().to_string();
                let mut entry = Map::new();
                entry.insert("value".into(), Value::String(val));
                obj.insert(name, Value::Object(entry));
            }
        }
        if obj.is_empty() {
            None
        } else {
            Some(Value::Object(obj))
        }
    }
}

fn stop_regex_cleaned() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `stopped: breakpoint 1 hit @ Ns.Class.Method() at /path/file.cs:42`
        Regex::new(
            r"^stopped:[^@]*@\s+(?P<frame>\S+?)\s+at\s+(?P<file>\S+):(?P<line>\d+)",
        )
        .unwrap()
    })
}

fn stop_regex_mi() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"thread-id[:=]\s*["]?(?P<tid>\d+)"#).unwrap()
    })
}

fn frame_regex_mi() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"frame=\{(.+?)\}").unwrap())
}

fn file_line_regex_mi() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"at\s+(?P<f>[^:}\s]+):(?P<l>\d+)").unwrap())
}

fn find_mi_field(blob: &str, key: &str) -> Option<String> {
    // Match `key="value"` (MI-style) or `key=value` (un-quoted) inside
    // the frame blob. Values may contain spaces but not `}` or `,`.
    let re_quoted = Regex::new(&format!(r#"{key}="([^"]*)""#)).ok()?;
    if let Some(c) = re_quoted.captures(blob) {
        return Some(c[1].to_string());
    }
    let re_unq = Regex::new(&format!(r"{key}=([^,}}]+)")).ok()?;
    re_unq.captures(blob).map(|c| c[1].trim().to_string())
}

fn locals_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.+)$").unwrap())
}

fn detect_dotnet_root() -> Option<String> {
    // Homebrew layout: .../dotnet/<ver>/bin/dotnet → sibling libexec/.
    // Standard layout: dotnet binary lives directly in the root.
    dbg_cli::deps::find_tool_root("dotnet", Some("libexec"), None, 2)
        .and_then(|p| p.to_str().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_breakpoint() {
        assert_eq!(
            NetCoreDbgBackend.format_breakpoint("Program.cs:10"),
            "break Program.cs:10"
        );
    }

    #[test]
    fn clean_parses_stopped_with_reason_and_frame() {
        let input = "stopped, reason: breakpoint 1 hit, thread-id: 1, frame={Program.Main() at Program.cs:4}";
        let r = NetCoreDbgBackend.clean("run", input);
        assert!(r.contains("stopped: breakpoint 1 hit"));
        assert!(r.contains("@ Program.Main() at Program.cs:4"));
    }

    #[test]
    fn clean_emits_library_events() {
        let input = "library loaded: System.dll, symbols loaded, base address: 0x1000\nthread created, id: 123\nbreakpoint modified, Breakpoint 1";
        let r = NetCoreDbgBackend.clean("run", input);
        assert!(r.is_empty());
    }

    #[test]
    fn clean_skips_empty_and_running() {
        let input = "\n^running\nactual output";
        let r = NetCoreDbgBackend.clean("continue", input);
        assert_eq!(r, "actual output");
    }

    #[test]
    fn parse_help_filters_dashes_and_command() {
        let raw = "command list:\n-h  show help\nbreak  Set breakpoint\ncontinue  Resume";
        let result = NetCoreDbgBackend.parse_help(raw);
        assert!(result.contains("break"));
        assert!(result.contains("continue"));
        assert!(!result.contains("command"));
    }

    // --------------------------------------------------------------
    // CanonicalOps
    // --------------------------------------------------------------

    #[test]
    fn canonical_break_ops() {
        let ops: &dyn CanonicalOps = &NetCoreDbgBackend;
        assert_eq!(
            ops.op_break(&BreakLoc::FileLine { file: "Program.cs".into(), line: 10 }).unwrap(),
            "break Program.cs:10"
        );
        assert_eq!(
            ops.op_break(&BreakLoc::Fqn("Foo.Bar.Baz".into())).unwrap(),
            "break Foo.Bar.Baz"
        );
        assert_eq!(
            ops.op_break(&BreakLoc::ModuleMethod { module: "MyLib".into(), method: "Baz".into() }).unwrap(),
            "break MyLib!Baz"
        );
    }

    #[test]
    fn canonical_exec_ops() {
        let ops: &dyn CanonicalOps = &NetCoreDbgBackend;
        assert_eq!(ops.op_continue().unwrap(), "continue");
        assert_eq!(ops.op_step().unwrap(), "step");
        assert_eq!(ops.op_next().unwrap(), "next");
        assert_eq!(ops.op_finish().unwrap(), "finish");
    }

    #[test]
    fn canonical_thread_ops() {
        let ops: &dyn CanonicalOps = &NetCoreDbgBackend;
        assert_eq!(ops.op_threads().unwrap(), "info threads");
        assert_eq!(ops.op_thread(2).unwrap(), "thread 2");
    }

    #[test]
    fn canonical_watch_unsupported() {
        let ops: &dyn CanonicalOps = &NetCoreDbgBackend;
        let err = ops.op_watch("x").unwrap_err().to_string();
        assert!(err.contains("netcoredbg"));
        assert!(err.contains("dbg raw"));
    }

    #[test]
    fn parse_hit_from_cleaned_stop_line() {
        // This is what Backend::clean produces out of the MI record —
        // it's also what the daemon feeds into parse_hit.
        let out = "stopped: breakpoint 1 hit @ DbgExample.Algos.Fibonacci() at /app/Program.cs:22";
        let hit = NetCoreDbgBackend.parse_hit(out).expect("should parse");
        assert_eq!(hit.file.as_deref(), Some("/app/Program.cs"));
        assert_eq!(hit.line, Some(22));
        assert_eq!(hit.frame_symbol.as_deref(), Some("DbgExample.Algos.Fibonacci()"));
    }

    #[test]
    fn parse_hit_from_mi_stopped_record() {
        let out = r#"stopped, reason: breakpoint 1 hit, thread-id: 1, frame={Program.Main() at Program.cs:4}"#;
        let hit = NetCoreDbgBackend.parse_hit(out).expect("should parse");
        assert_eq!(hit.file.as_deref(), Some("Program.cs"));
        assert_eq!(hit.line, Some(4));
        assert_eq!(hit.thread.as_deref(), Some("1"));
    }

    #[test]
    fn parse_hit_from_quoted_mi_record() {
        let out = r#"*stopped,reason="breakpoint-hit",thread-id="1",frame={func="Foo.Bar.Baz",file="Program.cs",line="42"}"#;
        let hit = NetCoreDbgBackend.parse_hit(out).expect("should parse");
        assert_eq!(hit.file.as_deref(), Some("Program.cs"));
        assert_eq!(hit.line, Some(42));
        assert_eq!(hit.frame_symbol.as_deref(), Some("Foo.Bar.Baz"));
    }

    #[test]
    fn parse_hit_none_without_stopped() {
        assert!(NetCoreDbgBackend.parse_hit("random output").is_none());
    }

    #[test]
    fn parse_locals_simple_values() {
        let out = "x = 42\nname = \"hello\"\nempty = {}";
        let v = NetCoreDbgBackend.parse_locals(out).expect("should parse");
        let obj = v.as_object().unwrap();
        assert_eq!(obj.get("x").unwrap().get("value").unwrap().as_str().unwrap(), "42");
        assert_eq!(obj.get("name").unwrap().get("value").unwrap().as_str().unwrap(), "\"hello\"");
    }

    #[test]
    fn backend_canonical_ops_hook_returns_self() {
        let b: Box<dyn Backend> = Box::new(NetCoreDbgBackend);
        assert_eq!(b.canonical_ops().unwrap().tool_name(), "netcoredbg");
    }
}
