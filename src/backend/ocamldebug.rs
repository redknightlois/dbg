use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

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

    fn clean(&self, cmd: &str, output: &str) -> CleanResult {
        let trimmed = cmd.trim();
        let mut events = Vec::new();
        let mut lines: Vec<String> = Vec::new();

        for line in output.lines() {
            let l = line.trim();

            // Extract stop events (breakpoint hits, time positions)
            if l.starts_with("Time:") || l.starts_with("Time :") {
                events.push(l.to_string());
            }

            // Extract breakpoint hit events
            if l.starts_with("Breakpoint:") {
                events.push(l.to_string());
                continue; // redundant with Time: line
            }

            // Extract breakpoint-set confirmations
            if l.starts_with("Breakpoint ") && l.contains("at") {
                events.push(l.to_string());
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

        CleanResult {
            output: lines.join("\n"),
            events,
        }
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
        assert!(r.events.iter().any(|e| e.contains("Time:")));
        assert!(r.output.contains("val x"));
    }

    #[test]
    fn clean_filters_loading_noise() {
        let input = "Loading program ./my_program\nactual output";
        let r = OcamlDebugBackend.clean("run", input);
        assert!(!r.output.contains("Loading program"));
        assert!(r.output.contains("actual output"));
    }

    #[test]
    fn clean_passthrough_normal() {
        let input = "x : int = 42";
        let r = OcamlDebugBackend.clean("print x", input);
        assert_eq!(r.output.trim(), "x : int = 42");
        assert!(r.events.is_empty());
    }

    #[test]
    fn clean_replaces_markers() {
        let input = "2   <|b|>if n = 0 then 1";
        let r = OcamlDebugBackend.clean("step", input);
        assert!(r.output.contains(">>> if n = 0"));
        assert!(!r.output.contains("<|b|>"));
    }

    #[test]
    fn clean_filters_position_out_of_range() {
        let input = "1 let x = 42\nPosition out of range.";
        let r = OcamlDebugBackend.clean("list", input);
        assert!(!r.output.contains("Position out of range"));
        assert!(r.output.contains("let x = 42"));
    }

    #[test]
    fn clean_extracts_breakpoint_hit() {
        let input = "Time: 19 - pc: 0:144156 - module Test\nBreakpoint: 1\n2   <|b|>if n = 0 then 1";
        let r = OcamlDebugBackend.clean("run", input);
        assert!(r.events.iter().any(|e| e.contains("Breakpoint: 1")));
        assert!(!r.output.contains("Breakpoint:"));
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
