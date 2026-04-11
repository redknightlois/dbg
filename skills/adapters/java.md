# Java Adapter

## CLI

`dbg start java <class-or-jar> [--break File.java:line] [--args ...] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| `jdb` | `which jdb` | Included with JDK: `sudo apt install default-jdk` |

Compile with debug info: `javac -g *.java`

## Build

```bash
javac -g *.java                           # compile with debug symbols
mvn compile -Dmaven.compiler.debug=true   # Maven
gradle compileJava                         # Gradle (debug on by default)
```

## Breakpoint Patterns

| Pattern | When |
|---------|------|
| `File.java:42` | File and line |
| `com.pkg.Class.method` | Fully qualified method |
| `com.pkg.Class:42` | Class and line |

## Type Display

- **Strings**: Shown directly with value.
- **Collections**: Shows type and size. Use `print list.get(0)` for elements.
- **Arrays**: `print arr[0]`, `print arr.length`.
- **Objects**: `dump obj` shows all fields. `print obj.field` for specific.
- **null**: Shown as `null`.

## Threads

```
threads                       # list all threads
thread <name>                 # switch thread
where all                     # bt for all threads
```

## Common Failures

| Symptom | Fix |
|---------|-----|
| `Unable to set breakpoint` | Class not loaded yet — use deferred breakpoint |
| No local vars | Compile with `javac -g` (includes local variable table) |
| `jdb` not found | Install JDK, not just JRE |
| ClassNotFound on run | Set classpath: `classpath` command or `-classpath` arg |
