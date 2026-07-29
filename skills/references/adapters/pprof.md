# Go pprof Adapter

## CLI

`dbg start pprof <binary> <profile.prof> [--run]`

Or just a profile file:

`dbg start pprof <profile.prof>`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `go` | `which go` | https://go.dev/dl/ |

## Generating Profiles

```bash
# CPU profile from tests
go test -cpuprofile=cpu.prof -bench=. -benchtime=5s

# CPU profile from running program (requires net/http/pprof import)
curl -o cpu.prof 'http://localhost:6060/debug/pprof/profile?seconds=30'

# Memory profile
go test -memprofile=mem.prof -bench=.

# From running program
curl -o heap.prof http://localhost:6060/debug/pprof/heap
```

## Key Commands

| Command | What it does |
|---------|-------------|
| `top` | Show top functions by cumulative time |
| `top -cum` | Sort by cumulative (includes callees) |
| `list <func>` | Source-annotated profile for a function |
| `peek <func>` | Show callers and callees of a function |
| `tree` | Call tree view |
| `web` | Open in browser (needs graphviz) |
| `traces` | Show all sample traces |
| `tags` | Show tag breakdown |
| `focus <func>` | Restrict analysis to stacks containing func |
| `ignore <func>` | Exclude stacks containing func |
| `reset` | Clear focus/ignore filters |

## Workflow

1. **Start broad**: `top` to find which functions dominate
2. **Zoom in**: `list <hot_func>` to see line-level cost
3. **Understand callers**: `peek <func>` to see who calls the hot path
4. **Filter noise**: `focus <func>` to restrict to relevant stacks
5. **Compare**: generate profiles before/after optimization

## Common Failures

| Symptom | Fix |
|---------|-----|
| No samples | Benchmark ran too briefly — increase `-benchtime` |
| `<unknown>` functions | Binary stripped — rebuild without `-ldflags=-s -w` |
| All time in runtime.* | No user code in hot path — profile is accurate, optimize elsewhere |
| `dbg start pprof <binary>` hangs / prompt timeout | You passed a raw binary instead of a `profile.prof`. `dbg` now detects ELF magic and surfaces this explicitly — pass the generated `.prof` file. |
