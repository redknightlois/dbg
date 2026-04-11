//! JIT disassembly parser and interactive REPL.
//!
//! Parses .NET JIT disassembly output (DOTNET_JitDisasm) into structured
//! method records, then provides a command-line interface for querying them.

use std::io::{self, BufRead, Write};

/// A single disassembled method.
#[derive(Debug)]
pub struct JitMethod {
    /// Full method signature, e.g. "MyNamespace.SimdOps:DotProduct(...):float"
    pub name: String,
    /// Total bytes of generated code.
    pub code_bytes: u32,
    /// Raw assembly lines (everything between the header and the next method).
    pub body: String,
}

/// Parsed index of all methods in a JIT disassembly file.
pub struct JitIndex {
    pub methods: Vec<JitMethod>,
}

impl JitIndex {
    /// Parse raw JIT disassembly text into an indexed structure.
    pub fn parse(text: &str) -> Self {
        let mut methods = Vec::new();
        let mut current_name: Option<String> = None;
        let mut current_body = String::new();
        let mut current_bytes: u32 = 0;

        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("; Assembly listing for method ") {
                // Flush previous method
                if let Some(name) = current_name.take() {
                    methods.push(JitMethod {
                        name,
                        code_bytes: current_bytes,
                        body: std::mem::take(&mut current_body),
                    });
                }
                // Parse method name (strip trailing "(FullOpts)" etc.)
                let name = rest
                    .rsplit_once(" (")
                    .map(|(n, _)| n)
                    .unwrap_or(rest)
                    .to_string();
                current_name = Some(name);
                current_body.clear();
                current_bytes = 0;
            }

            if let Some(rest) = line.strip_prefix("; Total bytes of code") {
                current_bytes = rest.trim().parse().unwrap_or(0);
            }

            if current_name.is_some() {
                current_body.push_str(line);
                current_body.push('\n');
            }
        }

        // Flush last method
        if let Some(name) = current_name {
            methods.push(JitMethod {
                name,
                code_bytes: current_bytes,
                body: current_body,
            });
        }

        JitIndex { methods }
    }

    /// Filter methods whose name matches a substring (case-sensitive).
    fn filter(&self, pattern: &str) -> Vec<&JitMethod> {
        if pattern.is_empty() || pattern == "." {
            self.methods.iter().collect()
        } else {
            self.methods
                .iter()
                .filter(|m| m.name.contains(pattern))
                .collect()
        }
    }

    /// `methods [pattern]` — list methods with code sizes, sorted largest first.
    pub fn cmd_methods(&self, pattern: &str) -> String {
        let mut matched = self.filter(pattern);
        matched.sort_by(|a, b| b.code_bytes.cmp(&a.code_bytes));
        let mut out = String::new();
        for m in &matched {
            out.push_str(&format!("{:<60} {} bytes\n", m.name, m.code_bytes));
        }
        if out.is_empty() {
            out.push_str("no methods found\n");
        }
        out
    }

    /// `disasm <pattern>` — show full disassembly for matching methods.
    pub fn cmd_disasm(&self, pattern: &str) -> String {
        let matched = self.filter(pattern);
        let mut out = String::new();
        for m in &matched {
            out.push_str(&m.body);
            out.push('\n');
        }
        if out.is_empty() {
            out.push_str("no methods found\n");
        }
        out
    }

    /// `search <instruction>` — find methods containing a specific instruction.
    pub fn cmd_search(&self, pattern: &str) -> String {
        let mut out = String::new();
        for m in &self.methods {
            let hits: Vec<&str> = m
                .body
                .lines()
                .filter(|l| !l.starts_with(';') && l.contains(pattern))
                .collect();
            if !hits.is_empty() {
                out.push_str(&format!("{} ({} hits):\n", m.name, hits.len()));
                for h in &hits {
                    out.push_str(&format!("  {}\n", h.trim()));
                }
            }
        }
        if out.is_empty() {
            out.push_str("no matches\n");
        }
        out
    }

    /// Extract call targets from a method body.
    fn extract_calls(body: &str) -> Vec<String> {
        let mut targets = Vec::new();
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with(';') {
                continue;
            }
            // Match: call [Target] or call Target
            if let Some(rest) = trimmed.strip_prefix("call") {
                let rest = rest.trim();
                // Strip brackets: [Foo:Bar(...)] → Foo:Bar(...)
                let target = rest
                    .strip_prefix('[')
                    .and_then(|s| s.strip_suffix(']'))
                    .unwrap_or(rest);
                if !target.is_empty() {
                    targets.push(target.to_string());
                }
            }
        }
        targets
    }

    /// `calls <pattern>` — what does this method call?
    pub fn cmd_calls(&self, pattern: &str) -> String {
        let matched = self.filter(pattern);
        let mut out = String::new();
        for m in &matched {
            let targets = Self::extract_calls(&m.body);
            if targets.is_empty() {
                out.push_str(&format!("{}: no calls\n", m.name));
            } else {
                out.push_str(&format!("{} ({} calls):\n", m.name, targets.len()));
                for t in &targets {
                    out.push_str(&format!("  → {}\n", t));
                }
            }
        }
        if out.is_empty() {
            out.push_str("no methods found\n");
        }
        out
    }

    /// `callers <pattern>` — who calls this method?
    pub fn cmd_callers(&self, pattern: &str) -> String {
        let mut out = String::new();
        for m in &self.methods {
            let targets = Self::extract_calls(&m.body);
            let hits: Vec<&String> = targets.iter().filter(|t| t.contains(pattern)).collect();
            if !hits.is_empty() {
                out.push_str(&format!("{} calls it {} time(s):\n", m.name, hits.len()));
                for t in &hits {
                    out.push_str(&format!("  → {}\n", t));
                }
            }
        }
        if out.is_empty() {
            out.push_str(&format!("no callers found for '{}'\n", pattern));
        }
        out
    }

    /// `stats [pattern]` — summary statistics.
    pub fn cmd_stats(&self, pattern: &str) -> String {
        let matched = self.filter(pattern);
        if matched.is_empty() {
            return "no methods found\n".into();
        }

        let total_bytes: u32 = matched.iter().map(|m| m.code_bytes).sum();

        // Collect all non-comment instruction lines
        let instructions: Vec<&str> = matched
            .iter()
            .flat_map(|m| m.body.lines())
            .filter(|l| !l.starts_with(';') && !l.is_empty())
            .collect();

        let count = |pats: &[&str]| -> usize {
            instructions.iter().filter(|l| pats.iter().any(|p| l.contains(p))).count()
        };

        let avx512 = count(&["zmm"]);
        let avx2 = count(&["ymm"]);
        let sse = count(&["xmm"]);
        let fma = count(&["vfmadd", "vfmsub", "vfnmadd", "vfnmsub"]);
        let neon = instructions.iter().filter(|l| l.contains("{v") && (l.contains("ld1") || l.contains("st1") || l.contains("fmla") || l.contains("fmul"))).count();
        let sve = instructions.iter().filter(|l| (l.contains("ld1w") || l.contains("st1w")) && l.contains("z")).count();
        let bounds = count(&["RNGCHKFAIL"]);
        let spills = instructions.iter().filter(|l| l.contains("mov") && l.contains("[rsp")).count();

        let label = if pattern.is_empty() || pattern == "." {
            "--- all methods ---".to_string()
        } else {
            format!("--- filter: {} ---", pattern)
        };

        let mut out = format!("{}\n", label);
        out.push_str(&format!("Methods:       {}\n", matched.len()));
        out.push_str(&format!("Total code:    {} bytes\n", total_bytes));

        if avx512 > 0 || avx2 > 0 || sse > 0 {
            out.push_str(&format!("AVX-512 (zmm): {} instructions\n", avx512));
            out.push_str(&format!("AVX2 (ymm):    {} instructions\n", avx2));
            out.push_str(&format!("SSE (xmm):     {} instructions\n", sse));
        }
        if neon > 0 || sve > 0 {
            out.push_str(&format!("NEON:          {} instructions\n", neon));
            out.push_str(&format!("SVE:           {} instructions\n", sve));
        }
        // If no SIMD detected at all, show zeros
        if avx512 == 0 && avx2 == 0 && sse == 0 && neon == 0 && sve == 0 {
            out.push_str("SIMD:          none detected\n");
        }
        out.push_str(&format!("FMA:           {} instructions\n", fma));
        out.push_str(&format!("Bounds checks: {}\n", bounds));
        out.push_str(&format!("Stack spills:  {}\n", spills));
        out
    }

    /// `hotspots [N] [pattern]` — top N methods by code size.
    pub fn cmd_hotspots(&self, n: usize, pattern: &str) -> String {
        let mut matched = self.filter(pattern);
        matched.sort_by(|a, b| b.code_bytes.cmp(&a.code_bytes));
        let mut out = String::new();
        for m in matched.iter().take(n) {
            out.push_str(&format!("{:<60} {} bytes\n", m.name, m.code_bytes));
        }
        if out.is_empty() {
            out.push_str("no methods found\n");
        }
        out
    }

    /// `simd` — find methods using SIMD instructions.
    pub fn cmd_simd(&self) -> String {
        const SIMD_PATTERNS: &[&str] = &[
            "vmovups", "vmovaps", "vmulps", "vaddps", "vfmadd", "vdpps",
            "vxorps", "vperm", "vbroadcast",
            // ARM NEON
            "ld1", "st1", "fmla", "fmul.v", "fadd.v",
        ];

        let mut out = String::new();
        for m in &self.methods {
            let hits: Vec<&str> = m
                .body
                .lines()
                .filter(|l| {
                    !l.starts_with(';')
                        && SIMD_PATTERNS.iter().any(|p| l.contains(p))
                })
                .collect();
            if !hits.is_empty() {
                out.push_str(&format!("{} ({} hits):\n", m.name, hits.len()));
                for h in &hits {
                    out.push_str(&format!("  {}\n", h.trim()));
                }
            }
        }
        if out.is_empty() {
            out.push_str("no SIMD instructions found\n");
        }
        out
    }
}

/// Run the interactive REPL. Reads commands from stdin, writes results to stdout.
pub fn run_repl(asm_path: &str) -> io::Result<()> {
    let text = std::fs::read_to_string(asm_path)?;
    let index = JitIndex::parse(&text);

    eprintln!(
        "--- ready: {} methods captured ---",
        index.methods.len()
    );
    eprintln!("Type: help");

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    loop {
        print!("jitdasm> ");
        stdout.flush()?;

        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break; // EOF
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        let cmd = parts[0];
        let arg1 = parts.get(1).copied().unwrap_or("");
        let arg2 = parts.get(2).copied().unwrap_or("");

        let result = match cmd {
            "methods" => index.cmd_methods(arg1),
            "disasm" => {
                if arg1.is_empty() {
                    "usage: disasm <pattern>\n".into()
                } else {
                    // Rejoin all args for patterns with spaces
                    let pat = if arg2.is_empty() { arg1.to_string() } else { format!("{} {}", arg1, arg2) };
                    index.cmd_disasm(&pat)
                }
            }
            "search" => {
                if arg1.is_empty() {
                    "usage: search <instruction>\n".into()
                } else {
                    index.cmd_search(arg1)
                }
            }
            "stats" => index.cmd_stats(arg1),
            "calls" => {
                if arg1.is_empty() {
                    "usage: calls <pattern>\n".into()
                } else {
                    index.cmd_calls(arg1)
                }
            }
            "callers" => {
                if arg1.is_empty() {
                    "usage: callers <pattern>\n".into()
                } else {
                    index.cmd_callers(arg1)
                }
            }
            "hotspots" => {
                let n: usize = arg1.parse().unwrap_or(10);
                index.cmd_hotspots(n, arg2)
            }
            "simd" => index.cmd_simd(),
            "help" => {
                "jitdasm commands:\n  \
                 methods [pattern]    list methods with code sizes (sorted by size)\n  \
                 disasm <pattern>     show full disassembly for matching methods\n  \
                 search <instruction> find methods containing an instruction\n  \
                 stats [pattern]      summary stats — scope to method, class, or namespace\n  \
                 calls <pattern>      what does this method call?\n  \
                 callers <pattern>    who calls this method?\n  \
                 hotspots [N] [pat]   top N methods by code size (default 10)\n  \
                 simd                 find all methods using SIMD instructions\n  \
                 help                 show this help\n  \
                 exit                 quit\n"
                    .into()
            }
            "exit" | "quit" => break,
            _ => format!("unknown command: {}. Type 'help' for available commands.\n", cmd),
        };

        print!("{}", result);
        stdout.flush()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../tests/fixtures/jitdasm_sample.asm");

    #[test]
    fn parse_finds_all_methods() {
        let idx = JitIndex::parse(SAMPLE);
        assert_eq!(idx.methods.len(), 4);
    }

    #[test]
    fn parse_method_names() {
        let idx = JitIndex::parse(SAMPLE);
        let names: Vec<&str> = idx.methods.iter().map(|m| m.name.as_str()).collect();
        assert!(names.iter().any(|n| n.contains("DotProduct") && !n.contains("Scalar")));
        assert!(names.iter().any(|n| n.contains("ScalarDotProduct")));
        assert!(names.iter().any(|n| n.contains("Normalize")));
        assert!(names.iter().any(|n| n.contains("Pipeline:Run")));
    }

    #[test]
    fn parse_code_bytes() {
        let idx = JitIndex::parse(SAMPLE);
        let dot = idx.methods.iter().find(|m| m.name.contains("DotProduct") && !m.name.contains("Scalar")).unwrap();
        assert_eq!(dot.code_bytes, 250);
        let scalar = idx.methods.iter().find(|m| m.name.contains("ScalarDotProduct")).unwrap();
        assert_eq!(scalar.code_bytes, 96);
        let norm = idx.methods.iter().find(|m| m.name.contains("Normalize")).unwrap();
        assert_eq!(norm.code_bytes, 64);
        let pipeline = idx.methods.iter().find(|m| m.name.contains("Pipeline")).unwrap();
        assert_eq!(pipeline.code_bytes, 48);
    }

    #[test]
    fn cmd_methods_lists_all() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_methods("");
        assert!(out.contains("DotProduct"));
        assert!(out.contains("ScalarDotProduct"));
        assert!(out.contains("Normalize"));
        assert!(out.contains("Pipeline:Run"));
        assert!(out.contains("250 bytes"));
        assert!(out.contains("96 bytes"));
        assert!(out.contains("64 bytes"));
        assert!(out.contains("48 bytes"));
    }

    #[test]
    fn cmd_methods_filtered_by_class() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_methods("SimdOps");
        assert!(out.contains("DotProduct"));
        assert!(out.contains("ScalarDotProduct"));
        assert!(!out.contains("Normalize"));
        assert!(!out.contains("Pipeline"));
    }

    #[test]
    fn cmd_stats_all() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_stats("");
        assert!(out.contains("Methods:       4"));
        assert!(out.contains("Total code:    458 bytes"));
        assert!(out.contains("Bounds checks: 2"));
    }

    #[test]
    fn cmd_stats_filtered_by_class() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_stats("SimdOps");
        assert!(out.contains("Methods:       2"));
        assert!(out.contains("Total code:    346 bytes"));
    }

    #[test]
    fn cmd_stats_filtered_by_method() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_stats("Normalize");
        assert!(out.contains("Methods:       1"));
        assert!(out.contains("Total code:    64 bytes"));
    }

    #[test]
    fn cmd_stats_filtered_by_namespace() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_stats("MyNamespace");
        assert!(out.contains("Methods:       4"));
    }

    #[test]
    fn cmd_disasm_specific_method() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_disasm("ScalarDotProduct");
        assert!(out.contains("ScalarDotProduct"));
        assert!(out.contains("vxorps   xmm0"));
        assert!(!out.contains("vmovups")); // from DotProduct only
    }

    #[test]
    fn cmd_search_instruction() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_search("RNGCHKFAIL");
        assert!(out.contains("DotProduct"));
        assert!(out.contains("ScalarDotProduct"));
        assert!(!out.contains("Normalize"));
    }

    #[test]
    fn cmd_search_spills() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_search("[rsp");
        assert!(out.contains("Normalize"));
    }

    #[test]
    fn cmd_hotspots_returns_sorted() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_hotspots(10, "");
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].contains("250")); // DotProduct first (largest)
    }

    #[test]
    fn cmd_simd_finds_vectorized() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_simd();
        assert!(out.contains("DotProduct"));
        assert!(out.contains("vmovups"));
        assert!(out.contains("vmulps"));
    }

    // --- calls / callers ---

    #[test]
    fn cmd_calls_shows_targets() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_calls("Pipeline");
        assert!(out.contains("Pipeline:Run"));
        assert!(out.contains("→ MyNamespace.MathUtils:Normalize"));
        assert!(out.contains("→ MyNamespace.SimdOps:DotProduct"));
        assert!(out.contains("→ MyNamespace.SimdOps:ScalarDotProduct"));
        assert!(out.contains("3 calls"));
    }

    #[test]
    fn cmd_calls_normalize() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_calls("Normalize");
        assert!(out.contains("→ MyNamespace.SimdOps:DotProduct"));
        assert!(out.contains("→ MyNamespace.MathUtils:Length"));
        assert!(out.contains("2 calls"));
    }

    #[test]
    fn cmd_calls_no_calls() {
        let idx = JitIndex::parse(SAMPLE);
        // ScalarDotProduct only calls CORINFO_HELP_RNGCHKFAIL (a JIT helper, not a method)
        let out = idx.cmd_calls("ScalarDotProduct");
        assert!(out.contains("1 call")); // RNGCHKFAIL
    }

    #[test]
    fn cmd_callers_dotproduct() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_callers("DotProduct");
        // Normalize and Pipeline both call DotProduct
        assert!(out.contains("Normalize"));
        assert!(out.contains("Pipeline:Run"));
    }

    #[test]
    fn cmd_callers_normalize() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_callers("Normalize");
        // Only Pipeline calls Normalize
        assert!(out.contains("Pipeline:Run"));
        assert!(!out.contains("DotProduct"));
    }

    #[test]
    fn cmd_callers_nobody() {
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_callers("Pipeline");
        assert!(out.contains("no callers found"));
    }

    #[test]
    fn extract_calls_strips_brackets() {
        let body = "       call     [Foo:Bar(int):void]\n       call     CORINFO_HELP_RNGCHKFAIL\n";
        let calls = JitIndex::extract_calls(body);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], "Foo:Bar(int):void");
        assert_eq!(calls[1], "CORINFO_HELP_RNGCHKFAIL");
    }

    #[test]
    fn extract_calls_skips_comments() {
        let body = "; call this is a comment\n       call     [Real:Call()]\n";
        let calls = JitIndex::extract_calls(body);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], "Real:Call()");
    }
}
