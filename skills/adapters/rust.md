# Rust Adapter

## CLI

`$DBG` = `~/.claude/skills/debug/scripts/dbg`

Start: `$DBG start rust <crate-or-binary> [--break file.rs:line] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `lldb` | `which lldb-20 \|\| which lldb` | `sudo apt install lldb-20` |
| `pexpect` | `python3 -c "import pexpect"` | `pip install pexpect` |

Set `LLDB_BIN=lldb-20` if the default `lldb` is too old.

## Build

```bash
cargo build -p <crate>          # debug (default)
```

Binary: `target/debug/<crate_name>` (hyphens → underscores). The `dbg start` command builds automatically if needed.

## Breakpoint Patterns

| Pattern | When |
|---------|------|
| `rust_panic` | Catch any panic |
| `rust_begin_unwind` | Catch unwinding |
| `file.rs:42` | File and line |

## Type Display

- **Option/Result**: `$variants$` layout. `$variant$0` = None/Err, `$variant$` with `value.__0` = Some/Ok
- **String/&str**: Fat pointer — `pointer` field shows UTF-8 bytes
- **PathBuf**: Nested `inner.inner...ptr.pointer` — look for `pointer` field
- **Vec<T>**: `ptr`, `len`, `cap`

## Async / Tokio

- Locals appear as `{async_block_env#N}` struct fields
- `tokio-rt-worker` threads are executor threads — look for your crate's frames in `bt`
- Set breakpoints on the function name, not executor internals

## Common Failures

| Symptom | Fix |
|---------|-----|
| Binary not found | Hyphens become underscores: `my-crate` → `target/debug/my_crate` |
| Breakpoint pending (0 locations) | `image lookup --name <partial>` to find correct symbol |
| Variables `<unavailable>` | Step to assignment line, or ensure debug build |
| DWARF indexing slow | Normal on first load |
