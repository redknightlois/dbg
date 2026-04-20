# Callgrind Adapter (Valgrind)

For the taxonomy see [`_taxonomy-debug.md`](./_taxonomy-debug.md). The callgrind REPL is profile-only (no debug track), so canonical debug ops don't apply here — but profile samples get written into the SessionDb and correlate with any debug-track `dbg cross <sym>` query run on the same binary.

## CLI

`dbg start callgrind <binary> [--args ...]`

## What It Profiles

Callgrind profiles **any native binary** — C, C++, Rust, Zig, Go, anything compiled to machine code. It simulates the CPU and counts every instruction executed. No kernel support needed, no sampling — exact, deterministic instruction counts. Same input always produces the same profile.

With branch-prediction simulation enabled (default), callgrind also tracks `Bi` (branches issued) and `Bcm` (branch mispredicts) per function — surfaced in the profile output for finding mispredict hotspots.

**Good at:** function-level instruction cost, finding hot paths deterministically, call graph analysis with exact call counts, branch-mispredict hunting.

**Cannot do:** interpreted languages (Python, Java — it profiles the interpreter, not your code), wall-clock time (measures instructions, not real time — I/O-bound programs look fast), production profiling (20-50x slowdown makes it lab-only).

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `valgrind` | `which valgrind` | `sudo apt install valgrind` |

Compile with `-g` for debug symbols. Do NOT use `-O0` — callgrind profiles instruction counts, and you want realistic optimization behavior.

## How It Works

Callgrind runs the binary under Valgrind's instrumentation. After the program finishes, dbg loads the callgrind output into an interactive profile REPL with the same commands as the PHP and Ruby profilers.

## Key Commands

| Command | What it does |
|---------|-------------|
| `hotspots [N] [pat]` | Top N functions by inclusive time (default 10) |
| `flat [N] [pat]` | Top N functions by self time (default 20) |
| `calls <pattern>` | What does this function call? |
| `callers <pattern>` | Who calls this function? |
| `inspect <pattern>` | Detailed breakdown of matching functions |
| `stats [pattern]` | Summary statistics |
| `search <pattern>` | Find functions matching a pattern |
| `tree [N]` | Call tree from roots (top N branches) |
| `hotpath` | Single most expensive call chain |
| `focus <pattern>` | Filter all commands to matching functions |
| `ignore <pattern>` | Exclude matching functions from all commands |
| `reset` | Clear focus/ignore filters |

## Workflow

1. Start session: `dbg start callgrind ./binary`
2. Wait for valgrind to finish (can take minutes for large programs)
3. Overview: `dbg hotspots` — find hot functions
4. Drill in: `dbg inspect <function>` — self vs inclusive time, callees
5. Call graph: `dbg calls <function>` and `dbg callers <function>`
6. Hot path: `dbg hotpath` — most expensive call chain
7. Focus: `dbg focus <pattern>` — zoom into a subsystem
8. Cross-track: run a debug session on the same binary (e.g. `dbg start cpp ./binary`) and query `dbg cross <hot_func>` to join profile samples with captured debug hits and disassembly.

## Common Failures

| Symptom | Fix |
|---------|-----|
| Too slow | Use a minimal test case — callgrind instruments every instruction |
| No source file info | Compile with `-g`, ensure source is accessible at same path |
| All time in interpreter | Callgrind profiles native code only — use language-specific profilers for Python/Java |
| `Bcm` column always zero | Branch-prediction sim disabled — rerun without `--branch-sim=no` (default is on) |
