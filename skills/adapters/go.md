# Go Adapter

For canonical commands and the investigation taxonomy see
[`_canonical-commands.md`](./_canonical-commands.md) and
[`_taxonomy-debug.md`](./_taxonomy-debug.md). This file covers only the
Go / Delve specifics.

## CLI

`dbg start go <binary-or-pkg> [--break file.go:line] [--run]`

Auto-detect: `dbg start <dir>` with a `go.mod` picks this adapter.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `dlv` (Delve) | `which dlv` | `go install github.com/go-delve/delve/cmd/dlv@latest` |

Build with `-gcflags="all=-N -l"` to disable optimizations and inlining — required for usable locals.

## Build

```bash
go build -gcflags="all=-N -l" -o app .
```

## Backend: delve-proto (DAP)

Canonical commands translate to Delve via its DAP transport. `dbg threads` maps to **goroutines**, `dbg thread <n>` switches goroutine. Translation table in `_canonical-commands.md`.

## Go-specific breakpoints

| Canonical form | When |
|---|---|
| `dbg break file.go:42` | File and line |
| `dbg break main.main` | Entry point |
| `dbg break package.Function` | Fully qualified function |
| `dbg break runtime.gopanic` | Catch all panics |
| `dbg break <loc> if <expr>` | Conditional |

## Type display under Delve

- **Strings**: content shown directly.
- **Slices**: `array`, `len`, `cap` fields. `dbg print s[0]` for elements.
- **Maps**: key-value pairs.
- **Interfaces**: concrete type and value.
- **Channels**: buffer contents and state.
- **Goroutines**: `dbg threads` lists all; the SessionDb stores the goroutine id on every captured hit.

## Known blind spots

| Symptom | Fix |
|---------|-----|
| Variables `<optimized away>` | Build with `-gcflags="all=-N -l"`. |
| Can't break in std | Use the fully qualified path: `dbg break runtime.gopanic`. |
| `dlv` not found | `go install github.com/go-delve/delve/cmd/dlv@latest`; ensure `$GOPATH/bin` is on PATH. |
| Goroutine count explodes hit capture | Filter with precise `<loc>` on `dbg hits`; use `dbg hit-trend` for per-goroutine-field trends. |
