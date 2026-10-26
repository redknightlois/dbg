# Canonical Command Vocabulary

`dbg` exposes a uniform command vocabulary across every supported debugger (lldb, pdb, delve, netcoredbg, jdb, node-inspect, …). Learn this one vocabulary instead of 10 tool-specific dialects. The underlying tool is always named in the first line of output (`[via lldb 20.1.0]`, etc.), and `dbg raw <native-cmd>` remains the explicit escape hatch when the canonical layer doesn't cover what you need.

## Quick reference

### Flow control

| Command | Semantic |
|---|---|
| `dbg run [args...]` | Start / restart the target |
| `dbg continue` | Resume until the next stop |
| `dbg step` | Step into |
| `dbg next` | Step over |
| `dbg finish` | Step out |

### Breakpoints

| Command | What it does |
|---|---|
| `dbg break <loc>` | Set breakpoint. `<loc>` is `file:line` \| `symbol` \| `module!method`. |
| `dbg unbreak <id>` | Remove breakpoint by id (from `dbg breaks`). |
| `dbg breaks` | List active breakpoints. |
| `dbg watch <expr>` | Watchpoint on expression. Unsupported on pdb (single-threaded). |

### Inspection at a stop

| Command | What it does |
|---|---|
| `dbg stack [N]` | Current stack (top N frames). |
| `dbg frame <n>` | Select frame `n`. |
| `dbg locals` | Local variables in the current frame. |
| `dbg print <expr>` | Evaluate an expression. |
| `dbg list [loc]` | Show source around the current PC (or at `loc`). |

### Concurrency

| Command | What it does |
|---|---|
| `dbg threads` | List threads. On Go this maps to goroutines. |
| `dbg thread <n>` | Switch to thread/goroutine `n`. |

### Meta

| Command | What it does |
|---|---|
| `dbg tool` | Print the active backend + version + escape-hatch hint. |
| `dbg raw <text>` | Send `<text>` to the debugger verbatim (no canonical translation, no `[via]` header). Use when the canonical layer can't express what you need. |

### Session capture (always on)

Every breakpoint hit is automatically captured into the SessionDb with the frame's locals and a short stack slice. You don't need to ask for it — just keep continuing. The capture powers the cross-track commands below.

| Command | What it does |
|---|---|
| `dbg hits <loc>` | List every captured hit at `<loc>` with a 4-field locals summary. |
| `dbg hit-diff <loc> <a> <b>` | Field-by-field diff of locals between hits #a and #b. |
| `dbg hit-trend <loc> <field>` | Sparkline / table of a single local's values across every hit at `<loc>`. |

### Cross-track (join debug + profile data)

| Command | What it does |
|---|---|
| `dbg disasm [<sym>] [--refresh]` | Collect + show disassembly for `<sym>`; no arg resolves to the current frame's function. Results dedupe on `(symbol_id, source, tier)`. |
| `dbg disasm-diff <sym_a> <sym_b>` | Side-by-side asm diff; highlights codegen differences (tier-0 vs tier-1 .NET, -O0 vs -O2 native). |
| `dbg source <sym> [radius]` | Show source file lines around `<sym>`. |
| `dbg cross <sym>` | **Headline.** One pane: captured hits + profile samples (if any) + JIT events + GC events + disassembly rows + source. Answers "what do I know about this function?". |
| `dbg at-hit disasm` | Convenience: disasm the current frame at the current hit. |

### Sessions

| Command | What it does |
|---|---|
| `dbg sessions [--group]` | List saved sessions under `.dbg/sessions/` (or just the current project group with `--group`). |
| `dbg save [<label>]` | Promote the active session (or a saved one by label) to `created_by=user` so prune won't touch it. |
| `dbg prune [--older-than <dur>] [--all]` | Delete auto sessions older than threshold (default 7 days). `--all` includes user sessions. Durations: `30s`/`5m`/`2h`/`7d`. |
| `dbg diff <other>` | ATTACH `<other>` (label or path) and run a full-outer-join on `(lang, fqn)` showing per-symbol hit-count deltas. |

## Per-backend translation table

All backends that implement `CanonicalOps` translate the canonical verbs into native commands. If you see `[via <tool>]` in the output, the translation worked.

| Canonical | lldb | pdb | delve | netcoredbg |
|---|---|---|---|---|
| `break <file:line>` | `breakpoint set --file F --line L` | `break F:L` | `break F:L` | `break F:L` |
| `break <fqn>` | `breakpoint set --name N` | `break N` | `break N` | `break N` |
| `break <mod>!<method>` | `breakpoint set --shlib M --name M` | `break M:M` | `break M.M` | `break M!M` |
| `continue` | `process continue` | `continue` | `continue` | `continue` |
| `step` | `thread step-in` | `step` | `step` | `step` |
| `next` | `thread step-over` | `next` | `next` | `next` |
| `finish` | `thread step-out` | `return` | `stepout` | `finish` |
| `stack [N]` | `thread backtrace [--count N]` | `where` | `stack [N]` | `backtrace [N]` |
| `locals` | `frame variable` | `pp locals()` | `locals` | `info locals` |
| `print <expr>` | `expression -- <expr>` | `p <expr>` | `print <expr>` | `print <expr>` |
| `watch <expr>` | `watchpoint set variable <expr>` | **unsupported** | `watch -w <expr>` | **unsupported** |
| `threads` | `thread list` | **unsupported** | `goroutines` | `info threads` |
| `thread <n>` | `thread select <n>` | **unsupported** | `goroutine <n>` | `thread <n>` |

Unsupported ops return a clean error that names the tool and tells you to use `dbg raw` for that specific feature.

## Design principles

1. **Tool transparency.** Every canonical command's output starts with `[via <tool> <version>]`. If the canonical vocabulary doesn't cover what you need, `dbg raw <native-cmd>` is always available.
2. **Capture is automatic.** Every `dbg continue` / `dbg step` that lands at a breakpoint writes a row to the SessionDb with locals + stack. No "remember to capture" step.
3. **Sessions persist.** Every `dbg start` creates an auto-labeled session. If it accumulates data, `dbg kill` backs it up to `.dbg/sessions/<label>.db`. User-promoted sessions are never auto-pruned.
4. **No backward compatibility on the DB.** Minor versions may break schema. Old DBs fail to load with a clear re-collect message — there are no migrations, because the raw native captures (`.nsys-rep`, `.perf.data`, `.nettrace`) under `.dbg/sessions/<label>/raw/` are the durable artifact. The SessionDb is a derived index.
