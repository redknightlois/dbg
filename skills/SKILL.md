---
name: dbg
description: >
  Debug programs and profile performance through a persistent CLI session.
  Triggers on: "debug this", "set a breakpoint", "run under debugger",
  "launch debugger", "debug <target>", "why is this crashing", "step through",
  "fix this bug", "find the bug", "track down this issue", "investigate this crash",
  "attach to pid", "post-mortem", "replay a session",
  "this is too slow", "make this faster", "profile this", "find the bottleneck",
  "why is this slow", "where is it spending time", "find the memory leak",
  "check for memory errors", "show the disassembly", "JIT disassembly",
  "what instructions", "is it vectorized", "check codegen", "show assembly",
  "SIMD", "bounds checks", "jitdasm", "diff two runs", "regression hunt",
  "GPU", "CUDA", "kernel", "training is slow", "optimize GPU", "roofline",
  "occupancy", "memory bound", "compute bound", "kernel fusion".
  Also use when you would otherwise guess at runtime state — if you're about to
  add print statements, re-read the same function a third time, speculate about
  variable values, or rewrite code hoping the bug disappears, use dbg instead.
---

# dbg

Persistent CLI for debugging, profiling, and JIT disassembly. One canonical command vocabulary across every supported backend. Every breakpoint hit is captured into a SessionDb so you can diff, trend, and cross-reference runs. Run `dbg` with no arguments to list backends and their readiness.

## Pick the right backend

Backend type is NOT always a language name — match the user's **goal**:

| Goal | Backend type | NOT |
|------|-------------|-----|
| Debug .NET code | `dotnet` / `csharp` / `fsharp` | |
| Profile .NET code | `dotnet-trace` | `dotnet` |
| .NET JIT disassembly | `jitdasm` | `dotnet` |
| Debug JS/TS/Node/Bun/Deno | `node` | |
| Profile JS/TS/Node.js | `nodeprof` | `node` |
| Profile C/C++/Rust | `callgrind` or `perf` | `lldb` |
| Memory errors (C/C++/Rust) | `memcheck` | `lldb` |
| Heap profiling (C/C++/Rust) | `massif` | `lldb` |
| Debug Python | `python` | |
| Profile Python | `pyprofile` / `pstats` | `python` |
| Debug Go | `go` (delve) | |
| Profile Go | `pprof` | |
| Debug Java/Kotlin | `jdb` | |
| Debug Haskell / OCaml / Ruby / PHP | `ghci` / `ocamldebug` / `rdbg` / `phpdbg` | |
| Profile Haskell / Ruby / PHP | `ghc-profile` / `stackprof` / `xdebug-profile` | |
| **GPU kernels / CUDA / PyTorch / Triton** | **`gdbg` (separate binary)** | `dbg` |

`dbg start` without a `<type>` auto-detects from the target's file extension. Pass `<type>` explicitly when you need a non-default (e.g. `callgrind` instead of `lldb` for the same C++ binary).

## GPU profiling — when to use `gdbg` instead of `dbg`

If the target imports `torch`, `triton`, `tensorflow`, `jax`, `cupy`, `mxnet`, or uses `.cuda()` / `.to('cuda')`, or if the user asks about GPU kernels, kernel fusion, memory bandwidth, SM occupancy, roofline, or training throughput — use `gdbg`, not `dbg`.

`gdbg` is a **separate binary** (installed alongside `dbg`). It is NOT a dbg backend — do not try `dbg start gdbg`. Load `adapters/gdbg.md` for its commands.

## Usage

```
dbg start [<type>] <target> [--break <spec>] [--args ...] [--run]
dbg start --attach-pid <PID>              # attach to a running process (DAP backends)
dbg start --attach-port <PORT>            # attach to a debug port
dbg <command>                             # send command to running session
dbg cancel                                # SIGINT the child without tearing down the session
dbg replay <label>                        # open a persisted session read-only
dbg help                                  # list commands the backend supports
dbg help <command>                        # ask the backend what a command does
dbg kill                                  # stop session (always do this when done)
```

Multiple daemons can coexist in the same cwd; sessions are keyed by label.

## Canonical command vocabulary

One vocabulary, every backend. The underlying tool is always named in the first line of output (`[via lldb 20.1.0]`). Full table in `adapters/_canonical-commands.md`. Short form:

- **Flow:** `run`, `continue`, `step`, `next`, `finish`, `pause`, `restart`
- **Breakpoints:** `break <file:line|symbol|module!method>`, `unbreak <id>`, `breaks`, `watch <expr>`, `catch <exception>`, conditional via `break <loc> if <expr>`, logpoints via `break <loc> log <msg>`
- **Inspection at a stop:** `stack [N]`, `frame <n>`, `locals`, `print <expr>`, `set <var>=<expr>`, `list [loc]`
- **Concurrency:** `threads`, `thread <n>` (maps to goroutines on Go)
- **Meta / escape:** `dbg tool` (prints active backend), `dbg raw <native-cmd>` (passthrough, no `[via]` header)

When the canonical vocabulary can't express something, use `dbg raw` — don't invent a new canonical command.

## Hit capture & cross-track queries (always on)

Every hit is written to the SessionDb with locals and a short stack slice. Query across captures instead of re-stepping.

- `dbg hits <loc>` — every hit at `<loc>` with a 4-field locals summary
- `dbg hit-diff <loc> <a> <b>` — field-by-field diff between hits
- `dbg hit-trend <loc> <field>` — sparkline of a local across every hit
- `dbg cross <sym>` — **headline**: hits + profile samples + JIT/GC events + disasm + source in one pane
- `dbg disasm [<sym>]`, `dbg disasm-diff <a> <b>`, `dbg source <sym>`
- `dbg at-hit disasm` — disasm the current frame at the current hit

Discipline: **diff, don't just look.** One hit tells you nothing; two adjacent hits with one changed field tell a story. See `adapters/_taxonomy-debug.md` for question-driven workflows (Hotspots → Analysis → Timeline → Drill-down).

## Session lifecycle

- Every `dbg start` creates an auto-labeled session. On `dbg kill`, if it saw any hits/layers, it's backed up to `.dbg/sessions/<label>/`.
- `dbg sessions [--group]` — list saved sessions (`--group` = same `(cwd, target)` only)
- `dbg save [<label>]` — promote a session so `dbg prune` won't touch it
- `dbg prune [--older-than 7d] [--all]` — clear old auto sessions
- `dbg diff <other>` — full-outer-join on `(lang, fqn)`, ranked by hit-count delta. The regression-hunting primitive.
- `dbg replay <label>` — open a persisted session read-only (post-mortem)

No DB migrations: schema mismatches fail loudly and ask you to re-collect. The raw captures under `.dbg/sessions/<label>/raw/` are the durable artifact.

## Preconditions & adapters

Before `dbg start`, load the matching file under `skills/adapters/`. Each adapter lists its preconditions, native-specific notes, and any commands that only exist on that backend. Two files are required reading before any session:

- `adapters/_canonical-commands.md` — the full canonical vocab + per-backend translation table
- `adapters/_taxonomy-debug.md` — how to turn a question into the right 2-3 commands

## Sandbox warning

Requires process control (fork, ptrace, PTY, DAP sockets). Will fail inside sandboxes — use unsandboxed execution.

## Invalid targets

`dbg` instruments the process it launches directly. Anything that spawns its own child workload **will not work** because the child doesn't inherit instrumentation. Write a small standalone driver instead.

Examples that **do NOT work**:
- **BenchmarkDotNet** projects — isolated child process per benchmark
- **Test runners with process isolation** (xunit `parallelizeAssembly`, NUnit process-level isolation)
- **Docker-wrapped** executables
- Any launcher that `fork+exec`s the real workload

## Rules

- Load the adapter first. Backend-specific knowledge lives there, not here.
- Check preconditions from the adapter before starting.
- Always `dbg kill` when done — leaked processes hold file locks.
- Interpret output for the user — translate mangled names, summarize state, name the hypothesis each diff tests.
- **Never prepend env vars to every `dbg` command.** The daemon inherits its environment from `dbg start`. If a tool needs env vars (e.g. `DOTNET_ROOT`), tell the user to set them in their shell profile once.
- When preflight fails, read the error — it names the missing dependency. Don't retry blindly.
