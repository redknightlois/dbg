use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::canonical::{BreakLoc, CanonicalOps, HitEvent};
use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct PhpdbgBackend;

impl Backend for PhpdbgBackend {
    fn name(&self) -> &'static str {
        "phpdbg"
    }

    fn description(&self) -> &'static str {
        "PHP debugger"
    }

    fn types(&self) -> &'static [&'static str] {
        &["php"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let mut spawn_args = vec!["-q".into(), "-e".into(), target.into()];
        spawn_args.extend(args.iter().cloned());

        Ok(SpawnConfig {
            bin: "phpdbg".into(),
            args: spawn_args,
            env: vec![],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"prompt>"
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "phpdbg",
            check: DependencyCheck::Binary {
                name: "phpdbg",
                alternatives: &["phpdbg"],
                version_cmd: None,
            },
            install: "sudo apt install php  # phpdbg is bundled with PHP since 5.6",
        }]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        format!("break {spec}")
    }

    fn run_command(&self) -> &'static str {
        "run"
    }

    fn quit_command(&self) -> &'static str {
        "quit"
    }

    fn parse_help(&self, raw: &str) -> String {
        let mut cmds: Vec<String> = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("phpdbg") || line.starts_with("To") {
                continue;
            }
            // phpdbg help lists commands as "  command   alias  description"
            if let Some(first) = line.split_whitespace().next() {
                if first.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && first.len() < 20
                    && first.len() > 1
                {
                    cmds.push(first.to_string());
                }
            }
        }
        cmds.sort();
        cmds.dedup();
        format!("phpdbg: {}", cmds.join(", "))
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("php.md", include_str!("../../skills/adapters/php.md"))]
    }

    fn clean(&self, cmd: &str, output: &str) -> CleanResult {
        let trimmed = cmd.trim();
        let mut events = Vec::new();
        let mut lines = Vec::new();

        for line in output.lines() {
            let l = line.trim();
            // Extract lifecycle events
            if l.starts_with("[Welcome to phpdbg") || l.starts_with("[Successful compilation") {
                events.push(l.to_string());
                continue;
            }
            // Extract stop/breakpoint hit events
            if l.starts_with("[Breakpoint") || l.starts_with("[Break") {
                events.push(l.to_string());
            }
            // Filter internal noise from backtraces
            if (trimmed == "back" || trimmed == "t") && l.contains("phpdbg_exec") {
                continue;
            }
            lines.push(line);
        }

        CleanResult {
            output: lines.join("\n"),
            events,
        }
    }

    fn canonical_ops(&self) -> Option<&dyn CanonicalOps> { Some(self) }
}

impl CanonicalOps for PhpdbgBackend {
    fn tool_name(&self) -> &'static str { "phpdbg" }
    fn auto_capture_locals(&self) -> bool { false }

    fn op_break(&self, loc: &BreakLoc) -> anyhow::Result<String> {
        Ok(match loc {
            BreakLoc::FileLine { file, line } => format!("break {file}:{line}"),
            BreakLoc::Fqn(name) => format!("break {name}"),
            BreakLoc::ModuleMethod { module, method } => format!("break {module}::{method}"),
        })
    }
    fn op_run(&self, _args: &[String]) -> anyhow::Result<String> { Ok("run".into()) }
    fn op_continue(&self) -> anyhow::Result<String> { Ok("continue".into()) }
    fn op_step(&self) -> anyhow::Result<String> { Ok("step".into()) }
    fn op_next(&self) -> anyhow::Result<String> { Ok("next".into()) }
    fn op_finish(&self) -> anyhow::Result<String> { Ok("finish".into()) }
    fn op_stack(&self, _n: Option<u32>) -> anyhow::Result<String> { Ok("back".into()) }
    fn op_frame(&self, n: u32) -> anyhow::Result<String> { Ok(format!("frame {n}")) }
    fn op_locals(&self) -> anyhow::Result<String> {
        // phpdbg's `info vars` lists names only; the canonical way to
        // recover both names and values in one round-trip is
        // `ev get_defined_vars()`, which prints a `var_dump`-style
        // nested array we parse below. The historical concern that
        // this "can hang the session" no longer holds — phpdbg 7.4+
        // returns immediately and our parse-locals regex handles the
        // `["name"]=> type(value)` layout that var_dump emits.
        Ok("ev var_dump(get_defined_vars())".into())
    }
    fn op_print(&self, expr: &str) -> anyhow::Result<String> {
        // phpdbg's `ev` eval runs its argument as PHP, so a bare
        // identifier like `a` resolves as a constant ("Undefined
        // constant 'a'") rather than the variable the user meant.
        // Rewrite bare identifiers to their `$name` form; leave
        // anything more complex (already-sigiled, operators, call
        // syntax, member access, ...) alone so agents stay in control.
        let trimmed = expr.trim();
        let is_bare_ident = !trimmed.is_empty()
            && !trimmed.starts_with('$')
            && trimmed
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if is_bare_ident {
            Ok(format!("ev ${trimmed}"))
        } else {
            Ok(format!("ev {expr}"))
        }
    }
    fn op_breaks(&self) -> anyhow::Result<String> {
        // phpdbg lists active breakpoints via `info break` (alias `i b`);
        // the default trait emits `breakpoint list`, which phpdbg rejects
        // with "command 'breakpoint' could not be found".
        Ok("info break".into())
    }
    fn op_list(&self, _loc: Option<&str>) -> anyhow::Result<String> { Ok("list".into()) }

    fn parse_hit(&self, output: &str) -> Option<HitEvent> {
        // phpdbg raw: `[Breakpoint #0 at /path/algos.php:19, hits: 1]`
        // or `[Break at /path/algos.php:19]`
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| {
            Regex::new(r"\[Break(?:point #\d+)? at (\S+):(\d+)").unwrap()
        });
        for line in output.lines() {
            if let Some(c) = re.captures(line) {
                let file = c[1].to_string();
                let line_no: u32 = c[2].parse().ok()?;
                return Some(HitEvent {
                    location_key: format!("{file}:{line_no}"),
                    thread: None,
                    frame_symbol: None,
                    file: Some(file),
                    line: Some(line_no),
                });
            }
        }
        None
    }

    fn parse_locals(&self, output: &str) -> Option<Value> {
        // `ev get_defined_vars()` produces PHP array output like:
        //   array(4) {
        //     ["n"]=> int(10)
        //     ["a"]=> int(0)
        //     ["b"]=> int(1)
        //     ["next"]=> int(1)
        //   }
        // Parse `["name"]=> type(value)` lines.
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| {
            Regex::new(r#"\["(\w+)"\]\s*=>\s*(.+)"#).unwrap()
        });
        let mut obj = Map::new();
        for line in output.lines() {
            if let Some(c) = re.captures(line) {
                let name = c[1].to_string();
                let val = c[2].trim().to_string();
                // Extract the inner value from type(value) → just value
                let clean_val = if let Some(inner) = val.strip_suffix(')') {
                    inner.rsplit_once('(').map(|(_, v)| v).unwrap_or(inner)
                } else {
                    &val
                };
                let mut entry = Map::new();
                entry.insert("value".into(), Value::String(clean_val.to_string()));
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
        assert_eq!(PhpdbgBackend.format_breakpoint("test.php:10"), "break test.php:10");
        assert_eq!(PhpdbgBackend.format_breakpoint("my_function"), "break my_function");
    }

    #[test]
    fn clean_extracts_breakpoint_events() {
        let input = "[Breakpoint #0 at test.php:10]\nsome output\nmore output";
        let r = PhpdbgBackend.clean("run", input);
        assert!(r.events.iter().any(|e| e.contains("Breakpoint #0")));
        assert!(r.output.contains("some output"));
    }

    #[test]
    fn clean_filters_phpdbg_exec_from_backtrace() {
        let input = "frame #0: test.php:10\nframe #1: phpdbg_exec stuff\nframe #2: test.php:5";
        let r = PhpdbgBackend.clean("back", input);
        assert!(!r.output.contains("phpdbg_exec"));
        assert!(r.output.contains("frame #0"));
        assert!(r.output.contains("frame #2"));
    }

    #[test]
    fn clean_welcome_as_event() {
        let input = "[Welcome to phpdbg, the interactive PHP debugger]\nready";
        let r = PhpdbgBackend.clean("", input);
        assert!(r.events.iter().any(|e| e.contains("Welcome")));
        assert!(r.output.contains("ready"));
    }

    #[test]
    fn clean_passthrough_normal_commands() {
        let input = "$x = 42";
        let r = PhpdbgBackend.clean("ev $x", input);
        assert_eq!(r.output, "$x = 42");
        assert!(r.events.is_empty());
    }

    #[test]
    fn spawn_config_includes_target_and_args() {
        let cfg = PhpdbgBackend
            .spawn_config("test.php", &["--verbose".into()])
            .unwrap();
        assert_eq!(cfg.bin, "phpdbg");
        assert!(cfg.args.contains(&"-q".to_string()));
        assert!(cfg.args.contains(&"-e".to_string()));
        assert!(cfg.args.contains(&"test.php".to_string()));
        assert!(cfg.args.contains(&"--verbose".to_string()));
    }

    #[test]
    fn parse_help_extracts_commands() {
        let raw = "phpdbg help\nTo get help...\n  exec     e   set execution context\n  run      r   attempt execution\n  step     s   step through\n  break    b   set breakpoint";
        let result = PhpdbgBackend.parse_help(raw);
        assert!(result.contains("exec"));
        assert!(result.contains("run"));
        assert!(result.contains("step"));
        assert!(result.contains("break"));
    }
}
