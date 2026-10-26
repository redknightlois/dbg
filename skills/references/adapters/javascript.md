# JavaScript / TypeScript Adapter

For canonical commands and the investigation taxonomy see
[`_canonical-commands.md`](./_canonical-commands.md) and
[`_taxonomy-debug.md`](./_taxonomy-debug.md). This file covers only the
Node / V8 Inspector specifics. For CPU profiling see `js-profile.md`.

## CLI

Start: `dbg start node <script.js> [--break file.js:line] [--args ...] [--run]`

Aliases: `js`, `javascript`, `ts`, `typescript`, `nodejs`, `bun`, `deno`. Auto-detect picks this adapter from `.js`/`.mjs`/`.ts` extensions.

Attach mode: `dbg start node --attach-port <PORT>` connects to an already-running Node process started with `--inspect=<PORT>`.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| Node.js 18+ | `node --version` | https://nodejs.org or `nvm install --lts` |

For TypeScript: compile with source maps (`tsc --sourceMap`) or run under `tsx`. Without maps, `dbg list` shows transpiled JS.

## Backend: node-proto (V8 Inspector)

The old PTY-based `node-inspect` REPL is retired; every Node alias now routes through the V8 Inspector transport. Canonical commands translate to Inspector RPCs — translation table in `_canonical-commands.md`. `dbg raw <native-cmd>` sends directly to the Inspector (not the dying `node inspect` CLI).

## JS/TS-specific breakpoints

| Canonical form | When |
|---|---|
| `dbg break file.js:42` | File and line |
| `dbg break functionName` | Function entry (script-scope) |
| `dbg break file.js:42 if <expr>` | Conditional (evaluated in V8) |
| `dbg break file.js:42 log "x={x}"` | Logpoint (no stop) |
| `dbg catch uncaught` | Pause on uncaught exceptions |
| `dbg catch all` | Pause on every thrown exception |

## Type display

- **Objects**: `dbg print JSON.stringify(obj, null, 2)` for a printable form.
- **Arrays**: `dbg print arr.length`, `dbg print arr.slice(0, 5)`.
- **Maps/Sets**: `dbg print [...myMap.entries()]`, `dbg print [...mySet]`.
- **Buffers**: `dbg print buf.toString('utf8')` (or `'hex'`).
- **Promises**: `dbg print await p` (V8 supports top-level await in eval).
- **Errors**: `dbg print err.stack`.

## Async / Promises

- `dbg stack` may include internal V8 async frames — filter to your files with `dbg frame <n>`.
- Breakpoints fire in `async` functions, `.then()` callbacks, and async generators.
- Set variables with `dbg set <name>=<expr>` (Inspector `Runtime.evaluate`).

## TypeScript source maps

- With valid source maps, `dbg break file.ts:42` resolves to the compiled location automatically.
- Without source maps, breakpoints must target the compiled `.js`. `dbg list` reflects whatever V8 loaded.

## Known blind spots

| Symptom | Fix |
|---------|-----|
| `Cannot find module` | Wrong working directory — `dbg print process.cwd()`. |
| Breakpoint not hit | Path mismatch — use absolute paths or the path V8 reports in `dbg stack`. |
| TypeScript source not shown | Compile with source maps or use `tsx`. |
| `EADDRINUSE` on attach | Previous Inspector didn't release the port — `dbg kill` before retrying. |
| Variables `undefined` | Step past the declaration line — V8 hoists but doesn't initialize. |
| Bun/Deno target quirks | They speak the Inspector protocol but with gaps — fall back to `dbg raw` if a canonical op errors. |
