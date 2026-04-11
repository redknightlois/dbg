---
name: dbg
description: >
  Debug programs and profile performance through a persistent CLI session.
  Triggers on: "debug this", "set a breakpoint", "run under debugger",
  "launch debugger", "debug <target>", "why is this crashing", "step through",
  "fix this bug", "find the bug", "track down this issue", "investigate this crash",
  "this is too slow", "make this faster", "profile this", "find the bottleneck",
  "why is this slow", "where is it spending time", "find the memory leak",
  "check for memory errors".
  Also use when you would otherwise guess at runtime state — if you're about to
  add print statements, re-read the same function a third time, speculate about
  variable values, or rewrite code hoping the bug disappears, use dbg instead.
---

# Debug

Debug programs through a persistent session managed by a CLI tool that keeps the debugger alive across commands via a UNIX socket daemon.

## When to Use

When you need to understand internal runtime state — what values variables actually hold, which branch was taken, what order things execute in — the debugger gives you ground truth that reading code can only guess at. Prefer this over speculating about state from source.

- Understanding runtime state that is ambiguous from code alone
- Investigating crashes, panics, segfaults, exceptions, or unexpected behavior
- Verifying assumptions about execution flow
- Inspecting state across threads, async tasks, or deep call chains where mental simulation breaks down

## When Not to Use

- Pure logic bugs discoverable by reading code or adding print statements
- Performance profiling (use `perf`, `flamegraph`, or language-specific profilers)

---

## Sandbox Warning

Debugging requires process control (fork, ptrace, PTY). If you are running inside a sandbox (e.g., Codex with bubblewrap), `dbg start` will fail because the sandbox blocks these operations. You must run `dbg` commands **without sandboxing**. In Codex, prefix with unsandboxed execution or use `--dangerously-bypass-approvals-and-sandbox`.

---

## Phase 0: Orient

1. **Identify the language.** Determine the target language from file extensions, build system, or user context. Find the matching adapter at `references/adapters/`. **Load it now** — it defines everything language-specific: which CLI tool to use, preconditions, build commands, breakpoint patterns, type display, and idioms.

   If no adapter exists, ask the user what debugger they use. If the session reveals reusable patterns, create an adapter afterward (Phase 3).

2. **Check preconditions.** Run the adapter's precondition checks. If any fail, report the specific failure and the adapter's fix. Do not proceed until preconditions are met. **On any subsequent failure during the session, re-check preconditions before retrying.**

3. **Understand the target.** What does the user want to debug? Use the adapter's build/discovery commands if needed.

4. **Understand the goal.** What is the user investigating?
   - A crash or exception → use the adapter's panic/exception breakpoint pattern
   - A specific function → breakpoint by name or file:line
   - Unexpected state → breakpoint and inspect locals
   - "I don't know where" → start at entry and step through

---

## Phase 1: Launch and Investigate

### Start the session

```
dbg start <type> <target> --break <spec> [--break <spec2>] [--args ...] [--run]
```

Where `<type>` is `rust`, `python`, or `dotnet`. The daemon stays alive until `dbg kill`.

### Investigate progressively

Each pass digs deeper. Do not ask "should I continue?" — keep going until the goal is met or you need user input on an ambiguity.

**Pass 1: Get to the point of interest**
```
dbg run                    # hit the breakpoint
dbg bt                     # where are we?
dbg locals                 # what state do we have?
```

**Pass 2: Inspect specific state**
```
dbg print <expr>           # evaluate expression
dbg cmd "<inspect cmd>"    # adapter-specific inspection
```

**Pass 3: Trace execution**
```
dbg next                   # step over
dbg step                   # step into
dbg finish                 # step out
dbg continue               # run to next breakpoint
```

**Pass 4: Widen the investigation**
```
dbg status                 # full state overview
dbg break <new-spec>       # add breakpoints based on findings
```

**Pass 5: Deep inspection** — use adapter-specific commands for memory, threads, in-process execution, etc.

### Stop conditions

- **Goal met**: Root cause identified. Report findings and stop.
- **Need code change**: Stop session, fix, rebuild/restart, re-launch.
- **Stuck**: Report what was tried. Suggest alternatives from the adapter's failure modes.

---

## Phase 2: Report and Clean Up

1. **Stop the session**: `dbg stop`

2. **Report findings**:
   - Root cause (or what was ruled out)
   - File:line of the problematic code
   - Observed state (variable values, thread state)
   - Suggested fix if the cause is clear

3. **If the user wants a fix**, implement it directly.

---

## Phase 3: Retrospective (adapter mutation)

After a session that surfaced new patterns, update the domain adapter:

- New breakpoint patterns → "Breakpoint Patterns"
- Type display tricks → "Type Display"
- Build/run quirks → "Build"
- New failure modes → "Common Failure Modes"

**Mutation Direction Rule**: every change must make the adapter simpler, more general, or less error-prone. Never more complex. Merge similar entries. Delete what didn't help.

If no adapter existed and the session produced enough reusable patterns, create one at `references/adapters/<language>.md`.

---

## Rules

- Load the adapter first. Everything language-specific lives there, not here.
- Check preconditions on first failure before retrying anything.
- Always stop the session when done. Leaked debugger processes hold file locks.
- Never modify the debug CLI scripts or their daemons. They are tested infrastructure.
- Interpret debugger output for the user — translate mangled names, explain type layouts, summarize state. Raw output is noisy; your value is synthesis.
