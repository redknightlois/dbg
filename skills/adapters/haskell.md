# Haskell Adapter (GHCi)

## CLI

Start: `dbg start haskell <script.hs> [--break Main.functionName] [--args ...] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| GHC 9.0+ | `ghc --version` | `curl --proto '=https' --tlsv1.2 -sSf https://get-ghcup.haskell.org \| sh` or `sudo apt install ghc` |
| ghci | `which ghci` | Installed with GHC |

## Build

None for scripts. GHCi interprets source directly. **Breakpoints only work on interpreted (not compiled) modules** — if loading a Cabal/Stack project, ensure target modules are loaded as source (`:load Module.hs`), not from compiled `.hi` files.

## Breakpoint Patterns

| Pattern | When |
|---------|------|
| `functionName` | Break at function entry |
| `Module.functionName` | Break at qualified function |
| `42` | Line in current module |
| `Module 42` | Line in specific module |
| `Module 42 5` | Line and column in module |

GHCi starts at the prompt (no code running). Use `--run` with `:trace main` or set breakpoints then `:trace main`.

## Key Commands

| Command | What it does |
|---------|-------------|
| `:break <spec>` | Set breakpoint (function, line, module+line) |
| `:show breaks` | List active breakpoints |
| `:delete <n>` | Delete breakpoint by number (`*` for all) |
| `:enable <n>` | Enable disabled breakpoint |
| `:disable <n>` | Disable without deleting |
| `:trace <expr>` | Run expression with history logging (preferred over `:step expr`) |
| `:step [expr]` | Single-step (to next breakpoint in any module) |
| `:steplocal` | Step within current top-level function only |
| `:stepmodule` | Step within current module only |
| `:continue` | Resume to next breakpoint |
| `:abandon` | Abort current evaluation, return to prompt |
| `:history [n]` | Show last n evaluation steps (default 20, needs `:trace`) |
| `:back [n]` | Move backward in history |
| `:forward [n]` | Move forward in history |
| `:list` | Show source around current breakpoint |
| `:list <ident>` | Show source for function |
| `:print <expr>` | Display value without forcing thunks (safe) |
| `:sprint <expr>` | Like `:print` but shows `_` for thunks (no binding) |
| `:force <expr>` | Display and fully evaluate (may loop or throw!) |
| `:show bindings` | All variables in current scope with types |
| `:type <expr>` | Show type of expression |
| `:info <name>` | Show info about name (type, class, source location) |
| `:load <file>` | Load/reload module for debugging |
| `:reload` | Reload current modules |

## Lazy Evaluation — Critical Differences

**Haskell is lazy.** Values are not computed until needed. This fundamentally changes debugging:

- **Thunks show as `_`** — `:print x` may show `x = _` meaning "not yet evaluated". This is normal, not an error.
- **`:print` is safe** — it never forces evaluation, creates `_t1`, `_t2` bindings for unevaluated subterms.
- **`:sprint` is minimal** — shows `_` for thunks without creating extra bindings.
- **`:force` is dangerous** — fully evaluates, which can trigger infinite loops, exceptions, or side effects.
- **`_result`** — auto-bound variable at each breakpoint holding the current expression's value (often a thunk).

**Strategy**: Use `:print` first, then `:force` only for values you know are finite. Use `:type` when `:print` shows `_` to understand what a thunk will produce.

## Execution Model

Unlike imperative debuggers, GHCi breaks on **expression reduction**, not "lines of code":

1. Load your module: `:load Main.hs`
2. Set breakpoints: `:break myFunction`
3. Run with tracing: `:trace main` (records history for `:back`/`:forward`)
4. At a stop: inspect with `:print`, `:show bindings`, `:type`
5. Navigate: `:step` (next reduction), `:continue` (next breakpoint), `:back`/`:forward` (history)

## In-Process Execution

Any Haskell expression at the prompt is evaluated in the current context:
```
:type map
let xs = [1..10]
filter even xs
:type xs
import Data.List (sort)
sort [3,1,2]
```

When stopped at a breakpoint, expressions have access to local bindings.

## Type Display

- **Basic**: `:type expr` shows the type, `:print var` shows the value
- **Records**: `:print myRecord` shows `MyRecord {field1 = val, field2 = _}`
- **Lists**: May show partial evaluation: `1 : 2 : _` (rest unevaluated)
- **Functions**: `:type f` shows signature; functions can't be "printed"
- **Polymorphic**: `:print` may fail on polymorphic values — use `:type` instead

## Common Failures

| Symptom | Fix |
|---------|-----|
| `ghci` not found | Install GHC via ghcup: `curl --proto '=https' --tlsv1.2 -sSf https://get-ghcup.haskell.org \| sh` |
| Breakpoint not hit | Module must be interpreted, not compiled — use `:load File.hs` not compiled `.hi` |
| `:print` shows all `_` | Value is unevaluated (thunk) — use `:force` carefully or `:step` to advance evaluation |
| `:force` hangs | Value is infinite or very large — Ctrl-C to interrupt, use `:print` instead |
| `:history` empty | Only recorded with `:trace`, not `:step` — restart with `:trace main` |
| `No breakpoints found` | Function may be inlined or in a compiled module — check with `:info functionName` |
| Polymorphic error | Breakpoint on polymorphic function — try monomorphic call: `:trace (myFunc :: Int -> Int) 5` |
| Can't break in Prelude | Base libraries are compiled — only interpreted source is debuggable |
