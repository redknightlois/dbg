# OCaml Adapter (ocamldebug)

## CLI

Start: `dbg start ocaml <bytecode_program> [--break Module line] [--args ...] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| OCaml 4.14+ | `ocaml -version` | `opam init && opam install ocaml` or `sudo apt install ocaml` |
| ocamldebug | `which ocamldebug` | Installed with OCaml; `sudo apt install ocaml-interp` on Debian |

## Build

**Bytecode only.** ocamldebug cannot debug native-compiled programs.

```
ocamlfind ocamlc -g -package somelib -linkpkg -o my_program src/main.ml
```

Or with dune — add `(modes byte)` to the executable stanza and build:

```
dune build ./my_program.bc
```

Key: ALL modules must be compiled with `-g`. Missing `-g` on any `.cmo` means no source-level debugging for that module.

## Breakpoint Patterns

| Pattern | When |
|---------|------|
| `42` | Line 42 in current module |
| `Parser 42` | Line 42 in module Parser |
| `Parser:42` | Same (convenience syntax) |
| `Parser 42 10` | Line 42, column 10 in Parser |
| `parse_expr` | Function entry (by name) |

ocamldebug starts paused before execution. Use `--run` to execute to first breakpoint.

## Key Commands

| Command | What it does |
|---------|-------------|
| `run` | Execute forward to next breakpoint or end |
| `step [n]` | Step forward n events (default 1) |
| `next [n]` | Step over function calls |
| `finish` | Run to end of current function |
| `reverse` | Execute backward to previous breakpoint |
| `backstep [n]` | Step backward n events |
| `previous [n]` | Step backward over function calls |
| `goto time` | Jump to exact event time (forward or backward) |
| `start` | Jump backward to current function entry |
| `break` | Set breakpoint at current position |
| `break @ Module line` | Set breakpoint at module and line |
| `break name` | Set breakpoint at function entry |
| `delete [n]` | Delete breakpoint (all if no arg) |
| `info breakpoints` | List all breakpoints |
| `backtrace [n]` | Show call stack (alias: `bt`) |
| `frame [n]` | Select stack frame |
| `up [n]` | Move up the call stack |
| `down [n]` | Move down the call stack |
| `print expr` | Print value (full depth) |
| `display expr` | Print value (depth-limited to 1) |
| `list [module]` | Show source code |
| `info locals` | Show local variables |
| `info modules` | Show loaded modules |
| `info events` | Show debuggable events |
| `set print_depth n` | Set print depth for values |
| `last [n]` | Revisit previous stopping points |

## Time-Travel Debugging — Key Differentiator

ocamldebug supports **reverse execution** via checkpoint-based time travel. This is its most powerful feature:

- **Every stop has a "time"** — an event counter from program start. Displayed as `Time: N`.
- **`backstep`** — step backward one event at a time. Like `step` but in reverse.
- **`reverse`** — run backward to the most recent breakpoint hit.
- **`goto 0`** — jump to program start. `goto 100` — jump to event 100.
- **`previous`** — like `next` but backward (skips over function calls in reverse).

**Strategy**: Run forward past the bug, then `backstep` or `reverse` to find exactly when state changed. No need to restart the program.

**How it works**: ocamldebug uses `fork()` to create checkpoints at intervals. Going backward restores the nearest checkpoint and replays forward. This means reverse debugging costs memory (one process per checkpoint) but works automatically — no recording needed.

## In-Process Execution

ocamldebug does NOT support evaluating arbitrary OCaml expressions at the prompt. You can only inspect existing variables with `print` and `display`.

## Type Display

- **Integers**: `x : int = 42`
- **Strings**: `s : string = "hello"`
- **Lists**: `xs : int list = [1; 2; 3]`
- **Records**: `r : record = {name = "foo"; count = 3}`
- **Variants**: `v : color = Red`
- **Tuples**: `p : int * string = (42, "hello")`
- **Abstract types**: shown as `<abstr>` (no internal representation visible)

Use `set print_depth N` to control display depth for nested values.

## Common Failures

| Symptom | Fix |
|---------|-----|
| `ocamldebug` not found | `sudo apt install ocaml-interp` or install via opam |
| "not a bytecode file" | Recompile with `ocamlc -g`, not `ocamlopt` (native code is not supported) |
| No source for module | Ensure `.ml` files are in path; use `-I dir` or `directory dir` in debugger |
| No events at line | Not all lines generate events — try nearby lines or function names |
| `backstep` slow | Increase checkpoints: launch with `ocamldebug -c 200 ./program` |
| Abstract type `<abstr>` | ocamldebug can't inspect abstract types — use `print` on concrete fields |
| Conditional breakpoints | Not supported — set breakpoint, then manually check with `print` |
| Can't debug .exe | Windows native port not supported; use WSL or Cygwin |
