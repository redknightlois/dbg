# Import Adapter

For canonical commands and the investigation taxonomy see
[`_canonical-commands.md`](./_canonical-commands.md) and
[`_taxonomy-debug.md`](./_taxonomy-debug.md). This file covers loading
a previously-collected profile snapshot into a fresh dbg session.

## CLI

```
dbg import <profile-file> [--label <name>]
```

Bridges externally-collected traces into the same profile-mode REPL
that fresh `dbg start dotnet-trace` etc. expose. After import, every
profile-mode command is available: `top`, `callers <fn>`, `callees <fn>`,
`traces`, `tree`, `hotpath`, `threads`, `stats`, `search`, `focus`,
`ignore`, `window`, `reset`.

`--label <name>` overrides the auto-generated session slug, so the
imported snapshot shows up in `dbg sessions` under a memorable name and
can be reopened later with `dbg replay <name>`.

## Accepted formats

All five formats below have been verified end-to-end (import → `top` → `kill` → `replay <label>`).

| Extension / shape | Source | Conversion path | Ready in |
|---|---|---|---|
| `.nettrace` | `dotnet-trace collect --output foo.nettrace` | shells out to `dotnet-trace convert --format Speedscope` | ~1–3s for small traces, longer for hundreds-of-MB traces |
| `.speedscope.json` | speedscope native, dotnet-trace's converted output, perf-flame | copied as-is | <1s |
| `.cpuprofile` | Chrome DevTools / V8 / Node `--cpu-prof` | content-detected, no conversion | <1s |
| perf-script text | `perf script > profile.txt` | content-detected, converted in-memory | <1s |
| pprof traces text | `go tool pprof -traces cpu.pprof` | content-detected, converted in-memory | <1s |

Detection is content-based — extension is just a hint. A speedscope
JSON file named `.json` works; a perf-script file named `.txt` works.
The only format that requires a specific extension is `.nettrace`,
because it's binary and only `dotnet-trace convert` understands it.

## Generating each format (recipes)

```bash
# .nettrace
dotnet-trace collect --output run.nettrace --duration 00:00:10 -- ./MyApp

# speedscope (already what we accept)
dotnet-trace convert --format Speedscope run.nettrace -o run
# produces run.speedscope.json

# V8 .cpuprofile
node --cpu-prof --cpu-prof-dir=. --cpu-prof-name=run.cpuprofile ./script.js

# perf-script text
perf record -F 99 -g -o perf.data -- ./mybin
perf script -i perf.data > perf.txt

# pprof traces text (Go)
go tool pprof -traces cpu.pprof > traces.txt
```

## Preconditions

| Requirement | Check | Fix |
|---|---|---|
| `dbg` | `which dbg` | `cargo install dbg-cli` |
| `dotnet-trace` (only for `.nettrace` inputs) | `which dotnet-trace` | `dotnet tool install -g dotnet-trace` and add `~/.dotnet/tools` to PATH |

## Workflow

1. Collect a trace with whatever native tool you prefer:
   ```bash
   dotnet-trace collect --output bench.nettrace -- ./MyApp
   # or:
   perf record -F 999 -g -- ./mybin && perf script > bench.txt
   # or:
   node --cpu-prof --cpu-prof-dir=./prof ./script.js
   ```
2. Import:
   ```bash
   dbg import bench.nettrace --label my-bench
   ```
3. Query — same commands as a live profile session:
   ```bash
   dbg top 30
   dbg callers MyNamespace.HotMethod
   dbg traces 50
   dbg focus Voron
   ```
4. `dbg kill` when done. The session persists under `.dbg/sessions/my-bench.db`.
5. Reopen later: `dbg replay my-bench`.

## Why import vs. re-collect

- The collect-step source code may not exist anymore (post-mortem from
  an external bench machine, CI artifact, customer-supplied trace).
- The collect step is expensive and you want to slice the same data
  many ways without re-running it.
- The original `.nettrace` came from a Windows / different-arch machine
  but you want to analyze it here.

## Known blind spots

| Symptom | Fix |
|---|---|
| `--label` rejected as "sanitizes to empty" | Use `[A-Za-z0-9-_]` characters; non-ASCII and most punctuation collapse to `_`. |
| `.nettrace` import errors with `dotnet-trace: not found` | Install via `dotnet tool install -g dotnet-trace`; add `~/.dotnet/tools` to PATH. |
| `.nettrace` import: `top` returns "unknown option" instead of profile rows | The converter is still running. dbg's PTY is in `bash --norc` mode until convert finishes. Wait until the prompt settles (a few seconds for tiny traces, longer for huge ones), or poll `dbg top` until the first row reads `Function ... Inclusive ... Exclusive`. |
| Speedscope JSON loads but `top` shows nothing | The trace ran too short to record samples. Re-collect with a longer workload, then re-import. |
| `perf-script` imports show only one function (`main.main` etc.) | Frame info missing — built without debug symbols, or `perf record` didn't get user-frame callstacks. Re-run with `perf record -F 99 -g --call-graph=dwarf` (or `fp` for go binaries built with frame pointers). |
| Per-thread filtering wanted | `focus <pattern>` / `ignore <pattern>` — same as live profile sessions. |
| Imported session not in `dbg sessions` after `dbg kill` | Pre-fix: `has_captured_data` didn't count `meta.profile_raw`, so profile-only sessions were silently discarded. Current builds persist correctly; if you see this on an old binary, rebuild. |
