# Linux perf Adapter

## CLI

`dbg start perf <perf.data>`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `perf` | `which perf` | `sudo apt install linux-tools-$(uname -r)` |

## Recording Profiles

```bash
# CPU profile with call graphs
perf record -g ./myapp

# CPU profile for 10 seconds of a running PID
perf record -g -p <PID> -- sleep 10

# With specific events
perf record -e cache-misses -g ./myapp

# Stat summary (no recording)
perf stat ./myapp
```

## Key Commands (perf report --stdio)

| Command | What it does |
|---------|-------------|
| `perf report --stdio` | Text-mode overhead report |
| `perf report --stdio --sort=dso` | Group by shared library |
| `perf annotate <func> --stdio` | Instruction-level profile |
| `perf script` | Raw trace (pipe to flamegraph) |
| `perf stat` | Hardware counter summary |

## Flamegraph

```bash
perf script | stackcollapse-perf.pl | flamegraph.pl > flame.svg
```

Or with `inferno`:
```bash
perf script | inferno-collapse-perf | inferno-flamegraph > flame.svg
```

## Common Failures

| Symptom | Fix |
|---------|-----|
| `Permission denied` | `sudo sysctl kernel.perf_event_paranoid=-1` or run as root |
| No symbols | Compile with `-g` or install debug symbols package |
| `perf not found for kernel` | Install matching `linux-tools-$(uname -r)` |
| WSL2 limited | perf requires matching kernel tools — use native Linux or a VM |
