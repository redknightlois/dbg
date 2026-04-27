use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::canonical::{BreakLoc, CanonicalOps, HitEvent};
use super::{Backend, Dependency, DependencyCheck, SpawnConfig};

pub struct OcamlDebugBackend;

impl Backend for OcamlDebugBackend {
    fn name(&self) -> &'static str {
        "ocamldebug"
    }

    fn description(&self) -> &'static str {
        "OCaml bytecode debugger"
    }

    fn types(&self) -> &'static [&'static str] {
        &["ocaml", "ml"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let mut spawn_args = vec![target.into()];
        if !args.is_empty() {
            spawn_args.extend(args.iter().cloned());
        }

        Ok(SpawnConfig {
            bin: "ocamldebug".into(),
            args: spawn_args,
            env: vec![],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"\(ocd\) "
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "ocamldebug",
            check: DependencyCheck::Binary {
                name: "ocamldebug",
                alternatives: &["ocamldebug"],
                version_cmd: Some(("ocaml", &["-version"])),
            },
            install: "opam install ocaml  # or: sudo apt install ocaml-interp",
        }]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        // Support "Module line" -> "break @ Module line"
        // and bare "line" -> "break @ line"
        // and "functionName" -> "break functionName"
        let trimmed = spec.trim();
        if trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            // Bare line number
            format!("break @ {trimmed}")
        } else if trimmed.contains(' ') {
            // "Module line" or "Module line col"
            format!("break @ {trimmed}")
        } else if trimmed.contains(':') {
            // "Module:line" convenience syntax -> "Module line"
            let parts: Vec<&str> = trimmed.splitn(2, ':').collect();
            format!("break @ {} {}", parts[0], parts[1])
        } else {
            // Function name
            format!("break {trimmed}")
        }
    }

    fn run_command(&self) -> &'static str {
        "run"
    }

    fn quit_command(&self) -> &'static str {
        "quit"
    }

    fn parse_help(&self, raw: &str) -> String {
        // ocamldebug help is a flat "List of commands:" followed by space-separated words
        let mut cmds: Vec<String> = Vec::new();
        let text = raw
            .strip_prefix("List of commands:")
            .unwrap_or(raw);
        for tok in text.split_whitespace() {
            if tok.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && tok.len() > 1
                && tok.len() < 25
            {
                cmds.push(tok.to_string());
            }
        }
        cmds.sort();
        cmds.dedup();
        format!("ocamldebug: {}", cmds.join(", "))
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("ocaml.md", include_str!("../../skills/adapters/ocaml.md"))]
    }

    fn canonical_ops(&self) -> Option<&dyn CanonicalOps> { Some(self) }

    fn clean(&self, cmd: &str, output: &str) -> String {
        let trimmed = cmd.trim();
        let mut lines: Vec<String> = Vec::new();

        for line in output.lines() {
            let l = line.trim();

            // Drop redundant breakpoint-hit lines (the "Time:" line that
            // follows already names the location).
            if l.starts_with("Breakpoint:") {
                continue;
            }

            // Filter loading/startup noise
            if l.starts_with("Loading program")
                || l.starts_with("Waiting for connection")
                || l == "Position out of range."
            {
                continue;
            }

            // For backtrace, filter internal frames
            if (trimmed == "bt" || trimmed == "backtrace")
                && (l.contains("Debugger") || l.contains("ocamldebug"))
            {
                continue;
            }

            // Clean <|b|> markers (active expression indicators) -> visible marker
            let cleaned = line.replace("<|b|>", ">>> ").replace("<|e|>", "");
            lines.push(cleaned);
        }

        lines.join("\n")
    }
}

/// Extract an OCaml module name from a file path.
/// `"/path/to/algos.ml"` → `"Algos"` (capitalised stem).
fn module_from_path(path: &str) -> String {
    let stem = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(path);
    let mut chars = stem.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => stem.to_string(),
    }
}

impl CanonicalOps for OcamlDebugBackend {
    fn tool_name(&self) -> &'static str { "ocamldebug" }
    fn auto_capture_locals(&self) -> bool { false }

    fn op_break(&self, loc: &BreakLoc) -> anyhow::Result<String> {
        Ok(match loc {
            BreakLoc::FileLine { file, line } => {
                let module = module_from_path(file);
                format!("break @ {module} {line}")
            }
            BreakLoc::Fqn(name) => format!("break {name}"),
            BreakLoc::ModuleMethod { module, method: _ } => format!("break @ {module}"),
        })
    }
    fn op_run(&self, _args: &[String]) -> anyhow::Result<String> { Ok("run".into()) }
    fn op_continue(&self) -> anyhow::Result<String> { Ok("run".into()) }
    fn op_step(&self) -> anyhow::Result<String> { Ok("step".into()) }
    fn op_next(&self) -> anyhow::Result<String> { Ok("next".into()) }
    fn op_finish(&self) -> anyhow::Result<String> { Ok("finish".into()) }
    fn op_stack(&self, n: Option<u32>) -> anyhow::Result<String> {
        Ok(match n {
            Some(k) => format!("bt {k}"),
            None => "bt".into(),
        })
    }
    fn op_frame(&self, n: u32) -> anyhow::Result<String> { Ok(format!("frame {n}")) }
    fn op_locals(&self) -> anyhow::Result<String> {
        // ocamldebug genuinely has no "enumerate local bindings" command:
        // `info` has no `locals`/`variables` subcommand, and `print` with
        // no argument just reprints the last value ("print" alone returns
        // blank on a fresh stop). The closest native signal is `frame`,
        // which reports the current function symbol and source line; we
        // emit that here as a best-effort "where am I" reply so agents
        // aren't left with an empty string. For real value inspection
        // agents must fall back to `dbg print <name>` per binding —
        // that's how ocamldebug is designed to be driven.
        Ok("frame".into())
    }
    fn op_print(&self, expr: &str) -> anyhow::Result<String> { Ok(format!("print {expr}")) }
    fn op_list(&self, _loc: Option<&str>) -> anyhow::Result<String> { Ok("list".into()) }
    fn op_breaks(&self) -> anyhow::Result<String> {
        // ocamldebug's breakpoint list lives under `info breakpoints`;
        // the default `breakpoint list` that the canonical trait emits
        // is parsed as a module/function name and returns "Unknown
        // command." for the second token.
        Ok("info breakpoints".into())
    }

    fn parse_hit(&self, output: &str) -> Option<HitEvent> {
        // ocamldebug stop output:
        //   Time: 19 - pc: 0:144156 - module Algos
        //   Breakpoint: 1
        //   16   let next = !a + !b in    (* ← fibonacci hot line *)
        //
        // We match the "Time: N - pc: ... - module M" line to extract
        // the module, then look for an indented source line "NN  ..." to
        // get the line number.
        static TIME_RE: OnceLock<Regex> = OnceLock::new();
        static SRC_RE: OnceLock<Regex> = OnceLock::new();
        let time_re = TIME_RE.get_or_init(|| {
            Regex::new(r"Time:\s*\d+\s*-\s*pc:\s*[\d:]+\s*-\s*module\s+(\S+)").unwrap()
        });
        let src_re = SRC_RE.get_or_init(|| {
            Regex::new(r"^\s*(\d+)\s+").unwrap()
        });

        let mut module = None;
        let mut line_no = None;
        for line in output.lines() {
            if module.is_none() {
                if let Some(c) = time_re.captures(line) {
                    module = Some(c[1].to_string());
                }
            } else if line_no.is_none() {
                // After the Time line, look for a source line number
                // (skip "Breakpoint: N" lines).
                let trimmed = line.trim();
                if trimmed.starts_with("Breakpoint:") {
                    continue;
                }
                if let Some(c) = src_re.captures(trimmed) {
                    line_no = c[1].parse::<u32>().ok();
                }
            }
        }

        let m = module?;
        let ln = line_no?;
        // Use lowercase_module.ml:line as the location_key so the
        // stem matcher (strips extension → "algos:16") can fuzzy-match
        // against the absolute path the agent passes to `dbg hits`.
        let file = format!("{}.ml", m.to_lowercase());
        Some(HitEvent {
            location_key: format!("{file}:{ln}"),
            thread: None,
            frame_symbol: Some(m),
            file: Some(file),
            line: Some(ln),
        })
    }

    fn parse_locals(&self, output: &str) -> Option<Value> {
        // ocamldebug `print` shows the most recent value:
        //   x : int = 42
        // Best-effort: single name = value pair.
        let mut obj = Map::new();
        for line in output.lines() {
            let line = line.trim();
            if let Some(eq_pos) = line.find('=') {
                let left = line[..eq_pos].trim();
                let value = line[eq_pos + 1..].trim().to_string();
                // left is "name : type", extract just the name
                let name = left.split(':').next().unwrap_or(left).trim();
                if !name.is_empty() {
                    obj.insert(
                        name.to_string(),
                        serde_json::json!({ "value": value }),
                    );
                }
            }
        }
        if obj.is_empty() { None } else { Some(Value::Object(obj)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_breakpoint_line() {
        assert_eq!(OcamlDebugBackend.format_breakpoint("42"), "break @ 42");
    }

    #[test]
    fn format_breakpoint_module_line() {
        assert_eq!(
            OcamlDebugBackend.format_breakpoint("Parser 42"),
            "break @ Parser 42"
        );
    }

    #[test]
    fn format_breakpoint_module_colon_line() {
        assert_eq!(
            OcamlDebugBackend.format_breakpoint("Parser:42"),
            "break @ Parser 42"
        );
    }

    #[test]
    fn format_breakpoint_function() {
        assert_eq!(
            OcamlDebugBackend.format_breakpoint("parse_expr"),
            "break parse_expr"
        );
    }

    #[test]
    fn clean_extracts_time_events() {
        let input = "Time: 21 - pc: 0:42756 - module Parser\nval x : int = 42";
        let r = OcamlDebugBackend.clean("step", input);
        assert!(r.contains("val x"));
    }

    #[test]
    fn clean_filters_loading_noise() {
        let input = "Loading program ./my_program\nactual output";
        let r = OcamlDebugBackend.clean("run", input);
        assert!(!r.contains("Loading program"));
        assert!(r.contains("actual output"));
    }

    #[test]
    fn clean_passthrough_normal() {
        let input = "x : int = 42";
        let r = OcamlDebugBackend.clean("print x", input);
        assert_eq!(r.trim(), "x : int = 42");
    }

    #[test]
    fn clean_replaces_markers() {
        let input = "2   <|b|>if n = 0 then 1";
        let r = OcamlDebugBackend.clean("step", input);
        assert!(r.contains(">>> if n = 0"));
        assert!(!r.contains("<|b|>"));
    }

    #[test]
    fn clean_filters_position_out_of_range() {
        let input = "1 let x = 42\nPosition out of range.";
        let r = OcamlDebugBackend.clean("list", input);
        assert!(!r.contains("Position out of range"));
        assert!(r.contains("let x = 42"));
    }

    #[test]
    fn clean_extracts_breakpoint_hit() {
        let input = "Time: 19 - pc: 0:144156 - module Test\nBreakpoint: 1\n2   <|b|>if n = 0 then 1";
        let r = OcamlDebugBackend.clean("run", input);
        assert!(!r.contains("Breakpoint:"));
    }

    #[test]
    fn spawn_config_basic() {
        let cfg = OcamlDebugBackend
            .spawn_config("./my_program", &[])
            .unwrap();
        assert_eq!(cfg.bin, "ocamldebug");
        assert!(cfg.args.contains(&"./my_program".to_string()));
    }

    #[test]
    fn spawn_config_with_args() {
        let cfg = OcamlDebugBackend
            .spawn_config("./my_program", &["arg1".into(), "arg2".into()])
            .unwrap();
        assert!(cfg.args.contains(&"arg1".to_string()));
        assert!(cfg.args.contains(&"arg2".to_string()));
    }

    #[test]
    fn prompt_pattern_matches() {
        let re = regex::Regex::new(OcamlDebugBackend.prompt_pattern()).unwrap();
        assert!(re.is_match("(ocd) "));
    }

    #[test]
    fn parse_hit_from_time_line() {
        let out = "Time: 19 - pc: 0:144156 - module Algos\nBreakpoint: 1\n16   let next = !a + !b in    (* ← fibonacci hot line *)";
        let hit = OcamlDebugBackend.parse_hit(out).expect("should parse");
        assert_eq!(hit.location_key, "algos.ml:16");
        assert_eq!(hit.line, Some(16));
        assert_eq!(hit.frame_symbol.as_deref(), Some("Algos"));
    }

    #[test]
    fn parse_hit_none_on_program_exit() {
        let out = "Time: 705\nProgram exit.";
        assert!(OcamlDebugBackend.parse_hit(out).is_none());
    }

    #[test]
    fn module_from_path_capitalises_stem() {
        assert_eq!(module_from_path("/path/to/algos.ml"), "Algos");
        assert_eq!(module_from_path("parser.ml"), "Parser");
    }

    #[test]
    fn canonical_ops_returns_self() {
        let b: Box<dyn Backend> = Box::new(OcamlDebugBackend);
        assert_eq!(b.canonical_ops().unwrap().tool_name(), "ocamldebug");
    }

    #[test]
    fn parse_help_extracts_commands() {
        let raw = "List of commands: cd complete pwd directory kill pid address help quit shell\nenvironment run reverse step backstep goto finish next start previous print\ndisplay source break delete set show info frame backtrace bt up down last\nlist load_printer install_printer remove_printer";
        let result = OcamlDebugBackend.parse_help(raw);
        assert!(result.contains("backtrace"));
        assert!(result.contains("break"));
        assert!(result.contains("backstep"));
        assert!(result.contains("reverse"));
        assert!(result.contains("run"));
        assert!(result.contains("goto"));
        assert!(result.contains("print"));
    }
}
