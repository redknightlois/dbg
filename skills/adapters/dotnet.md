# .NET Adapter

For canonical commands and the investigation taxonomy see
[`_canonical-commands.md`](./_canonical-commands.md) and
[`_taxonomy-debug.md`](./_taxonomy-debug.md). This file covers only the
.NET / netcoredbg specifics. For .NET JIT disassembly see `jitdasm.md`;
for CPU profiling see `dotnet-trace.md`.

## CLI

`dbg start dotnet <exe-or-dll-or-project-or-csproj> [--break File.cs:line] [--args ...] [--run]`

Aliases: `csharp`, `fsharp`. The CLI prefers the apphost over `.dll`. `.csproj` targets are built automatically before launch. Source files (`.cs`) are rejected with a hint to build first.

**Launch vs. attach.** Prefer launching through `dbg start` — the daemon becomes the parent, so ptrace is always permitted. `--attach-pid <PID>` works against an already-running .NET process, but only if the kernel allows it (see `ptrace_scope` row below). For a service-managed Raven.Server / aspnet host you can't restart, attach is the only option; otherwise launch.

**Flag ordering.** `--args` consumes everything that follows as debuggee arguments. Put `--break` and `--run` **before** `--args`, never after — `dbg start ... --args --ServerUrl=... --break Foo.cs:42` would silently pass `--break Foo.cs:42` to the debuggee. dbg now errors out instead of misrouting, but the rule still applies.

**Cleanup on .NET launch sessions.** dbg sends DAP `disconnect` with `terminateDebuggee: true` on quit and SIGKILLs the tracked debuggee pid as a fallback. If you still see an orphan after `dbg kill`, verify by apphost name (`pgrep -af Raven.Server`, **not** `Raven.Server.dll`) and SIGKILL it.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| .NET SDK | `dotnet --version` | https://dot.net/install |
| `netcoredbg` | `which netcoredbg` or `$NETCOREDBG` | See below |

Install netcoredbg:
```bash
mkdir -p ~/.local/share/netcoredbg
curl -sL https://github.com/Samsung/netcoredbg/releases/latest/download/netcoredbg-linux-amd64.tar.gz | tar xz -C ~/.local/share/netcoredbg
export NETCOREDBG=~/.local/share/netcoredbg/netcoredbg/netcoredbg
```

If `DOTNET_ROOT` is required (homebrew, custom installs), add it to the user's shell profile — not per-command. The daemon inherits env from `dbg start`.

## Backend: netcoredbg-proto (DAP)

Canonical commands translate through netcoredbg's DAP adapter. Translation table in `_canonical-commands.md`. `watch` is unsupported on netcoredbg — use `dbg catch` for exceptions and conditional breaks for state changes.

## .NET-specific breakpoints

| Canonical form | When |
|---|---|
| `dbg break File.cs:42` | File and line |
| `dbg break Namespace.Class.Method` | Fully qualified method |
| `dbg break Module!Namespace.Class.Method` | Method in a specific assembly |
| `dbg catch System.NullReferenceException` | Exception breakpoint |
| `dbg catch System.Exception` | All exceptions |
| `dbg break <loc> if <expr>` | Conditional |
| `dbg break <loc> log "x={x}"` | Logpoint (no stop) |

Breakpoints are **pending** until the assembly loads — this is normal; `dbg run` resolves them.

## Type display

- **Collections**: full internals dumped — focus on `Count`; use `dbg print dict[key]`.
- **Strings**: printed directly.
- **`Nullable<T>`**: `HasValue` + `Value` fields.
- **Async state machines**: `dbg stack` shows `MoveNext()` frames; locals appear as state-machine fields and may hop threads between awaits.

## Cross-track with jitdasm

After a hit, `dbg at-hit disasm` captures the current frame's JIT'd code into the SessionDb. `dbg disasm-diff <sym_a> <sym_b>` highlights tier-0 vs tier-1 codegen differences.

## Known blind spots

| Symptom | Fix |
|---------|-----|
| `COR_E_FILENOTFOUND` | Set `DOTNET_ROOT` in shell profile. |
| Breakpoint stays pending | Normal until `dbg run` loads the module. |
| `.dll` fails to launch | Pass the native apphost executable instead. |
| `dbg list` shows no source | Add `<EmbedAllSources>true</EmbedAllSources>` to csproj. |
| BenchmarkDotNet target | Not supported — BDN spawns isolated child processes. Write a standalone driver. |
| `--attach-pid` → `cannot attach to pid N: kernel.yama.ptrace_scope = 1 ...` | dbg's preflight detected Yama-restricted ptrace. Either relaunch through `dbg start` (preferred — daemon becomes parent) or `sudo sysctl kernel.yama.ptrace_scope=0` for the session. |
| `(no breakpoints set)` after `dbg start` | Pre-fix-A: flag ordering — `--break` must precede `--args`. Current builds error out with a fix-instruction; if you see this on an old binary, upgrade. |
| Port still bound after `dbg kill` | Adapter didn't terminate the launched dotnet host cleanly. Current builds send DAP `disconnect terminateDebuggee=true` and SIGKILL the tracked pid; if it still leaks, find by apphost name (`pgrep -af Raven.Server`) and SIGKILL manually. |
