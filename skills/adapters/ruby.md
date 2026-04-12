# Ruby Adapter (rdbg)

## CLI

Start: `dbg start ruby <script.rb> [--break file.rb:line] [--args ...] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| Ruby 3.1+ | `ruby --version` | `sudo apt install ruby` or `brew install ruby` |
| rdbg | `which rdbg` | `gem install debug` — Ruby 3.1+ bundles it; older versions need the gem |

## Build

None. Scripts run directly. Verify correct Ruby: `which ruby`, `ruby --version`.

## Breakpoint Patterns

| Pattern | When |
|---------|------|
| `file.rb:42` | File and line |
| `MyClass#method` | Instance method entry |
| `MyClass.method` | Class method entry |
| `method_name` | Method entry (any class) |

rdbg starts paused at line 1. Use `--run` to continue to first breakpoint.

## Key Commands

| Command | Alias | What it does |
|---------|-------|-------------|
| `continue` | `c` | Continue to next breakpoint |
| `step` | `s` | Step into (enters methods) |
| `next` | `n` | Step over |
| `finish` | `fin` | Run to end of current method |
| `break` | `b` | Set breakpoint |
| `delete` | `del` | Delete breakpoint |
| `backtrace` | `bt` | Show backtrace |
| `frame` | `f` | Select stack frame |
| `up` | — | Move up one frame |
| `down` | — | Move down one frame |
| `list` | `l` | Display source code |
| `info` | `i` | Show debug info (`i b` = breakpoints, `i l` = locals) |
| `p <expr>` | — | Evaluate and print expression |
| `pp <expr>` | — | Pretty-print expression |
| `watch` | — | Set watchpoint on expression |
| `catch <Exception>` | — | Break on exception |
| `quit!` | `q!` | Force quit (no confirmation) |

## Key Differences from PDB/LLDB

- Evaluate: `p expression` or just type the expression (not `ev` like phpdbg)
- Backtrace: `bt` or `backtrace` (like LLDB, not `where` like PDB)
- Step out: `finish` / `fin` (not `return` like PDB)
- Locals: `i l` or `info locals` (not `locals()`)
- Catch exception: `catch ExceptionClass` (not `catch throw`)
- Force quit: `quit!` or `q!` to skip confirmation

## In-Process Execution

Any expression typed at the prompt is evaluated in the current frame:
```
p local_var
pp @instance_var
p self.class.ancestors
p ENV['RAILS_ENV']
p ObjectSpace.count_objects
```

Multi-line evaluation with `eval`:
```
eval do
  hash.each { |k, v| puts "#{k}: #{v}" }
end
```

## Type Display

- **Arrays/Hashes**: `pp my_hash` or `p my_array.length`
- **Objects**: `pp instance_variables.map { |v| [v, instance_variable_get(v)] }.to_h`
- **Large collections**: `p big_array[0..4]`
- **Class info**: `p obj.class` and `p obj.methods.sort`
- **ActiveRecord**: `pp record.attributes`
- **Type check**: `p obj.class` or `p obj.is_a?(String)`

## Common Failures

| Symptom | Fix |
|---------|-----|
| `rdbg` not found | `gem install debug` — or check Ruby 3.1+ is installed |
| Breakpoint not hit | Check file path — use path relative to execution dir |
| `Gem::MissingSpecError` | Run `bundle exec rdbg` instead, or `gem install` the dependency |
| Frozen string error | Some gems freeze strings; use `.dup` before mutation |
| Rails app won't start | Use `rdbg -c -- rails server` for command mode |
| Encoding error on eval | Prefix expression with `# encoding: utf-8` |
