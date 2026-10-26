# Java / Kotlin Adapter

For canonical commands and the investigation taxonomy see
[`_canonical-commands.md`](./_canonical-commands.md) and
[`_taxonomy-debug.md`](./_taxonomy-debug.md). This file covers only the
JVM / jdb specifics.

## CLI

`dbg start java <class-or-jar> [--break File.java:line] [--args ...] [--run]`

Alias: `kotlin`.

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `jdb` | `which jdb` | Included with JDK: `sudo apt install default-jdk` (JDK, not JRE) |

Compile with `-g` (or equivalent Maven/Gradle flag) for usable locals. If a breakpoint never fires, `dbg` surfaces a hint about missing `-g`.

## Build

```bash
javac -g *.java                                    # compile with debug symbols
mvn compile -Dmaven.compiler.debug=true            # Maven
gradle compileJava                                  # Gradle (debug on by default)
```

## Backend: jdb

Canonical commands translate to jdb vocabulary. Translation table in `_canonical-commands.md`. Breakpoints are **deferred** until the class loads — this is normal.

## Java-specific breakpoints

| Canonical form | When |
|---|---|
| `dbg break File.java:42` | File and line |
| `dbg break com.pkg.Class.method` | Fully qualified method |
| `dbg break com.pkg.Class:42` | Class and line |
| `dbg catch java.lang.NullPointerException` | Exception breakpoint |

## Type display

- **Strings**: printed directly.
- **Collections**: type + size; `dbg print list.get(0)` for elements.
- **Arrays**: `dbg print arr[0]`, `dbg print arr.length`.
- **Objects**: `dbg raw dump obj` shows all fields; `dbg print obj.field` for specific.
- **`null`**: shown as `null`.

## Threads

`dbg threads` lists threads, `dbg thread <name>` switches. `dbg raw where all` still works for a full-process backtrace when needed.

## Known blind spots

| Symptom | Fix |
|---------|-----|
| `Unable to set breakpoint` | Class not loaded yet — deferred breakpoints fire once the classloader pulls it. |
| No local vars | Compile with `javac -g` (local-variable table). |
| `jdb` not found | Install JDK, not just JRE. |
| `ClassNotFound` on run | Set classpath via `-classpath` arg or `dbg raw classpath`. |
| Kotlin inlined calls missing from stack | `-Xno-optimized-callable-references` and disable inlining for debug builds. |
