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
use crate::jitdasm::JitIndex;
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

    /// Prefers the `jitdasm` backend's pre-captured `capture.asm` (the
    /// backend ran `DOTNET_JitDisasm='*'` once at session start and
    /// dumped every method to a file). When that file is missing —
    /// e.g. the session was started with a different .NET backend —
    /// falls back to spawning a fresh `dotnet` with `DOTNET_JitDisasm`
    /// scoped to the requested symbol. The live debug session is
    /// untouched either way.
    fn collect(
        &self,
        ctx: &CollectCtx<'_>,
        _live: Option<&dyn LiveDebugger>,
    ) -> Result<DisasmOutput> {
        let capture = std::env::var_os("DBG_JITDASM_CAPTURE").map(std::path::PathBuf::from);
        let (text, source_desc) = match capture.as_ref().filter(|p| p.is_file()) {
            Some(p) => (
                std::fs::read_to_string(p)
                    .with_context(|| format!("reading {}", p.display()))?,
                p.display().to_string(),
            ),
            None => (run_jitdasm_fresh(ctx.target, ctx.symbol)?, "fresh dotnet run".into()),
        };

        // `::` is a C++/docs-style separator; .NET uses a single `:`.
        // Normalise so the same symbol works in either shape.
        let needle = ctx.symbol.replace("::", ":");

        // Use the shared `JitIndex` fallback path so `dbg disasm` from
        // a shell behaves exactly like the interactive REPL: when the
        // target has no standalone body but the call graph knows
        // callers, return the callers' bodies banner-separated (the
        // inlined code lives embedded inside them). A plain header
        // scan would bail with "no matching header" and leave the
        // agent stuck.
        let index = JitIndex::parse(&text);
        let asm_text = index.disasm_with_parent_fallback(&needle).ok_or_else(|| {
            anyhow::anyhow!(
                "jitdasm produced no assembly for {} — no standalone body and no caller \
                 references the name (so the method is either truly absent from this capture, \
                 the pattern is misspelled, or every caller was also inlined away). Searched {}",
                ctx.symbol,
                source_desc,
            )
        })?;

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

/// Fallback when no pre-captured `capture.asm` is available.
///
/// `dotnet run --project <csproj>` appears to drop `DOTNET_JitDisasm`
/// (and friends) somewhere between the `dotnet` CLI host and the user
/// process — an empty capture with zero `; Assembly listing` headers
/// is the observable symptom. `dotnet exec <dll>` bypasses the
/// intermediate host and the env vars reach the JIT. So: for a
/// project target, first `dotnet build` and then resolve the output
/// dll via the `runtimeconfig.json` sibling (only executable outputs
/// emit one); for an already-built dll/exe target, exec it directly.
fn run_jitdasm_fresh(target: &str, symbol: &str) -> Result<String> {
    let dotnet = std::env::var("DOTNET").unwrap_or_else(|_| "dotnet".into());

    let dll_path: std::path::PathBuf = if target.ends_with(".csproj") || target.ends_with(".fsproj")
    {
        // Build the project so bin/Release/net*/ is populated.
        let build = Command::new(&dotnet)
            .args(["build", target, "-c", "Release", "--nologo", "-v", "q"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("invoking {dotnet} build for {target}"))?;
        if !build.status.success() {
            bail!(
                "dotnet build {} failed:\n{}",
                target,
                String::from_utf8_lossy(&build.stderr)
            );
        }
        let proj_dir = std::path::Path::new(target)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        locate_executable_dll(proj_dir).with_context(|| {
            format!(
                "locating built dll under {}/bin/Release/net*/",
                proj_dir.display()
            )
        })?
    } else {
        std::path::PathBuf::from(target)
    };

    let output = Command::new(&dotnet)
        .arg("exec")
        .arg(&dll_path)
        .env("DOTNET_JitDisasm", symbol)
        .env("DOTNET_TieredCompilation", "0") // deterministic tier
        .env("DOTNET_JitDiffableDasm", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("invoking {dotnet} exec {}", dll_path.display()))?;
    // JitDisasm writes to stdout under `dotnet exec` (stderr-vs-stdout
    // varies by host); concatenate both so the parser sees everything.
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if text.trim().is_empty() {
        bail!("dotnet exited {} with no output", output.status);
    }
    Ok(text)
}

/// Find the executable dll in a project's Release output directory.
///
/// Executable projects emit a `<AssemblyName>.runtimeconfig.json` next
/// to the dll — library projects do not. Using its presence as a
/// marker is more reliable than globbing *.dll and guessing which one
/// is the entry point when dependencies are side-by-side.
fn locate_executable_dll(proj_dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let release_root = proj_dir.join("bin").join("Release");
    let tfm_dirs = std::fs::read_dir(&release_root)
        .with_context(|| format!("reading {}", release_root.display()))?;
    // Multiple `net*` TFMs may coexist (e.g. net6.0 + net8.0); prefer
    // the most recently modified so a fresh build wins.
    let mut candidates: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    for tfm in tfm_dirs.flatten() {
        let tfm_path = tfm.path();
        if !tfm_path.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&tfm_path) else { continue };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".runtimeconfig.json"))
            {
                let dll = p.with_extension("").with_extension("dll");
                if dll.is_file() {
                    let mtime = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    candidates.push((mtime, dll));
                }
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates
        .into_iter()
        .next()
        .map(|(_, p)| p)
        .ok_or_else(|| anyhow::anyhow!("no <name>.runtimeconfig.json found under {}/net*/", release_root.display()))
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
    fn jitdasm_collector_isolates_target_method() {
        let text = "\
Hello from pre-JIT chatter.
; Assembly listing for method MyApp.Foo:Bar() (Tier1)
 mov rax, rbx
 ret
; Assembly listing for method MyApp.Baz:Qux()
 mov rcx, rdx";
        let got = JitIndex::parse(text)
            .disasm_with_parent_fallback("MyApp.Foo:Bar")
            .expect("expected match");
        assert!(got.contains("MyApp.Foo:Bar"));
        assert!(got.contains("mov rax, rbx"));
        assert!(!got.contains("MyApp.Baz:Qux"));
    }

    #[test]
    fn jitdasm_collector_normalizes_double_colon() {
        // Callers (and human intuition) often write C++/docs-style
        // `Namespace::Type::Method`; the .NET jitdasm header uses a
        // single `:` between type and method. The collector
        // normalises `::` → `:` before lookup so `dbg disasm
        // "Broken.Program::SumFast"` still works.
        let text = "\
; Assembly listing for method Broken.Program:SumFast(System.Int32[]):int (Tier1)
 vaddps ymm0, ymm0, ymm1
 ret";
        let needle = "Broken.Program::SumFast".replace("::", ":");
        let got = JitIndex::parse(text)
            .disasm_with_parent_fallback(&needle)
            .expect("expected match");
        assert!(
            got.contains("vaddps"),
            "double-colon form did not match single-colon header: got={got:?}"
        );
    }

    #[test]
    fn jitdasm_collector_empty_when_no_match_and_no_callers() {
        let text = "; Assembly listing for method MyApp.X:Y\n mov rax, rbx";
        let got = JitIndex::parse(text).disasm_with_parent_fallback("Other.Method");
        assert!(
            got.is_none(),
            "no standalone body and no caller references should return None: {got:?}"
        );
    }

    #[test]
    fn jitdasm_collector_falls_back_to_parent_on_inlined_target() {
        // Inlined target: no standalone `Helper:Probe` header, but
        // `Outer:Run` contains a call to it. The collector should
        // surface `Outer:Run`'s body with the banner.
        let text = "\
; Assembly listing for method MyNs.Outer:Run():int (FullOpts)
       push     rbp
       call     [MyNs.Helper:Probe(long):int]
       pop      rbp
       ret
; Total bytes of code 9";
        let got = JitIndex::parse(text)
            .disasm_with_parent_fallback("Helper:Probe")
            .expect("expected fallback-to-parent hit");
        assert!(
            got.contains("no standalone body") && got.contains("parent:"),
            "fallback output should carry the banner+parent header: {got}"
        );
        assert!(
            got.contains("MyNs.Outer:Run"),
            "parent method name should be shown: {got}"
        );
        assert!(
            got.contains("call     [MyNs.Helper:Probe"),
            "parent body should be embedded verbatim: {got}"
        );
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
