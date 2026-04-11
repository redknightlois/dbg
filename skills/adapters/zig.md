# Zig Adapter

## CLI

`dbg start zig <binary> [--break file.zig:line] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `lldb` | `which lldb-20 \|\| which lldb` | `sudo apt install lldb-20` |

Build with debug info: `zig build` (default Debug mode includes symbols).

## Build

```bash
zig build                    # Debug mode (default)
zig build-exe src/main.zig   # single file
```

## Breakpoint Patterns

| Pattern | When |
|---------|------|
| `file.zig:42` | File and line |
| `main` | Entry point |
| `@panic` | Catch Zig panics |

## Type Display

- **Slices**: Shows `ptr` and `len`. Use `memory read` on ptr for raw data.
- **Optionals** (`?T`): Similar to Rust `Option` — discriminant + value.
- **Error unions** (`!T`): Shows error code or payload.
- **Strings** (`[]const u8`): Slice of bytes — `ptr` field shows content.
- **Packed structs**: May show unexpected layout — field order matches declaration.

## Common Failures

| Symptom | Fix |
|---------|-----|
| Variables optimized out | Build in Debug mode (`zig build` default) |
| No source for std | Zig std is precompiled — breakpoint on your code instead |
| Mangled names | Zig uses its own mangling — look for your module name in bt |
