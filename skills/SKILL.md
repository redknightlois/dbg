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

Persistent CLI for debugging, profiling, and JIT disassembly. Keeps the backend alive across commands via a UNIX socket daemon.

## When to Use

- **Debugging** — understand runtime state: variable values, execution flow, thread state
- **Profiling** — find bottlenecks, memory leaks, time spent (pprof, perf, callgrind, memcheck, massif, cProfile, dotnet-trace, stackprof, xdebug, GHC profiling)
- **JIT disassembly** — inspect generated machine code: SIMD vectorization, bounds checks, register allocation, codegen quality (`dbg start jitdasm`)

## When Not to Use

- Pure logic bugs obvious from reading the code
- Observing patterns across many execution points simultaneously where setting that many breakpoints is impractical — use logging instead

## Sandbox Warning

Debugging requires process control (fork, ptrace, PTY). If running inside a sandbox (e.g., Codex with bubblewrap), `dbg start` will fail. Run dbg commands without sandboxing.

---

## Getting Started

1. **Run `dbg` with no arguments** to see all backends and their status.

2. **Pick the right backend.** Match the user's goal to a backend type — this is not always a language name. JIT disassembly uses `jitdasm`, not `dotnet`. Profiling uses `callgrind`, `perf`, `pyprofile`, etc.

3. **Load the adapter** from `references/adapters/` matching the backend. The adapter defines preconditions, commands, and workflows. If no adapter exists, ask the user what tool they use.

4. **Check preconditions** from the adapter. If any fail, report the failure and the adapter's fix. On any subsequent failure, re-check preconditions before retrying.

## Starting a Session

```
dbg start <type> <target> [--break <spec>] [--args ...] [--run]
```

The `<type>` comes from the backend list. The `<target>` is backend-specific — check the adapter. The daemon stays alive until `dbg kill`.

## Debugging Commands

```
dbg run                    # hit the breakpoint
dbg bt                     # backtrace
dbg locals                 # local variables
dbg print <expr>           # evaluate expression
dbg next                   # step over
dbg step                   # step into
dbg finish                 # step out
dbg continue               # run to next breakpoint
dbg break <spec>           # add breakpoint
dbg help                   # list available commands
```

Keep investigating — do not ask "should I continue?" — until the goal is met or you need user input on an ambiguity.

## When Done

1. `dbg kill` — always stop the session. Leaked debugger processes hold file locks.
2. Report: root cause (or what was ruled out), file:line, observed state, suggested fix.
3. If the user wants a fix, implement it directly.

## Rules

- Load the adapter first. Everything backend-specific lives there, not here.
- Check preconditions on first failure before retrying anything.
- Never modify the dbg CLI or its daemons. They are tested infrastructure.
- Interpret debugger output — translate mangled names, explain type layouts, summarize state. Raw output is noisy; your value is synthesis.
