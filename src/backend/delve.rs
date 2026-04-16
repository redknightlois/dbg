use std::process::Command;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::canonical::{BreakId, BreakLoc, CanonicalOps, HitEvent};
use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct DelveBackend;

impl Backend for DelveBackend {
    fn name(&self) -> &'static str {
        "delve"
    }

    fn description(&self) -> &'static str {
        "Go debugger"
    }

    fn types(&self) -> &'static [&'static str] {
        &["go"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let mut spawn_args = vec!["exec".into(), target.into()];
        if !args.is_empty() {
            spawn_args.push("--".into());
            spawn_args.extend(args.iter().cloned());
        }

        Ok(SpawnConfig {
            bin: "dlv".into(),
            args: spawn_args,
            env: vec![],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"\(dlv\) "
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![
            Dependency {
                name: "go",
                check: DependencyCheck::Binary {
                    name: "go",
                    alternatives: &["go"],
                    version_cmd: None,
                },
                install: "https://go.dev/dl/",
            },
            Dependency {
                name: "dlv",
                check: DependencyCheck::Binary {
                    name: "dlv",
                    alternatives: &["dlv"],
                    version_cmd: None,
                },
                install: "go install github.com/go-delve/delve/cmd/dlv@latest",
            },
        ]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        format!("break {spec}")
    }

    fn run_command(&self) -> &'static str {
        "continue"
    }

    fn parse_help(&self, raw: &str) -> String {
        let mut cmds = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            // Delve help: "command (alias) description" or "command  description"
            if let Some(tok) = line.split_whitespace().next() {
                if tok.chars().all(|c| c.is_ascii_alphabetic())
                    && tok.len() < 20
                    && !tok.is_empty()
                    && tok != "Type"
                    && tok != "Aliases"
                {
                    cmds.push(tok.to_string());
                }
            }
        }
        cmds.dedup();
        format!("delve: {}", cmds.join(", "))
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        let mut events = Vec::new();
        let mut lines = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("Created breakpoint") {
                events.push(trimmed.to_string());
                continue;
            }
            if trimmed.contains("goroutine") && trimmed.contains("exited") {
                events.push(trimmed.to_string());
                continue;
            }
            lines.push(line);
        }
        CleanResult {
            output: lines.join("\n"),
            events,
        }
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("go.md", include_str!("../../skills/adapters/go.md"))]
    }

    fn canonical_ops(&self) -> Option<&dyn CanonicalOps> {
        Some(self)
    }
}

impl CanonicalOps for DelveBackend {
    fn tool_name(&self) -> &'static str { "delve" }

    fn tool_version(&self) -> Option<String> {
        static V: OnceLock<Option<String>> = OnceLock::new();
        V.get_or_init(|| {
            let out = Command::new("dlv").arg("version").output().ok()?;
            let s = String::from_utf8_lossy(&out.stdout);
            s.lines().next().map(|l| l.trim().to_string())
        })
        .clone()
    }

    fn op_break(&self, loc: &BreakLoc) -> anyhow::Result<String> {
        Ok(match loc {
            BreakLoc::FileLine { file, line } => format!("break {file}:{line}"),
            BreakLoc::Fqn(name) => format!("break {name}"),
            // Go has no shared-library concept; route to fqn form.
            BreakLoc::ModuleMethod { module, method } => format!("break {module}.{method}"),
        })
    }

    fn op_unbreak(&self, id: BreakId) -> anyhow::Result<String> {
        Ok(format!("clear {}", id.0))
    }
    fn op_breaks(&self) -> anyhow::Result<String> { Ok("breakpoints".into()) }

    fn op_run(&self, _args: &[String]) -> anyhow::Result<String> {
        // Delve launches paused at the entry point. `continue` starts
        // execution and stops at the first breakpoint — the right
        // behaviour for the initial `--run` flag. If the agent needs a
        // full restart later, `dbg raw restart` followed by `continue`
        // achieves that explicitly.
        Ok("continue".into())
    }
    fn op_continue(&self) -> anyhow::Result<String> { Ok("continue".into()) }
    fn op_step(&self) -> anyhow::Result<String> { Ok("step".into()) }
    fn op_next(&self) -> anyhow::Result<String> { Ok("next".into()) }
    fn op_finish(&self) -> anyhow::Result<String> { Ok("stepout".into()) }

    fn op_stack(&self, n: Option<u32>) -> anyhow::Result<String> {
        Ok(match n {
            Some(k) => format!("stack {k}"),
            None => "stack".into(),
        })
    }
    fn op_frame(&self, n: u32) -> anyhow::Result<String> {
        Ok(format!("frame {n}"))
    }
    fn op_locals(&self) -> anyhow::Result<String> { Ok("locals".into()) }
    fn op_print(&self, expr: &str) -> anyhow::Result<String> {
        Ok(format!("print {expr}"))
    }
    fn op_watch(&self, expr: &str) -> anyhow::Result<String> {
        // Delve: `watch -w <expr>` = break on write.
        Ok(format!("watch -w {expr}"))
    }
    fn op_threads(&self) -> anyhow::Result<String> {
        // Delve: Go uses goroutines, not OS threads. Canonical
        // `threads` maps to goroutines so cross-backend queries work.
        Ok("goroutines".into())
    }
    fn op_thread(&self, n: u32) -> anyhow::Result<String> {
        Ok(format!("goroutine {n}"))
    }
    fn op_list(&self, loc: Option<&str>) -> anyhow::Result<String> {
        Ok(match loc {
            Some(s) => format!("list {s}"),
            None => "list".into(),
        })
    }

    /// Delve emits a stop banner like:
    ///   `> main.main() ./main.go:10 (hits goroutine(1):1 total:1) (PC: 0x48e120)`
    fn parse_hit(&self, output: &str) -> Option<HitEvent> {
        let re = stop_regex();
        for line in output.lines() {
            if let Some(c) = re.captures(line) {
                let symbol = c[1].to_string();
                let file = c[2].to_string();
                let line_no: u32 = c[3].parse().ok()?;
                let goroutine = c.get(4).map(|m| m.as_str().to_string());
                return Some(HitEvent {
                    location_key: format!("{file}:{line_no}"),
                    thread: goroutine,
                    frame_symbol: Some(symbol),
                    file: Some(file),
                    line: Some(line_no),
                });
            }
        }
        None
    }

    /// `locals` emits lines like `name = value` — no type annotation.
    fn parse_locals(&self, output: &str) -> Option<Value> {
        let re = locals_regex();
        let mut obj = Map::new();
        for line in output.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            if let Some(c) = re.captures(line) {
                let name = c[1].to_string();
                let value = c[2].trim().to_string();
                let mut entry = Map::new();
                entry.insert("value".into(), Value::String(value));
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

fn stop_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // delve emits two forms after `clean()`:
        //   > main.main() ./main.go:10 (hits goroutine(1):1 total:1) ...
        //   > [Breakpoint 1] main.fibonacci() ./main.go:22 (hits goroutine(1):1 total:1)
        // The bracketed `[Breakpoint N]` prefix appears on the first hit
        // of a newly-installed breakpoint; subsequent hits drop it.
        Regex::new(
            r"^>\s+(?:\[Breakpoint\s+\d+\]\s+)?([A-Za-z_][\w./()*]*)\s+(\S+):(\d+)(?:\s+\(hits goroutine\((\d+)\))?",
        )
        .unwrap()
    })
}

fn locals_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.+)$").unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_breakpoint() {
        assert_eq!(DelveBackend.format_breakpoint("main.go:10"), "break main.go:10");
    }

    #[test]
    fn clean_extracts_breakpoint_events() {
        let input = "Created breakpoint 1 at main.go:10\nnormal output";
        let r = DelveBackend.clean("break main.go:10", input);
        assert_eq!(r.output, "normal output");
        assert_eq!(r.events.len(), 1);
        assert!(r.events[0].contains("Created breakpoint"));
    }

    #[test]
    fn clean_extracts_goroutine_exit() {
        let input = "some output\ngoroutine 1 exited\nmore output";
        let r = DelveBackend.clean("continue", input);
        assert!(r.output.contains("some output"));
        assert!(r.output.contains("more output"));
        assert!(!r.output.contains("goroutine"));
        assert_eq!(r.events.len(), 1);
    }

    #[test]
    fn spawn_config_exec_with_args() {
        let cfg = DelveBackend
            .spawn_config("./app", &["--port".into(), "8080".into()])
            .unwrap();
        assert_eq!(cfg.args[0], "exec");
        assert_eq!(cfg.args[1], "./app");
        assert_eq!(cfg.args[2], "--");
        assert_eq!(cfg.args[3], "--port");
    }

    #[test]
    fn spawn_config_exec_no_args() {
        let cfg = DelveBackend.spawn_config("./app", &[]).unwrap();
        assert_eq!(cfg.args, vec!["exec", "./app"]);
    }

    #[test]
    fn parse_help_filters_noise() {
        let raw = "The following commands are available:\n  break   Set breakpoint\n  continue Resume\n  Type help for more\n  Aliases for break: b";
        let result = DelveBackend.parse_help(raw);
        assert!(result.contains("break"));
        assert!(result.contains("continue"));
        assert!(!result.contains("Type"));
        assert!(!result.contains("Aliases"));
    }

    // --------------------------------------------------------------
    // CanonicalOps
    // --------------------------------------------------------------

    #[test]
    fn canonical_break_ops() {
        let ops: &dyn CanonicalOps = &DelveBackend;
        assert_eq!(
            ops.op_break(&BreakLoc::FileLine { file: "main.go".into(), line: 10 }).unwrap(),
            "break main.go:10"
        );
        assert_eq!(
            ops.op_break(&BreakLoc::Fqn("main.main".into())).unwrap(),
            "break main.main"
        );
        assert_eq!(
            ops.op_break(&BreakLoc::ModuleMethod { module: "pkg".into(), method: "Foo".into() }).unwrap(),
            "break pkg.Foo"
        );
    }

    #[test]
    fn canonical_exec_ops() {
        let ops: &dyn CanonicalOps = &DelveBackend;
        assert_eq!(ops.op_continue().unwrap(), "continue");
        assert_eq!(ops.op_step().unwrap(), "step");
        assert_eq!(ops.op_next().unwrap(), "next");
        assert_eq!(ops.op_finish().unwrap(), "stepout");
    }

    #[test]
    fn canonical_threads_map_to_goroutines() {
        let ops: &dyn CanonicalOps = &DelveBackend;
        assert_eq!(ops.op_threads().unwrap(), "goroutines");
        assert_eq!(ops.op_thread(3).unwrap(), "goroutine 3");
    }

    #[test]
    fn canonical_watch_uses_write_mode() {
        let ops: &dyn CanonicalOps = &DelveBackend;
        assert_eq!(ops.op_watch("x").unwrap(), "watch -w x");
    }

    #[test]
    fn parse_hit_from_banner() {
        let out = "> main.main() ./main.go:10 (hits goroutine(1):1 total:1) (PC: 0x48e120)\n   9:   x := 1\n=> 10:   fmt.Println(x)";
        let hit = DelveBackend.parse_hit(out).expect("should parse");
        assert_eq!(hit.location_key, "./main.go:10");
        assert_eq!(hit.line, Some(10));
        assert_eq!(hit.frame_symbol.as_deref(), Some("main.main()"));
        assert_eq!(hit.thread.as_deref(), Some("1"));
    }

    #[test]
    fn parse_hit_with_breakpoint_prefix() {
        // First-hit form: delve prefixes with `[Breakpoint N]`.
        let out = "> [Breakpoint 1] main.fibonacci() ./examples/go/main.go:22 (hits goroutine(1):1 total:1) (PC: 0x4be442)";
        let hit = DelveBackend.parse_hit(out).expect("should parse");
        assert_eq!(hit.line, Some(22));
        assert_eq!(hit.frame_symbol.as_deref(), Some("main.fibonacci()"));
        assert_eq!(hit.thread.as_deref(), Some("1"));
    }

    #[test]
    fn parse_hit_none_without_banner() {
        assert!(DelveBackend.parse_hit("random output").is_none());
    }

    #[test]
    fn parse_locals_simple() {
        let out = "x = 42\nname = \"hello\"\ncfg = main.Config {Host: \"localhost\", Port: 8080}";
        let v = DelveBackend.parse_locals(out).expect("should parse");
        let obj = v.as_object().unwrap();
        assert_eq!(obj.get("x").unwrap().get("value").unwrap().as_str().unwrap(), "42");
        assert!(obj.contains_key("name"));
        assert!(obj.contains_key("cfg"));
    }

    #[test]
    fn backend_canonical_ops_hook_returns_self() {
        let b: Box<dyn Backend> = Box::new(DelveBackend);
        assert_eq!(b.canonical_ops().unwrap().tool_name(), "delve");
    }
}
