# Python Adapter

## CLI

`$DBG` = `~/.claude/skills/debug/scripts/dbg`

Start: `$DBG start python <script.py> [--break file.py:line] [--args ...] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| Python 3.12+ | `python3 --version` | System package |
| `pexpect` | `python3 -c "import pexpect"` | `pip install pexpect` |

## Build

None. Scripts run directly. Verify correct virtualenv: `which python3`.

## Breakpoint Patterns

| Pattern | When |
|---------|------|
| `file.py:42` | File and line |
| `function_name` | Function entry |
| `file.py:42, x > 10` | Conditional |

PDB starts paused at line 1. Use `--run` to continue to first breakpoint.

## Key Differences from LLDB/netcoredbg

- Backtrace: `where` (not `bt`)
- Step out: `return` (not `finish`)
- Locals: `pp {k: v for k, v in locals().items() if not k.startswith('_')}`
- Execute Python in-frame: prefix with `!`

## In-Process Execution

The `!` prefix runs arbitrary Python in the current frame:
```
!import torch; print(torch.cuda.memory_allocated() / 1e9, 'GB')
!torch.save(tensor, '/tmp/debug_tensor.pt')
!learning_rate = 0.001
```

## Type Display

- **Tensors**: `pp f"shape={t.shape}, dtype={t.dtype}, device={t.device}"`
- **Large collections**: `pp list(big_dict.items())[:5]`
- **DataFrames**: `!print(df.head().to_string())`
- **Objects**: `pp vars(obj)`

## Common Failures

| Symptom | Fix |
|---------|-----|
| `ModuleNotFoundError` | Wrong virtualenv — check `which python3` |
| Breakpoint not hit | File path mismatch — use absolute path |
| `locals` shows framework internals | Use `up` / `down` to navigate to user frames |
| `SyntaxError` on print | Prefix with `!`: `!print(p)` |
