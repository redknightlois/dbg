use regex::Regex;

use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct LldbBackend;

impl Backend for LldbBackend {
    fn name(&self) -> &'static str {
        "lldb"
    }

    fn types(&self) -> &'static [&'static str] {
        &["rust", "c", "cpp", "zig"]
    }

    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig> {
        let lldb_bin =
            std::env::var("LLDB_BIN").unwrap_or_else(|_| find_lldb().unwrap_or("lldb".into()));

        let mut init_commands = vec![format!("file {target}")];
        if !args.is_empty() {
            init_commands.push(format!("settings set target.run-args {}", args.join(" ")));
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

    fn clean(&self, cmd: &str, output: &str) -> CleanResult {
        let noise = [
            "Manually indexing DWARF",
            "Parsing symbol table",
            "Locating external symbol",
            "Reading binary from memory",
        ];

        let mut events = Vec::new();
        let mut lines = Vec::new();
        for line in output.lines() {
            if noise.iter().any(|n| line.contains(n)) {
                continue;
            }
            // Capture thread/process lifecycle as events
            if line.contains("Process") && line.contains("launched") {
                events.push(line.trim().to_string());
                continue;
            }
            if line.contains("Process") && line.contains("exited") {
                events.push(line.trim().to_string());
                continue;
            }
            lines.push(line);
        }
        let cleaned = lines.join("\n");

        let trimmed = cmd.trim();
        let output = if trimmed == "bt" || trimmed == "backtrace" {
            clean_bt(&cleaned)
        } else {
            cleaned
        };

        CleanResult { output, events }
    }
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
        assert_eq!(r.output, "actual output");
        assert!(r.events.is_empty());
    }

    #[test]
    fn clean_extracts_process_events() {
        let b = LldbBackend;
        let input = "Process 1234 launched: '/bin/test'\nsome output\nProcess 1234 exited with status = 0";
        let r = b.clean("continue", input);
        assert_eq!(r.output, "some output");
        assert_eq!(r.events.len(), 2);
        assert!(r.events[0].contains("launched"));
        assert!(r.events[1].contains("exited"));
    }

    #[test]
    fn clean_bt_reformats_frames() {
        let input = "* thread #1, name = 'test', stop reason = breakpoint 1.1\n    frame #0: 0x00005555 test`main + 12 at main.c:4\n    frame #1: 0x00007fff libc`__libc_start_main + 128 at start.c:100";
        let r = LldbBackend.clean("bt", input);
        assert!(r.output.contains("frame #0: main at main.c:4"));
        assert!(r.output.contains("frame #1: __libc_start_main at start.c:100"));
        assert!(r.output.contains("* thread"));
    }

    #[test]
    fn clean_bt_passthrough_on_no_frames() {
        let r = LldbBackend.clean("bt", "no frames here");
        assert_eq!(r.output, "no frames here");
    }

    #[test]
    fn spawn_config_with_args() {
        let b = LldbBackend;
        let cfg = b
            .spawn_config("./test", &["arg1".into(), "arg2".into()])
            .unwrap();
        assert_eq!(cfg.init_commands.len(), 2);
        assert_eq!(cfg.init_commands[0], "file ./test");
        assert!(cfg.init_commands[1].contains("arg1 arg2"));
    }

    #[test]
    fn spawn_config_no_args() {
        let cfg = LldbBackend.spawn_config("./test", &[]).unwrap();
        assert_eq!(cfg.init_commands.len(), 1);
        assert_eq!(cfg.init_commands[0], "file ./test");
    }

    #[test]
    fn parse_help_extracts_commands() {
        let raw = "  breakpoint -- Set a breakpoint\n  continue   -- Continue execution\nSome other line";
        let result = LldbBackend.parse_help(raw);
        assert!(result.contains("breakpoint"));
        assert!(result.contains("continue"));
        assert!(!result.contains("Some other"));
    }
}
