use std::process::Command;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::canonical::{BreakId, BreakLoc, CanonicalOps, HitEvent, unsupported};
use super::{Backend, Dependency, DependencyCheck, SpawnConfig};

pub struct PdbBackend {
    /// pdb stops at line 1 of the module before any user command runs.
    /// That synthetic stop must be skipped, but only the first one —
    /// a user breakpoint at line 1 during a later `continue` is a real
    /// hit. Track whether we've consumed the synthetic stop.
    module_load_consumed: std::sync::atomic::AtomicBool,
}

impl PdbBackend {
    pub const fn new() -> Self {
        Self {
            module_load_consumed: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl Default for PdbBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// If `file` exists and line `line` is a `def`/`class`/decorator line
/// that can't hold a bytecode breakpoint in pdb, advance to the first
/// statement inside the function body. Returns `None` when the file
/// isn't readable or the line looks fine as-is.
fn advance_to_body_line(file: &str, line: u32) -> Option<u32> {
    let text = std::fs::read_to_string(file).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    let idx = (line as usize).saturating_sub(1);
    if idx >= lines.len() {
        return None;
    }
    let trimmed = lines[idx].trim_start();
    let is_header = trimmed.starts_with("def ")
        || trimmed.starts_with("async def ")
        || trimmed.starts_with("class ")
        || trimmed.starts_with('@'); // decorator line
    if !is_header {
        return None;
    }
    // Determine the header's indent, then walk forward until we find a
    // non-blank, non-comment line at a strictly deeper indent — that's
    // the first body statement.
    let header_indent = lines[idx].chars().take_while(|c| c.is_whitespace()).count();
    // Also skip past continuation lines (multi-line `def` signatures):
    // keep advancing until we've passed the line that ends with `:` at
    // the header indent level.
    let mut i = idx;
    loop {
        if i >= lines.len() {
            return None;
        }
        let cur = lines[i].trim_end();
        let cur_indent = lines[i].chars().take_while(|c| c.is_whitespace()).count();
        if cur_indent == header_indent && cur.ends_with(':') {
            i += 1;
            break;
        }
        i += 1;
    }
    while i < lines.len() {
        let cur = lines[i].trim_start();
        let cur_indent = lines[i].chars().take_while(|c| c.is_whitespace()).count();
        if cur.is_empty() || cur.starts_with('#') {
            i += 1;
            continue;
        }
        if cur_indent > header_indent {
            return Some((i + 1) as u32);
        }
        break;
    }
    None
}

impl Backend for PdbBackend {
    fn name(&self) -> &'static str {
        "pdb"
    }

    fn description(&self) -> &'static str {
        "Python debugger"
    }

    fn types(&self) -> &'static [&'static str] {
        &["python", "py"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let python = std::env::var("PYTHON").unwrap_or_else(|_| "python3".into());
        let mut spawn_args = vec!["-u".into(), "-m".into(), "pdb".into(), target.into()];
        spawn_args.extend(args.iter().cloned());

        Ok(SpawnConfig {
            bin: python,
            args: spawn_args,
            env: vec![("PYTHONDONTWRITEBYTECODE".into(), "1".into())],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"\(Pdb\) "
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "python3",
            check: DependencyCheck::Binary {
                name: "python3",
                alternatives: &["python3"],
                version_cmd: None,
            },
            install: "sudo apt install python3  # or: brew install python",
        }]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        format!("break {spec}")
    }

    fn run_command(&self) -> &'static str {
        "continue"
    }

    fn parse_help(&self, raw: &str) -> String {
        let mut cmds: Vec<String> = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty()
                || line.starts_with('=')
                || line.starts_with("Documented")
                || line.starts_with("Undocumented")
                || line.starts_with("Miscellaneous")
            {
                continue;
            }
            for tok in line.split_whitespace() {
                if tok.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && tok.len() < 20
                    && !tok.is_empty()
                {
                    cmds.push(tok.to_string());
                }
            }
        }
        cmds.sort();
        cmds.dedup();
        format!("pdb: {}", cmds.join(", "))
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("python.md", include_str!("../../skills/adapters/python.md"))]
    }

    fn clean(&self, cmd: &str, output: &str) -> String {
        let trimmed = cmd.trim();
        if trimmed == "where" || trimmed == "bt" {
            output
                .lines()
                .filter(|l| !l.contains("bdb.py") && !l.contains("<string>(1)"))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            output.to_string()
        }
    }

    fn canonical_ops(&self) -> Option<&dyn CanonicalOps> {
        Some(self)
    }
}

/// Remove the internal `exec(cmd, globals, locals)` dispatcher frame
/// that pdb's `where` emits at the bottom of every stack. pdb prints
/// frames as two-line pairs:
///
///   ```text
///     /path/file.py(42)func()
///   -> some_line_of_source
///   ```
///
/// When the daemon drives pdb it ends up executing user commands via
/// Python's `exec(...)`, which leaves a frame like
/// `  <string>(1)<module>()` with `-> exec(cmd, globals, locals)` at
/// the very top of the walk. That frame is noise to the agent and
/// steals the slot where the real top frame belongs.
pub(crate) fn filter_pdb_where(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let mut keep = vec![true; lines.len()];
    for i in 0..lines.len() {
        let cur = lines[i].trim_start();
        if cur.starts_with("-> exec(cmd, globals, locals)") {
            keep[i] = false;
            // The `->` line is always preceded by its frame header;
            // drop that too so we don't leave an orphaned path line.
            if i > 0 {
                keep[i - 1] = false;
            }
        }
    }
    lines
        .iter()
        .zip(keep.iter())
        .filter(|(_, k)| **k)
        .map(|(l, _)| *l)
        .collect::<Vec<_>>()
        .join("\n")
}

impl CanonicalOps for PdbBackend {
    fn tool_name(&self) -> &'static str { "pdb" }

    fn postprocess_output(&self, canonical_op: &str, out: &str) -> String {
        match canonical_op {
            "stack" => filter_pdb_where(out),
            _ => out.to_string(),
        }
    }

    fn tool_version(&self) -> Option<String> {
        static V: OnceLock<Option<String>> = OnceLock::new();
        V.get_or_init(|| {
            let python = std::env::var("PYTHON").unwrap_or_else(|_| "python3".into());
            let out = Command::new(&python).arg("--version").output().ok()?;
            let s = String::from_utf8_lossy(&out.stdout);
            let s = if s.trim().is_empty() {
                String::from_utf8_lossy(&out.stderr).to_string()
            } else {
                s.to_string()
            };
            // The `[via pdb <ver>]` header already names pdb via
            // `tool_name`, so the version string shouldn't repeat it.
            s.lines().next().map(|l| l.trim().to_string())
        })
        .clone()
    }

    fn op_break(&self, loc: &BreakLoc) -> anyhow::Result<String> {
        Ok(match loc {
            BreakLoc::FileLine { file, line } => {
                // pdb silently accepts a `break` on a `def`/`class`
                // header line but the trap never fires — the
                // compiler emits no bytecode for the `def` line
                // itself. Bump to the first executable body line
                // when we detect this, so the breakpoint actually
                // triggers. If we can't read the file (relative
                // path, etc.) we fall through unchanged.
                let line = advance_to_body_line(file, *line).unwrap_or(*line);
                format!("break {file}:{line}")
            }
            BreakLoc::Fqn(name) => format!("break {name}"),
            BreakLoc::ModuleMethod { module, method } => {
                // pdb accepts `module:function` to break by symbol within a module.
                format!("break {module}:{method}")
            }
        })
    }

    fn op_unbreak(&self, id: BreakId) -> anyhow::Result<String> {
        Ok(format!("clear {}", id.0))
    }

    fn op_breaks(&self) -> anyhow::Result<String> { Ok("break".into()) }

    fn op_run(&self, _args: &[String]) -> anyhow::Result<String> {
        // pdb launches the script on daemon start and pauses at the
        // module's first line. `continue` runs to the first breakpoint
        // — the expected behaviour for the `--run` flag.
        Ok("continue".into())
    }
    fn op_continue(&self) -> anyhow::Result<String> { Ok("continue".into()) }
    fn op_step(&self) -> anyhow::Result<String> { Ok("step".into()) }
    fn op_next(&self) -> anyhow::Result<String> { Ok("next".into()) }
    fn op_finish(&self) -> anyhow::Result<String> { Ok("return".into()) }

    fn op_stack(&self, _n: Option<u32>) -> anyhow::Result<String> {
        // pdb `where` has no count arg — full stack always.
        Ok("where".into())
    }
    fn op_frame(&self, n: u32) -> anyhow::Result<String> {
        // Requires Python 3.8+; earlier pdb versions only have up/down.
        Ok(format!("frame {n}"))
    }
    fn op_locals(&self) -> anyhow::Result<String> {
        // pdb has no dedicated "locals" command; pretty-print the
        // `locals()` builtin which yields a Python dict.
        Ok("pp locals()".into())
    }
    fn op_print(&self, expr: &str) -> anyhow::Result<String> {
        Ok(format!("p {expr}"))
    }
    fn op_watch(&self, _expr: &str) -> anyhow::Result<String> {
        Err(unsupported(self.tool_name(), "watchpoints"))
    }
    fn op_threads(&self) -> anyhow::Result<String> {
        Err(unsupported(
            self.tool_name(),
            "thread listing (stock pdb is single-threaded)",
        ))
    }
    fn op_thread(&self, _n: u32) -> anyhow::Result<String> {
        Err(unsupported(
            self.tool_name(),
            "thread switching (stock pdb is single-threaded)",
        ))
    }
    fn op_list(&self, loc: Option<&str>) -> anyhow::Result<String> {
        Ok(match loc {
            Some(s) => format!("list {s}"),
            None => "list".into(),
        })
    }

    /// pdb prints a `> <file>(<line>)<func>()` marker whenever execution
    /// stops. That single line is the authoritative stop signal.
    fn parse_hit(&self, output: &str) -> Option<HitEvent> {
        let re = stop_regex();
        for line in output.lines() {
            if let Some(c) = re.captures(line) {
                let file = c[1].to_string();
                let line_no: u32 = c[2].parse().ok()?;
                let func = c[3].to_string();
                // Skip synthetic stops at module load — pdb pauses at
                // line 1 of the module before any breakpoint is reached.
                // That initial stop is not a real breakpoint hit.
                // Bug 2 fix: only skip the very first line (line_no == 1);
                // a user breakpoint at module-level line N > 1 IS a real
                // hit and must be captured.
                // The synthetic module-load stop fires once at line 1
                // before any user command runs. Skip it the first time
                // we see it; on subsequent line-1 <module> stops the
                // user has set a real breakpoint there and the hit
                // must be captured.
                if func == "<module>" && line_no == 1 {
                    use std::sync::atomic::Ordering;
                    if !self.module_load_consumed.swap(true, Ordering::Relaxed) {
                        continue;
                    }
                }
                return Some(HitEvent {
                    location_key: format!("{file}:{line_no}"),
                    thread: None,
                    frame_symbol: Some(func),
                    file: Some(file),
                    line: Some(line_no),
                });
            }
        }
        None
    }

    /// Parse the output of `pp locals()` — a Python dict literal —
    /// into a JSON object of `name -> {value: "<repr>"}`. A full Python
    /// expression parser is out of scope; we walk the dict body
    /// respecting bracket/quote depth so nested collections stay with
    /// their key instead of leaking into the next pair.
    fn parse_locals(&self, output: &str) -> Option<Value> {
        let text = output.trim();
        let inner = text
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .unwrap_or(text);
        let mut obj = Map::new();
        for pair in split_top_level_commas(inner) {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            // Find the first top-level ':' — that's the key/value boundary.
            let colon = match find_top_level_colon(pair) {
                Some(i) => i,
                None => continue,
            };
            let key = pair[..colon].trim();
            let val = pair[colon + 1..].trim();
            // Strip the surrounding quotes from the key.
            let key = key.trim_matches(|c| c == '\'' || c == '"');
            if key.is_empty() {
                continue;
            }
            // Bug 1 fix: at module-level, `locals()` returns the entire
            // module namespace — hundreds of dunder entries (`__name__`,
            // `__builtins__`, `__file__`, …) plus every imported name.
            // These are noise for an agent inspecting user variables.
            // Filter out any key that starts and ends with `__`.
            if key.starts_with("__") && key.ends_with("__") {
                continue;
            }
            let mut entry = Map::new();
            entry.insert("value".into(), Value::String(val.to_string()));
            obj.insert(key.to_string(), Value::Object(entry));
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
        Regex::new(r"^>\s+(\S+?)\((\d+)\)([A-Za-z_<][A-Za-z0-9_<>]*)").unwrap()
    })
}

/// Split on commas at bracket-depth zero, respecting `'`/`"` quotes.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str: Option<char> = None;
    let mut last = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if let Some(q) = in_str {
            if c == '\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if c == q {
                in_str = None;
            }
        } else {
            match c {
                '\'' | '"' => in_str = Some(c),
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                ',' if depth == 0 => {
                    out.push(&s[last..i]);
                    last = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }
    if last < s.len() {
        out.push(&s[last..]);
    }
    out
}

fn find_top_level_colon(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str: Option<char> = None;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if let Some(q) = in_str {
            if c == '\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if c == q {
                in_str = None;
            }
        } else {
            match c {
                '\'' | '"' => in_str = Some(c),
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                ':' if depth == 0 => return Some(i),
                _ => {}
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_breakpoint() {
        assert_eq!(PdbBackend::new().format_breakpoint("test.py:10"), "break test.py:10");
    }

    #[test]
    fn filter_pdb_where_strips_exec_dispatcher_frame() {
        // Regression: pdb's `where` walk includes the daemon's
        // `exec(cmd, globals, locals)` frame at the bottom of every
        // stack. That frame is internal plumbing — reporting it as
        // the top of the user's stack misleads agents about what is
        // actually running.
        let raw = "  <string>(1)<module>()\n\
                   -> exec(cmd, globals, locals)\n\
                   > /tmp/broken.py(25)fetch_page()\n\
                   -> start = (page - 1) * per_page\n\
                   > /tmp/broken.py(40)<module>()\n\
                   -> print(list(paginate(items, 3, 3)))\n";
        let got = filter_pdb_where(raw);
        assert!(
            !got.contains("exec(cmd, globals, locals)"),
            "exec dispatcher frame leaked:\n{got}"
        );
        assert!(
            !got.contains("<string>(1)<module>()"),
            "orphaned `<string>(1)` frame header not dropped:\n{got}"
        );
        // The real user frames must still be present.
        assert!(got.contains("broken.py(25)fetch_page()"));
        assert!(got.contains("start = (page - 1) * per_page"));
        assert!(got.contains("broken.py(40)<module>()"));
    }

    #[test]
    fn filter_pdb_where_preserves_real_stacks() {
        // Real user stacks must pass through untouched.
        let raw = "  /a/b.py(10)run()\n\
                   -> step()\n\
                   > /a/b.py(22)step()\n\
                   -> x = 1\n";
        assert_eq!(filter_pdb_where(raw), raw.trim_end());
    }

    #[test]
    fn postprocess_output_only_fires_on_stack_op() {
        // Regression guard: the filter must be scoped to the stack op.
        // Other ops ship their own formatting (locals, print, …) and
        // must not be touched even if an `exec(...)` line coincidentally
        // appears in them (e.g. a printed local whose repr contains it).
        let weird_locals_dump =
            "{'code': '-> exec(cmd, globals, locals)', 'n': 3}";
        let got = PdbBackend::new().postprocess_output("locals", weird_locals_dump);
        assert_eq!(got, weird_locals_dump);
    }

    #[test]
    fn advance_def_line_to_first_body_line() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            "def foo(\n    a,\n    b,\n):\n    # first body line is a comment\n    return a + b\n",
        )
        .unwrap();
        // Line 1 is `def foo(` — advance to line 6 (`return a + b`).
        let p = tmp.path().to_str().unwrap();
        assert_eq!(advance_to_body_line(p, 1), Some(6));
    }

    #[test]
    fn advance_decorator_line() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            "@cache\ndef foo():\n    return 1\n",
        )
        .unwrap();
        let p = tmp.path().to_str().unwrap();
        assert_eq!(advance_to_body_line(p, 1), Some(3));
    }

    #[test]
    fn advance_noop_on_body_line() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            "def foo():\n    return 1\n",
        )
        .unwrap();
        // Already a body line — no advance.
        let p = tmp.path().to_str().unwrap();
        assert_eq!(advance_to_body_line(p, 2), None);
    }

    #[test]
    fn clean_where_filters_bdb() {
        let input = "> script.py(5)main()\n  bdb.py(123)dispatch()\n> script.py(10)<module>()\n  <string>(1)<module>()";
        let r = PdbBackend::new().clean("where", input);
        assert!(!r.contains("bdb.py"));
        assert!(!r.contains("<string>(1)"));
        assert!(r.contains("script.py(5)"));
        assert!(r.contains("script.py(10)"));
    }

    #[test]
    fn clean_other_cmd_passthrough() {
        let input = "bdb.py line should stay";
        let r = PdbBackend::new().clean("p x", input);
        assert!(r.contains("bdb.py"));
    }

    #[test]
    fn spawn_config_includes_target_and_args() {
        let cfg = PdbBackend::new()
            .spawn_config("test.py", &["--verbose".into()])
            .unwrap();
        assert!(cfg.args.contains(&"test.py".to_string()));
        assert!(cfg.args.contains(&"--verbose".to_string()));
        assert!(cfg.args.contains(&"-m".to_string()));
    }

    #[test]
    fn parse_help_extracts_and_deduplicates() {
        let raw = "Documented commands:\n========\nbreak  continue  help\nbreak  next  step";
        let result = PdbBackend::new().parse_help(raw);
        assert!(result.contains("break"));
        assert!(result.contains("continue"));
        let count = result.matches("break").count();
        assert_eq!(count, 1);
    }

    // --------------------------------------------------------------
    // CanonicalOps
    // --------------------------------------------------------------

    #[test]
    fn canonical_break_ops() {
        let ops: &dyn CanonicalOps = &PdbBackend::new();
        assert_eq!(
            ops.op_break(&BreakLoc::FileLine { file: "app.py".into(), line: 10 }).unwrap(),
            "break app.py:10"
        );
        assert_eq!(
            ops.op_break(&BreakLoc::Fqn("main".into())).unwrap(),
            "break main"
        );
        assert_eq!(
            ops.op_break(&BreakLoc::ModuleMethod { module: "app".into(), method: "main".into() }).unwrap(),
            "break app:main"
        );
    }

    #[test]
    fn canonical_exec_ops() {
        let ops: &dyn CanonicalOps = &PdbBackend::new();
        assert_eq!(ops.op_continue().unwrap(), "continue");
        assert_eq!(ops.op_step().unwrap(), "step");
        assert_eq!(ops.op_next().unwrap(), "next");
        assert_eq!(ops.op_finish().unwrap(), "return");
    }

    #[test]
    fn canonical_locals_uses_pp_locals_builtin() {
        let ops: &dyn CanonicalOps = &PdbBackend::new();
        assert_eq!(ops.op_locals().unwrap(), "pp locals()");
    }

    #[test]
    fn canonical_watch_is_unsupported() {
        let ops: &dyn CanonicalOps = &PdbBackend::new();
        let err = ops.op_watch("x").unwrap_err().to_string();
        assert!(err.contains("pdb"));
        assert!(err.contains("watchpoints"));
        assert!(err.contains("dbg raw"));
    }

    #[test]
    fn canonical_threads_are_unsupported() {
        let ops: &dyn CanonicalOps = &PdbBackend::new();
        assert!(ops.op_threads().is_err());
        assert!(ops.op_thread(1).is_err());
    }

    #[test]
    fn parse_hit_from_stop_marker() {
        let out = "> /app/main.py(42)handle_request()\n-> return result\n(Pdb) ";
        let hit = PdbBackend::new().parse_hit(out).expect("should parse");
        assert_eq!(hit.location_key, "/app/main.py:42");
        assert_eq!(hit.file.as_deref(), Some("/app/main.py"));
        assert_eq!(hit.line, Some(42));
        assert_eq!(hit.frame_symbol.as_deref(), Some("handle_request"));
    }

    #[test]
    fn parse_hit_none_without_marker() {
        assert!(PdbBackend::new().parse_hit("some output without marker").is_none());
    }

    #[test]
    fn parse_hit_skips_module_load_stop() {
        // pdb stops at the very first line of the module (line 1) before any
        // breakpoint fires. That synthetic stop must be skipped.
        let out = "> /app/main.py(1)<module>()\n-> \"\"\"Algorithms.\"\"\"\n(Pdb) ";
        assert!(PdbBackend::new().parse_hit(out).is_none(), "should skip <module> stop at line 1");
    }

    #[test]
    fn parse_locals_from_pp_dict() {
        let out = "{'x': 42, 'name': 'hello', 'items': [1, 2, 3]}";
        let v = PdbBackend::new().parse_locals(out).expect("should parse");
        let obj = v.as_object().unwrap();
        assert_eq!(obj.get("x").unwrap().get("value").unwrap().as_str().unwrap(), "42");
        assert_eq!(
            obj.get("name").unwrap().get("value").unwrap().as_str().unwrap(),
            "'hello'"
        );
        assert_eq!(
            obj.get("items").unwrap().get("value").unwrap().as_str().unwrap(),
            "[1, 2, 3]"
        );
    }

    #[test]
    fn parse_locals_handles_nested_dicts() {
        let out = "{'cfg': {'host': 'localhost', 'port': 8080}, 'ready': True}";
        let v = PdbBackend::new().parse_locals(out).expect("should parse");
        let obj = v.as_object().unwrap();
        assert_eq!(
            obj.get("cfg").unwrap().get("value").unwrap().as_str().unwrap(),
            "{'host': 'localhost', 'port': 8080}"
        );
        assert_eq!(obj.get("ready").unwrap().get("value").unwrap().as_str().unwrap(), "True");
    }

    #[test]
    fn parse_locals_handles_commas_inside_strings() {
        let out = "{'greeting': 'hello, world', 'n': 3}";
        let v = PdbBackend::new().parse_locals(out).expect("should parse");
        let obj = v.as_object().unwrap();
        assert_eq!(
            obj.get("greeting").unwrap().get("value").unwrap().as_str().unwrap(),
            "'hello, world'"
        );
        assert_eq!(obj.get("n").unwrap().get("value").unwrap().as_str().unwrap(), "3");
    }

    #[test]
    fn backend_canonical_ops_hook_returns_self() {
        let b: Box<dyn Backend> = Box::new(PdbBackend::new());
        assert!(b.canonical_ops().is_some());
        assert_eq!(b.canonical_ops().unwrap().tool_name(), "pdb");
    }

    // ----------------------------------------------------------------
    // Bug 1: parse_locals at module-level must filter dunder keys
    // ----------------------------------------------------------------

    /// Regression: `dbg locals` at a `<module>` stop (before any user
    /// breakpoint fires) returns `locals()` which is the module's global
    /// namespace — hundreds of dunder keys like `__builtins__`, `__file__`,
    /// `__spec__` plus every builtin function. `parse_locals` must discard
    /// keys that start and end with `__` so agents only see user variables.
    #[test]
    fn parse_locals_filters_dunder_keys() {
        let out = "{'__name__': '__main__', '__file__': 'broken.py', \
                   '__builtins__': <module 'builtins'>, 'x': 42, 'items': [1, 2, 3]}";
        let v = PdbBackend::new().parse_locals(out).expect("should parse");
        let obj = v.as_object().unwrap();
        // Dunder keys must be absent.
        assert!(
            !obj.contains_key("__name__"),
            "__name__ must be filtered out, got keys: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
        assert!(
            !obj.contains_key("__file__"),
            "__file__ must be filtered out"
        );
        assert!(
            !obj.contains_key("__builtins__"),
            "__builtins__ must be filtered out"
        );
        // User variables must remain.
        assert!(obj.contains_key("x"), "x must be present");
        assert!(obj.contains_key("items"), "items must be present");
    }

    /// When ALL keys are dunders (pure module-level namespace), `parse_locals`
    /// should return `None` (nothing useful to show) rather than an empty dict.
    #[test]
    fn parse_locals_all_dunders_returns_none() {
        let out = "{'__name__': '__main__', '__doc__': None, '__builtins__': <module 'builtins'>}";
        let v = PdbBackend::new().parse_locals(out);
        assert!(
            v.is_none(),
            "all-dunder dict should yield None, got: {v:?}"
        );
    }

    // ----------------------------------------------------------------
    // Bug 2: parse_hit must NOT skip <module> stops beyond line 1
    // ----------------------------------------------------------------

    /// Regression: when a user sets a breakpoint at a module-level line
    /// (not inside a function), pdb reports `> file.py(26)<module>()`.
    /// The old code skipped ALL `<module>` stops so this hit was silently
    /// dropped and never recorded. Only the synthetic line-1 stop should
    /// be ignored.
    #[test]
    fn parse_hit_captures_module_level_breakpoint_beyond_line_1() {
        // Breakpoint at line 26 in module scope — must be captured.
        let out = "> /app/broken.py(26)<module>()\n-> result = compute()\n(Pdb) ";
        let hit = PdbBackend::new().parse_hit(out);
        assert!(
            hit.is_some(),
            "module-level breakpoint hit at line 26 must be captured, got None"
        );
        let h = hit.unwrap();
        assert_eq!(h.line, Some(26));
        assert_eq!(h.file.as_deref(), Some("/app/broken.py"));
    }

    /// The synthetic module-load stop at line 1 must still be skipped.
    #[test]
    fn parse_hit_still_skips_synthetic_module_load_at_line_1() {
        let out = "> /app/broken.py(1)<module>()\n-> \"\"\"Module docstring.\"\"\"\n(Pdb) ";
        assert!(
            PdbBackend::new().parse_hit(out).is_none(),
            "line-1 <module> stop is synthetic and must be skipped"
        );
    }

    /// Regression: a user breakpoint at line 1 of a module produces
    /// the same `> file.py(1)<module>()` marker as the synthetic
    /// module-load stop. The first line-1 stop is the synthetic one;
    /// any subsequent line-1 stop is the user's breakpoint and must
    /// be captured rather than silently swallowed.
    #[test]
    fn parse_hit_captures_user_breakpoint_at_module_line_1() {
        let backend = PdbBackend::new();
        let out = "> /app/broken.py(1)<module>()\n-> import sys\n(Pdb) ";
        // First call: synthetic module-load stop, must be skipped.
        assert!(
            backend.parse_hit(out).is_none(),
            "first line-1 <module> stop should be skipped"
        );
        // Second call (after user `continue` returns to line 1): the
        // user has a breakpoint at line 1 that just fired. Capture it.
        let hit = backend.parse_hit(out);
        assert!(
            hit.is_some(),
            "second line-1 <module> stop should be a real user-breakpoint hit"
        );
        let h = hit.unwrap();
        assert_eq!(h.line, Some(1));
        assert_eq!(h.location_key, "/app/broken.py:1");
    }
}
