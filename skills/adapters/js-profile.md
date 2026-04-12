# JavaScript / Node.js Profiler Adapter

## CLI

Profile a script: `dbg start nodeprof <script.js> [--args ...]`

Open existing profile: `dbg start nodeprof <profile.cpuprofile>`

Also: `dbg start js-profile`

## What It Profiles

V8 CPU sampling profiler. Records which functions are on the call stack at regular intervals (~1ms), then builds a statistical picture of where time is spent.

**Good at:** finding CPU-bound hotspots, call graph analysis, comparing before/after optimizations, profiling real workloads with low overhead.

**Cannot do:** memory profiling (use `--heap-prof` or Chrome DevTools for that), wall-clock accuracy for I/O-bound code (idle time shows as `(idle)`), instruction-level profiling, line-level attribution.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| Node.js 18+ | `node --version` | https://nodejs.org or `nvm install --lts` |

## Build

None. Scripts run directly. For TypeScript, compile first with `tsc` or use `tsx`.

## Key Commands

After profiling completes, the session enters profile mode. All commands go through `dbg`:

| Command | What it does |
|---------|-------------|
| `dbg top [N]` | Top N functions by exclusive (self) time (default 20) |
| `dbg callers <func>` | Who calls this function and how much time |
| `dbg callees <func>` | What this function calls |
| `dbg traces [N]` | Top N distinct stack traces (default 20) |
| `dbg tree [N]` | Call tree from root frames (top N branches) |
| `dbg hotpath` | Single hottest execution path |
| `dbg threads` | Number of threads in profile |
| `dbg stats` | Profile metadata (total time, frame count, stack count) |
| `dbg search <pattern>` | Find functions matching a substring |
| `dbg focus <pattern>` | Filter all output to stacks containing pattern |
| `dbg ignore <pattern>` | Exclude stacks containing pattern |
| `dbg reset` | Clear focus/ignore filters |

## Investigation Strategy

1. `dbg top` — identify which functions dominate CPU time
2. `dbg callers <hot-func>` — understand why the hot function is called
3. `dbg callees <hot-func>` — see where it spends its time internally
4. `dbg traces` — examine full call stacks for the costliest paths
5. `dbg focus <module>` — narrow to your code, re-run top/traces
6. `dbg hotpath` — get the single worst path for targeted optimization

## Pattern Matching

`callers`, `callees`, `search`, `focus`, and `ignore` accept substring patterns:

```
dbg focus app.js           # only functions in app.js
dbg ignore node:internal   # hide Node.js internals
dbg search parse           # find all functions with "parse" in the name
```

## Common Failures

| Symptom | Fix |
|---------|-----|
| No `.cpuprofile` generated | Script may exit too fast — add enough work |
| Profile shows only `(idle)` | Script is I/O-bound, not CPU-bound — `--cpu-prof` only captures on-CPU samples |
| `--cpu-prof` not recognized | Node.js version too old — need 18+ |
| All time in `node:internal` | Use `dbg ignore node:internal` then `dbg top` to focus on your code |
| Function names are `(anonymous)` | Name your functions — `const fn = function myName() {}` or use named exports |
