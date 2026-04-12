# JavaScript / TypeScript Adapter

## CLI

Start: `dbg start node <script.js> [--break file.js:line] [--args ...] [--run]`

Also works with: `dbg start js`, `dbg start typescript`, `dbg start ts`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| Node.js 18+ | `node --version` | https://nodejs.org or `nvm install --lts` |

## Build

None. Scripts run directly. For TypeScript, compile first with `tsc` or use `tsx`.

## Breakpoint Patterns

| Pattern | When |
|---------|------|
| `file.js:42` | File and line |
| `42` | Line in current file |
| `functionName` | Function entry |

The debugger starts paused at line 1. Use `--run` to continue to first breakpoint.

## Key Commands

| Command | Alias | What it does |
|---------|-------|-------------|
| `cont` | `c` | Continue to next breakpoint |
| `next` | `n` | Step over |
| `step` | `s` | Step into |
| `out` | `o` | Step out of current function |
| `bt` | — | Show backtrace |
| `list(N)` | — | Show N lines of surrounding source |
| `sb('file', line)` | — | Set breakpoint |
| `cb('file', line)` | — | Clear breakpoint |
| `breakpoints` | — | List all breakpoints |
| `watch('expr')` | — | Watch an expression |
| `unwatch('expr')` | — | Remove watch |
| `repl` | — | Enter REPL in current frame |
| `exec <expr>` | — | Evaluate expression in current frame |
| `.exit` | — | Quit debugger |

## Key Differences from PDB/LLDB

- Continue: `cont` or `c` (not `continue`)
- Step out: `out` or `o` (not `finish` or `return`)
- Backtrace: `bt` (like LLDB, not `where` like PDB)
- Set breakpoint: `sb('file.js', line)` (function call syntax, not `break file:line`)
- Evaluate: `exec expression` or enter `repl` mode (not `p` or `!`)

## REPL Mode

Type `repl` to enter a full JavaScript REPL in the current frame context:
```
repl
> variableName
> obj.method()
> require('util').inspect(deepObj, {depth: null})
```
Press Ctrl+C to return to debugger.

## Type Display

- **Objects**: `exec JSON.stringify(obj, null, 2)` or `repl` then inspect directly
- **Arrays**: `exec arr.length` and `exec arr.slice(0, 5)`
- **Maps/Sets**: `exec [...myMap.entries()]` or `exec [...mySet]`
- **Buffers**: `exec buf.toString('hex')` or `exec buf.toString('utf8')`
- **Promises**: `exec p.then(v => console.log(v))`
- **Classes**: `exec Object.getOwnPropertyNames(obj)` for all properties
- **Errors**: `exec err.stack`

## Async / Promises

- Backtrace may show internal V8 frames for async code
- Use `exec` to inspect promise state
- Breakpoints work in `async` functions and `.then()` callbacks
- `await` expressions can be evaluated in `repl` mode

## Common Failures

| Symptom | Fix |
|---------|-----|
| `Cannot find module` | Wrong working directory — check `exec process.cwd()` |
| Breakpoint not hit | File path mismatch — use path relative to entry script |
| TypeScript source not shown | Compile with source maps (`tsc --sourceMap`) or use `tsx` |
| `EADDRINUSE` | Port conflict — kill previous debugger: `dbg kill` |
| Variables show `undefined` | Step past the declaration line — V8 hoists but doesn't initialize |
| Stuck after `cont` | Program may have exited — check output for completion message |
