# PHP Adapter (phpdbg)

## CLI

Start: `dbg start php <script.php> [--break file.php:line] [--args ...] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| PHP 8.0+ | `php --version` | `sudo apt install php` or `brew install php` |
| phpdbg | `which phpdbg` | Bundled with PHP since 5.6 |

## Build

None. Scripts run directly. Verify correct PHP: `which php`, `php --version`.

## Breakpoint Patterns

| Pattern | When |
|---------|------|
| `file.php:42` | File and line |
| `my_function` | Function entry |
| `MyClass::method` | Method entry |
| `0x<address>` | Opline address |

phpdbg starts paused before execution. Use `--run` to continue to first breakpoint.

## Key Commands

| Command | Alias | What it does |
|---------|-------|-------------|
| `run` | `r` | Start/restart execution |
| `step` | `s` | Step into (enters functions) |
| `next` | `n` | Step over |
| `continue` | `c` | Continue to next breakpoint |
| `finish` | `F` | Run to end of current function |
| `leave` | `L` | Run to return of current function |
| `until` | `u` | Continue past current line |
| `break` | `b` | Set breakpoint |
| `ev <expr>` | — | Evaluate PHP expression in current scope |
| `back` | `t` | Show backtrace |
| `frame <n>` | — | Select stack frame |
| `info` | `i` | Show debug info (`info break`, `info locals`) |
| `list` | `l` | Display source code |
| `print` | `p` | Print opcodes |
| `watch` | `w` | Set watchpoint on variable |
| `clear` | — | Clear all breakpoints |

## Key Differences from PDB/LLDB

- Evaluate expressions: `ev $variable` (not `p $variable`)
- Backtrace: `back` or `t` (not `bt` or `where`)
- Step out: `finish` / `F` (not `return`)
- Locals: `info locals` (not `locals()`)
- Opcodes: `print exec` shows opcodes for the current file
- No `!` prefix — `ev` handles all expression evaluation

## In-Process Execution

The `ev` command runs arbitrary PHP in the current scope:
```
ev $x + $y
ev var_dump($object)
ev array_keys($config)
ev count($items)
ev json_encode($data, JSON_PRETTY_PRINT)
```

## Type Display

- **Arrays**: `ev print_r($array)` or `ev var_dump($array)`
- **Objects**: `ev var_dump($obj)` or `ev get_object_vars($obj)`
- **Large data**: `ev array_slice($big_array, 0, 5)`
- **Class info**: `ev get_class($obj)` and `ev get_class_methods($obj)`
- **Type check**: `ev gettype($var)` or `ev get_debug_type($var)`

## Common Failures

| Symptom | Fix |
|---------|-----|
| `phpdbg` not found | Install PHP — phpdbg ships with standard PHP packages |
| Breakpoint not hit | Check file path — use path relative to execution dir |
| `ev` shows opcodes instead of value | Use `ev var_dump($x)` for complex types |
| Extensions missing | Check `php -m` for loaded extensions |
| Segfault on startup | Try `phpdbg -n -e script.php` to disable extensions |
