# Callgrind Adapter (Valgrind)

## CLI

`dbg start callgrind <binary> [--args ...]`

## What It Profiles

Callgrind profiles **any native binary** — C, C++, Rust, Zig, Go, anything compiled to machine code. It simulates the CPU and counts every instruction executed. No kernel support needed, no sampling — exact, deterministic instruction counts. Same input always produces the same profile.

**Good at:** line-level instruction cost with source annotation, finding hot paths deterministically, cache simulation (with `--cache-sim=yes` for L1/LL cache misses).

**Cannot do:** interpreted languages (Python, Java — it profiles the interpreter, not your code), wall-clock time (measures instructions, not real time — I/O-bound programs look fast), production profiling (20-50x slowdown makes it lab-only).

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `valgrind` | `which valgrind` | `sudo apt install valgrind` |

Compile with `-g` for debug symbols. Do NOT use `-O0` — callgrind profiles instruction counts, and you want realistic optimization behavior.

## How It Works

Callgrind runs the binary under Valgrind's instrumentation. The session starts a shell. Valgrind runs as an init command and writes `callgrind.out.dbg`. Then you query results with `callgrind_annotate`.

## Key Commands

All commands are shell commands — prefix with `callgrind_annotate`:

| Command | What it does |
|---------|-------------|
| `callgrind_annotate /tmp/callgrind.out.dbg` | Full annotated report |
| `callgrind_annotate --auto=yes /tmp/callgrind.out.dbg` | Source-annotated report |
| `callgrind_annotate --tree=both /tmp/callgrind.out.dbg` | Show caller/callee trees |
| `callgrind_annotate --threshold=95 /tmp/callgrind.out.dbg` | Only functions in top 95% |

## Workflow

1. Start session: `dbg start callgrind ./binary`
2. Wait for "callgrind data ready" (can take minutes for large programs)
3. Overview: `callgrind_annotate /tmp/callgrind.out.dbg` — find hot functions
4. Source detail: `callgrind_annotate --auto=yes /tmp/callgrind.out.dbg` — line-level cost
5. Call tree: `callgrind_annotate --tree=both /tmp/callgrind.out.dbg` — who calls the hot path

## Common Failures

| Symptom | Fix |
|---------|-----|
| Too slow | Use a minimal test case — callgrind instruments every instruction |
| No source annotation | Compile with `-g`, ensure source is accessible at same path |
| `callgrind.out.dbg` not found | Valgrind may have crashed — check events |
| All time in interpreter | Callgrind profiles native code only — use language-specific profilers for Python/Java |
