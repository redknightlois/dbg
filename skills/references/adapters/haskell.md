# Haskell Adapter (GHCi)

For canonical commands and the investigation taxonomy see
[`_canonical-commands.md`](./_canonical-commands.md) and
[`_taxonomy-debug.md`](./_taxonomy-debug.md). This file covers only the
Haskell / GHCi specifics. For profiling see `haskell-profile.md`.

## CLI

Start: `dbg start haskell <script.hs> [--break Module.functionName] [--args ...] [--run]`

Alias: `hs`, `ghci`.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| GHC 9.0+ | `ghc --version` | `curl --proto '=https' --tlsv1.2 -sSf https://get-ghcup.haskell.org \| sh` |
| ghci | `which ghci` | Installed with GHC |

**Breakpoints only work on interpreted modules.** Load target modules as source (`:load Module.hs`), not from compiled `.hi` — `dbg raw :load ...` for a Cabal/Stack project's module.

## Backend: GHCi

Canonical commands translate to GHCi where they have equivalents. GHCi's execution model is **expression reduction**, not lines of code — keep that in mind when reading `dbg step` output. Translation table in `_canonical-commands.md`.

Haskell's laziness requires backend-specific care that the canonical layer can't paper over. Use the GHCi-native forms below for the lazy-aware inspection commands.

## Haskell-specific breakpoints

| Canonical form | When |
|---|---|
| `dbg break Main.functionName` | Break at qualified function |
| `dbg break functionName` | Function in current module |
| `dbg break file.hs:42` | File and line |

After setting breakpoints, run with `dbg raw :trace main` (enables history for `:back`/`:forward`) or `dbg run` (no history).

## GHCi-native commands that matter

The canonical vocabulary doesn't cover these — use `dbg raw`:

| Raw command | What it does |
|---|---|
| `:trace <expr>` | Run expression with history logging (preferred over `:step`) |
| `:history [n]` | Show last n evaluation steps |
| `:back [n]` / `:forward [n]` | Time-travel within the recorded history |
| `:print <expr>` | Display value without forcing thunks (safe) |
| `:sprint <expr>` | Like `:print` but shows `_` for thunks (no bindings) |
| `:force <expr>` | Fully evaluate (may loop or throw) |
| `:type <expr>` | Show the type |
| `:show bindings` | All variables in current scope with types |

## Lazy evaluation — critical

- **Thunks render as `_`**. `dbg print x` may show `x = _`. Normal, not an error.
- **`:print` is safe** — never forces evaluation; creates `_t1`, `_t2` bindings for unevaluated subterms.
- **`:force` is dangerous** — infinite loops, exceptions, and side effects are all possible.
- **`_result`** is auto-bound at each breakpoint with the current expression's value (usually a thunk).

**Strategy**: `:print` first, `:type` when it shows `_`, `:force` only for values known to be finite.

## Type display under GHCi

- **Records**: `MyRecord {field1 = val, field2 = _}` — `_` means unforced.
- **Lists**: partial evaluation shown as `1 : 2 : _`.
- **Functions**: can't be printed; `:type f` for the signature.
- **Polymorphic**: `:print` may fail; monomorphize the call site (`:trace (f :: Int -> Int) 5`).

## Known blind spots

| Symptom | Fix |
|---------|-----|
| `ghci` not found | Install via ghcup. |
| Breakpoint not hit | Module must be interpreted, not compiled — `:load File.hs` not `.hi`. |
| `:print` all `_` | Value unevaluated — step further or `:force` carefully. |
| `:force` hangs | Infinite structure — Ctrl-C, then `:print`. |
| `:history` empty | Only recorded under `:trace`, not `:step`. Restart with `:trace main`. |
| Polymorphic error | Try monomorphic call: `:trace (fn :: Int -> Int) 5`. |
| Can't break in Prelude | Base libraries are compiled — only interpreted source is debuggable. |
