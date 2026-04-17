use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::canonical::{BreakLoc, CanonicalOps, HitEvent};
use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct JdbBackend;

impl Backend for JdbBackend {
    fn name(&self) -> &'static str {
        "jdb"
    }

    fn description(&self) -> &'static str {
        "Java/Kotlin debugger"
    }

    fn types(&self) -> &'static [&'static str] {
        &["java", "kotlin"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let mut spawn_args = vec![target.into()];
        spawn_args.extend(args.iter().cloned());

        Ok(SpawnConfig {
            bin: "jdb".into(),
            args: spawn_args,
            env: vec![],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"(\n> \z|\n\w+\[\d+\] \z)"
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "jdb",
            check: DependencyCheck::Binary {
                name: "jdb",
                alternatives: &["jdb"],
                version_cmd: None,
            },
            install: "sudo apt install default-jdk  # or: brew install openjdk",
        }]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        format!("stop at {spec}")
    }

    fn run_command(&self) -> &'static str {
        "run"
    }

    fn parse_help(&self, raw: &str) -> String {
        let mut cmds = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if let Some(tok) = line.split_whitespace().next() {
                if tok.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
                    && tok.len() < 20
                    && !tok.is_empty()
                    && tok.len() > 1
                {
                    cmds.push(tok.to_string());
                }
            }
        }
        cmds.dedup();
        format!("jdb: {}", cmds.join(", "))
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        let mut events = Vec::new();
        let mut lines = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("Set breakpoint") || trimmed.starts_with("Deferring breakpoint") {
                events.push(trimmed.to_string());
                continue;
            }
            if trimmed.contains("thread") && (trimmed.contains("started") || trimmed.contains("died")) {
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
        vec![("java.md", include_str!("../../skills/adapters/java.md"))]
    }

    fn canonical_ops(&self) -> Option<&dyn CanonicalOps> { Some(self) }
}

impl CanonicalOps for JdbBackend {
    fn tool_name(&self) -> &'static str { "jdb" }
    fn auto_capture_locals(&self) -> bool { false }

    fn op_break(&self, loc: &BreakLoc) -> anyhow::Result<String> {
        Ok(match loc {
            BreakLoc::FileLine { file, line } => {
                // jdb expects `stop at <ClassName>:<line>`, not a file path.
                // Strip the directory and `.java` extension to get the class name.
                let class = std::path::Path::new(file)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(file);
                format!("stop at {class}:{line}")
            }
            BreakLoc::Fqn(name) => format!("stop at {name}"),
            BreakLoc::ModuleMethod { module, method } => format!("stop at {module}.{method}"),
        })
    }
    fn op_run(&self, _args: &[String]) -> anyhow::Result<String> { Ok("run".into()) }
    fn op_continue(&self) -> anyhow::Result<String> { Ok("cont".into()) }
    fn op_step(&self) -> anyhow::Result<String> { Ok("step".into()) }
    fn op_next(&self) -> anyhow::Result<String> { Ok("next".into()) }
    fn op_finish(&self) -> anyhow::Result<String> { Ok("step up".into()) }
    fn op_stack(&self, _n: Option<u32>) -> anyhow::Result<String> { Ok("where".into()) }
    fn op_frame(&self, n: u32) -> anyhow::Result<String> { Ok(format!("up {n}")) }
    fn op_locals(&self) -> anyhow::Result<String> { Ok("locals".into()) }
    fn op_print(&self, expr: &str) -> anyhow::Result<String> { Ok(format!("print {expr}")) }
    fn op_threads(&self) -> anyhow::Result<String> { Ok("threads".into()) }
    fn op_thread(&self, n: u32) -> anyhow::Result<String> { Ok(format!("thread {n}")) }
    fn op_list(&self, _loc: Option<&str>) -> anyhow::Result<String> { Ok("list".into()) }
    fn op_breaks(&self) -> anyhow::Result<String> {
        // jdb has no dedicated "breakpoint list" verb; the default
        // `breakpoint` sent by the canonical trait yields
        // "Unrecognized command: 'breakpoint'". jdb's `clear` with no
        // argument is the documented way to print currently-set
        // breakpoints ("Current breakpoints set: …").
        Ok("clear".into())
    }

    fn parse_hit(&self, output: &str) -> Option<HitEvent> {
        // jdb stop banners come in several shapes; we match them all:
        //
        //   Breakpoint hit: "thread=main", Algos.fibonacci(), line=15 bci=0
        //   Breakpoint hit: "thread=main", Broken.merge(int[], int, int), line=35 bci=0
        //   Breakpoint hit: "thread=main", Broken$Inner.run(), line=42 bci=0
        //   Step completed: "thread=main", Algos.fibonacci(), line=16 bci=5
        //
        // The earlier version required `\(\)` (empty parens), so line
        // breakpoints in methods with parameters like `merge(int[], int, int)`
        // never matched and the hit was silently dropped. The fix: allow
        // anything inside the parens (non-greedy), and use `[^\s,()]+`
        // for the fully-qualified class.method so nested-class names
        // (`Outer$Inner.method`) and generic-less Kotlin names work too.
        static BP_RE: OnceLock<Regex> = OnceLock::new();
        let bp_re = BP_RE.get_or_init(|| {
            Regex::new(
                r#"Breakpoint hit:.*?"thread=([^"]+)".*?([A-Za-z_$][\w$.]*\.[A-Za-z_$][\w$]*)\([^)]*\)[^,]*,\s*line=(\d+)"#,
            )
            .unwrap()
        });
        static STEP_RE: OnceLock<Regex> = OnceLock::new();
        let step_re = STEP_RE.get_or_init(|| {
            Regex::new(
                r#"Step completed:.*?"thread=([^"]+)".*?([A-Za-z_$][\w$.]*\.[A-Za-z_$][\w$]*)\([^)]*\)[^,]*,\s*line=(\d+)"#,
            )
            .unwrap()
        });

        let parse_with = |re: &Regex| -> Option<HitEvent> {
            for line in output.lines() {
                if let Some(c) = re.captures(line) {
                    let thread = c[1].to_string();
                    let symbol = c[2].to_string();
                    let line_no: u32 = c[3].parse().ok()?;
                    // Use the OUTER class name (strip nested $Inner and
                    // method suffix) as the location stem so
                    // `dbg hits Broken.java:35` matches via
                    // `stem_line_key` → `Broken:35`.
                    let class_part = symbol.rsplit_once('.').map(|x| x.0).unwrap_or(&symbol);
                    let outer_class = class_part.rsplit_once('$').map(|x| x.0).unwrap_or(class_part);
                    // Further strip package prefix so `com.foo.Broken` →
                    // `Broken` for the key.
                    let short = outer_class.rsplit_once('.').map(|x| x.1).unwrap_or(outer_class);
                    return Some(HitEvent {
                        location_key: format!("{short}:{line_no}"),
                        thread: Some(thread),
                        frame_symbol: Some(symbol),
                        file: None,
                        line: Some(line_no),
                    });
                }
            }
            None
        };
        parse_with(bp_re).or_else(|| parse_with(step_re))
    }

    fn parse_locals(&self, output: &str) -> Option<Value> {
        // jdb `locals`: `name = value` lines, sometimes with type prefix.
        let mut obj = Map::new();
        for line in output.lines() {
            let line = line.trim();
            if let Some((name, val)) = line.split_once(" = ") {
                let name = name.trim().to_string();
                if name.is_empty() || name.contains(' ') { continue; }
                let mut entry = Map::new();
                entry.insert("value".into(), Value::String(val.trim().to_string()));
                obj.insert(name, Value::Object(entry));
            }
        }
        if obj.is_empty() { None } else { Some(Value::Object(obj)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_breakpoint() {
        assert_eq!(JdbBackend.format_breakpoint("Main:10"), "stop at Main:10");
    }

    #[test]
    fn clean_extracts_breakpoint_events() {
        let input = "Set breakpoint at Main:10\nnormal output\nDeferring breakpoint Main:20";
        let r = JdbBackend.clean("stop at Main:10", input);
        assert_eq!(r.output, "normal output");
        assert_eq!(r.events.len(), 2);
    }

    #[test]
    fn clean_extracts_thread_events() {
        let input = "thread \"main\" started\noutput\nthread \"worker\" died";
        let r = JdbBackend.clean("run", input);
        assert_eq!(r.output, "output");
        assert_eq!(r.events.len(), 2);
    }

    #[test]
    fn parse_hit_breakpoint_banner() {
        let raw = "> \nBreakpoint hit: \"thread=main\", Algos.fibonacci(), line=17 bci=13\n17                long next = a + b;\n\nmain[1] ";
        let hit = JdbBackend.parse_hit(raw);
        assert!(hit.is_some(), "parse_hit should match jdb breakpoint banner");
        let hit = hit.unwrap();
        assert_eq!(hit.thread.as_deref(), Some("main"));
        assert_eq!(hit.frame_symbol.as_deref(), Some("Algos.fibonacci"));
        assert_eq!(hit.line, Some(17));
    }

    #[test]
    fn parse_hit_line_breakpoint_with_args() {
        // Line breakpoints in methods with parameters used to return
        // None — the regex required empty `()`. This is the exact
        // banner the jdb docs show for such a break.
        let raw = "Breakpoint hit: \"thread=main\", Broken.merge(int[], int, int), line=35 bci=12\n";
        let hit = JdbBackend.parse_hit(raw).expect("should match parameterized method");
        assert_eq!(hit.line, Some(35));
        assert_eq!(hit.location_key, "Broken:35");
        assert_eq!(hit.frame_symbol.as_deref(), Some("Broken.merge"));
    }

    #[test]
    fn parse_hit_nested_class() {
        let raw = "Breakpoint hit: \"thread=main\", com.x.Outer$Inner.run(), line=42 bci=0";
        let hit = JdbBackend.parse_hit(raw).expect("nested class");
        assert_eq!(hit.line, Some(42));
        // Outer class, package stripped, $Inner stripped.
        assert_eq!(hit.location_key, "Outer:42");
    }

    #[test]
    fn parse_locals_simple() {
        let output = "a = 0\nb = 1\nnext = 1\ni = 0";
        let v = JdbBackend.parse_locals(output).expect("should parse");
        assert_eq!(v.as_object().unwrap().get("a").unwrap().get("value").unwrap().as_str().unwrap(), "0");
    }

    #[test]
    fn parse_help_allows_hyphens() {
        let raw = "stop-in  Set breakpoint\ncont     Continue execution\nx single-char excluded";
        let result = JdbBackend.parse_help(raw);
        assert!(result.contains("stop-in"));
        assert!(result.contains("cont"));
        // single-char "x" excluded (len <= 1)
        assert!(!result.contains(", x,"));
    }
}
