# .NET Adapter

## CLI

`$DBG` = `~/.claude/skills/debug/scripts/dbg`

Start: `$DBG start dotnet <exe-or-dll-or-dir> [--break File.cs:line] [--args ...] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| .NET SDK | `dotnet --version` | https://dot.net/install |
| `netcoredbg` | `which netcoredbg` or `$NETCOREDBG` | See below |
| `pexpect` | `python3 -c "import pexpect"` | `pip install pexpect` |

Install netcoredbg:
```bash
mkdir -p ~/.local/share/netcoredbg
curl -sL https://github.com/Samsung/netcoredbg/releases/latest/download/netcoredbg-linux-amd64.tar.gz | tar xz -C ~/.local/share/netcoredbg
export NETCOREDBG=~/.local/share/netcoredbg/netcoredbg/netcoredbg
```

Set `DOTNET_ROOT` if auto-detection fails (homebrew, custom installs).

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
