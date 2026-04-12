# GPU Profiler Adapter — gdbg

`gdbg` is a separate binary from `dbg`. Do NOT use `dbg start` — run `gdbg` directly.

## CLI

```bash
gdbg <target>            # collect data + open REPL
gdbg --from <name>       # reload a saved session
gdbg list                # list saved sessions
gdbg diff <a> <b>        # compare two saved sessions
gdbg check               # verify dependencies are installed
```

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
| `warmup` | Detect warmup launches before steady state |
| `small [N]` | Kernels where launch overhead > compute |
| `fuse [N]` | Sequential kernels that could be fused |
| `concurrency` | Stream utilization and parallelism gaps |
| `hotpath` | Critical path through ops (CPU vs GPU bound) |
| `compare-ops [N]` | CPU vs GPU time ratio per operator |
| `breakdown <op>` | Which kernels an op expands into |
| `idle-between <a> <b>` | GPU idle gap between two ops |

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
