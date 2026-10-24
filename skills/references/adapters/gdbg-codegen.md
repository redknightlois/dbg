# CUDA Code Generation Adapter: gdbg codegen

`gdbg codegen` inspects and compares the machine code of CUDA kernels. It builds PTX, cubin and SASS for an explicit target, captures the cubins a workload actually loads, diffs two builds or captures structurally, and keeps a versioned reference archive with a required-kernel inventory.

Static differences are evidence for a hypothesis, not a measured speedup. Equal instruction counts or resources do not establish equivalent code or performance.

## CLI

```bash
gdbg codegen compile --arch sm_90 --out DIR INPUT [-- NVCC_FLAGS...]
gdbg codegen compare --arch sm_90 --out DIR REFERENCE CANDIDATE [-- NVCC_FLAGS...]
gdbg codegen capture --out DIR [--provenance DIR]... [--session FILE.gpu.db] [--all-sass] -- COMMAND [ARGS...]
gdbg codegen archive add ARCHIVE SNAPSHOT --kernel ID --role original|accepted|experimental [--evidence FILE]... [--specialization S] [--note TEXT]
gdbg codegen archive status ARCHIVE
```

Every command writes a versioned JSON manifest (`schema: gdbg.codegen/1`) and prints a readable summary. Output directories must not exist; an existing directory is never overwritten. Any failure exits 1 and keeps the diagnostics.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| CUDA toolkit (`nvcc`, `ptxas`, `nvdisasm`, `cuobjdump`) | `nvcc --version` | Install the toolkit, set `CUDA_HOME`, or pass `--nvcc` |
| Python 3 (stdlib only) | `python3 --version` | Set `GDBG_PYTHON` to another interpreter |
| torch, for `load_inline` inputs | `PY -c "import torch"` | Pass `--torch-python PY` |
| CUPTI headers and library, for `capture` | `ls $CUDA_HOME/extras/CUPTI` or `/usr/include/cupti.h` | Install the CUPTI development package |

Capture runs the workload on the local GPU. Take any shared GPU lock the environment requires, for example `flock /tmp/openkernel-tracker-gpu.lock gdbg codegen capture ...`.

## Inputs

| Input | Meaning |
|-------|---------|
| `kernel.cu` | A standalone translation unit. The original directory stays first on the include path. |
| `script.py::NAME` | The `torch.utils.cpp_extension.load_inline` call whose `name` is `NAME`. |
| `script.py::@LINE` | The `load_inline` call that starts on `LINE`. |
| `script.py` | Allowed only when the file has exactly one `load_inline` call. |
| `DIR` | An existing `compile` output (compare only). |
| `DIR::SYMBOL` or `DIR::SHA256-PREFIX` | One module from a `capture` output, chosen by a mangled or demangled function name or a cubin hash prefix (compare only). |

The target is always the explicit `--arch` (a SASS target such as `sm_90`); `--ptx-arch` defaults to `compute_<arch>`. It is never inferred from the local GPU. Architecture flags in the user's or the script's flags are removed and recorded. Flags that change the output mode (`-o`, `-c`, `-ptx`, `-cubin`, `-MF`, `--keep` and similar) are rejected.

`--instantiate TEXT` appends source to the translation unit, for example an explicit template instantiation. `--param KEY=VALUE` records specialization or launch parameters in the manifest. `-D` defines are recorded as specialization too.

### load_inline extraction

Python files are parsed with `ast`. Candidate code is never imported or executed. The helper resolves the subset of Python used to assemble sources and flags:

- string, number, list, tuple and dict literals, f-strings, `+`, `%` and `*`
- names bound exactly once in the enclosing scopes, and import aliases such as `from torch.utils.cpp_extension import load_inline as build` or `import torch.utils.cpp_extension as cx`
- `str`, `int`, `textwrap.dedent` and the string methods `format`, `replace`, `strip`, `join`, `split` and similar

`name`, `cuda_sources`, `extra_cuda_cflags`, `extra_include_paths` and `no_implicit_headers` must resolve; otherwise the build fails with the file, line and expression. Other arguments that do not resolve are recorded under `input.unresolved`. Unsupported: function parameters, names bound more than once (including loop or conditional rebinding), `*args` and `**kwargs`, sources read from files at run time, and any other call.

The translation unit and nvcc flags follow torch's JIT rules on Linux, using the selected interpreter's torch: the implicit `#include <torch/types.h>`, `<cuda.h>` and `<cuda_runtime.h>` prefix unless `no_implicit_headers=True`, `-DTORCH_EXTENSION_NAME`, the torch and Python include directories, `COMMON_NVCC_FLAGS`, `-fPIC`, `-std=c++17` when no `-std` is given, and `-ccbin $CC` when `CC` is set. Without `--nvcc`, the build uses the nvcc under torch's `CUDA_HOME`. Only `cuda_sources` are compiled; `cpp_sources` hold host code.

## compile

```
DIR/
  manifest.json      status, target, tools, flags, specialization, functions, dependencies, git, artifact hashes
  module.ptx         the PTX that ptxas compiled
  module.cubin
  module.sass        nvdisasm -g
  resources.txt      cuobjdump -res-usage
  compiler.log       command line and nvcc/ptxas -v output
  deps.d             nvcc dependency list
  source/            the translation unit, plus copies of user headers under source/deps
  build/             nvcc intermediates, kept only when the build fails
```

`functions` lists every entry and non-inlined helper with registers, shared memory, stack, local memory, constant banks and ptxas spill bytes. `dependencies` hashes every header nvcc read, including CUDA and torch headers. `git` records the repository head and dirty files for the input and user headers. `target.cubin_arch` is read back from the cubin.

Build with `-lineinfo` to keep source locations in the diff.

## compare

Each side is a source input (built into `DIR/reference` or `DIR/candidate`), a compile output, or one captured module. If either build fails, the other side's output and all diagnostics stay, and the command exits 1.

`DIR/diff/` holds raw and normalized unified diffs for PTX and SASS. Normalization removes comments, instruction offsets, `.loc`/`.file` directives and debug sections, so a change in build paths alone shows as identical normalized output. Address operands, immediates, constant-bank references and data are kept.

`manifest.json` `report` and `summary.txt` give, for PTX and SASS separately:

- entry points and helper functions added and removed
- per-function instruction counts, opcode deltas and category deltas (tensor, memory, synchronization, control and so on for SASS; the opcode family for PTX)
- `operand_changes`: instruction forms added and removed per opcode after register, predicate and label names are masked. `operand_only` means the opcode count is unchanged but an operand or modifier changed, for example `cp.async.wait_group 2` to `3`, or `DEPBAR.LE SB0, 0x2` to `0x3`.
- source locations of the changed lines, when built with `-lineinfo`
- module data changes: PTX `.const`/`.global` initializers and prototypes, SASS non-text sections such as `.nv.constant3` (`__constant__`) and `.nv.global.init` (`__device__`)
- per-function resource changes for fields present on both sides
- notes when the architectures differ, when an offline build is compared with a capture, and when a captured side only has SASS for launched kernels

## capture

`capture` runs `COMMAND` once with a CUPTI injection library (`CUDA_INJECTION64_PATH`) and keeps every cubin the driver loads, plus the names and counts of kernels launched through the runtime and driver APIs. It is a separate phase: it refuses to run when `CUDA_INJECTION64_PATH` is already set, as it is under Nsight tools, and it does not change the workload's arguments.

```
DIR/
  manifest.json          command, env (CUDA_/TRITON_/TORCH_/PYTORCH_/NVIDIA_), modules, launches, gaps, errors
  modules/<sha256>.cubin
  modules/<sha256>.resources.txt
  modules/<sha256>.sass  launched kernels only, or the whole module with --all-sass
  modules/<sha256>.provenance/   files next to a byte-identical cubin, such as Triton's .ptx and .json
  raw/                   per-process records from the injection library
  capture-lib/           the injection library and its build log
  workload.stdout, workload.stderr
```

Each module records the architecture read from the cubin and the device compute capability that loaded it. The captured architecture is separate from any offline `--arch` build.

PTX: the driver hands CUPTI only the loaded cubin, so captured modules have no PTX (`ptx.available: false` with the reason). When a provenance directory holds a byte-identical cubin with a sibling `.ptx`, that file is attached with `origin: provenance`. The Triton cache (`TRITON_CACHE_DIR` or `~/.triton/cache`) is searched by default; add others with `--provenance`. PTX is never reconstructed from SASS.

Launch attribution matches launched symbols to module function names: `unique`, `ambiguous` (several modules define it), `unmatched` or `unnamed`. With `--session`, kernel names from a gdbg session are matched to modules the same way.

Gaps are listed in `gaps` and the summary:

- kernels replayed from CUDA graphs are neither counted nor attributed; launches made during stream capture are counted although they run only on replay
- a process that does not exit normally has no launch counts, only first launches
- launches past the injection library's symbol table are counted as dropped
- CUPTI errors and a nonzero workload exit mark the capture failed; modules captured before the failure stay

## archive

An archive keeps reference snapshots of compile or capture outputs per kernel and role:

```
ARCHIVE/
  inventory.json
  index.json                      written by every add and status
  <kernel>/original/v001/         pinned; a second original is refused
  <kernel>/accepted/vNNN/         the highest version is the current accepted reference
  <kernel>/experimental/vNNN/
    snapshot/                     a copy of the output directory
    evidence/                     acceptance evidence files
    entry.json                    role, version, specialization, note, source path, manifest and evidence hashes
```

Only successful outputs are archived. `accepted` requires `--evidence`. Prior versions are never replaced.

`inventory.json` lists every kernel, architecture and specialization that must have a current accepted reference:

```json
{
  "schema": "gdbg.codegen.inventory/1",
  "required": [
    {"kernel": "gemm", "arch": "sm_90", "source": "offline", "specialization": "bf16-128x128", "symbol": "^_Z4gemm"},
    {"kernel": "gemm", "arch": "sm_90", "source": "capture", "symbol": "gemm"}
  ]
}
```

`source` is `offline` (default) or `capture`; `specialization` must match the accepted entry's `--specialization`; `symbol` is a regular expression that must match a built function, or for a capture a function launched from a module of that architecture. `archive status` writes `index.json` with each requirement's current accepted version, PTX availability, `gaps` and `coverage_complete`, and exits 1 unless coverage is complete. An empty inventory is never complete.

## Limits

- One translation unit per build; device code from other translation units is not linked.
- Dynamic Python configuration is not extracted; pass a `.cu` file with the generated source instead.
- Captured SASS covers launched kernels unless `--all-sass`; other functions show as removed in a compare.
- The structured diff aligns lines with `difflib`, which slows down on very large modules.
- Capture needs Linux and CUPTI.

## Evals

```bash
TORCH_PYTHON=/path/to/python python3 src/gdbg/test_codegen.py          # offline, inline and archive
TORCH_PYTHON=/path/to/python python3 src/gdbg/test_codegen.py --gpu    # also capture on the local GPU
GDBG_BIN=target/debug/gdbg python3 src/gdbg/test_codegen.py            # through the gdbg binary
```
