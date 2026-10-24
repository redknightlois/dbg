# GPU Profiler Adapter — gdbg

`gdbg` is a separate binary from `dbg`. Do NOT use `dbg start` — run `gdbg` directly.

## CLI

```bash
gdbg <target>            # collect data + open REPL
gdbg --from <name>       # reload a saved session
gdbg list                # list saved sessions
gdbg diff <a> <b>        # compare two saved sessions
gdbg check               # verify dependencies are installed
gdbg codegen ...         # build, capture, diff and archive CUDA PTX/SASS
```

For kernel machine code (offline PTX/SASS builds for an explicit target, loaded-module capture, structured diffs and the reference archive), see [`gdbg-codegen.md`](./gdbg-codegen.md).

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `gdbg` | `which gdbg` | `cargo install dbg-cli` |
| `nsys` | `gdbg check` | Install NVIDIA Nsight Systems |
| `ncu` | `gdbg check` | Install NVIDIA Nsight Compute |
| `python3` | `which python3` | Only needed for PyTorch/Triton targets |

On WSL2: `nsys` works but cannot trace GPU kernel internals (no CUPTI kernel activity). `gdbg` falls back to CUDA runtime API tracing and warns you. For full profiling, run on native Linux with a GPU.

## How it works

`gdbg` collects in three phases, each independent (failures in one don't block the others):

1. **nsys** — GPU timeline: kernel launches, memory transfers, streams, NVTX regions
2. **ncu** — Hardware metrics on top-5 hottest kernels: occupancy, throughput, registers, shared memory, L2 hit rate
3. **torch.profiler** — CPU op mapping: which PyTorch operator launched which kernel

Everything goes into a single SQLite session. The REPL queries the database directly.

## Staged counter collection

Start with a timeline, then collect only the names and instances needed to test a
specific hypothesis. Put gdbg flags before the target; subsequent flags belong
to the target.

```bash
gdbg --timeline-only ./worker.py --case-index 0
# Copy the native symbol from the timeline into KERNEL.
gdbg --counters-only --kernel-name "$KERNEL" --launch-skip 1 --launch-count 1 \
  --section SpeedOfLight --section WarpStateStats ./worker.py --case-index 0
```

`--kernel-name` repeats for several literal native names; gdbg escapes regex
metacharacters. NCU output uses native demangled names with simplification disabled, preserving namespaces for exact timeline joins. Each selection runs a separate NCU invocation. Skip/count refer
to matching launches within that invocation, not global timeline IDs. For W13
and W2 sharing one symbol in a single replay, skip 0 selects the first matching
instance and skip 1 selects the second. Verify raw IDs, order, geometry, source,
case and replay identity before assigning either meaning. A count of 2 collects
both; normalized kernel summaries still merge identical symbols.

`--section ID` repeats and replaces the default full metric set. Use
`ncu --list-sections` to discover section IDs for the installed version.
`--ncu-set NAME` selects an NCU set instead; it cannot be combined with sections.
Use only sections that discriminate the current hypothesis, then collect missing
sections or instances. Reduced sections do not establish full counter coverage.

`--timeline-only` skips counters and operator tracing. `--counters-only` requires
explicit names and skips another Nsys/operator capture. It creates a separate
session, preserving the original timeline. Callers match source, hardware,
runtime, workload and cache policy across sessions; it does not claim equal
physical state or comparable NCU/Nsys durations. The shared GPU lock still
belongs to the workload's GPU phases; no outer lock around gdbg or analysis.

For native ELF/CUDA targets, `--gpu-lock PATH` serializes each Nsys/NCU capture
command. It waits before starting the profiler and releases the same lock inode
before exports, SQLite imports, the next capture, or REPL analysis. Compilation
also stays outside the lock. Do not wrap gdbg itself in flock. An application's
CPU work inside the capture process remains within that command's lock scope;
harnesses needing finer boundaries lock internally and omit this flag. Python
targets require their own GPU-phase lock and reject `--gpu-lock`.

Defaults remain top five timeline names, full sections and first matching
launch. `--all-kernels --all-launches` keeps complete instance collection.
Each completed NCU invocation is transactionally imported before the next;
partial/unfinished collection state and raw base-unit values remain available. Failed-command stdout and stderr are retained separately; errors show up to 4 KiB from each stream and link to the full logs.
No active capture should have its tool binary or runtime replaced.

Missing or empty Unified Memory GPU fault records leave coverage unverified,
even when the saved command and capture metadata enable fault collection.
They do not establish zero faults or a missing flag. Do not recollect solely
because those records are absent; inspect the retained Nsight metadata first.

## Target detection

`gdbg` reads the target file and auto-detects:

| Target | Detection |
|--------|-----------|
| Python + PyTorch | imports `torch` |
| Python + Triton | imports `triton` |
| Python + TensorFlow | imports `tensorflow` |
| Python + JAX | imports `jax` |
| Plain Python + CUDA | imports `cupy`, `pycuda`, etc. |
| CUDA source | `.cu` file |
| Binary | ELF/PE executable |

## REPL commands

After collection, `gdbg` drops into an interactive REPL. Key commands:

### Hotspots
| Command | What it does |
|---------|-------------|
| `kernels [N] [pattern]` | Top kernels by total GPU time |
| `ops [N] [pattern]` | Top operators (needs torch.profiler data) |
| `stats` | Overall summary |
| `top-ops [N] [pattern]` | Ops ranked by GPU time |

### Analysis
| Command | What it does |
|---------|-------------|
| `roofline [pattern]` | Classify compute-bound vs memory-bound |
| `bound <kernel>` | Detailed boundedness diagnosis |
| `occupancy [N]` | SM occupancy ranking |
| `variance <kernel>` | Launch-to-launch timing variance |
| `outliers <kernel>` | Slow launches with their timeline positions |
| `launches <kernel> [N]` | Per-launch time, launch shape, stream, and gap |
| `source <kernel>` | Source or operator that launched a kernel |
| `memory [N]` | Recorded allocation peaks, unmatched allocations, largest blocks, and churn |
| `bandwidth [N] [pattern]` | Achieved memory bandwidth from NCU metrics |
| `compare <a> <b>` | Side-by-side statistics for two kernels |
| `regressions <session> [%] [us]` | Regressions above relative and absolute thresholds |
| `warmup` | Detect warmup launches before steady state |
| `small [N]` | Short kernels; inspect measured gaps before inferring launch overhead |
| `fuse [N]` | Adjacent kernel sequences and observed gaps |
| `concurrency` | Stream utilization and parallelism gaps |
| `hotpath` | Inclusive CPU operator totals and GPU attribution |
| `compare-ops [N]` | CPU / attributed GPU time per operator |
| `breakdown <op>` | Which kernels an op expands into |
| `idle-between <a> <b>` | GPU idle gap between two ops |
| `hotspot <window_us>` | Hottest fixed-width interval |
| `critical-path [gap_us]` | Longest serialized same-stream chain |
| `stream-graph [width]` | Stream-aligned ASCII timeline |

### Timeline
| Command | What it does |
|---------|-------------|
| `transfers [N]` | Memory copies ranked by cost |
| `gaps [N]` | GPU idle periods |
| `overlap` | Compute/transfer concurrency |
| `streams` | Per-stream utilization |
| `timeline [N]` | Chronological kernel launches |

### Drill-down
| Command | What it does |
|---------|-------------|
| `inspect <kernel>` | Full detail from all data layers |
| `trace <op>` | Op to kernel mapping |
| `callers <kernel>` | Which op launched this kernel |

### Data management
| Command | What it does |
|---------|-------------|
| `layers` | Show loaded data layers |
| `suggest` | Suggest what data to collect next |
| `save <name>` | Save session to `.dbg/gpu/` |
| `list` | List saved sessions |
| `diff <name>` | Compare against saved session |

### Filtering
| Command | What it does |
|---------|-------------|
| `focus <pattern>` | Show only matching kernels |
| `ignore <pattern>` | Hide matching kernels |
| `region <name>` | Focus on NVTX / profiler step |
| `reset` | Clear all filters |

## Workflow

1. Run: `gdbg train.py`
2. Review: `stats` then `kernels` to see what's hot
3. Classify: `roofline` to see compute vs memory bound
4. Drill down: `inspect <hot_kernel>` for hardware counters
5. Find opportunities: `fuse` for fusion candidates, `small` for tiny kernels, `gaps` for idle time
6. Save baseline: `save before-optimization`
7. Make changes, re-run: `gdbg train.py`
8. Compare: `diff before-optimization`

## Common failures

| Symptom | Fix |
|---------|-----|
| No kernel data | WSL2 — run on native Linux for full GPU tracing |
| `nsys` not found | Install NVIDIA Nsight Systems, ensure it's in PATH |
| `ncu` permission denied | Run with `sudo` or set `/proc/sys/kernel/perf_event_paranoid` to -1 |
| No ops data | Target doesn't use PyTorch — only nsys/ncu data available |
| 0 GPU time on ops | Kernel names don't match between layers — check target consistency |

## Interpreting advanced views

`transfers` distinguishes explicit copies from capture-wide Unified Memory GPU
fault counts. Missing fault coverage is unknown, not zero. Fault counts do not
measure migration bytes or host-mapped GPU traffic. Collection enables graph
node tracing and GPU fault events.

`memory` reports recorded allocation lifetimes. An unmatched free does not
establish a memory leak or physical VRAM residency. `small` and `fuse` report
short kernels and adjacency; CUDA graph nodes and replay boundaries require
measured gap analysis. CPU operator totals can include nested events and a
different capture region, so `hotpath` and `compare-ops` do not infer a global
CPU/GPU bottleneck from those totals.

CUDA source targets compile for the current GPU with `-arch=native`. C++
counter filters retain the profiler's native signature while database joins
use canonical names. PyTorch traces accept integer or string track IDs;
unique external IDs associate asynchronous GPU launches with CPU operators.
Missing or ambiguous IDs remain unmapped.
