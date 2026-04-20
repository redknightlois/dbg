# OCaml Adapter (ocamldebug)

For canonical commands and the investigation taxonomy see
[`_canonical-commands.md`](./_canonical-commands.md) and
[`_taxonomy-debug.md`](./_taxonomy-debug.md). This file covers only the
OCaml / ocamldebug specifics.

## CLI

Start: `dbg start ocaml <bytecode_program> [--break Module line] [--args ...] [--run]`

Alias: `ml`.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| OCaml 4.14+ | `ocaml -version` | `opam init && opam install ocaml` or `sudo apt install ocaml` |
| ocamldebug | `which ocamldebug` | Ships with OCaml; `sudo apt install ocaml-interp` on Debian |

## Build

**Bytecode only.** ocamldebug cannot debug native-compiled programs.

```
ocamlfind ocamlc -g -package somelib -linkpkg -o my_program src/main.ml
```

Or with dune — add `(modes byte)` to the executable stanza:

```
dune build ./my_program.bc
```

**All modules** must be compiled with `-g`. Missing `-g` on any `.cmo` means no source-level debugging for that module.

## Backend: ocamldebug

Canonical commands translate where possible — see `_canonical-commands.md`. The time-travel features are ocamldebug-only and require `dbg raw`.

## OCaml-specific breakpoints

| Canonical form | When |
|---|---|
| `dbg break 42` | Line 42 in current module |
| `dbg break Parser:42` | Line 42 in module Parser |
| `dbg break parse_expr` | Function entry |

Conditional breakpoints and logpoints are **not supported** by ocamldebug — stop, then query with `dbg print`.

## Time-travel (ocamldebug exclusive)

Reverse execution via fork-based checkpoints. Use `dbg raw`:

| Raw command | What it does |
|---|---|
| `reverse` | Run backward to the previous breakpoint hit |
| `backstep [n]` | Step backward n events |
| `previous [n]` | Step backward over function calls |
| `goto <time>` | Jump to exact event time (forward or backward) |
| `start` | Jump backward to current function's entry |
| `last [n]` | Revisit previous stopping points |

Every stop has a "time" — an event counter from program start. Launch with `ocamldebug -c 200 ./program` to increase checkpoint density if `backstep` feels slow.

**Strategy**: run forward past the bug, then `backstep` or `reverse` to find exactly when state changed. No restart needed.

## Type display

- **Integers**: `x : int = 42`
- **Strings**: `s : string = "hello"`
- **Lists**: `xs : int list = [1; 2; 3]`
- **Records**: `r : record = {name = "foo"; count = 3}`
- **Variants**: `v : color = Red`
- **Tuples**: `p : int * string = (42, "hello")`
- **Abstract types**: `<abstr>` — no internal representation available.

Control depth: `dbg raw set print_depth N`.

## Known blind spots

| Symptom | Fix |
|---------|-----|
| `ocamldebug` not found | `sudo apt install ocaml-interp` or install via opam. |
| "not a bytecode file" | Recompile with `ocamlc -g`, not `ocamlopt`. |
| No source for module | Ensure `.ml` files are on path; `dbg raw directory <dir>`. |
| No events at line | Not every line generates events — try nearby lines or function names. |
| `backstep` slow | Raise checkpoints: `ocamldebug -c 200 ./program`. |
| Abstract type `<abstr>` | Can't inspect — print concrete fields instead. |
| No arbitrary-expression eval | ocamldebug only inspects existing bindings — plan your prints at break time. |
