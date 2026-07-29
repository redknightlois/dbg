# Runtime and GPU Investigation Taxonomy

Organize investigations by **question**, not by tool command. Use `dbg` for
runtime state and CPU-side behavior. Use `gdbg` for GPU timelines, kernels, and
hardware metrics. A GPU application may need both, but they collect separate
sessions.

In the tables, `gdbg:` identifies a command in the interactive `gdbg` REPL.
Run these commands after `gdbg <target>` finishes collection. Commands such as
`gdbg check`, `gdbg list`, and `gdbg diff` run from the shell.

## Route the question first

| Question | Tool |
|---|---|
| Why did this value, branch, exception, or crash occur? | `dbg` with the language debugger |
| Which CPU function, allocation, or instruction is expensive? | `dbg` with a profiling backend |
| Which kernel, transfer, stream, or GPU operator is expensive? | `gdbg` |
| Is end-to-end time on the CPU or GPU? | `gdbg`: `hotpath` and `compare-ops` |
| Did a change improve the workload? | Save comparable sessions and diff with the same tool |

Start with the cheapest evidence that can answer the question. Do not collect
hardware counters when a breakpoint can prove the bug, or step through code
when a profile is needed.

## Hotspots — "where is the cost or repeated behavior?"

| Question | Commands |
|---|---|
| Which runtime path repeats? | `dbg break <loc>` → continue several times → `dbg hits <loc>` |
| Which CPU function is expensive? | Start the matching profile backend → inspect its `hotspots` or `top` output |
| Which kernels dominate GPU time? | `gdbg`: `stats` → `kernels [N]` |
| Which framework operators dominate GPU time? | `gdbg`: `top-ops [N]` or `ops [N]` |
| Which short interval dominates GPU time? | `gdbg`: `hotspot <window_us>` |
| Is launch overhead larger than useful GPU work? | `gdbg`: `small [N]` |

For `dbg`, place breakpoints at state transitions, loop bodies, or request
boundaries. For `gdbg`, identify the few kernels or operators that dominate
total time before inspecting individual launches.

## Analysis — "why is it happening?"

| Question | Commands |
|---|---|
| Which local changed between two hits? | `dbg hit-diff <loc> <a> <b>` |
| How does one value evolve over repeated hits? | `dbg hit-trend <loc> <field>` |
| What source, captures, profiles, and codegen exist for a symbol? | `dbg cross <sym>` |
| What code did the compiler or JIT emit? | `dbg at-hit disasm` or `dbg disasm <sym>` |
| Is a kernel compute-, memory-, or latency-bound? | `gdbg`: `roofline [pattern]` → `bound <kernel>` |
| Is occupancy limiting the hot kernels? | `gdbg`: `occupancy [N]` → `inspect <kernel>` |
| What bandwidth does each hot kernel achieve? | `gdbg`: `bandwidth [N] [pattern]` |
| Are allocations, leaks, or churn consuming GPU memory? | `gdbg`: `memory [N]` |
| Is startup or launch variance distorting the result? | `gdbg`: `warmup` and `variance <kernel>` |
| Which launches are anomalously slow? | `gdbg`: `outliers <kernel>` → `launches <kernel> [N]` |
| How do two kernels differ inside one session? | `gdbg`: `compare <kernel_a> <kernel_b>` |
| Could adjacent kernels be combined? | `gdbg`: `fuse [N]` |

Diff before interpreting. One runtime hit or one kernel launch is an example;
the change across hits, launches, or sessions is the evidence.

## Timeline — "when and in what order?"

| Question | Commands |
|---|---|
| What called this runtime location? | `dbg stack`, then `dbg frame <n>` and `dbg locals` |
| What is the ordered state history at a breakpoint? | `dbg hits <loc>` |
| Which thread or goroutine owns the stop? | `dbg threads` → `dbg thread <n>` |
| Where is the GPU idle? | `gdbg`: `gaps [N]` and `timeline [N]` |
| Are streams running concurrently? | `gdbg`: `streams`, `concurrency`, and `overlap` |
| Are transfers blocking compute? | `gdbg`: `transfers [N]` and `overlap` |
| Is there avoidable idle time between two operators? | `gdbg`: `idle-between <a> <b>` |
| What is the longest serialized kernel chain? | `gdbg`: `critical-path [gap_us]` |
| How do launches overlap across streams? | `gdbg`: `stream-graph [width]` |

## Drill-down — "I have a suspect; show its context"

| Question | Commands |
|---|---|
| Everything known about a runtime symbol | `dbg cross <sym>` |
| Source around the symbol | `dbg source <sym>` |
| Runtime state in its callers | `dbg stack` → select frames → `dbg locals` |
| Every GPU data layer for a kernel | `gdbg`: `inspect <kernel>` |
| Which source location launched a kernel? | `gdbg`: `source <kernel>` |
| Which operator launched a kernel? | `gdbg`: `callers <kernel>` |
| Which kernels implement an operator? | `gdbg`: `trace <op>` or `breakdown <op>` |

## Filtering — "remove unrelated evidence"

| Question | Commands |
|---|---|
| Restrict runtime captures to one state transition | Use a precise breakpoint, condition, or logpoint |
| Compare only sessions for the active target | `dbg sessions --group` |
| Restrict GPU results to a kernel family | `gdbg`: `focus <pattern>` |
| Remove a known GPU noise source | `gdbg`: `ignore <pattern>` |
| Restrict GPU results to one profiler or NVTX region | `gdbg`: `region <name>` |
| Return to the complete GPU session | `gdbg`: `reset` |

Filters change the view, not the captured data. Reset them before drawing a
workload-wide conclusion.

## Advanced workflows

### State drift or intermittent corruption

1. Break at the state transition, not at the eventual failure.
2. Continue until several hits are captured.
3. Use `dbg hit-trend` to locate the first divergent field.
4. Use `dbg hit-diff` around that transition.
5. Select the responsible caller frame and inspect its locals.

### CPU/GPU attribution

1. Run `gdbg <target>`, then use `stats`, `hotpath`, and `compare-ops`.
2. If GPU work dominates, use `kernels`, `roofline`, and `inspect`.
3. If CPU launch or framework work dominates, profile the host with the
   matching `dbg` backend.
4. Use `trace`, `callers`, or `breakdown` to connect operators and kernels.
5. Use a separate `dbg` session to inspect host-side shapes, branches, or
   synchronization around the suspect operator.

### Regression comparison

1. Use the same target, inputs, warmup, and filters for both runs.
2. Save the baseline: `dbg save <name>` or `gdbg` REPL `save <name>`.
3. Collect the candidate once under the same conditions.
4. Compare with `dbg diff <baseline>`, `gdbg` REPL `diff <baseline>`, or
   `gdbg diff <baseline> <candidate>`.
5. Use the `gdbg` REPL command `regressions <baseline> [%] [us]` when both
   relative and absolute thresholds must be exceeded.
6. Treat unmatched symbols, kernels, missing layers, or changed filters as an
   invalid comparison until explained.

## Data coverage and session hygiene

- Before GPU analysis, run `gdbg check`. After collection, use `layers` and
  `suggest`; a missing layer cannot support conclusions that require it.
- Every `dbg start` creates a session. Always run `dbg kill` when finished.
- Save evidence worth comparing before pruning or replacing it.
- Use `dbg raw <native-command>` only when the canonical vocabulary cannot
  express the operation.
