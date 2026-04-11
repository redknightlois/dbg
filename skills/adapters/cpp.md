# C++ Adapter

## CLI

`dbg start cpp <binary> [--break file.cpp:line] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `lldb` | `which lldb-20 \|\| which lldb` | `sudo apt install lldb-20` |

Compile with `-g` for debug symbols: `g++ -g -o myapp main.cpp`

## Build

```bash
cmake --build build --config Debug   # CMake projects
make                                  # Makefile projects
g++ -g -O0 -o app main.cpp           # direct
```

## Breakpoint Patterns

| Pattern | When |
|---------|------|
| `file.cpp:42` | File and line |
| `MyClass::method` | Class method |
| `__cxa_throw` | Catch all C++ exceptions |
| `__assert_fail` | Catch assertion failures |

## Type Display

- **std::string**: LLDB shows content via data formatter. `print str` works.
- **std::vector<T>**: Shows size and elements via formatter. `print vec[0]` for indexing.
- **std::map / std::unordered_map**: Shown as key-value pairs.
- **std::shared_ptr<T>**: Shows refcount and pointee. `print *ptr` to dereference.
- **std::optional<T>**: Shows engaged/disengaged state.
- **Vtable / virtual**: `print *obj` shows actual derived type fields.
- **Templates**: Mangled in `bt` — read the demangled parts between backticks.

## Exceptions

```
breakpoint set --name __cxa_throw    # all C++ throws
```

Then `bt` at the throw site to see where the exception originated.

## Common Failures

| Symptom | Fix |
|---------|-----|
| Variables `<unavailable>` | Compile with `-g -O0` — optimizations hide locals |
| STL types show raw memory | LLDB data formatters not loaded — check `type summary list` |
| Mangled names in bt | Normal for C++ — demangled name is between backticks |
| Template noise in bt | Focus on your namespace's frames |
