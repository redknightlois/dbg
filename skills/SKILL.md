---
name: dbg
description: >
  Debug programs and profile performance through a persistent CLI session.
  Triggers on: "debug this", "set a breakpoint", "run under debugger",
  "launch debugger", "debug a target", "why is this crashing", "step through",
  "fix this bug", "find the bug", "track down this issue", "investigate this crash",
  "attach to pid", "post-mortem", "replay a session",
  "this is too slow", "make this faster", "profile this", "find the bottleneck",
  "why is this slow", "where is it spending time", "find the memory leak",
  "check for memory errors", "show the disassembly", "JIT disassembly",
  "what instructions", "is it vectorized", "check codegen", "show assembly",
  "SIMD", "bounds checks", "jitdasm", "diff two runs", "regression hunt",
  "GPU", "CUDA", "kernel", "training is slow", "optimize GPU", "roofline",
  "occupancy", "memory bound", "compute bound", "kernel fusion".
  Also use when you would otherwise guess at runtime state, add print statements,
  or rewrite code without runtime evidence.
---

# dbg

Use `dbg` for runtime state, CPU profiling, memory diagnostics, and codegen. Use
the separate `gdbg` binary for GPU timelines and hardware metrics.

## Choose the tool

Match the backend to the question, not only the language:

| Goal | Tool or backend |
|---|---|
| Debug runtime state | `dbg` with the language backend |
| Profile C, C++, or Rust | `callgrind` or `perf` |
| Find native memory errors or heap growth | `memcheck` or `massif` |
| Profile Python, Node, Go, or .NET | `pyprofile`, `nodeprof`, `pprof`, or `dotnet-trace` |
| Inspect .NET JIT code | `jitdasm` |
| Profile CUDA, PyTorch, Triton, JAX, or GPU kernels | `gdbg` |

Use `dbg start` auto-detection only for the default debugger. Pass the backend
explicitly for profiling, memory analysis, or JIT inspection. Never run
`dbg start gdbg`.

## Load references progressively

Load only the material needed for the current investigation:

| Need | Reference |
|---|---|
| Start a specific debugger or profiler | The matching file under `references/adapters/` |
| Profile GPU code | `references/adapters/gdbg.md` |
| Check exact canonical command support or translation | `references/adapters/_canonical-commands.md` |
| Plan an ambiguous, intermittent, comparative, or CPU/GPU investigation | `references/adapters/_taxonomy-debug.md` |

Do not load unrelated backend references. Backend files contain their own
preconditions, target rules, and tool-specific limitations.

## Core workflow

1. Load the selected backend reference and check its preconditions.
2. Start from the source root so relative targets and breakpoints resolve.
3. Collect the smallest runtime or profile evidence that tests the hypothesis.
4. Compare hits or saved sessions instead of interpreting one sample alone.
5. Interpret the native output for the user.
6. Run `dbg kill` before finishing or switching targets.

```text
dbg start [<type>] <target> [--break <spec>] [--args ...] [--run]
dbg <command>
dbg help [<command>]
dbg cancel
dbg kill
```

Use canonical commands first. Use `dbg raw <native-command>` only when the
canonical vocabulary cannot express the operation. Every breakpoint hit is
captured with locals and a short stack for later `hits`, `hit-diff`,
`hit-trend`, and `cross` queries.

## Sessions

- Use `dbg sessions [--group]` to find captured sessions.
- Use `dbg save [<label>]` before a comparison or prune.
- Use `dbg diff <other>` to compare saved runtime or profile evidence.
- Use `dbg replay <label>` for read-only post-mortem inspection.
- Treat raw captures as durable; schema mismatches require recollection.

Multiple daemons may coexist in one directory and are keyed by session label.
Always run `dbg kill`; leaving the client does not stop the daemon.

## Report findings

- Use plain English.
- Use STE100 wording when a condition, sequence, reference, or causal claim can
  be ambiguous. Use short sentences and one term for one concept.
- Keep command names and program symbols exact.
- Separate observations, inferences, and untested hypotheses.

## Constraints

- Run outside sandboxes because debugging needs process control, PTYs, and
  local sockets.
- Instrument the real workload directly. Process-isolating test runners,
  Docker wrappers, and launchers that spawn the workload hide the child from
  instrumentation; use a standalone driver.
- Put environment configuration in the shell used for `dbg start`; the daemon
  inherits that environment.
- Stop on a preflight error and fix the named dependency.
