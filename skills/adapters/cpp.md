# C++ Adapter

For canonical commands and the investigation taxonomy see
[`_canonical-commands.md`](./_canonical-commands.md) and
[`_taxonomy-debug.md`](./_taxonomy-debug.md). This file covers only the
C++ / LLDB specifics.

## CLI

`dbg start cpp <binary> [--break file.cpp:line] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `lldb` | `which lldb-20 \|\| which lldb` | `sudo apt install lldb-20` |

Compile with `-g -O0`: `g++ -g -O0 -o myapp main.cpp`. Source files are rejected — pass the built binary.

## Build

```bash
cmake --build build --config Debug   # CMake
make                                  # Makefile
g++ -g -O0 -o app main.cpp            # direct
```

## Backend: LLDB

Canonical commands translate to standard LLDB vocabulary — see the mapping table in `_canonical-commands.md`. The canonicalizer uses the `cxx` language id for cross-session joins.

## C++-specific breakpoints

| Canonical form | When |
|---|---|
| `dbg break file.cpp:42` | File and line |
| `dbg break MyClass::method` | Class method (resolves overloads to all instances) |
| `dbg break module!MyClass::method` | Method in a specific shared object |
| `dbg catch cxx` | Every `throw` (raw equivalent: `breakpoint set --name __cxa_throw`) |
| `dbg break __assert_fail` | `assert()` failures |
| `dbg break <loc> if <expr>` | Conditional |

After a throw trap, `dbg stack` shows the originating frames.

## Type display under LLDB

- **`std::string`**: content shown via data formatter; `dbg print str` works.
- **`std::vector<T>`**: size + elements; `dbg print vec[0]` for indexing.
- **`std::map` / `std::unordered_map`**: key-value pairs.
- **`std::shared_ptr<T>`**: refcount + pointee; `dbg print *ptr` to dereference.
- **`std::optional<T>`**: engaged/disengaged state.
- **Virtual dispatch**: `dbg print *obj` shows actual derived fields.
- **Templates**: mangled in raw `bt`; canonical `dbg stack` shows demangled names. Filter by your namespace.

## Known blind spots

| Symptom | Fix |
|---------|-----|
| Variables `<unavailable>` | Compile with `-g -O0`. |
| STL types show raw memory | Data formatters not loaded — `dbg raw type summary list`. |
| Template noise in `dbg stack` | Focus on your namespace's frames; use `dbg frame <n>`. |
| Mangled names | LLDB demangler engaged; canonicalizer normalizes for `dbg cross <fqn>`. |
