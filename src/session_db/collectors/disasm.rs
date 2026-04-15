//! Three disasm collectors for Phase 1: lldb (native), jitdasm (.NET),
//! go-objdump (Go).
//!
//! Each implements `OnDemandCollector` and produces a `DisasmOutput`
//! the caller can feed to `persist_disasm`. Collectors do NOT touch
//! the DB directly — that keeps the shell-out + parse logic testable
//! in isolation and lets the daemon batch multiple collections.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use regex::Regex;

use super::{CollectCtx, DisasmOutput, LiveDebugger, OnDemandCollector};
use crate::session_db::TargetClass;

// ============================================================
// lldb `disassemble --name <sym>`
// ============================================================

pub struct LldbDisassembleCollector;

impl OnDemandCollector for LldbDisassembleCollector {
    fn kind(&self) -> &'static str {
        "lldb-disassemble"
    }

    fn supports(&self, class: TargetClass) -> bool {
        matches!(class, TargetClass::NativeCpu)
    }

    fn collect(
        &self,
        ctx: &CollectCtx<'_>,
        live: Option<&dyn LiveDebugger>,
    ) -> Result<DisasmOutput> {
        let cmd = format!("disassemble --name {}", ctx.symbol);
        let raw = match live {
            Some(l) => l.send(&cmd)?,
            None => run_oneshot_lldb(ctx.target, &cmd)?,
        };
        let asm_text = raw.trim().to_string();
        if asm_text.is_empty() {
            bail!("lldb produced no disassembly for {}", ctx.symbol);
        }
        let code_bytes = count_instruction_bytes(&asm_text);
        Ok(DisasmOutput {
            source: "lldb-disassemble",
            tier: None,
            code_bytes,
            asm_text,
            asm_lines_json: None,
        })
    }
}

fn run_oneshot_lldb(target: &str, disasm_cmd: &str) -> Result<String> {
    let bin = std::env::var("LLDB_BIN").unwrap_or_else(|_| "lldb".into());
    let output = Command::new(&bin)
        .args([
            "--batch",
            "--no-use-colors",
            "-o",
            &format!("target create \"{}\"", target.replace('"', "\\\"")),
            "-o",
            disasm_cmd,
            "-o",
            "quit",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("invoking {bin} for disasm"))?;
    if !output.status.success() && output.stdout.is_empty() {
        bail!(
            "{bin} exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Very rough upper bound on code size: count `0x` prefixes on the
/// address column of each disasm line. lldb emits `0x<hex>:` per
/// instruction — works for both x86-64 and aarch64 dumps.
fn count_instruction_bytes(asm: &str) -> Option<i64> {
    let re = lldb_addr_regex();
    let mut addrs = asm
        .lines()
        .filter_map(|l| re.captures(l).and_then(|c| u64::from_str_radix(&c[1], 16).ok()));
    let first = addrs.next()?;
    let last = addrs.last()?;
    Some((last as i64) - (first as i64))
}

fn lldb_addr_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:->\s+)?\s*0x([0-9a-fA-F]+)\s*[:<]").unwrap())
}

// ============================================================
// .NET jitdasm (via `DOTNET_JitDisasm` env on a fresh process)
// ============================================================

pub struct JitDasmCollector;

impl OnDemandCollector for JitDasmCollector {
    fn kind(&self) -> &'static str {
        "jitdasm"
    }

    fn supports(&self, class: TargetClass) -> bool {
        matches!(class, TargetClass::ManagedDotnet)
    }

    /// jitdasm always spawns a *fresh* `dotnet <target>` — never reuses
    /// the live debug session, because `DOTNET_JitDisasm` must be set
    /// in the environment before the runtime starts. The live session
    /// is untouched.
    fn collect(
        &self,
        ctx: &CollectCtx<'_>,
        _live: Option<&dyn LiveDebugger>,
    ) -> Result<DisasmOutput> {
        let dotnet = std::env::var("DOTNET").unwrap_or_else(|_| "dotnet".into());
        // JitDisasm accepts a wildcard-free method-name substring match.
        // We pass the caller's symbol verbatim.
        let output = Command::new(&dotnet)
            .arg(ctx.target)
            .env("DOTNET_JitDisasm", ctx.symbol)
            .env("DOTNET_TieredCompilation", "0") // deterministic tier
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("invoking {dotnet} for jitdasm"))?;
        // JitDisasm writes to stderr; target program may also emit
        // normal stdout we want to ignore.
        let text = String::from_utf8_lossy(&output.stderr).to_string();
        let asm_text = extract_jitdasm_section(&text, ctx.symbol);
        if asm_text.is_empty() {
            bail!(
                "jitdasm produced no assembly for {} (dotnet exit {})",
                ctx.symbol,
                output.status
            );
        }
        let tier = parse_jitdasm_tier(&asm_text);
        Ok(DisasmOutput {
            source: "jitdasm",
            tier,
            code_bytes: None,
            asm_text,
            asm_lines_json: None,
        })
    }
}

/// jitdasm writes one method listing at a time, each starting with a
/// `; Assembly listing for method <method> (...)`-style header. We
/// keep everything from the first header that mentions our symbol up
/// to the next header (or end).
fn extract_jitdasm_section(stderr: &str, symbol: &str) -> String {
    let mut out = Vec::new();
    let mut capturing = false;
    for line in stderr.lines() {
        if line.starts_with("; Assembly listing for method") {
            if capturing {
                break;
            }
            if line.contains(symbol) {
                capturing = true;
                out.push(line);
            }
        } else if capturing {
            out.push(line);
        }
    }
    out.join("\n")
}

fn parse_jitdasm_tier(asm: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\((Tier-?\d|OSR|MinOpts|FullOpts)\)").unwrap());
    for line in asm.lines().take(3) {
        if let Some(c) = re.captures(line) {
            return Some(c[1].to_lowercase().replace('-', ""));
        }
    }
    None
}

// ============================================================
// Go: `go tool objdump -s <sym> <target>`
// ============================================================

pub struct GoDisassCollector;

impl OnDemandCollector for GoDisassCollector {
    fn kind(&self) -> &'static str {
        "go-objdump"
    }

    fn supports(&self, class: TargetClass) -> bool {
        // Go binaries compile to native, but we expose them as
        // NativeCpu in the target-class taxonomy. This collector is
        // safe to register alongside the lldb collector — the
        // dispatcher (task 8) picks one per invocation.
        matches!(class, TargetClass::NativeCpu)
    }

    fn collect(
        &self,
        ctx: &CollectCtx<'_>,
        _live: Option<&dyn LiveDebugger>,
    ) -> Result<DisasmOutput> {
        let output = Command::new("go")
            .args(["tool", "objdump", "-s", ctx.symbol, ctx.target])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .context("invoking `go tool objdump`")?;
        if !output.status.success() {
            bail!(
                "go tool objdump failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let asm_text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if asm_text.is_empty() {
            bail!("go tool objdump produced no output for {}", ctx.symbol);
        }
        Ok(DisasmOutput {
            source: "go-objdump",
            tier: None,
            code_bytes: None,
            asm_text,
            asm_lines_json: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Parser-level tests — don't require the tool to be installed.

    #[test]
    fn extract_jitdasm_isolates_target_method() {
        let stderr = "\
Hello from pre-JIT chatter.
; Assembly listing for method MyApp.Foo:Bar() (Tier1)
 mov rax, rbx
 ret
; Assembly listing for method MyApp.Baz:Qux()
 mov rcx, rdx";
        let got = extract_jitdasm_section(stderr, "MyApp.Foo:Bar");
        assert!(got.contains("MyApp.Foo:Bar"));
        assert!(got.contains("mov rax, rbx"));
        assert!(!got.contains("MyApp.Baz:Qux"));
    }

    #[test]
    fn extract_jitdasm_empty_when_no_match() {
        let stderr = "; Assembly listing for method MyApp.X:Y\n mov rax, rbx";
        let got = extract_jitdasm_section(stderr, "Other.Method");
        assert!(got.is_empty());
    }

    #[test]
    fn parse_tier_from_header() {
        let asm = "; Assembly listing for method MyApp.Foo:Bar() (Tier1)\n mov rax, rbx";
        assert_eq!(parse_jitdasm_tier(asm), Some("tier1".into()));
        let asm = "; Assembly listing for method ... (Tier-0)\nnop";
        assert_eq!(parse_jitdasm_tier(asm), Some("tier0".into()));
        let asm = "; Assembly listing for method ... (FullOpts)\nnop";
        assert_eq!(parse_jitdasm_tier(asm), Some("fullopts".into()));
        let asm = "; Assembly listing for method ... (no tier mark)\nnop";
        assert_eq!(parse_jitdasm_tier(asm), None);
    }

    #[test]
    fn count_bytes_from_address_column() {
        let asm = "\
test`main:
    0x100003f80 <+0>:   push   rbp
    0x100003f84 <+4>:   mov    rbp, rsp
    0x100003f88 <+8>:   mov    eax, 0x0
    0x100003f8d <+13>:  pop    rbp
    0x100003f8e <+14>:  ret";
        let bytes = count_instruction_bytes(asm);
        assert_eq!(bytes, Some(0x100003f8e - 0x100003f80));
    }

    #[test]
    fn count_bytes_none_on_empty() {
        assert_eq!(count_instruction_bytes(""), None);
        assert_eq!(count_instruction_bytes("no addrs here"), None);
    }

    #[test]
    fn collector_supports_matrix() {
        let l = LldbDisassembleCollector;
        assert!(l.supports(TargetClass::NativeCpu));
        assert!(!l.supports(TargetClass::ManagedDotnet));
        assert!(!l.supports(TargetClass::Python));

        let j = JitDasmCollector;
        assert!(j.supports(TargetClass::ManagedDotnet));
        assert!(!j.supports(TargetClass::NativeCpu));

        let g = GoDisassCollector;
        assert!(g.supports(TargetClass::NativeCpu));
        assert!(!g.supports(TargetClass::Python));
    }

    #[test]
    fn kinds_match_source_column() {
        assert_eq!(LldbDisassembleCollector.kind(), "lldb-disassemble");
        assert_eq!(JitDasmCollector.kind(), "jitdasm");
        assert_eq!(GoDisassCollector.kind(), "go-objdump");
    }
}
