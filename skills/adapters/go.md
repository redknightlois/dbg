# Go Adapter

## CLI

`dbg start go <binary> [--break file.go:line] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `dlv` (Delve) | `which dlv` | `go install github.com/go-delve/delve/cmd/dlv@latest` |

Build with debug info: `go build -gcflags="all=-N -l"` (disables optimizations and inlining).

## Build

```bash
go build -gcflags="all=-N -l" -o app .
```

## Breakpoint Patterns

| Pattern | When |
|---------|------|
| `file.go:42` | File and line |
| `main.main` | Entry point |
| `package.Function` | Fully qualified function |
| `runtime.gopanic` | Catch all panics |

## Type Display

- **Strings**: Shown directly with content.
- **Slices**: Shows `array`, `len`, `cap`. Use `print s[0]` for elements.
- **Maps**: Shows key-value pairs.
- **Interfaces**: Shows concrete type and value.
- **Goroutines**: `goroutines` command lists all; `goroutine N` switches.
- **Channels**: Shows buffer contents and state.

## Goroutines

Delve is goroutine-aware:
```
goroutines                    # list all goroutines
goroutine 1                   # switch to goroutine 1
bt                            # bt of current goroutine
```

## Common Failures

| Symptom | Fix |
|---------|-----|
| Variables `<optimized away>` | Build with `-gcflags="all=-N -l"` |
| Can't set breakpoint in std | Use fully qualified path: `runtime.gopanic` |
| `dlv` not found | `go install github.com/go-delve/delve/cmd/dlv@latest` |
