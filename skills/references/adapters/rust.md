# Rust Adapter

For canonical commands and the investigation taxonomy see
[`_canonical-commands.md`](./_canonical-commands.md) and
[`_taxonomy-debug.md`](./_taxonomy-debug.md). This file covers only the
Rust / LLDB specifics.

## CLI

Start: `dbg start rust <crate-name> [--break file.rs:line] [--run]`

`<crate-name>` is the Cargo package name (e.g., `my-crate`), **not** a file path. `dbg` builds (if needed) and locates the binary automatically — do not pass `./target/debug/...`.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `lldb` | `which lldb-20 \|\| which lldb` | `sudo apt install lldb-20` or `brew install llvm` |

If `sudo` is not available, check versioned names (`lldb-18`, `lldb-16`, etc.) with `which lldb-{20,18,16,15,14} 2>/dev/null`. Set `LLDB_BIN=lldb-<version>` if the default `lldb` is not the right one.

## Build

```bash
cargo build -p <crate>          # debug (default)
```

Binary: `target/debug/<crate_name>` (hyphens → underscores). `dbg start` builds automatically if needed.

## Backend: LLDB

Rust targets go through `lldb`. `dbg tool` reports the exact version. Canonical commands translate to standard LLDB vocabulary — see the mapping table in `_canonical-commands.md`.

## Rust-specific breakpoints

| Pattern | When |
|---------|------|
| `dbg break rust_panic` | Catch any panic |
| `dbg break rust_begin_unwind` | Catch unwinding |
| `dbg break file.rs:42` | File and line (most common) |
| `dbg break my_crate::module::function` | Canonical fqn (stripped of `::h<hash>` suffix by the canonicalizer) |

## Type display under LLDB

| Type | How it renders |
|------|----------------|
| `Option<T>` / `Result<T, E>` | `$variants$` layout. `$variant$0` = `None`/`Err`; `$variant$` with `value.__0` = `Some`/`Ok`. |
| `String` / `&str` | Fat pointer — `pointer` field shows UTF-8 bytes. |
| `PathBuf` | Nested `inner.inner…ptr.pointer` — look for `pointer` field. |
| `Vec<T>` | `ptr`, `len`, `cap`. |
| Closures | Canonicalize to `{closure#N}` and flagged `is_synthetic=true` — cross-session joins on closures are ordinal-unstable. |

## Async / Tokio quirks

- Locals appear as `{async_block_env#N}` struct fields — not user-named.
- `tokio-rt-worker` threads are executor threads; your frames are somewhere in the backtrace. `dbg stack` starts at the current frame, walk up with `dbg frame <n>`.
- Break on the function name, not executor internals.

## Known blind spots

| Symptom | Reason / fix |
|---------|--------------|
| Binary not found | Hyphens become underscores: `my-crate` → `target/debug/my_crate`. |
| Breakpoint pending (0 locations) | `dbg raw image lookup --name <partial>` to find the canonical symbol form, then break on that. |
| Variables `<unavailable>` | Step to the assignment line, or ensure debug build. Release binaries elide locals. |
| DWARF indexing slow | Normal on first load; subsequent sessions are cached by LLDB. |
| `::h<hash>` in a stack frame | The canonicalizer strips this suffix in the SessionDb (so cross-session joins work) but LLDB's raw output keeps it. Use `dbg cross <fqn-without-hash>`. |
