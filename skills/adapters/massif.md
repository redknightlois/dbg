# Massif Adapter (Valgrind)

## CLI

`dbg start massif <binary> [--args ...]`

## What It Profiles

Massif is a **heap profiler** for native binaries (C, C++, Rust, Zig, Go). It tracks every allocation and builds a timeline of memory usage showing exactly when and where memory was allocated.

- **Peak memory usage** — the exact moment and call stack of maximum heap consumption
- **Allocation timeline** — ASCII graph of memory usage over time
- **Allocation sites** — which functions allocate the most, with full call trees
- **Allocation lifetimes** — how long memory stays allocated

**Cannot do:** stack memory (use `--stacks=yes` but expect 100x slowdown), interpreted languages, real-time profiling.

**Tradeoff:** ~20x slowdown. Lab use only.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `valgrind` | `which valgrind` | `sudo apt install valgrind` |

Compile with `-g` for source-level allocation attribution.

## Key Commands

After the program finishes, the session is a shell with the massif output file:

| Command | What it does |
|---------|-------------|
| `ms_print /tmp/massif.out.dbg` | Full report with ASCII memory graph and allocation trees |
| `ms_print --threshold=1 /tmp/massif.out.dbg` | Show allocations down to 1% (more detail) |

## Reading the Output

`ms_print` shows:

1. **ASCII graph** — memory usage over time, X axis is instructions executed, Y axis is bytes
2. **Snapshots** — detailed breakdown at each measurement point
3. **Peak snapshot** — the allocation tree at maximum memory, showing which functions hold the most

Each tree node shows: bytes allocated, percentage, and the call stack.

## Workflow

1. Start: `dbg start massif ./binary`
2. Wait for program to complete
3. View: `ms_print /tmp/massif.out.dbg` — find peak memory and who allocated it
4. For more detail: `ms_print --threshold=1 /tmp/massif.out.dbg`

## Common Failures

| Symptom | Fix |
|---------|-----|
| No useful allocation sites | Compile with `-g` |
| Graph shows no growth | Program uses little heap — check stack with `--stacks=yes` (slow) |
| Custom allocators invisible | Add `--pages-as-heap=yes` to track mmap-based allocators |
