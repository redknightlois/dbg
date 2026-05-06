# .NET Trace Adapter (dotnet-trace)

For the profiling taxonomy see [`_taxonomy-debug.md`](./_taxonomy-debug.md). Profile samples feed the SessionDb, so `dbg cross <sym>` from a debug session on the same binary joins CPU samples with captured hits.

## CLI

`dbg start dotnet-trace <exe-or-dll-or-csproj> [--args ...]`

Pass a `.csproj` and `dbg` builds it before tracing (routed through `resolve_dotnet`).

## What It Profiles

dotnet-trace is the official Microsoft .NET CLI profiler. It captures CPU samples, GC events, thread contention, and custom EventPipe events from any .NET application. The trace is automatically collected, converted to Speedscope format, and loaded into an interactive profile REPL.

- **CPU sampling** — which methods spend the most time on the callstack
- **GC events** — allocation rates, collection pauses, heap sizes
- **Thread contention** — lock waits and thread pool starvation
- **Custom events** — any EventSource/EventPipe provider

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `dotnet` | `which dotnet` | https://dot.net/install |
| `dotnet-trace` | `which dotnet-trace` | `dotnet tool install -g dotnet-trace` |

## Workflow

1. Start: `dbg start dotnet-trace ./myapp`
2. The app runs and trace collects automatically. Wait for completion.
3. Profile data is loaded into memory. All subsequent `dbg` commands query the profile directly.

### Long-running server (profile under load)

`dotnet-trace collect -- <target>` blocks until the target exits, and the speedscope conversion runs only after that. So for a server you want to profile under live load:

1. `dbg start dotnet-trace <server.dll>` — server boots under the collector
2. drive load against it (bench, replay traffic, ...) — do **not** try to query yet; the daemon's accept loop hasn't started
3. `dbg finalize` — SIGINTs the dotnet-trace collector via the daemon's child tree. Collector flushes the `.nettrace`, the daemon runs the speedscope convert step, then transitions into query mode. (Equivalent to externally killing the target — `dbg finalize` just doesn't make you look up the PID.)
4. `dbg top`, `dbg callers <sym>`, etc. now work
5. `dbg kill` to persist the session under `.dbg/sessions/<label>/` for later `dbg replay`

`dbg cancel` does **not** substitute for `dbg finalize` here: `cancel` rides the daemon socket, but the accept loop only starts after init — and init is exactly what's blocked on the collector. `finalize` is client-side: it reads the daemon's pid file and signals the child tree directly.

## Key Commands

After the trace completes, the session enters profile mode. All commands go through `dbg`:

| Command | What it does |
|---------|-------------|
| `dbg top` | Top 20 functions by inclusive time |
| `dbg top 50` | Top 50 functions |
| `dbg callers <func>` | Who calls this function and how much time |
| `dbg callees <func>` | What this function calls |
| `dbg traces` | Top 20 distinct stack traces (text flamegraph) |
| `dbg traces 50` | Top 50 stack traces |
| `dbg tree` | Call tree from root frames |
| `dbg hotpath` | Single hottest execution path |
| `dbg threads` | Number of threads in profile |
| `dbg stats` | Profile metadata (total time, frame count, stack count) |
| `dbg search <pattern>` | Find functions matching a substring |
| `dbg focus <pattern>` | Filter all output to stacks containing pattern |
| `dbg ignore <pattern>` | Exclude stacks containing pattern |
| `dbg reset` | Clear focus/ignore filters |

## Investigation Strategy

1. `top` — identify which functions dominate wall time
2. `callers <hot-func>` — understand why the hot function is called
3. `callees <hot-func>` — see where it spends its time internally
4. `traces` — examine full call stacks for the costliest paths
5. `focus <module>` — narrow to a specific area, re-run top/traces
6. `hotpath` — get the single worst path for targeted optimization

## Common Failures

| Symptom | Fix |
|---------|-----|
| `dotnet-trace` not found | `dotnet tool install -g dotnet-trace` and add `~/.dotnet/tools` to PATH |
| Runtime version mismatch | Set `DOTNET_ROLL_FORWARD=LatestMajor` (already set by dbg) |
| No useful samples | App ran too briefly — use a longer workload |
| Missing method names | Ensure debug symbols are available (build in Debug config) |
| "no stacks recorded" | Trace collected but Speedscope conversion produced empty data |
