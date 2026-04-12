use regex::Regex;

use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

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
        let mut cmds = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('-') || line.starts_with("command") {
                continue;
            }
            if let Some(tok) = line.split_whitespace().next() {
                if tok.chars().all(|c| c.is_ascii_alphabetic()) && tok.len() < 20 && seen.insert(tok.to_string()) {
                    cmds.push(tok.to_string());
                }
            }
        }
        format!("netcoredbg: {}", cmds.join(", "))
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("dotnet.md", include_str!("../../skills/adapters/dotnet.md"))]
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        let stop_re = Regex::new(r"reason: (.+?)(?:, thread|, stopped|$)").unwrap();
        let frame_re = Regex::new(r"frame=\{(.+?)\}").unwrap();

        let mut events = Vec::new();
        let mut lines = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.contains("^running") {
                continue;
            }
            // Emit lifecycle noise as events instead of dropping
            if trimmed.contains("library loaded:") || trimmed.contains("symbols loaded, base") {
                events.push(trimmed.to_string());
                continue;
            }
            if trimmed.contains("no symbols loaded") {
                events.push(trimmed.to_string());
                continue;
            }
            if trimmed.contains("thread created") || trimmed.contains("thread exited") {
                events.push(trimmed.to_string());
                continue;
            }
            if trimmed.contains("breakpoint modified") {
                events.push(trimmed.to_string());
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
        assert!(r.output.contains("stopped: breakpoint 1 hit"));
        assert!(r.output.contains("@ Program.Main() at Program.cs:4"));
    }

    #[test]
    fn clean_emits_library_events() {
        let input = "library loaded: System.dll, symbols loaded, base address: 0x1000\nthread created, id: 123\nbreakpoint modified, Breakpoint 1";
        let r = NetCoreDbgBackend.clean("run", input);
        assert!(r.output.is_empty());
        assert_eq!(r.events.len(), 3);
    }

    #[test]
    fn clean_skips_empty_and_running() {
        let input = "\n^running\nactual output";
        let r = NetCoreDbgBackend.clean("continue", input);
        assert_eq!(r.output, "actual output");
    }

    #[test]
    fn parse_help_filters_dashes_and_command() {
        let raw = "command list:\n-h  show help\nbreak  Set breakpoint\ncontinue  Resume";
        let result = NetCoreDbgBackend.parse_help(raw);
        assert!(result.contains("break"));
        assert!(result.contains("continue"));
        assert!(!result.contains("command"));
    }
}

fn detect_dotnet_root() -> Option<String> {
    let dotnet = which::which("dotnet").ok()?;
    let real = std::fs::canonicalize(dotnet).ok()?;
    // Homebrew: .../dotnet/10.0.103/bin/dotnet → libexec is sibling to bin
    let parent = real.parent()?;
    let libexec = parent.parent().map(|p| p.join("libexec"));
    if let Some(ref le) = libexec {
        if le.is_dir() {
            return le.to_str().map(|s| s.to_string());
        }
    }
    // Standard: dotnet binary is in the root
    parent.to_str().map(|s| s.to_string())
}
