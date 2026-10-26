# Python Adapter

For canonical commands and the investigation taxonomy see
[`_canonical-commands.md`](./_canonical-commands.md) and
[`_taxonomy-debug.md`](./_taxonomy-debug.md). This file covers only the
Python / debugpy (DAP) + pdb specifics.

## CLI

Start: `dbg start python <script.py> [--break file.py:line] [--args ...] [--run]`

Auto-detect: `dbg start <script.py>` picks this adapter from the `.py` extension. If the script imports `torch`, `triton`, `tensorflow`, `jax`, `cupy`, or uses `.cuda()`, **switch to `gdbg` instead** — `dbg` cannot profile GPU kernels.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| Python 3.12+ | `python3 --version` | System package |
| `debugpy` (preferred) | `python3 -m debugpy --version` | `pip install debugpy` |

Verify the correct virtualenv: `which python3`. `dbg` inherits the environment from `dbg start` — do not prepend env vars per command.

## Backend

Default: **debugpy-proto** (DAP transport, full canonical-ops parity). Falls back to **pdb** when `debugpy` is unavailable. `dbg tool` reports which one is active.

`watch`, `threads`, and `thread <n>` are unsupported under pdb (single-threaded REPL) but available under debugpy. Attach mode (`dbg start --attach-port <PORT>`) is debugpy-only.

## Python-specific breakpoints

| Canonical form | When |
|---|---|
| `dbg break file.py:42` | File and line |
| `dbg break module.function_name` | Function entry |
| `dbg break file.py:42 if x > 10` | Conditional |
| `dbg break file.py:42 log "x={x}"` | Logpoint (no stop) — debugpy only |
| `dbg catch ValueError` | Exception breakpoint — debugpy only |

## Type display

- **Tensors**: `dbg print f"shape={t.shape}, dtype={t.dtype}, device={t.device}"`
- **Large collections**: `dbg print list(big_dict.items())[:5]`
- **DataFrames**: `dbg print df.head().to_string()`
- **Objects**: `dbg print vars(obj)`
- **In-process Python** (pdb only): prefix native commands with `!` via `dbg raw !...`. With debugpy, `dbg print <expr>` handles arbitrary expressions.

## Known blind spots

| Symptom | Fix |
|---------|-----|
| `ModuleNotFoundError` | Wrong virtualenv — check `which python3` before `dbg start`. |
| Breakpoint not hit | File path mismatch — use absolute path, or verify with `dbg breaks`. |
| `dbg locals` shows framework internals | Use `dbg frame <n>` to walk to your code; pdb-mode also filters module-level noise. |
| `SyntaxError` evaluating `print` | Under pdb, use `dbg raw !print(p)`; under debugpy, `dbg print p`. |
| Slow init / session drop | cProfile/pstats startup can exceed default init timeout — retry, or let the pstats adapter handle profiling instead. |
