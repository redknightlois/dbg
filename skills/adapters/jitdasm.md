# .NET JIT Disassembly Adapter

## CLI

```
dbg start jitdasm <project.csproj> --args <method-pattern>
dbg start jitdasm <project.csproj>                          # captures ALL methods
```

The session builds the project, captures JIT disassembly, indexes it, and drops you into an interactive shell with query commands.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| .NET SDK 7+ | `dotnet --version` | https://dot.net/install |

## Commands

After the session starts, use these commands to query the captured disassembly:

| Command | What it does |
|---------|-------------|
| `methods` | List all methods with code sizes, sorted by size (largest first) |
| `disasm <pattern>` | Show full disassembly for methods matching pattern |
| `search <instruction>` | Find methods containing a specific instruction (e.g., `vfmadd`, `RNGCHKFAIL`) |
| `stats` | Summary: method count, total code size, SIMD/FMA/bounds check counts |
| `hotspots [N]` | Top N methods by code size (default 10) |
| `simd` | Find all methods using SIMD instructions |
| `help` | Show available commands |

## Method Pattern Syntax

The .NET JIT uses **single colon** `Class:Method` notation, NOT C++ double-colon `Class::Method`.

| Pattern | Matches |
|---------|---------|
| `SimdOps:DotProduct` | Specific method |
| `SimdOps:*` | All methods in a class |
| `*Distance*` | Contains "Distance" |
| `*` | Everything (default when no args) |

## Workflow

1. **Start a session** — capture all methods or a specific pattern:
   ```
   dbg start jitdasm myproject.csproj
   ```

2. **Get an overview** — check what was captured:
   ```
   dbg stats
   dbg hotspots
   ```

3. **Inspect specific methods**:
   ```
   dbg disasm SimdOps:DotProduct
   ```

4. **Search for instructions**:
   ```
   dbg search vfmadd        # find FMA usage
   dbg search RNGCHKFAIL    # find bounds checks
   dbg simd                   # find all SIMD methods
   ```

## What to Look For

| Instruction | Meaning |
|-------------|---------|
| `vfmadd231ps` | FMA — fused multiply-add, optimal for dot products |
| `zmm` registers | AVX-512 (512-bit SIMD) |
| `ymm` registers | AVX2 (256-bit SIMD) |
| `xmm` registers | SSE (128-bit SIMD) |
| `CORINFO_HELP_RNGCHKFAIL` | Bounds check — possible missed optimization |
| `vzeroupper` missing | AVX/SSE transition penalty risk |
| `mov [rsp+...]` | Stack spill — register pressure |

## Common Failures

| Symptom | Fix |
|---------|-----|
| No methods captured | The target must be an **executable** project (has a `Main`), not a library. Write a small console app that calls the target code. |
| No methods captured (BenchmarkDotNet) | BenchmarkDotNet spawns child processes — dbg can't instrument them. Write a standalone console app instead. |
| `Class::Method` not found | Use single colon: `Class:Method` (JIT convention) |
| Too much output | Use a specific pattern: `--args "Class:Method"` |
| App exits immediately | The app needs to run long enough for JIT compilation |
