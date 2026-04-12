# .NET Adapter

## CLI

Start: `dbg start dotnet <exe-or-dll-or-dir> [--break File.cs:line] [--args ...] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| .NET SDK | `dotnet --version` | https://dot.net/install |
| `netcoredbg` | `which netcoredbg` or `$NETCOREDBG` | See below |

Install netcoredbg:
```bash
mkdir -p ~/.local/share/netcoredbg
curl -sL https://github.com/Samsung/netcoredbg/releases/latest/download/netcoredbg-linux-amd64.tar.gz | tar xz -C ~/.local/share/netcoredbg
export NETCOREDBG=~/.local/share/netcoredbg/netcoredbg/netcoredbg
```

If `DOTNET_ROOT` is needed (homebrew, custom installs), tell the user to add it to their shell profile (`~/.bashrc` or `~/.zshrc`) rather than setting it per-command. The daemon inherits the environment from `dbg start` — do NOT prepend env vars to every `dbg` command.

## Build

```bash
dotnet build -c Debug
```

Target resolution: pass a native executable (preferred), `.dll`, project directory, or `.csproj`. The CLI prefers the apphost over `.dll`.

## Breakpoint Patterns

| Pattern | When |
|---------|------|
| `File.cs:42` | File and line |
| `Namespace.Class.Method` | Fully qualified method |
| `catch System.NullReferenceException` | Exception breakpoint |
| `catch System.Exception` | All exceptions |

Breakpoints are **pending** until `run` loads the assembly. This is normal.

## Type Display

- **Collections**: Dumps all internals. Focus on `Count`, use `print dict[key]` for values
- **Strings**: Printed directly as values
- **Nullable<T>**: Shows `HasValue` and `Value`

## Async / Task

- Backtrace shows `MoveNext()` frames (state machine steps)
- Locals may appear as state machine fields
- Tasks can hop threads

## Common Failures

| Symptom | Fix |
|---------|-----|
| `COR_E_FILENOTFOUND` | Set `DOTNET_ROOT` |
| Breakpoint stays pending | Normal until `run` loads the module |
| `.dll` fails to launch | Use the native executable instead |
| `list` shows no source | Add `<EmbedAllSources>true</EmbedAllSources>` to csproj |
