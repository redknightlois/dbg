# Debug-Track Taxonomy

Organize your investigation by **question**, not by debugger command. Each question maps to one or two canonical commands. Follow the order Hotspots → Analysis → Timeline → Drill-down → Filtering and you'll rarely flail.

The star feature of the debug track is **hit capture**: every time your breakpoint fires, `dbg` records the frame's locals + a short stack slice into the SessionDb automatically. That turns "set breakpoint, step, read value, forget, repeat" into "set breakpoint, continue N times, query across captures." Questions below lean on this heavily.

## Hotspots — "where is execution spending its time / hitting the most?"

| Question | Canonical commands |
|---|---|
| Which code paths am I reaching repeatedly? | `dbg break <loc>` → a few `dbg continue` → `dbg hits <loc>` |
| What are the N most-hit locations across this session? | `dbg sessions --group` shows peers; query `breakpoint_hits` via `dbg cross` |

Tip: put the breakpoint at a loop body or a per-request handler entry; 5-10 continues is usually enough to see a pattern.

## Analysis — "how do the hits differ?"

| Question | Canonical commands |
|---|---|
| Did a local change between hit #a and hit #b? | `dbg hit-diff <loc> <a> <b>` |
| How does a single local trend across every hit? | `dbg hit-trend <loc> <field>` (sparkline for numerics) |
| What does the JIT / compiler emit for the function I'm in? | `dbg at-hit disasm` (or `dbg disasm <sym>`) |
| How does version A's codegen compare to version B's? | Collect `dbg disasm` under each, then `dbg diff <other>` + `dbg disasm-diff` |
| What's in memory / on the heap at this point? | `dbg print <expr>` for ad-hoc; `dbg locals` for the whole frame |

**Discipline:** diff, don't just look. A single hit's locals might mean anything; two adjacent hits with one changed field tell you a story.

## Timeline — "when / in what order did things happen?"

| Question | Canonical commands |
|---|---|
| What happened before this hit? | `dbg stack` — captured automatically on every hit |
| What's the sequence of hits at this location? | `dbg hits <loc>` — rows ordered by `hit_seq` |
| Which thread / goroutine was running? | `dbg threads` / `dbg thread <n>`; `breakpoint_hits.thread` is captured |

## Drill-down — "I have a suspect. Tell me everything about it."

`dbg cross <symbol>` is the headline: one pane showing every row keyed to that symbol across the debug and profile tracks.

| Question | Canonical commands |
|---|---|
| Everything you know about this function | `dbg cross <sym>` |
| Show me the source around it | `dbg source <sym>` |
| Show me the compiled / JIT'd code | `dbg disasm <sym>` |
| Where is it called from? | `dbg stack` at a hit (then `dbg frame <n>` + `dbg locals` at each caller) |

## Filtering — "narrow the set"

| Question | Canonical commands |
|---|---|
| Ignore noise from frequent passes | Use precise `<loc>` filters on `dbg hits` / `dbg hit-diff` |
| Only sessions against the same target in this dir | `dbg sessions --group` |
| Only real regressions vs baseline | `dbg diff <other>` — ranked by `abs(hits_a - hits_b)` |

## Always-on: session hygiene

- Every `dbg start` auto-labels a session. Data you capture is persisted on `dbg kill` if the session saw any hits or layers.
- `dbg save [<label>]` promotes a session so `dbg prune` never touches it.
- `dbg prune [--older-than 7d]` clears auto sessions older than the threshold.

## Escape hatch

When the canonical vocabulary can't express what you need, `dbg raw <native-cmd>` sends `<native-cmd>` to the underlying debugger verbatim. The `[via ...]` header is suppressed on raw passthrough. Use this when:

- You need a backend-specific feature (e.g., lldb `expression` evaluation language modes, delve `tracepoint`, netcoredbg `set variable`).
- You're scripting a complex init sequence.
- You've hit a corner case the canonical layer doesn't handle yet.

Stock debugger docs still apply — `dbg raw breakpoint set -r 'MyClass::.*'` does exactly what lldb would.
