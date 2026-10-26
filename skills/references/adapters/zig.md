# Zig Adapter

For canonical commands and the investigation taxonomy see
[`_canonical-commands.md`](./_canonical-commands.md) and
[`_taxonomy-debug.md`](./_taxonomy-debug.md). This file covers only the
Zig / LLDB specifics.

## CLI

`dbg start zig <binary> [--break file.zig:line] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `lldb` | `which lldb-20 \|\| which lldb` | `sudo apt install lldb-20` |

Build in Debug mode (default) for usable locals: `zig build`.

## Build

```bash
zig build                    # Debug mode (default)
zig build-exe src/main.zig   # single file
```

## Backend: LLDB

Canonical commands translate to standard LLDB vocabulary — see the mapping table in `_canonical-commands.md`.

## Zig-specific breakpoints

| Canonical form | When |
|---|---|
| `dbg break file.zig:42` | File and line |
| `dbg break main` | Entry point |
| `dbg break std.debug.panic` | Catch Zig panics |

## Type display under LLDB

- **Slices** (`[]T`): `ptr` + `len`. Use `dbg raw memory read <ptr>` for raw bytes.
- **Optionals** (`?T`): discriminant + value, similar to Rust `Option`.
- **Error unions** (`!T`): error code or payload.
- **Strings** (`[]const u8`): slice of bytes — `ptr` shows content.
- **Packed structs**: field order matches declaration; layout may surprise.

## Known blind spots

| Symptom | Fix |
|---------|-----|
| Variables optimized out | Build in Debug mode (`zig build` default). |
| No source for std | Zig std is precompiled — break on your code. |
| Mangled names | Zig mangling differs from C++; look for your module prefix. |
