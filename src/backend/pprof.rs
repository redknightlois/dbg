use super::{Backend, CleanResult, Dependency, DependencyCheck, SpawnConfig};

pub struct PprofBackend;

impl Backend for PprofBackend {
    fn name(&self) -> &'static str {
        "pprof"
    }

    fn types(&self) -> &'static [&'static str] {
        &["pprof"]
    }

    fn spawn_config(&self, target: &str, _args: &[String]) -> anyhow::Result<SpawnConfig> {
        // target is a profile file path (cpu.prof, mem.prof, etc.)
        // Optionally prefixed with binary: "binary profile"
        let parts: Vec<&str> = target.splitn(2, ' ').collect();
        let args = if parts.len() == 2 {
            vec![parts[0].into(), parts[1].into()]
        } else {
            vec![target.into()]
        };

        Ok(SpawnConfig {
            bin: "go".into(),
            args: [vec!["tool".into(), "pprof".into()], args].concat(),
            env: vec![],
            init_commands: vec![],
        })
    }

    fn prompt_pattern(&self) -> &str {
        r"\(pprof\) "
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            name: "go",
            check: DependencyCheck::Binary {
                name: "go",
                alternatives: &["go"],
                version_cmd: None,
            },
            install: "https://go.dev/dl/",
        }]
    }

    fn format_breakpoint(&self, _spec: &str) -> String {
        String::new()
    }

    fn run_command(&self) -> &'static str {
        "top"
    }

    fn quit_command(&self) -> &'static str {
        "quit"
    }

    fn parse_help(&self, raw: &str) -> String {
        let mut cmds = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if let Some(tok) = line.split_whitespace().next() {
                if tok.chars().all(|c| c.is_ascii_alphabetic())
                    && tok.len() > 1
                    && tok.len() < 20
                {
                    cmds.push(tok.to_string());
                }
            }
        }
        cmds.dedup();
        format!("pprof: {}", cmds.join(", "))
    }

    fn clean(&self, _cmd: &str, output: &str) -> CleanResult {
        CleanResult {
            output: output.to_string(),
            events: vec![],
        }
    }

    fn adapters(&self) -> Vec<(&'static str, &'static str)> {
        vec![("pprof.md", include_str!("../../skills/adapters/pprof.md"))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_config_single_profile() {
        let cfg = PprofBackend.spawn_config("cpu.prof", &[]).unwrap();
        assert_eq!(cfg.args, vec!["tool", "pprof", "cpu.prof"]);
    }

    #[test]
    fn spawn_config_binary_and_profile() {
        let cfg = PprofBackend
            .spawn_config("./mybin cpu.prof", &[])
            .unwrap();
        assert_eq!(cfg.args, vec!["tool", "pprof", "./mybin", "cpu.prof"]);
    }

    #[test]
    fn format_breakpoint_empty() {
        assert_eq!(PprofBackend.format_breakpoint("anything"), "");
    }

    #[test]
    fn clean_passthrough() {
        let r = PprofBackend.clean("top", "some output");
        assert_eq!(r.output, "some output");
        assert!(r.events.is_empty());
    }
}
