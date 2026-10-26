# Memcheck Adapter (Valgrind)

## CLI

`dbg start memcheck <binary> [--args ...]`

Or using the `valgrind` alias:

`dbg start valgrind <binary> [--args ...]`

## What It Detects

Memcheck finds memory errors in **any native binary** (C, C++, Rust, Zig, Go). It instruments every memory access at runtime — no false negatives for the errors it checks.

- **Use after free** — accessing memory that was already freed
- **Uninitialized reads** — using values that were never written (with `--track-origins=yes`, shows where the uninitialized value came from)
- **Buffer overflows** — reading/writing past allocated bounds
- **Memory leaks** — allocated memory never freed (with `--leak-check=full`)
- **Double free** — freeing memory twice
- **Mismatched alloc/free** — `new[]` with `delete`, `malloc` with `delete`, etc.

**Cannot detect:** logic bugs, data races (use helgrind), stack overflows beyond the guard page.

**Tradeoff:** ~10-20x slowdown, ~2x memory overhead. Lab use only.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `valgrind` | `which valgrind` | `sudo apt install valgrind` |

Compile with `-g` for debug symbols and line numbers in error reports.

## Reading the Output

Memcheck reports errors as the program runs. Each error shows:

1. **Error type** — e.g., "Invalid read of size 4"
2. **Stack trace** — where the bad access happened
3. **Origin** — where the problematic memory was allocated or freed

At exit, it prints a **leak summary** with categories:
- **definitely lost** — memory leaked, no pointer to it exists
- **indirectly lost** — leaked because the pointer to it was itself leaked
- **possibly lost** — pointer exists but points to middle of block
- **still reachable** — pointer exists at exit (often not a real leak)

## Workflow

1. Start: `dbg start memcheck ./binary`
2. Wait for program to complete (errors print during execution)
3. Read the error report — focus on "Invalid read/write" and "definitely lost"
4. Each error has a stack trace with file:line — go fix the source

## Common Failures

| Symptom | Fix |
|---------|-----|
| No line numbers | Compile with `-g` |
| "Uninitialised value" everywhere | Add `--track-origins=yes` (already default in dbg) to find the source |
| Too many errors | Add `--max-stackframe=8000000` for deep stacks, or `--suppressions=<file>` for known issues |
| Rust false positives | Rare — Rust's allocator is memcheck-compatible. If you see noise, check unsafe blocks |
