use std::process::Command;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::canonical::{BreakId, BreakLoc, CanonicalOps, HitEvent};
use super::{Backend, Dependency, DependencyCheck, SpawnConfig};

pub struct LldbBackend;

impl Backend for LldbBackend {
    fn name(&self) -> &'static str {
        "lldb"
    }

    fn description(&self) -> &'static str {
        "native debugger for Rust, C, C++, Zig, D, Nim"
    }

    fn types(&self) -> &'static [&'static str] {
        // `gdb` is included as an alias: users who reach for the
        // familiar GDB name get the lldb backend with a clear note
        // instead of "unknown type: gdb".
        &["rust", "c", "cpp", "zig", "d", "nim", "gdb"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let lldb_bin =
            std::env::var("LLDB_BIN").unwrap_or_else(|_| find_lldb().unwrap_or("lldb".into()));

        let escaped_target = target.replace('\\', "\\\\").replace('"', "\\\"");
        let mut init_commands = vec![format!("file \"{escaped_target}\"")];
        if !args.is_empty() {
            let escaped_args: Vec<String> = args.iter().map(|a| {
                let e = a.replace('\\', "\\\\").replace('"', "\\\"");
                format!("\"{e}\"")
            }).collect();
            init_commands.push(format!("settings set target.run-args {}", escaped_args.join(" ")));
        }

        Ok(SpawnConfig {
            bin: lldb_bin,
            args: vec!["--no-use-colors".into()],
            env: vec![],
            init_commands,
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"\(lldb\) "
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "lldb",
            check: DependencyCheck::Binary {
                name: "lldb",
                alternatives: &["lldb-20", "lldb-18", "lldb"],
                version_cmd: None,
            },
            install: "sudo apt install lldb-20  # or: brew install llvm",
        }]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        if let Some((file, line)) = parse_file_line(spec) {
            format!("breakpoint set --file {file} --line {line}")
        } else {
            format!("breakpoint set --name {spec}")
        }
    }

    fn run_command(&self) -> &'static str {
        "process launch"
    }

    fn parse_help(&self, raw: &str) -> String {
        let re = Regex::new(r"^\s{1,4}(\w[\w -]*\w)\s+--\s+").unwrap();
        let cmds: Vec<&str> = raw
            .lines()
            .filter_map(|line| re.captures(line).map(|c| c.get(1).unwrap().as_str()))
            .collect();
        format!("lldb: {}", cmds.join(", "))
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("rust.md", include_str!("../../skills/adapters/rust.md")),
            ("c.md", include_str!("../../skills/adapters/c.md")),
            ("cpp.md", include_str!("../../skills/adapters/cpp.md")),
            ("zig.md", include_str!("../../skills/adapters/zig.md")),
        ]
    }

    fn clean(&self, cmd: &str, output: &str) -> String {
        let noise = [
            "Manually indexing DWARF",
            "Parsing symbol table",
            "Locating external symbol",
            "Reading binary from memory",
        ];

        let mut lines = Vec::new();
        for line in output.lines() {
            if noise.iter().any(|n| line.contains(n)) {
                continue;
            }
            // Drop process-lifecycle banners — they belong to the
            // session log, not user-facing command output.
            if line.contains("Process") && (line.contains("launched") || line.contains("exited")) {
                continue;
            }
            lines.push(line);
        }
        let cleaned = lines.join("\n");

        let trimmed = cmd.trim();
        if trimmed == "bt" || trimmed == "backtrace" {
            clean_bt(&cleaned)
        } else {
            cleaned
        }
    }

    fn canonical_ops(&self) -> Option<&dyn CanonicalOps> {
        Some(self)
    }
}

impl CanonicalOps for LldbBackend {
    fn tool_name(&self) -> &'static str {
        "lldb"
    }

    fn tool_version(&self) -> Option<String> {
        static V: OnceLock<Option<String>> = OnceLock::new();
        V.get_or_init(|| {
            let bin = find_lldb().unwrap_or_else(|| "lldb".into());
            let out = Command::new(&bin).arg("--version").output().ok()?;
            let s = String::from_utf8_lossy(&out.stdout);
            s.lines().next().map(|l| l.trim().to_string())
        })
        .clone()
    }

    fn op_break(&self, loc: &BreakLoc) -> anyhow::Result<String> {
        Ok(match loc {
            BreakLoc::FileLine { file, line } => {
                format!("breakpoint set --file {file} --line {line}")
            }
            BreakLoc::Fqn(name) => format!("breakpoint set --name {name}"),
            BreakLoc::ModuleMethod { module, method } => {
                format!("breakpoint set --shlib {module} --name {method}")
            }
        })
    }

    fn op_unbreak(&self, id: BreakId) -> anyhow::Result<String> {
        Ok(format!("breakpoint delete {}", id.0))
    }

    fn op_breaks(&self) -> anyhow::Result<String> {
        Ok("breakpoint list".into())
    }

    fn op_run(&self, args: &[String]) -> anyhow::Result<String> {
        if args.is_empty() {
            Ok("process launch".into())
        } else {
            let joined = args
                .iter()
                .map(|a| {
                    let e = a.replace('\\', "\\\\").replace('"', "\\\"");
                    format!("\"{e}\"")
                })
                .collect::<Vec<_>>()
                .join(" ");
            Ok(format!("process launch -- {joined}"))
        }
    }

    fn op_continue(&self) -> anyhow::Result<String> { Ok("process continue".into()) }
    fn op_step(&self) -> anyhow::Result<String> { Ok("thread step-in".into()) }
    fn op_next(&self) -> anyhow::Result<String> { Ok("thread step-over".into()) }
    fn op_finish(&self) -> anyhow::Result<String> { Ok("thread step-out".into()) }

    fn op_stack(&self, n: Option<u32>) -> anyhow::Result<String> {
        Ok(match n {
            Some(k) => format!("thread backtrace --count {k}"),
            None => "thread backtrace".into(),
        })
    }
    fn op_frame(&self, n: u32) -> anyhow::Result<String> {
        Ok(format!("frame select {n}"))
    }
    fn op_locals(&self) -> anyhow::Result<String> { Ok("frame variable".into()) }
    fn op_print(&self, expr: &str) -> anyhow::Result<String> {
        Ok(format!("expression -- {expr}"))
    }
    fn op_watch(&self, expr: &str) -> anyhow::Result<String> {
        Ok(format!("watchpoint set variable {expr}"))
    }
    fn op_threads(&self) -> anyhow::Result<String> { Ok("thread list".into()) }
    fn op_thread(&self, n: u32) -> anyhow::Result<String> {
        Ok(format!("thread select {n}"))
    }
    fn op_list(&self, loc: Option<&str>) -> anyhow::Result<String> {
        Ok(match loc {
            Some(s) => format!("source list --name {s}"),
            None => "source list".into(),
        })
    }

    /// lldb stops announce via lines like
    ///   ` * thread #1, queue = 'com.apple.main-thread', stop reason = breakpoint 1.1`
    /// followed shortly by a frame line:
    ///   `    frame #0: 0x... foo`main + 12 at main.c:42`
    /// We treat the latter as authoritative for file/line + symbol.
    fn parse_hit(&self, output: &str) -> Option<HitEvent> {
        // Require a stop banner to count this as a hit.
        let has_stop = output.lines().any(|l| {
            l.contains("stop reason = breakpoint")
                || l.contains("stop reason = watchpoint")
                || l.contains("stop reason = step")
                || l.contains("stop reason = signal")
        });
        if !has_stop {
            return None;
        }
        let frame_re = frame_regex();
        let thread_re = thread_regex();

        let thread = output.lines().find_map(|l| {
            thread_re.captures(l).map(|c| c[1].to_string())
        });

        let frame = output.lines().find_map(|l| frame_re.captures(l));
        let (symbol, file, line) = match frame.as_ref() {
            Some(c) => (
                Some(c[2].to_string()),
                Some(c[3].to_string()),
                c.get(4).and_then(|m| m.as_str().parse::<u32>().ok()),
            ),
            None => (None, None, None),
        };

        let location_key = match (&file, line, &symbol) {
            (Some(f), Some(l), _) => format!("{f}:{l}"),
            (_, _, Some(s)) => s.clone(),
            _ => return None,
        };

        Some(HitEvent {
            location_key,
            thread,
            frame_symbol: symbol,
            file,
            line,
        })
    }

    /// `frame variable` emits a `(Type) name = value` table. We parse
    /// each top-level entry into a JSON object: { name: {type, value} }.
    fn parse_locals(&self, output: &str) -> Option<Value> {
        // Bug 1: LLDB returns this error verbatim when the debuggee has
        // already exited but the LLDB process itself is still alive.
        // Map it to a structured post-mortem sentinel so callers get a
        // consistent signal rather than a raw error string.
        if output.contains("Command requires a process which is currently stopped") {
            let mut entry = Map::new();
            entry.insert(
                "value".into(),
                Value::String(
                    "[post-mortem] debuggee has exited — use `dbg hits`, `dbg cross`, \
                     or `dbg start` for a new session"
                        .into(),
                ),
            );
            let mut obj = Map::new();
            obj.insert("[post-mortem]".into(), Value::Object(entry));
            return Some(Value::Object(obj));
        }

        let mut obj = Map::new();
        let re = locals_regex();
        for line in output.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            if let Some(c) = re.captures(line) {
                let ty = c.get(1).map(|m| m.as_str().trim().to_string());
                let raw_name = c.get(2).unwrap().as_str().to_string();
                let val = c.get(3).unwrap().as_str().trim().to_string();

                // Bug 2a: Filter bare struct-open placeholder lines like
                // `(MyStruct) remaining = {` — LLDB emits these when it
                // can't expand the type; they add noise without content.
                if val == "{" {
                    continue;
                }

                // Bug 2b: Rename Rust tuple-field synthetic names
                // (`__0`, `__1`, …) to plain integer strings (`0`, `1`, …).
                let name = if raw_name.starts_with("__")
                    && raw_name[2..].chars().all(|c| c.is_ascii_digit())
                    && raw_name.len() > 2
                {
                    raw_name[2..].to_string()
                } else {
                    raw_name
                };

                let mut entry = Map::new();
                if let Some(t) = ty {
                    entry.insert("type".into(), Value::String(t));
                }
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

fn frame_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // lldb 22+ appends a column number: `at algos.c:26:18`. The
        // trailing `(?::\d+)?` absorbs it so we capture file=algos.c
        // and line=26, not file=algos.c:26 and line=18.
        Regex::new(
            r"^\s*(?:\*\s*)?frame #(\d+):[^`]*`([^+]+?)(?:\s+\+\s+\d+)?\s+at\s+(\S+?):(\d+)(?::\d+)?",
        )
        .unwrap()
    })
}

fn thread_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\*?\s*thread\s+#(\d+)").unwrap())
}

fn locals_regex() -> &'static Regex {
    // `(Type) name = <value ...>` — the initial `(...)` is optional
    // because lldb sometimes prints `name = ...` alone for reprinted
    // locals after step.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(?:\(([^)]+)\)\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.+)$")
            .unwrap()
    })
}

fn find_lldb() -> Option<String> {
    for name in &["lldb-20", "lldb-18", "lldb"] {
        if which::which(name).is_ok() {
            return Some(name.to_string());
        }
    }
    None
}

fn parse_file_line(spec: &str) -> Option<(&str, &str)> {
    let (file, line) = spec.rsplit_once(':')?;
    if line.chars().all(|c| c.is_ascii_digit()) && !line.is_empty() {
        Some((file, line))
    } else {
        None
    }
}

fn clean_bt(output: &str) -> String {
    let frame_re =
        Regex::new(r"^\s*\*?\s*(frame #\d+):.*?`(.+?)(?:\s+\+\s+\d+)?\s+at\s+(\S+)").unwrap();
    let mut cleaned = Vec::new();

    for line in output.lines() {
        if let Some(caps) = frame_re.captures(line) {
            cleaned.push(format!(
                "  {}: {} at {}",
                &caps[1], &caps[2], &caps[3]
            ));
        } else if line.starts_with("* thread") || line.starts_with("  thread") {
            cleaned.push(line.to_string());
        } else if line.contains("stop reason") {
            cleaned.push(line.trim().to_string());
        }
    }

    if cleaned.is_empty() {
        output.to_string()
    } else {
        cleaned.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_breakpoint_file_line() {
        let b = LldbBackend;
        assert_eq!(
            b.format_breakpoint("main.c:42"),
            "breakpoint set --file main.c --line 42"
        );
    }

    #[test]
    fn format_breakpoint_function_name() {
        let b = LldbBackend;
        assert_eq!(b.format_breakpoint("main"), "breakpoint set --name main");
    }

    #[test]
    fn format_breakpoint_colon_in_path() {
        assert_eq!(
            parse_file_line("src/main.rs:10"),
            Some(("src/main.rs", "10"))
        );
        assert_eq!(parse_file_line("main"), None);
        assert_eq!(parse_file_line("foo:bar"), None);
    }

    #[test]
    fn clean_strips_dwarf_noise() {
        let b = LldbBackend;
        let input = "Manually indexing DWARF in foo.o\nactual output\nParsing symbol table";
        let r = b.clean("p x", input);
        assert_eq!(r, "actual output");
    }

    #[test]
    fn clean_extracts_process_events() {
        let b = LldbBackend;
        let input = "Process 1234 launched: '/bin/test'\nsome output\nProcess 1234 exited with status = 0";
        let r = b.clean("continue", input);
        assert_eq!(r, "some output");
    }

    #[test]
    fn clean_bt_reformats_frames() {
        let input = "* thread #1, name = 'test', stop reason = breakpoint 1.1\n    frame #0: 0x00005555 test`main + 12 at main.c:4\n    frame #1: 0x00007fff libc`__libc_start_main + 128 at start.c:100";
        let r = LldbBackend.clean("bt", input);
        assert!(r.contains("frame #0: main at main.c:4"));
        assert!(r.contains("frame #1: __libc_start_main at start.c:100"));
        assert!(r.contains("* thread"));
    }

    #[test]
    fn clean_bt_passthrough_on_no_frames() {
        let r = LldbBackend.clean("bt", "no frames here");
        assert_eq!(r, "no frames here");
    }

    #[test]
    fn spawn_config_with_args() {
        let b = LldbBackend;
        let cfg = b
            .spawn_config("./test", &["arg1".into(), "arg2".into()])
            .unwrap();
        assert_eq!(cfg.init_commands.len(), 2);
        assert_eq!(cfg.init_commands[0], "file \"./test\"");
        assert!(cfg.init_commands[1].contains("\"arg1\" \"arg2\""));
    }

    #[test]
    fn spawn_config_no_args() {
        let cfg = LldbBackend.spawn_config("./test", &[]).unwrap();
        assert_eq!(cfg.init_commands.len(), 1);
        assert_eq!(cfg.init_commands[0], "file \"./test\"");
    }

    #[test]
    fn spawn_config_escapes_spaces_in_target() {
        let cfg = LldbBackend.spawn_config("./my app", &[]).unwrap();
        assert_eq!(cfg.init_commands[0], "file \"./my app\"");
    }

    #[test]
    fn spawn_config_escapes_quotes_in_target() {
        let cfg = LldbBackend.spawn_config("./te\"st", &[]).unwrap();
        assert_eq!(cfg.init_commands[0], "file \"./te\\\"st\"");
    }

    #[test]
    fn parse_help_extracts_commands() {
        let raw = "  breakpoint -- Set a breakpoint\n  continue   -- Continue execution\nSome other line";
        let result = LldbBackend.parse_help(raw);
        assert!(result.contains("breakpoint"));
        assert!(result.contains("continue"));
        assert!(!result.contains("Some other"));
    }

    // --------------------------------------------------------------
    // CanonicalOps
    // --------------------------------------------------------------

    #[test]
    fn canonical_break_file_line() {
        let ops: &dyn CanonicalOps = &LldbBackend;
        let s = ops.op_break(&BreakLoc::FileLine { file: "main.c".into(), line: 42 }).unwrap();
        assert_eq!(s, "breakpoint set --file main.c --line 42");
    }

    #[test]
    fn canonical_break_fqn() {
        let ops: &dyn CanonicalOps = &LldbBackend;
        let s = ops.op_break(&BreakLoc::Fqn("main".into())).unwrap();
        assert_eq!(s, "breakpoint set --name main");
    }

    #[test]
    fn canonical_break_module_method() {
        let ops: &dyn CanonicalOps = &LldbBackend;
        let s = ops.op_break(&BreakLoc::ModuleMethod {
            module: "libfoo.so".into(),
            method: "bar".into(),
        }).unwrap();
        assert_eq!(s, "breakpoint set --shlib libfoo.so --name bar");
    }

    #[test]
    fn canonical_exec_ops() {
        let ops: &dyn CanonicalOps = &LldbBackend;
        assert_eq!(ops.op_continue().unwrap(), "process continue");
        assert_eq!(ops.op_step().unwrap(), "thread step-in");
        assert_eq!(ops.op_next().unwrap(), "thread step-over");
        assert_eq!(ops.op_finish().unwrap(), "thread step-out");
    }

    #[test]
    fn canonical_run_with_args_quoted() {
        let ops: &dyn CanonicalOps = &LldbBackend;
        let s = ops.op_run(&["arg 1".into(), "arg\"2".into()]).unwrap();
        assert_eq!(s, "process launch -- \"arg 1\" \"arg\\\"2\"");
    }

    #[test]
    fn canonical_stack_with_and_without_count() {
        let ops: &dyn CanonicalOps = &LldbBackend;
        assert_eq!(ops.op_stack(None).unwrap(), "thread backtrace");
        assert_eq!(ops.op_stack(Some(5)).unwrap(), "thread backtrace --count 5");
    }

    #[test]
    fn canonical_locals_and_print() {
        let ops: &dyn CanonicalOps = &LldbBackend;
        assert_eq!(ops.op_locals().unwrap(), "frame variable");
        assert_eq!(ops.op_print("x + 1").unwrap(), "expression -- x + 1");
    }

    #[test]
    fn canonical_tool_name() {
        let ops: &dyn CanonicalOps = &LldbBackend;
        assert_eq!(ops.tool_name(), "lldb");
    }

    #[test]
    fn parse_hit_from_breakpoint_stop() {
        let output = "* thread #1, queue = 'com.apple.main-thread', stop reason = breakpoint 1.1\n\
                      * frame #0: 0x00005555 test`main + 12 at main.c:42\n\
                        frame #1: 0x00007fff libc`__libc_start_main + 128 at start.c:100";
        let hit = LldbBackend.parse_hit(output).expect("should parse");
        assert_eq!(hit.location_key, "main.c:42");
        assert_eq!(hit.file.as_deref(), Some("main.c"));
        assert_eq!(hit.line, Some(42));
        assert_eq!(hit.thread.as_deref(), Some("1"));
        assert_eq!(hit.frame_symbol.as_deref(), Some("main"));
    }

    #[test]
    fn parse_hit_with_column_number() {
        // lldb 22+ appends :column to the file:line location.
        let output = "* thread #1, name = 'algos', stop reason = breakpoint 1.1\n\
                        frame #0: 0x55555555518f algos`fibonacci(n=10) at algos.c:26:18";
        let hit = LldbBackend.parse_hit(output).expect("should parse");
        assert_eq!(hit.file.as_deref(), Some("algos.c"));
        assert_eq!(hit.line, Some(26));
        assert_eq!(hit.frame_symbol.as_deref(), Some("fibonacci(n=10)"));
    }

    #[test]
    fn parse_hit_none_when_no_stop_reason() {
        let output = "some unrelated output";
        assert!(LldbBackend.parse_hit(output).is_none());
    }

    #[test]
    fn parse_locals_typed_entries() {
        let output = "(int) x = 42\n(const char *) name = \"hello\"\n(std::vector<int>) v = size=3";
        let v = LldbBackend.parse_locals(output).expect("should parse");
        let obj = v.as_object().unwrap();
        assert_eq!(obj.get("x").unwrap().get("type").unwrap().as_str().unwrap(), "int");
        assert_eq!(obj.get("x").unwrap().get("value").unwrap().as_str().unwrap(), "42");
        assert_eq!(obj.get("name").unwrap().get("type").unwrap().as_str().unwrap(), "const char *");
        assert!(obj.contains_key("v"));
    }

    #[test]
    fn parse_locals_returns_none_on_empty() {
        assert!(LldbBackend.parse_locals("").is_none());
        assert!(LldbBackend.parse_locals("garbage with no = sign").is_none());
    }

    #[test]
    fn backend_canonical_ops_hook_returns_self() {
        let b: Box<dyn Backend> = Box::new(LldbBackend);
        assert!(b.canonical_ops().is_some());
        assert_eq!(b.canonical_ops().unwrap().tool_name(), "lldb");
    }

    // ------------------------------------------------------------------
    // Bug 1: parse_locals on post-exit LLDB error → [post-mortem] message
    // ------------------------------------------------------------------
    #[test]
    fn parse_locals_post_exit_error_maps_to_post_mortem() {
        // When the debuggee has already exited, `frame variable` returns
        // this verbatim error string.  parse_locals must detect it and
        // return a structured post-mortem value rather than None (which
        // would be silently dropped) or a raw error string passed through.
        let raw = "error: Command requires a process which is currently stopped.";
        let v = LldbBackend.parse_locals(raw).expect("should return a value for the error case");
        let obj = v.as_object().unwrap();
        // Must contain the sentinel key "[post-mortem]"
        assert!(
            obj.contains_key("[post-mortem]"),
            "expected [post-mortem] key, got: {v}"
        );
        let msg = obj["[post-mortem]"]
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            msg.contains("exited") || msg.contains("post-mortem"),
            "message should mention exit: {msg}"
        );
    }

    // ------------------------------------------------------------------
    // Bug 2: Rust tuple-field names (__0, __1) and placeholder lines
    // ------------------------------------------------------------------
    #[test]
    fn parse_locals_rust_tuple_fields_renamed() {
        // LLDB names Rust tuple fields __0, __1 etc.
        // They should be exposed as "0", "1".
        let output = "(u32) __0 = 42\n(u64) __1 = 100";
        let v = LldbBackend.parse_locals(output).expect("should parse");
        let obj = v.as_object().unwrap();
        assert!(
            obj.contains_key("0"),
            "expected key '0', got keys: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
        assert!(
            obj.contains_key("1"),
            "expected key '1', got keys: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
        assert!(
            !obj.contains_key("__0"),
            "raw __0 key should have been renamed"
        );
    }

    #[test]
    fn parse_locals_rust_pointer_address_filtered() {
        // Bare pointer-address locals like `expires_at = 0x00007fff…`
        // pollute the output with LLDB internals. They should be kept
        // as values (not filtered entirely) but at minimum not crash.
        let output = "(u64 *) expires_at = 0x00007fff12345678";
        let v = LldbBackend.parse_locals(output).expect("should parse");
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("expires_at"));
    }

    #[test]
    fn parse_locals_rust_bare_brace_placeholder_filtered() {
        // Lines that end with a lone `{` are struct-open placeholders
        // emitted by LLDB when it can't expand the type; they add noise.
        // The parser should drop them.
        let output = "(int) x = 42\n(MyStruct) remaining = {\n(int) y = 7";
        let v = LldbBackend.parse_locals(output).expect("should parse");
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("x"));
        assert!(obj.contains_key("y"));
        // "remaining = {" is a placeholder and must NOT appear as a key
        assert!(
            !obj.contains_key("remaining"),
            "bare-brace placeholder should be filtered out"
        );
    }

    // ------------------------------------------------------------------
    // Bug 3 (investigation): parse_hit must fire when stop banner appears
    // anywhere in multi-chunk output (e.g. after PTY flush boundary).
    // ------------------------------------------------------------------
    #[test]
    fn parse_hit_detects_stop_when_banner_precedes_frame_on_same_chunk() {
        // Canonical case: stop reason and frame on same chunk.
        let output = "\
* thread #1, name = 'main', stop reason = breakpoint 1.1\n\
    frame #0: 0x0000555555555190 broken`main at broken.cpp:10:5";
        let hit = LldbBackend.parse_hit(output).expect("should parse hit");
        assert_eq!(hit.file.as_deref(), Some("broken.cpp"));
        assert_eq!(hit.line, Some(10));
    }

    #[test]
    fn parse_hit_detects_stop_when_only_banner_no_frame() {
        // If stop reason is present but frame info is absent, parse_hit
        // should still return None (no location to record) — NOT panic.
        let output = "* thread #1, stop reason = breakpoint 1.1\n(lldb) ";
        // No frame line → location_key cannot be formed → None is correct.
        assert!(
            LldbBackend.parse_hit(output).is_none(),
            "without a frame line there is no location to record"
        );
    }

    #[test]
    fn gdb_is_a_registered_type() {
        // Regression: `dbg start gdb ./mybin` failed with "unknown type:
        // gdb". `gdb` must be an alias for the lldb backend so users
        // who reach for the familiar name don't get an opaque error.
        let b = LldbBackend;
        assert!(
            b.types().contains(&"gdb"),
            "`gdb` must be listed in LldbBackend.types(); got: {:?}",
            b.types()
        );
    }
}
