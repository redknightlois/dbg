# PHP Profiler Adapter (Xdebug)

## CLI

`dbg start php-profile <script.php> [--args ...]`

## What It Profiles

Xdebug in profiling mode instruments every PHP function call. The session runs the script, captures the cachegrind output, then drops into an interactive REPL for querying function-level timing, call graphs, and memory allocation.

**Good at:** finding hot functions, call counts, inclusive/exclusive time, call trees, memory allocations per function.

**Cannot do:** sampling-based low-overhead profiling (Xdebug instruments every call), line-level source annotation, wall-clock accuracy under I/O.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| PHP 8.0+ | `php --version` | `sudo apt install php-cli` or `brew install php` |
| Xdebug 3.x | `php -m \| grep xdebug` | `sudo apt install php-xdebug` or `pecl install xdebug` |

## How It Works

The session starts a shell, runs PHP with Xdebug profiling enabled, captures a cachegrind file, then replaces the shell with the `php-profile>` REPL. All queries run against the parsed profile data — no external tools needed.

## Key Commands

| Command | What it does |
|---------|-------------|
| `hotspots [N] [pattern]` | Top N functions by inclusive time (default 10) |
| `flat [N] [pattern]` | Top N functions by self time (default 20) |
| `calls <pattern>` | What does this function call? |
| `callers <pattern>` | Who calls this function? |
| `inspect <pattern>` | Detailed breakdown: self/inclusive time, memory, callees |
| `stats [pattern]` | Summary statistics (function count, total time, memory) |
| `memory [N] [pattern]` | Top N functions by memory allocation |

## Workflow

1. Start session: `dbg start php-profile script.php`
2. Wait for "ready: N functions profiled"
3. Overview: `hotspots` — find the most expensive functions
4. Self time: `flat` — where is time actually spent (excluding callees)?
5. Drill down: `inspect multiply` — detailed breakdown of a function
6. Call graph: `calls multiply` — what does it call? `callers multiply` — who calls it?
7. Memory: `memory` — who allocates the most?

## Pattern Matching

All commands accept optional patterns that filter by function name (case-insensitive substring match):

```
hotspots 5 Matrix     # top 5 Matrix methods by inclusive time
flat 10 build         # top 10 functions matching "build" by self time
stats Matrix          # summary for Matrix class methods only
```

## Common Failures

| Symptom | Fix |
|---------|-----|
| "no functions found" | Script may have exited early — check for PHP errors |
| Xdebug not loaded | `php -m` should list xdebug — install with `pecl install xdebug` |
| Profile seems empty | Ensure `xdebug.mode=profile` is active — check `php -i \| grep xdebug.mode` |
| Very short scripts show 0ns | Xdebug resolution is 10ns — very fast scripts may show zero |
