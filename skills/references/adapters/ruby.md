# Ruby Adapter

For canonical commands and the investigation taxonomy see
[`_canonical-commands.md`](./_canonical-commands.md) and
[`_taxonomy-debug.md`](./_taxonomy-debug.md). This file covers only the
Ruby / rdbg specifics. For CPU profiling see `ruby-profile.md`.

## CLI

Start: `dbg start ruby <script.rb> [--break file.rb:line] [--args ...] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| Ruby 3.1+ | `ruby --version` | `sudo apt install ruby` or `brew install ruby` |
| `rdbg` | `which rdbg` | `gem install debug` (bundled with 3.1+) |

## Backend: rdbg

Canonical commands translate to rdbg — see `_canonical-commands.md`.

## Ruby-specific breakpoints

| Canonical form | When |
|---|---|
| `dbg break file.rb:42` | File and line |
| `dbg break MyClass#method` | Instance method entry |
| `dbg break MyClass.method` | Class method entry |
| `dbg break method_name` | Method entry (any class) |
| `dbg catch StandardError` | Exception breakpoint |
| `dbg break <loc> if <expr>` | Conditional |

## Type display

- **Arrays/Hashes**: `dbg print my_hash` (rdbg pretty-prints by default).
- **Objects**: `dbg print instance_variables.map { |v| [v, instance_variable_get(v)] }.to_h`.
- **Large collections**: `dbg print big_array[0..4]`.
- **Class info**: `dbg print obj.class`, `dbg print obj.methods.sort`.
- **ActiveRecord**: `dbg print record.attributes`.

## In-process execution

Arbitrary Ruby expressions work via `dbg print`. For multi-line evaluation drop to `dbg raw eval ...`.

## Known blind spots

| Symptom | Fix |
|---------|-----|
| `rdbg` not found | `gem install debug` or install Ruby 3.1+. |
| Breakpoint not hit | Path mismatch — use absolute paths. |
| `Gem::MissingSpecError` | Start under Bundler: `bundle exec dbg start ruby script.rb`. |
| Rails app won't start | Prefer `rdbg -c --` command mode; launch Rails as the script argument. |
| Frozen string errors | Some gems freeze strings; use `.dup` before mutating in `dbg print`. |
