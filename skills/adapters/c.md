# C Adapter

## CLI

`dbg start c <binary> [--break file.c:line] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `lldb` | `which lldb-20 \|\| which lldb` | `sudo apt install lldb-20` |

Compile with `-g` for debug symbols: `gcc -g -o myapp main.c`

## Build

```bash
make            # if Makefile exists
gcc -g -o app main.c   # or direct
```

## Breakpoint Patterns

| Pattern | When |
|---------|------|
| `file.c:42` | File and line |
| `main` | Function entry |
| `__assert_fail` | Catch assertion failures |

## Type Display

- **Pointers**: Shown as hex addresses. Use `print *ptr` to dereference.
- **Strings** (`char*`): LLDB shows the string content directly.
- **Arrays**: `print arr[0]` for elements, `memory read arr --count 10` for raw.
- **Structs**: All fields shown inline. Nested structs are expanded.

## Common Failures

| Symptom | Fix |
|---------|-----|
| Variables `<unavailable>` | Compile with `-g -O0` |
| No source in bt | Binary lacks debug info — rebuild with `-g` |
| Breakpoint pending | Wrong file path — use `image lookup --name <partial>` |
