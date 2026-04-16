pub mod canonical;
pub mod delve;
pub mod delve_proto;
pub mod dotnettrace;
pub mod callgrind;
pub mod ghci;
pub mod ghcprof;
pub mod jdb;
pub mod jitdasm;
pub mod lldb;
pub mod netcoredbg;
pub mod massif;
pub mod memcheck;
pub mod node_inspect;
pub mod node_proto;
pub mod nodeprof;
pub mod ocamldebug;
pub mod perf;
pub mod pdb;
pub mod phpdbg;
pub mod pprof;
pub mod pstats;
pub mod rdbg;
pub mod stackprof;
pub mod xdebug;
use std::collections::HashMap;

// Re-export dependency types from shared crate for backwards compatibility
pub use dbg_cli::deps::{Dependency, DependencyCheck, DepStatus};

pub use canonical::{BreakId, BreakLoc, CanonicalOps};

/// Result of cleaning debugger output.
pub struct CleanResult {
    pub output: String,
    pub events: Vec<String>,
}

/// Configuration for spawning a debugger process.
pub struct SpawnConfig {
    pub bin: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Commands to run after spawn before the session is ready.
    pub init_commands: Vec<String>,
}

/// The backend trait. Implement this for each debugger.
pub trait Backend: Send + Sync {
    /// Human-readable name for this backend.
    fn name(&self) -> &'static str;

    /// Short description shown in backend listing.
    fn description(&self) -> &'static str;

    /// The type names this backend handles.
    fn types(&self) -> &'static [&'static str];

    /// How to spawn the debugger for the given target.
    fn spawn_config(&self, target: &str, args: &[String]) -> anyhow::Result<SpawnConfig>;

    /// Regex matching the debugger's ready-for-input prompt.
    fn prompt_pattern(&self) -> &str;

    /// Dependencies this backend requires.
    fn dependencies(&self) -> Vec<Dependency>;

    /// Runtime preflight check — called before fork/spawn. Use this
    /// for conditions that aren't "is this binary installed?" but still
    /// need to hold for the backend to work: kernel settings, perf
    /// paranoia level, capabilities, writable cwd, etc. Default: ok.
    fn preflight(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Format a breakpoint spec for this debugger.
    /// Profiler backends that don't support breakpoints return empty by default.
    fn format_breakpoint(&self, _spec: &str) -> String {
        String::new()
    }

    /// The command to start/continue execution.
    fn run_command(&self) -> &'static str;

    /// The command to quit the debugger.
    fn quit_command(&self) -> &'static str {
        "quit"
    }

    /// The command to request help from the debugger.
    fn help_command(&self) -> &'static str {
        "help"
    }

    /// Adapter markdown files for AI skill integration.
    fn adapters(&self) -> Vec<(&'static str, &'static str)>;

    /// Parse raw help output into compact command list.
    fn parse_help(&self, raw: &str) -> String;

    /// Path to a Speedscope JSON file produced after init commands complete.
    fn profile_output(&self) -> Option<String> {
        None
    }

    /// Clean noise from command output.
    fn clean(&self, cmd: &str, output: &str) -> CleanResult {
        let _ = cmd;
        CleanResult {
            output: output.to_string(),
            events: vec![],
        }
    }

    /// Canonical-operations hook. Debug backends that implement the
    /// `CanonicalOps` trait should override this to return `Some(self)`.
    /// Profiler backends and backends not yet ported return `None`; the
    /// canonical dispatcher will surface a clear "canonical ops not
    /// available for <tool>" to the agent in that case.
    fn canonical_ops(&self) -> Option<&dyn CanonicalOps> {
        None
    }

    /// Override to route through the V8 Inspector transport instead
    /// of the default PTY transport. Only `node-proto` returns true
    /// today; future protocol backends (DAP) add their own hooks.
    fn uses_inspector(&self) -> bool {
        false
    }

    /// Override to route through the DAP transport. When true, the
    /// backend must provide `dap_launch` to describe how to spawn
    /// the adapter and configure `launch`. The daemon calls this
    /// instead of `spawn_config`.
    fn uses_dap(&self) -> bool {
        false
    }

    /// Adapter spawn + launch configuration for DAP backends. Only
    /// invoked when `uses_dap()` returns true.
    fn dap_launch(&self, _target: &str, _args: &[String]) -> anyhow::Result<crate::dap::DapLaunchConfig> {
        anyhow::bail!("dap_launch not implemented for this backend")
    }
}

/// Path to the current dbg binary, for exec-ing into sub-REPLs.
pub fn self_exe() -> String {
    std::env::current_exe()
        .unwrap_or_else(|_| "dbg".into())
        .display()
        .to_string()
}

/// Escape a string for safe interpolation into a bash command.
/// Wraps in single quotes and escapes embedded single quotes.
pub fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    // If the string is simple (no special chars), return as-is
    if s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'/' || b == b'-' || b == b'_' || b == b'=' || b == b':') {
        return s.to_string();
    }
    // Wrap in single quotes, escaping existing single quotes as '\''
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Registry of all available backends.
pub struct Registry {
    backends: Vec<Box<dyn Backend>>,
    type_map: HashMap<String, usize>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            backends: Vec::new(),
            type_map: HashMap::new(),
        }
    }

    pub fn register(&mut self, backend: Box<dyn Backend>) {
        let idx = self.backends.len();
        self.type_map.insert(backend.name().to_string(), idx);
        for t in backend.types() {
            self.type_map.insert(t.to_string(), idx);
        }
        self.backends.push(backend);
    }

    pub fn get(&self, type_name: &str) -> Option<&dyn Backend> {
        self.type_map
            .get(type_name)
            .map(|&idx| self.backends[idx].as_ref())
    }

    pub fn available_types(&self) -> Vec<&str> {
        let mut types: Vec<&str> = self.type_map.keys().map(|s| s.as_str()).collect();
        types.sort();
        types
    }

    pub fn all_backends(&self) -> &[Box<dyn Backend>] {
        &self.backends
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_escape_simple_passthrough() {
        assert_eq!(shell_escape("./myapp"), "./myapp");
        assert_eq!(shell_escape("target/debug/foo"), "target/debug/foo");
        assert_eq!(shell_escape("a-b_c.d"), "a-b_c.d");
    }

    #[test]
    fn shell_escape_empty() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn shell_escape_spaces() {
        assert_eq!(shell_escape("my app"), "'my app'");
        assert_eq!(shell_escape("/path/to/my app"), "'/path/to/my app'");
    }

    #[test]
    fn shell_escape_single_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_escape_special_chars() {
        assert_eq!(shell_escape("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_escape("foo;bar"), "'foo;bar'");
        assert_eq!(shell_escape("a&b"), "'a&b'");
        assert_eq!(shell_escape("a|b"), "'a|b'");
    }

    #[test]
    fn shell_escape_backticks() {
        assert_eq!(shell_escape("`whoami`"), "'`whoami`'");
    }
}
