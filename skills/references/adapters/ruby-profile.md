# Ruby Profiler (stackprof)

## CLI

`dbg start ruby-profile <script.rb> [--args ...]`

## What It Profiles

StackProf is a sampling CPU profiler for Ruby. The session runs the script under StackProf, converts the profile to callgrind format, then drops into an interactive REPL for querying function-level timing, call graphs, and hotspots.

**Good at:** finding hot functions, call counts, inclusive/exclusive time, call trees, hotpath analysis.

**Cannot do:** memory profiling (use `ruby-prof` or `memory_profiler` gem for that), line-level annotation, wall-clock accuracy under I/O.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| Ruby 3.0+ | `ruby --version` | `sudo apt install ruby` or `brew install ruby` |
| stackprof gem | `ruby -e "require 'stackprof'"` | `gem install stackprof` |
| ruby-dev | — | `sudo apt install ruby-dev` if gem native extension fails |

## Build

None. Scripts run directly. The profiler wraps execution automatically.

## How It Works

1. `dbg start ruby-profile script.rb` wraps the script with StackProf sampling
2. After execution, profile data is converted to callgrind format
3. The session drops into the `ruby-profile>` REPL with all parsed data
4. Use commands to explore hotspots, call graphs, and timing

## Key Commands

| Command | What it does |
|---------|-------------|
| `hotspots [N] [pattern]` | Top N functions by inclusive time (default 10) |
| `flat [N] [pattern]` | Top N functions by self time (default 20) |
| `calls <pattern>` | What does this function call? |
| `callers <pattern>` | Who calls this function? |
| `inspect <pattern>` | Detailed breakdown: self/inclusive time, callees |
| `stats [pattern]` | Summary statistics (function count, total time) |
| `memory [N] [pattern]` | Top N functions by memory (if available) |
| `search <pattern>` | Find functions matching pattern |
| `tree <pattern>` | Call tree rooted at matching function |
| `hotpath` | Critical path through the call graph |
| `focus <pattern>` | Filter all views to functions matching pattern |
| `ignore <pattern>` | Exclude functions matching pattern |
| `reset` | Clear focus/ignore filters |

## Workflow

1. Start session: `dbg start ruby-profile script.rb`
2. Wait for "ready: N functions profiled"
3. Overview: `hotspots` — find the most expensive functions
4. Self time: `flat` — where is time actually spent (excluding callees)?
5. Drill down: `inspect fibonacci` — detailed breakdown of a function
6. Call graph: `calls fibonacci` — what does it call? `callers fibonacci` — who calls it?
7. Critical path: `hotpath` — which call chain consumes the most time?

## Pattern Matching

All commands accept optional patterns that filter by function name (case-insensitive substring match):

```
hotspots 5 fibonacci   # top 5 methods matching "fibonacci" by inclusive time
flat 10 Object         # top 10 Object methods by self time
stats Array            # summary for Array methods only
```

## Common Failures

| Symptom | Fix |
|---------|-----|
| `cannot load such file -- stackprof` | `gem install stackprof` |
| Native extension build fails | `sudo apt install ruby-dev` for header files |
| 0 functions profiled | Script too fast — add iterations or longer workload |
| Missing method names | C extensions may show as `<cfunc>` — this is normal |
| Permission denied on output | Check write permissions on temp directory |
