# .NET JIT Disassembly Adapter

For canonical cross-track queries see [`_canonical-commands.md`](./_canonical-commands.md). `dbg disasm <sym>` inside any .NET debug session will serve pre-captured jitdasm output when available; `dbg disasm-diff <sym_a> <sym_b>` highlights tier-0 vs tier-1 codegen differences across runs.

## CLI

```
dbg start jitdasm <project.csproj> --args <method-pattern>
dbg start jitdasm <project.csproj>                          # captures ALL methods
```

The session builds the project, captures JIT disassembly, indexes it, and drops you into an interactive shell with query commands. The indexed data is also written to the SessionDb, so downstream `dbg cross <sym>` queries pick it up.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| .NET SDK 7+ | `dotnet --version` | https://dot.net/install |

## Commands

After the session starts, use these commands to query the captured disassembly:

| Command | What it does |
|---------|-------------|
| `methods` | List all methods with code sizes, sorted by size (largest first) |
| `disasm <pattern>` | Show full disassembly for methods matching pattern |
| `search <instruction>` | Find methods containing a specific instruction (e.g., `vfmadd`, `RNGCHKFAIL`) |
| `stats` | Summary: method count, total code size, SIMD/FMA/bounds check counts |
| `hotspots [N]` | Top N methods by code size (default 10) |
| `simd` | Find all methods using SIMD instructions |
| `help` | Show available commands |

## Method Pattern Syntax

The .NET JIT uses **single colon** `Class:Method` notation, NOT C++ double-colon `Class::Method`.

| Pattern | Matches |
|---------|---------|
| `SimdOps:DotProduct` | Specific method |
| `SimdOps:*` | All methods in a class |
| `*Distance*` | Contains "Distance" |
| `*` | Everything (default when no args) |

## Workflow

1. **Start a session** — capture all methods or a specific pattern:
   ```
   dbg start jitdasm myproject.csproj
   ```

2. **Get an overview** — check what was captured:
   ```
   dbg stats
   dbg hotspots
   ```

3. **Inspect specific methods**:
   ```
   dbg disasm SimdOps:DotProduct
   ```

4. **Search for instructions**:
   ```
   dbg search vfmadd        # find FMA usage
   dbg search RNGCHKFAIL    # find bounds checks
   dbg simd                   # find all SIMD methods
   ```

5. **Close the session when done** (critical — easily forgotten):
   ```
   dbg kill
   ```
   Exiting the interactive REPL (`exit`, `quit`, Ctrl-D) only closes *your* view. The daemon keeps running, holds the capture file open, and will collide with the next `dbg start`. Treat `dbg kill` as part of the task, not optional cleanup — run it before moving on.

## What to Look For

| Instruction | Meaning |
|-------------|---------|
| `vfmadd231ps` | FMA — fused multiply-add, optimal for dot products |
| `zmm` registers | AVX-512 (512-bit SIMD) |
| `ymm` registers | AVX2 (256-bit SIMD) |
| `xmm` registers | SSE (128-bit SIMD) |
| `CORINFO_HELP_RNGCHKFAIL` | Bounds check — possible missed optimization |
| `vzeroupper` missing | AVX/SSE transition penalty risk |
| `mov [rsp+...]` | Stack spill — register pressure |

## Workflow with a debug session

1. `dbg start jitdasm myapp.csproj` — capture codegen for everything.
2. `dbg kill`, then `dbg start dotnet myapp.csproj --break File.cs:42 --run`.
3. At the hit: `dbg at-hit disasm` serves the pre-captured jitdasm for the current frame; `dbg cross <sym>` joins hits + disasm + source.
4. Run a second baseline (before an optimization): `dbg save before`. After changes: `dbg diff before` shows hit-count deltas, and `dbg disasm-diff <sym_a> <sym_b>` shows the codegen shift.

## Inlined methods (automatic parent fallback)

`capture.asm` only contains a `; Assembly listing for method X` block for methods the JIT emitted **standalone**. Small/hot methods the inliner swallowed at every call site leave no header under their own name — the code still runs, but embedded inside the callers' listings.

`dbg disasm` handles this automatically. When a pattern matches no standalone listing but the capture's call graph names callers, the output is banner-separated and *contains the callers' full disasm* (where the inlined body actually lives):

```
── `FooHelper` has no standalone body — inlined at every call site.
   Showing N caller listing(s); the inlined body is embedded in each. ──

════════ parent: Namespace.Caller:Method(...) ════════
; Assembly listing for method Namespace.Caller:Method(...)
...
```

What you still need to do:

1. **Scan the parent's body for the inlined block.** It appears right after the call-site's argument setup. Look for the operations the callee uniquely emits — a specific mask/shift/compare, a helper call (`CORINFO_HELP_…`), a constant it hard-codes.
2. **If `dbg disasm` reports "no standalone body *and no caller references the name*"**, the target is either absent entirely (not yet JIT-compiled, capture too narrow), misspelled, or every caller was also inlined. Broaden the capture (`--args '*'`) and retry.
3. **Force standalone codegen only as a last resort** — one-off diagnostic, revert after:
   ```csharp
   [MethodImpl(MethodImplOptions.NoInlining)]
   ```
   Leaving it on distorts the real disasm everywhere else.

## Common Failures

| Symptom | Fix |
|---------|-----|
| No methods captured | The target must be an **executable** project (has a `Main`), not a library. Write a small console app that calls the target code. |
| No methods captured (BenchmarkDotNet) | BenchmarkDotNet spawns child processes — dbg can't instrument them. Write a standalone console app instead. |
| `Class::Method` not found | Use single colon: `Class:Method` (JIT convention) |
| Too much output | Use a specific pattern: `--args "Class:Method"` |
| `disasm FooHelper` returns "no methods found" but code clearly runs | Method was **inlined** — see **Inlined methods are invisible** above. |
| App exits immediately | The app needs to run long enough for JIT compilation |
| `dbg start jitdasm` hangs for minutes | Your target runs until killed (QPS bench, server, game loop). See **Long-running targets** below. |

## Long-running targets

`dbg start jitdasm` runs the target to completion via `dotnet run` and captures its stdout. For a QPS benchmark, server, or REPL, that means it *never* returns and the session never opens. **You don't need it to converge — the JIT emits each matched method once, on first compilation.** Options, in preference order:

1. **Bound the capture with `--capture-duration`** (recommended):
   ```
   dbg start jitdasm <csproj> --capture-duration 30s --args 'FlatLongIntMap:*'
   ```
   Wraps the child with `timeout --preserve-status`; 30s is usually enough for cold JIT to emit a matched method set with `DOTNET_TieredCompilation=0`.

2. **Pass a short-run flag through to the project**, if it has one. Everything after the pattern is forwarded as extra args:
   ```
   dbg start jitdasm <csproj> --args 'FlatLongIntMap:*' -- --iterations 1 --warmup 0
   ```

3. **Point at a small driver instead of the benchmark harness.** A console app that calls the target API once exits cleanly and emits the same disasm. This is the cleanest option when you'll jitdasm the same code repeatedly.

Avoid: killing `dbg start` with Ctrl+C mid-run. The adapter can't distinguish "finished normally" from "killed"; `--capture-duration` uses `--preserve-status` so a graceful timeout still looks like success.
