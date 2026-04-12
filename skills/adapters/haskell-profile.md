# Haskell Profile Adapter (GHC Cost Centres)

## CLI

`dbg start haskell-profile <script.hs> [--args ...]`

For pre-compiled binaries (must be built with `-prof -fprof-late -rtsopts`):

`dbg start haskell-profile ./my-binary [--args ...]`

## What It Profiles

GHC cost-centre profiling measures **time** (CPU ticks) and **allocation** (bytes allocated on the heap) per function. It instruments the compiled Haskell program and attributes costs to cost centres (functions annotated by `-fprof-late`).

**Good at:** finding which functions consume the most CPU time and heap allocation, understanding the call tree, spotting space leaks (excessive allocation).

**Cannot do:** wall-clock time for I/O-bound programs, instruction-level profiling (use `perf` or `callgrind` for that on the compiled binary), live/interactive profiling.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| GHC 9.0+ | `ghc --version` | `curl --proto '=https' --tlsv1.2 -sSf https://get-ghcup.haskell.org \| sh` |

For `.hs` source files, `dbg` compiles automatically with `-prof -fprof-late -rtsopts`.

For pre-compiled binaries, you must compile with:
```
ghc -prof -fprof-late -rtsopts -o myapp Main.hs
```

## Key Commands

| Command | What it does |
|---------|-------------|
| `hotspots [N] [pat]` | Top N functions by inclusive time (default 10) |
| `flat [N] [pat]` | Top N functions by self time (default 20) |
| `calls <pattern>` | What does this function call? |
| `callers <pattern>` | Who calls this function? |
| `inspect <pattern>` | Detailed breakdown of matching functions |
| `stats [pattern]` | Summary statistics |
| `memory [N]` | Top N functions by allocation |
| `search <pattern>` | Find functions matching a pattern |
| `tree [N]` | Call tree from roots (top N branches) |
| `hotpath` | Single most expensive call chain |
| `focus <pattern>` | Filter all commands to matching functions |
| `ignore <pattern>` | Exclude matching functions from all commands |
| `reset` | Clear focus/ignore filters |

## Haskell-Specific Notes

- **Function names** appear as `Module.function` (e.g., `Main.fib`, `Data.List.sort`)
- **CAF** (Constant Applicative Form) entries are top-level thunks — high allocation there means space leaks
- **Allocation is often more useful than time** for Haskell — excessive allocation drives GC pressure
- Use `memory` to find allocation hotspots, then `inspect` to drill into them

## Workflow

1. Start session: `dbg start haskell-profile app.hs`
2. Wait for compilation + profiled run to finish
3. Overview: `dbg hotspots` — find hot functions
4. Allocation: `dbg memory` — find allocation-heavy functions
5. Drill in: `dbg inspect Main.fib` — self vs inclusive cost
6. Call graph: `dbg calls Main.main` and `dbg callers Main.fib`
7. Hot path: `dbg hotpath` — most expensive call chain

## Common Failures

| Symptom | Fix |
|---------|-----|
| "not built for profiling" | Recompile with `-prof -fprof-late -rtsopts` |
| No cost centres | Ensure `-fprof-late` (or `-fprof-auto`) is passed to GHC |
| Only CAFs visible | Your functions are too small / inlined — try `-fprof-auto-calls` |
| Long compile time | `-prof` recompiles everything; first run is slow, subsequent are cached |
| Missing `+RTS` | Binary must be compiled with `-rtsopts` to accept runtime flags |
