# scenarios/

Self-contained "broken" programs paired with an `instructions.md` describing the task as a user would phrase it. Each scenario exercises a different slice of `dbg`'s surface — breakpoints + locals, profiling + hotspots, JIT disassembly, or recursion-stack inspection — so you can validate the toolchain end-to-end and reproduce realistic debugging sessions.

### Debug (01–12)

| # | Scenario | Language | Surface exercised |
|---|---|---|---|
| 01 | Off-by-one in pagination | Python | break / continue / locals / hit-trend |
| 02 | Accidental quadratic string concat | Go | profile / hotspots / hit-counts |
| 03 | Inverted comparator → wrong sort | C++ | break / locals / step-into |
| 04 | Method fails to auto-vectorize | C# / .NET | jitdasm / disasm / simd |
| 05 | Runaway recursion in Collatz variant | Python | stack / hit-trend / break-condition |
| 06 | Bad equals / hash in dedupe | Java | break / locals |
| 07 | Flaky TTL cache | Rust | break / hits / hit-diff |
| 08 | Naked bug report (go) | Go | delve-proto / break / locals |
| 09 | Mismatched units | Python | break / locals / hit-trend |
| 10 | Throughput collapse | C++ | save / replay / cross |
| 11 | LRU corruption under load | Python | break / hits / hit-diff |
| 12 | Lying rate-limiter at window boundaries | Python | hits --group-by / hit-trend |

### CPU profiling (13–20)

| # | Scenario | Language | Profiler surface |
|---|---|---|---|
| 13 | Hot dedup loop, accidentally O(n²) | Rust | perf (CPU sampling, flamegraph view) |
| 14 | Branch-mispredict hotspot in a classifier | C | callgrind (cycle/branch counters) |
| 15 | Renderer setup dominates request time | Python | pstats / cumulative time |
| 16 | Unbounded cache leaks RSS | C | massif (heap-over-time) |
| 17 | Uninitialized config flag reads | C | memcheck (uninit-value reads) |
| 18 | Hot-path string allocs, GC pressure | C# / .NET | dotnet-trace (gen0 rate) |
| 19 | Regex recompiled per log line | Go | pprof (CPU top/flame) |
| 20 | Sync fs read blocks event loop | Node.js | nodeprof (V8 CPU profile) |

### GPU profiling (21–24)

| # | Scenario | Stack | Profiler surface |
|---|---|---|---|
| 21 | Kernel launch overhead (per-element launch) | CUDA | gdbg / nsys timeline / ncu occupancy |
| 22 | Hidden `.item()` sync in training loop | PyTorch | gdbg / torch.profiler CPU-GPU timeline |
| 23 | Bandwidth-bound elementwise kernel mistaken for compute-bound | CUDA | gdbg / ncu roofline, DRAM throughput |
| 24 | Hardcoded tile sizes regress on production shapes | Triton | gdbg / ncu occupancy + autotune |

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
