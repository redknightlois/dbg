# C Adapter

For canonical commands and the investigation taxonomy see
[`_canonical-commands.md`](./_canonical-commands.md) and
[`_taxonomy-debug.md`](./_taxonomy-debug.md). This file covers only the
C / LLDB specifics.

## CLI

`dbg start c <binary> [--break file.c:line] [--run]`

`dbg start <binary>` (no type) auto-detects from ELF. Pass `c` explicitly when you need to force this adapter (e.g., to override auto-detect picking `callgrind`).

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `lldb` | `which lldb-20 \|\| which lldb` | `sudo apt install lldb-20` |

Compile with `-g -O0` for debug symbols and usable locals: `gcc -g -O0 -o myapp main.c`. Source files are rejected — pass the built binary.

## Build

```bash
make                      # if Makefile exists
gcc -g -O0 -o app main.c  # direct
```

## Backend: LLDB

All canonical commands route through LLDB. `dbg tool` reports the exact version. Translation table in `_canonical-commands.md`.

## C-specific breakpoints

| Canonical form | When |
|---|---|
| `dbg break file.c:42` | File and line |
| `dbg break main` | Function entry |
| `dbg break __assert_fail` | Catch assertion failures |
| `dbg catch SIGSEGV` | Signal-based trap (raw: `process handle SIGSEGV -s true`) |

## Type display under LLDB

- **Pointers**: hex addresses; `dbg print *ptr` to dereference.
- **Strings (`char*`)**: content shown directly.
- **Arrays**: `dbg print arr[0]`, or `dbg raw memory read arr --count 10` for raw bytes.
- **Structs**: all fields inline; nested structs expanded.

## Known blind spots

| Symptom | Fix |
|---------|-----|
| Variables `<unavailable>` | Compile with `-g -O0` — optimizations elide locals. |
| No source in `dbg stack` | Binary lacks debug info — rebuild with `-g`. |
| Breakpoint pending | Wrong file path — `dbg raw image lookup --name <partial>` to find the canonical symbol. |
