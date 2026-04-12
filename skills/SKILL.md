---
name: dbg
description: >
  Debug programs and profile performance through a persistent CLI session.
  Triggers on: "debug this", "set a breakpoint", "run under debugger",
  "launch debugger", "debug <target>", "why is this crashing", "step through",
  "fix this bug", "find the bug", "track down this issue", "investigate this crash",
  "this is too slow", "make this faster", "profile this", "find the bottleneck",
  "why is this slow", "where is it spending time", "find the memory leak",
  "check for memory errors", "show the disassembly", "JIT disassembly",
  "what instructions", "is it vectorized", "check codegen", "show assembly",
  "SIMD", "bounds checks", "jitdasm".
  Also use when you would otherwise guess at runtime state — if you're about to
  add print statements, re-read the same function a third time, speculate about
  variable values, or rewrite code hoping the bug disappears, use dbg instead.
---

# dbg

Persistent CLI for debugging, profiling, and JIT disassembly. Run `dbg` with no arguments to see all available backends.

## Pick the right backend

The backend type is NOT always a language name. Match the user's goal:

| Goal | Backend type | NOT |
|------|-------------|-----|
| Debug .NET code | `dotnet` | |
| Profile .NET code | `dotnet-trace` | `dotnet` |
| .NET JIT disassembly | `jitdasm` | `dotnet` |
| Profile C/C++/Rust | `callgrind` or `perf` | `lldb` |
| Memory errors | `memcheck` | `lldb` |
| Heap profiling | `massif` | `lldb` |

**Load the adapter** from `references/adapters/` matching the backend type. The adapter has preconditions, commands, and workflows. Follow the adapter.

## Usage

```
dbg start <type> <target> [--break <spec>] [--args ...] [--run]
dbg <command>              # send command to running session
dbg help                   # list commands the backend supports
dbg kill                   # stop session (always do this when done)
```

## Sandbox Warning

Requires process control (fork, ptrace, PTY). Will fail inside sandboxes. Use unsandboxed execution.

## Rules

- Load the adapter first. Everything backend-specific lives there.
- Check preconditions from the adapter before starting.
- Always `dbg kill` when done — leaked processes hold file locks.
- Interpret output for the user — translate mangled names, summarize state.
