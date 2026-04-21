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

/// Message shown when a `methods`/`disasm`/etc. pattern matches nothing.
///
/// The common trap: the JIT only emits a standalone body (with a
/// `; Assembly listing for method …` header) for methods it compiled
/// on their own. Small/hot methods get inlined at every call site and
/// leave no trace in `capture.asm` under their own name — `dbg disasm
/// FooHelper` correctly but misleadingly reports "no methods found".
/// When the pattern looks specific (not bare `*` / empty), steer the
/// agent toward `search` (find callers that contain the tell-tale
/// instructions of the inlined body) before they conclude the code
/// path is missing.
fn empty_match_hint(pattern: &str, callers: &[&str]) -> String {
    let p = pattern.trim().trim_matches('*');
    if p.is_empty() {
        return "no methods found\n".into();
    }

    // Preferred path: we already know, from the call-graph built at
    // parse time, which methods reference this target. Advertise them
    // concretely — the agent can jump straight to `disasm <caller>`
    // without first running `search` to locate one. This is the
    // "preemptive" behavior: information derived from one pass over
    // the capture, surfaced at the moment it's actionable.
    if !callers.is_empty() {
        let list: Vec<String> = callers
            .iter()
            .take(8)
            .map(|c| format!("    • {c}"))
            .collect();
        let more = if callers.len() > 8 {
            format!("\n    … and {} more", callers.len() - 8)
        } else {
            String::new()
        };
        return format!(
            "no standalone body for `{pattern}` — it was inlined at every call\n\
             site. The inlined code runs, but lives embedded inside the callers'\n\
             listings, not under its own header.\n\
             \n\
             Known callers (from the call graph of this capture):\n\
             {}{more}\n\
             \n\
             Try `disasm {}` — the inlined body appears right after that\n\
             call-site's argument setup. `search <tell-tale-instruction>` still\n\
             works if you want to narrow down which caller actually hit a\n\
             specific codegen variant.\n",
            list.join("\n"),
            callers[0],
        );
    }

    // Fallback: no caller info (target not referenced by any standalone
    // method either, e.g. everything up the chain also got inlined, or
    // the pattern is misspelled). Give the generic workflow.
    format!(
        "no methods found matching `{pattern}`.\n\
         If the method is small/hot it was probably inlined — the JIT emits no\n\
         standalone body for inlined methods, so it won't appear here. The\n\
         inlined code still executes: it lives inside the caller's disasm.\n\
         \n\
         Workflow:\n\
           1. Find a caller (the \"parent\" method that invokes it):\n\
                `search <tell-tale-instruction-or-helper>`\n\
              Pick an op the callee is likely to emit (a distinctive mask,\n\
              shift, compare, or helper call). Every method whose body\n\
              contains it is a caller that inlined the target.\n\
           2. `disasm <parent-method>` — the inlined body appears embedded\n\
              in the caller's listing, usually right after the call-site's\n\
              argument setup.\n\
           3. `methods *{p}*` — double-check the name isn't just qualified\n\
              differently (generics, overloads, nested types).\n\
           4. Only as a last resort: add `[MethodImpl(MethodImplOptions.NoInlining)]`\n\
              for a one-off run to force standalone codegen. Revert after —\n\
              it distorts everything else's disasm.\n"
    )
}

/// The .NET JIT emits `vxorps reg, reg, reg` (and `vpxor`, `xorps`, …)
/// purely to zero a register before scalar work. Counting it as a SIMD
/// hit in `simd` output turned scalar methods into false positives,
/// defeating the point of the command. The check is intentionally
/// narrow: a same-register xor is the only form treated as zero-init.
fn is_zero_init_xor(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let (mnemonic, operands) = match lower
        .trim()
        .split_once(|c: char| c.is_ascii_whitespace())
    {
        Some(pair) => pair,
        None => return false,
    };
    if !matches!(mnemonic, "vxorps" | "vxorpd" | "vpxor" | "xorps" | "pxor") {
        return false;
    }
    let ops: Vec<&str> = operands
        .split(',')
        .map(|t| t.split(';').next().unwrap_or("").trim())
        .filter(|t| !t.is_empty())
        .collect();
    !ops.is_empty() && ops.iter().all(|r| *r == ops[0])
}

/// Parsed index of all methods in a JIT disassembly file.
pub struct JitIndex {
    pub methods: Vec<JitMethod>,
    /// Inverted call graph: callee-name-substring → caller method names.
    ///
    /// Built from `call     [Namespace.Class:Method(args):ret]` operands
    /// in every method body. When `disasm <X>` matches no standalone
    /// listing (because `X` was inlined at every site), we look up `X`
    /// here and advertise the callers by name — the user can jump
    /// straight to `disasm <caller>` and find the inlined body embedded
    /// there, instead of having to discover the caller via `search`
    /// first.
    ///
    /// Keyed by both the fully-qualified `Namespace.Class:Method` form
    /// and the short `Class:Method` form, so a pattern in either shape
    /// resolves.
    pub call_graph: std::collections::HashMap<String, Vec<String>>,
}

/// Pull callee names out of a single disassembly line of the form
/// `       call     [Namespace.Class:Method(args):ret]`.
///
/// Returns `(fq_name, short_name)` where `short_name` drops the leading
/// `Namespace.` before `Class`. Returns `None` for runtime-helper calls
/// (`CORINFO_HELP_*`) and indirect calls that don't name a method.
fn extract_call_target(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("call")?;
    // Must be whitespace-separated "call" followed by arguments — don't
    // match `callq` or stray identifiers containing "call".
    if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
        return None;
    }
    let after = rest.trim_start();
    let inside = after.strip_prefix('[').and_then(|s| s.split_once(']'))?.0;
    // Take up to the first `(` — the signature is noise for graph purposes.
    let name = inside.split('(').next().unwrap_or(inside).trim();
    if !name.contains(':') {
        return None;
    }
    let short = match name.rsplit_once('.') {
        Some((_, tail)) if tail.contains(':') => tail.to_string(),
        _ => name.to_string(),
    };
    Some((name.to_string(), short))
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
                // Format: "; Total bytes of code 42" or "; Total bytes of code = 42"
                current_bytes = rest
                    .trim()
                    .trim_start_matches('=')
                    .trim()
                    .parse()
                    .unwrap_or(0);
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

        // Build the inverted call graph. See struct doc for rationale.
        let mut call_graph: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for m in &methods {
            let mut already: std::collections::HashSet<(String, String)> =
                std::collections::HashSet::new();
            for line in m.body.lines() {
                if let Some((fq, short)) = extract_call_target(line) {
                    // Dedup per-caller so a method calling the same target
                    // twice doesn't list itself twice in the advert.
                    if already.insert((fq.clone(), short.clone())) {
                        call_graph.entry(fq).or_default().push(m.name.clone());
                        if !call_graph.get(&short).is_some_and(|v| v.contains(&m.name)) {
                            call_graph.entry(short).or_default().push(m.name.clone());
                        }
                    }
                }
            }
        }

        JitIndex { methods, call_graph }
    }

    /// Callers that reference `pattern` in a `call` instruction. Used
    /// by the empty-match hint to advertise where an inlined target's
    /// body can actually be inspected (inside the caller's listing).
    ///
    /// Substring match so `MathUtils:Length`, `:Length`, and the full
    /// `MyNamespace.MathUtils:Length` all resolve.
    pub fn callers_of(&self, pattern: &str) -> Vec<&str> {
        let needle = pattern.trim().trim_matches('*');
        if needle.is_empty() {
            return Vec::new();
        }
        let mut seen = std::collections::HashSet::new();
        let mut out: Vec<&str> = Vec::new();
        for (callee, callers) in &self.call_graph {
            if callee.contains(needle) {
                for c in callers {
                    if seen.insert(c.as_str()) {
                        out.push(c.as_str());
                    }
                }
            }
        }
        out.sort_unstable();
        out
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
            let callers = self.callers_of(pattern);
            out.push_str(&empty_match_hint(pattern, &callers));
        }
        out
    }

    /// Produce real disassembly text for `pattern` — the method's own
    /// body if it was emitted standalone, or the callers' bodies
    /// (banner-separated) when it was inlined away. Returns `None`
    /// when neither path has anything: no standalone match, no
    /// caller references in the capture. Callers decide whether to
    /// render a hint, bail, etc.
    ///
    /// Shared between the interactive REPL and the on-demand
    /// disasm collector so both surfaces benefit from the
    /// inlined-parent fallback — previously the collector did a
    /// plain header scan and bailed with "no matching header",
    /// making `dbg disasm` from a shell strictly worse than the
    /// REPL for inlined targets.
    pub fn disasm_with_parent_fallback(&self, pattern: &str) -> Option<String> {
        let matched = self.filter(pattern);
        if !matched.is_empty() {
            let mut out = String::new();
            for m in &matched {
                out.push_str(&m.body);
                out.push('\n');
            }
            return Some(out);
        }

        let callers = self.callers_of(pattern);
        if callers.is_empty() {
            return None;
        }

        // Cap at the 6 largest caller bodies to keep the output
        // navigable. Larger methods are more likely to contain the
        // inlined body in a recognisable form; if the agent wants all
        // of them, they can ask for each by name.
        const MAX: usize = 6;
        let mut caller_methods: Vec<&JitMethod> = callers
            .iter()
            .filter_map(|c| self.methods.iter().find(|m| m.name == *c))
            .collect();
        caller_methods.sort_by(|a, b| b.code_bytes.cmp(&a.code_bytes));
        let truncated = caller_methods.len() > MAX;
        caller_methods.truncate(MAX);

        let mut out = format!(
            "── `{pattern}` has no standalone body — inlined at every call site. \
             Showing {} caller listing(s); the inlined body is embedded in each. ──\n\n",
            caller_methods.len()
        );
        for m in &caller_methods {
            out.push_str(&format!(
                "════════ parent: {} ════════\n",
                m.name
            ));
            out.push_str(&m.body);
            out.push('\n');
        }
        if truncated {
            out.push_str(&format!(
                "\n(… {} more caller(s) omitted; request by name if needed.)\n",
                callers.len() - MAX
            ));
        }
        Some(out)
    }

    /// `disasm <pattern>` — REPL command. Thin wrapper over
    /// `disasm_with_parent_fallback` that renders the generic
    /// inlining hint when there's nothing useful to show.
    pub fn cmd_disasm(&self, pattern: &str) -> String {
        self.disasm_with_parent_fallback(pattern)
            .unwrap_or_else(|| empty_match_hint(pattern, &self.callers_of(pattern)))
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

    /// `simd [pattern]` — find methods using SIMD instructions,
    /// optionally scoped to a name-substring filter.
    pub fn cmd_simd_filtered(&self, pattern: &str) -> String {
        const SIMD_PATTERNS: &[&str] = &[
            "vmovups", "vmovaps", "vmulps", "vaddps", "vfmadd", "vdpps",
            "vxorps", "vperm", "vbroadcast",
            // ARM NEON
            "ld1", "st1", "fmla", "fmul.v", "fadd.v",
        ];

        let methods = self.filter(pattern);
        let mut out = String::new();
        for m in &methods {
            let hits: Vec<&str> = m
                .body
                .lines()
                .filter(|l| {
                    !l.starts_with(';')
                        && SIMD_PATTERNS.iter().any(|p| l.contains(p))
                        && !is_zero_init_xor(l)
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

/// Normalize user-typed verb to the REPL's canonical form. `jitdasm`
/// is a documented synonym for `disasm`: the scenario instructions and
/// the top-level `dbg jitdasm <pattern>` command line mirror the
/// session type, and users followed that naming into the REPL where
/// only `disasm` was recognized.
pub(crate) fn canonical_verb(cmd: &str) -> &str {
    match cmd {
        "jitdasm" => "disasm",
        other => other,
    }
}

/// Run the interactive REPL. Reads commands from stdin, writes results to stdout.
pub fn run_repl(asm_path: &str, default_pattern: &str) -> io::Result<()> {
    let text = std::fs::read_to_string(asm_path)?;
    let index = JitIndex::parse(&text);

    eprintln!(
        "--- ready: {} methods captured ---",
        index.methods.len()
    );
    if !default_pattern.is_empty() {
        eprintln!(
            "--- default filter: `{}` (stats/simd/hotspots narrow to this) ---",
            default_pattern
        );
    }
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

        let pat = if arg2.is_empty() { arg1.to_string() } else { format!("{arg1} {arg2}") };

        // Default summary-style commands to the session's pattern
        // when the user didn't pass one. Explicit args always win.
        let stats_arg = if arg1.is_empty() { default_pattern } else { arg1 };
        let methods_arg = if arg1.is_empty() { default_pattern } else { arg1 };
        let hotspots_arg = if arg2.is_empty() { default_pattern } else { arg2 };

        let cmd = canonical_verb(cmd);

        let result = match cmd {
            "methods" => index.cmd_methods(methods_arg),
            "disasm" if arg1.is_empty() && !default_pattern.is_empty() => {
                index.cmd_disasm(default_pattern)
            }
            "disasm" if arg1.is_empty() => "usage: disasm <pattern>\n".into(),
            "disasm" => index.cmd_disasm(&pat),
            "search" if arg1.is_empty() => "usage: search <instruction>\n".into(),
            "search" => index.cmd_search(arg1),
            "stats" => index.cmd_stats(stats_arg),
            "calls" if arg1.is_empty() => "usage: calls <pattern>\n".into(),
            "calls" => index.cmd_calls(arg1),
            "callers" if arg1.is_empty() => "usage: callers <pattern>\n".into(),
            "callers" => index.cmd_callers(arg1),
            "hotspots" => {
                let n: usize = arg1.parse().unwrap_or(10);
                index.cmd_hotspots(n, hotspots_arg)
            }
            "simd" => index.cmd_simd_filtered(default_pattern),
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
            "exit" | "quit" => {
                // Leaving the REPL does NOT kill the daemon; the
                // session keeps the capture file and any child
                // subprocesses alive. Agents (and humans) routinely
                // forget this and move on, leaving leaked state that
                // confuses the next `dbg start`. Surface the reminder
                // at the exit boundary — it's the last thing they see.
                println!(
                    "\nREPL closed. The dbg session is still running in the background.\n\
                     Run `dbg kill` now to release the capture file and any subprocesses."
                );
                break;
            }
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

    /// Regression: `dbg jitdasm <pattern>` inside a jitdasm session
    /// returned "unknown command" because the REPL only recognised
    /// `disasm`. The scenario instructions and top-level `dbg jitdasm`
    /// verb both suggest the user-facing name, so the REPL now accepts
    /// `jitdasm` as a synonym.
    #[test]
    fn empty_match_hint_plain_for_wildcard() {
        // Bare `*` or empty means "whole capture is empty" — a
        // different problem (no methods compiled at all), so no
        // inlining hint.
        assert_eq!(empty_match_hint("*", &[]), "no methods found\n");
        assert_eq!(empty_match_hint("", &[]), "no methods found\n");
        assert_eq!(empty_match_hint("  ", &[]), "no methods found\n");
    }

    #[test]
    fn empty_match_hint_generic_when_no_callers_known() {
        let msg = empty_match_hint("FlatLongIntMap:TryGetValue", &[]);
        assert!(msg.contains("inlined"), "hint should mention inlining: {msg}");
        assert!(msg.contains("search"), "hint should suggest `search`: {msg}");
        assert!(
            msg.contains("caller"),
            "hint should tell the agent to look at the caller: {msg}"
        );
        assert!(
            msg.contains("disasm <parent-method>"),
            "hint should spell out the parent-disasm step: {msg}"
        );
        assert!(
            msg.contains("NoInlining"),
            "hint should mention NoInlining escape hatch: {msg}"
        );
        assert!(
            msg.contains("FlatLongIntMap:TryGetValue"),
            "hint should echo the pattern: {msg}"
        );
    }

    #[test]
    fn empty_match_hint_advertises_known_callers() {
        let msg = empty_match_hint(
            "MathUtils:Length",
            &["MyNamespace.MathUtils:Normalize(float[]):float[]"],
        );
        assert!(
            msg.contains("Known callers"),
            "hint should label the caller list: {msg}"
        );
        assert!(
            msg.contains("MathUtils:Normalize"),
            "hint should name the actual caller: {msg}"
        );
        assert!(
            msg.contains("disasm MyNamespace.MathUtils:Normalize"),
            "hint should include a ready-to-run `disasm <caller>` command: {msg}"
        );
        // Preemptive-mode hint shouldn't belabour the NoInlining escape
        // hatch — the agent has a concrete next step.
        assert!(
            !msg.contains("NoInlining"),
            "preemptive hint should skip the last-resort escape hatch: {msg}"
        );
    }

    #[test]
    fn extract_call_target_parses_managed_call() {
        let (fq, short) = extract_call_target(
            "       call     [MyNamespace.SimdOps:DotProduct(System.ReadOnlySpan`1[float],System.ReadOnlySpan`1[float]):float]",
        )
        .unwrap();
        assert_eq!(fq, "MyNamespace.SimdOps:DotProduct");
        assert_eq!(short, "SimdOps:DotProduct");
    }

    #[test]
    fn extract_call_target_ignores_runtime_helpers() {
        assert!(extract_call_target("       call     CORINFO_HELP_RNGCHKFAIL").is_none());
        assert!(extract_call_target("       call     qword ptr [rax+0x10]").is_none());
        // Must not match lines that merely contain "call" as a substring.
        assert!(extract_call_target("       mov      rax, callable_ptr").is_none());
    }

    #[test]
    fn call_graph_maps_inlined_callees_to_their_callers() {
        // Fixture scenario: `MyNamespace.MathUtils:Length` is called
        // inside `MathUtils:Normalize` but has no standalone listing.
        // Exactly the inlinee → parent case we want to advertise.
        let idx = JitIndex::parse(SAMPLE);
        let callers = idx.callers_of("MathUtils:Length");
        assert!(
            callers.iter().any(|c| c.contains("MathUtils:Normalize")),
            "MathUtils:Length should be advertised as called-by MathUtils:Normalize, got {callers:?}"
        );
    }

    #[test]
    fn cmd_disasm_of_inlined_method_emits_parent_body() {
        // The central behavior: asking for an inlined method's disasm
        // should transparently show the parent's disassembly (where
        // the inlined body actually lives), not just name the parent
        // and make the agent run a second command.
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_disasm("MathUtils:Length");

        // Banner makes the substitution explicit.
        assert!(
            out.contains("no standalone body"),
            "output should flag that the target is inlined: {out}"
        );
        assert!(
            out.contains("parent:") && out.contains("MathUtils:Normalize"),
            "output should label the parent being shown: {out}"
        );

        // The parent's actual disassembly must be present, not just
        // its name. `Normalize`'s body in the fixture contains this
        // call instruction.
        assert!(
            out.contains("call     [MyNamespace.MathUtils:Length"),
            "output should embed the parent's real disasm body, not a summary: {out}"
        );
    }

    #[test]
    fn cmd_disasm_falls_back_to_generic_hint_when_no_callers() {
        let idx = JitIndex::parse(SAMPLE);
        // A name that is neither a standalone method nor referenced by
        // any call instruction → no call-graph entry, generic hint.
        let out = idx.cmd_disasm("GhostMethod:Nope");
        assert!(
            out.contains("no methods found") || out.contains("NoInlining"),
            "should fall back to the generic hint: {out}"
        );
        assert!(
            !out.contains("parent:"),
            "must not fabricate parent listings: {out}"
        );
    }

    #[test]
    fn canonical_verb_maps_jitdasm_to_disasm() {
        assert_eq!(canonical_verb("jitdasm"), "disasm");
        assert_eq!(canonical_verb("disasm"), "disasm");
        assert_eq!(canonical_verb("methods"), "methods");
        assert_eq!(canonical_verb("garbage"), "garbage");
    }

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
    fn cmd_simd_filtered_narrows_to_pattern() {
        // Regression: `simd` used to scan every captured method,
        // so scoping it (via the REPL default pattern) was impossible.
        let idx = JitIndex::parse(SAMPLE);
        let narrow = idx.cmd_simd_filtered("DotProduct");
        let wide = idx.cmd_simd_filtered("");
        // Narrowed output must be a strict subset of the wide output.
        assert!(wide.len() >= narrow.len(), "wide should be >= narrow");
        // Narrow must not mention methods outside the filter.
        assert!(!narrow.contains("Normalize"), "narrow leaked Normalize:\n{narrow}");
        assert!(!narrow.contains("Pipeline:Run"), "narrow leaked Pipeline:\n{narrow}");
    }

    #[test]
    fn cmd_stats_narrows_by_method_token() {
        // `:DotProduct` should match only the SimdOps:DotProduct
        // method listing, not ScalarDotProduct.
        let idx = JitIndex::parse(SAMPLE);
        let out = idx.cmd_stats(":DotProduct");
        assert!(out.contains("filter:"), "expected filter label: {out}");
        assert!(out.contains("Methods:       1"), "expected 1 method:\n{out}");
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
        let out = idx.cmd_simd_filtered("");
        assert!(out.contains("DotProduct"));
        assert!(out.contains("vmovups"));
        assert!(out.contains("vmulps"));
    }

    #[test]
    fn cmd_simd_ignores_vxorps_zero_init_idiom() {
        // Regression: `vxorps xmm0, xmm0, xmm0` is the .NET JIT's
        // standard zero-initialization preamble — it doesn't imply the
        // method is vectorized. Counting it as a SIMD hit made scalar
        // methods look indistinguishable from AVX hot loops in `simd`
        // output, which was the opposite of the command's purpose.
        let asm = "\
; Assembly listing for method Broken.Program:SumSlow(System.Int32[]):int (Tier1)
G_M1_IG01:
            vxorps   xmm0, xmm0, xmm0
            xor      eax, eax
            mov      edx, dword ptr [rcx+0x08]
            test     edx, edx
            jle      SHORT G_M1_IG03
G_M1_IG02:
            add      eax, dword ptr [rcx+4*r8+0x10]
            inc      r8d
            cmp      r9d, r8d
            jl       SHORT G_M1_IG02
G_M1_IG03:
            ret

; Total bytes of code: 40
";
        let idx = JitIndex::parse(asm);
        let out = idx.cmd_simd_filtered("");
        assert!(
            out.contains("no SIMD instructions found"),
            "vxorps zero-init should not count as SIMD, got:\n{out}"
        );
        // The generic xmm-register counter on `stats` is still fine
        // showing xmm usage, but the top-level SIMD hit list must be
        // limited to *real* vector compute/IO.
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

    #[test]
    fn parse_code_bytes_with_equals_format() {
        // Some .NET versions emit "; Total bytes of code = 42"
        let asm = "; Assembly listing for method Foo:Bar()\n\
                    ; Emitting BLENDED_CODE for X64\n\n\
                    push rbp\n\
                    ret\n\
                    ; Total bytes of code = 42\n";
        let idx = JitIndex::parse(asm);
        assert_eq!(idx.methods.len(), 1);
        assert_eq!(idx.methods[0].code_bytes, 42);
    }

    #[test]
    fn parse_code_bytes_plain_format() {
        // Standard format: "; Total bytes of code 250"
        let asm = "; Assembly listing for method Foo:Baz()\n\n\
                    nop\n\
                    ; Total bytes of code 250\n";
        let idx = JitIndex::parse(asm);
        assert_eq!(idx.methods.len(), 1);
        assert_eq!(idx.methods[0].code_bytes, 250);
    }
}
