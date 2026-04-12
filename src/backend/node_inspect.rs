use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct NodeInspectBackend;

impl Backend for NodeInspectBackend {
    fn name(&self) -> &'static str {
        "node-inspect"
    }

    fn description(&self) -> &'static str {
        "Node.js / Bun / Deno debugger"
    }

    fn types(&self) -> &'static [&'static str] {
        &["node", "nodejs", "js", "javascript", "ts", "typescript", "bun", "deno"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let mut spawn_args = vec!["inspect".into(), target.into()];
        spawn_args.extend(args.iter().cloned());

        Ok(SpawnConfig {
            bin: "node".into(),
            args: spawn_args,
            env: vec![],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"debug> "
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "node",
            check: DependencyCheck::Binary {
                name: "node",
                alternatives: &["node"],
                version_cmd: Some(("node", &["--version"])),
            },
            install: "https://nodejs.org  # or: nvm install --lts",
        }]
    }

    fn format_breakpoint(&self, spec: &str) -> String {
        // node inspect uses: sb('file.js', line) or sb(line)
        if let Some((file, line)) = spec.rsplit_once(':') {
            format!("sb('{file}', {line})")
        } else if spec.chars().all(|c| c.is_ascii_digit()) {
            format!("sb({spec})")
        } else {
            // Function name — set breakpoint on function entry
            format!("sb('{spec}')")
        }
    }

    fn run_command(&self) -> &'static str {
        "cont"
    }

    fn quit_command(&self) -> &'static str {
        ".exit"
    }

    fn help_command(&self) -> &'static str {
        "help"
    }

    fn parse_help(&self, raw: &str) -> String {
        let mut cmds = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // node inspect help lines are like "cont, c    Resume execution"
            if let Some(left) = line.split("  ").next() {
                for tok in left.split(", ") {
                    let tok = tok.trim();
                    if !tok.is_empty()
                        && tok.len() < 20
                        && tok.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
                    {
                        cmds.push(tok.to_string());
                    }
                }
            }
        }
        cmds.sort();
        cmds.dedup();
        format!("node-inspect: {}", cmds.join(", "))
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("javascript.md", include_str!("../../skills/adapters/javascript.md"))]
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        let mut events = Vec::new();
        let mut lines = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            // Filter debugger connection noise
            if trimmed.starts_with("< Debugger listening on ws://")
                || trimmed.starts_with("< For help, see:")
                || trimmed.starts_with("connecting to ")
                || trimmed == "< Debugger attached."
                || trimmed == "< "
                || trimmed == "ok"
            {
                continue;
            }
            // Extract breakpoint events
            if trimmed.contains("Breakpoint") || trimmed.starts_with("break in ") {
                events.push(trimmed.to_string());
            }
            // Extract exception events
            if trimmed.starts_with("< Uncaught") || trimmed.starts_with("< Error") {
                events.push(trimmed.trim_start_matches("< ").to_string());
            }
            // Strip "< " prefix from debugger output lines
            if let Some(rest) = trimmed.strip_prefix("< ") {
                lines.push(rest);
            } else {
                lines.push(line);
            }
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
    fn format_breakpoint_file_line() {
        assert_eq!(
            NodeInspectBackend.format_breakpoint("app.js:10"),
            "sb('app.js', 10)"
        );
    }

    #[test]
    fn format_breakpoint_line_only() {
        assert_eq!(NodeInspectBackend.format_breakpoint("42"), "sb(42)");
    }

    #[test]
    fn format_breakpoint_function() {
        assert_eq!(
            NodeInspectBackend.format_breakpoint("handleRequest"),
            "sb('handleRequest')"
        );
    }

    #[test]
    fn clean_filters_connection_noise() {
        let input = "< Debugger listening on ws://127.0.0.1:9229/abc\n< For help, see: https://nodejs.org\n< \nconnecting to 127.0.0.1:9229 ... ok\n< Debugger attached.\n< \nBreak on start in app.js:1\n> 1 const x = 1;";
        let r = NodeInspectBackend.clean("", input);
        assert!(!r.output.contains("Debugger listening"));
        assert!(!r.output.contains("connecting to"));
        assert!(r.output.contains("const x = 1"));
    }

    #[test]
    fn clean_extracts_breakpoint_events() {
        let input = "break in app.js:10\n> 10 console.log(x)";
        let r = NodeInspectBackend.clean("cont", input);
        assert_eq!(r.events.len(), 1);
        assert!(r.events[0].contains("break in"));
    }

    #[test]
    fn clean_extracts_exception_events() {
        let input = "< Uncaught ReferenceError: x is not defined\n< at app.js:5";
        let r = NodeInspectBackend.clean("cont", input);
        assert!(r.events.iter().any(|e| e.contains("Uncaught")));
    }

    #[test]
    fn spawn_config_basic() {
        let cfg = NodeInspectBackend
            .spawn_config("app.js", &[])
            .unwrap();
        assert_eq!(cfg.bin, "node");
        assert_eq!(cfg.args, vec!["inspect", "app.js"]);
    }

    #[test]
    fn spawn_config_with_args() {
        let cfg = NodeInspectBackend
            .spawn_config("server.js", &["--port".into(), "3000".into()])
            .unwrap();
        assert_eq!(cfg.args[0], "inspect");
        assert_eq!(cfg.args[1], "server.js");
        assert_eq!(cfg.args[2], "--port");
        assert_eq!(cfg.args[3], "3000");
    }
}
