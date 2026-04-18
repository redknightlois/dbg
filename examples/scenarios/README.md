# scenarios/

Self-contained "broken" programs paired with an `instructions.md` describing the task as a user would phrase it. Each scenario exercises a different slice of `dbg`'s surface — breakpoints + locals, profiling + hotspots, JIT disassembly, or recursion-stack inspection — so you can validate the toolchain end-to-end and reproduce realistic debugging sessions.

| # | Scenario | Language | Surface exercised |
|---|---|---|---|
| 01 | Off-by-one in pagination | Python | break / continue / locals / hit-trend |
| 02 | Accidental quadratic string concat | Go | profile / hotspots / hit-counts |
| 03 | Inverted comparator → wrong sort | C++ | break / locals / step-into |
| 04 | Method fails to auto-vectorize | C# / .NET | jitdasm / disasm / simd |
| 05 | Runaway recursion in Collatz variant | Python | stack / hit-trend / break-condition |

## How to use a scenario

```bash
cd examples/scenarios/01-pagination-offbyone-py
cat instructions.md           # read the task
dbg start pdb broken.py       # launch — adapter chosen per scenario
# ... follow the task ...
```

Each `instructions.md` is written from the perspective of a user *reporting* the bug, not a walkthrough — the LLM is supposed to figure out where to break and what to inspect. The fix is intentionally one or two lines once located, so the value is in the *finding*, not the patching.

## Conventions

- One root file per scenario named `broken.<ext>` (or equivalent project layout for csproj/Go module). Run it directly to reproduce the symptom before debugging.
- A `solution.md` (where present) sketches the correct fix — read only after attempting the scenario, otherwise it's a spoiler.
- No scenario takes more than a few seconds to run; debugging session itself is the slow part.
