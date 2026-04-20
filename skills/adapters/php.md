# PHP Adapter

For canonical commands and the investigation taxonomy see
[`_canonical-commands.md`](./_canonical-commands.md) and
[`_taxonomy-debug.md`](./_taxonomy-debug.md). This file covers only the
PHP / phpdbg specifics. For profiling see `php-profile.md`.

## CLI

Start: `dbg start php <script.php> [--break file.php:line] [--args ...] [--run]`

## Preconditions

| Requirement | Check | Fix |
|-------------|-------|-----|
| `dbg` | `which dbg` | `cargo install dbg-cli` — ensure `~/.cargo/bin` is in PATH |
| PHP 8.0+ | `php --version` | `sudo apt install php` or `brew install php` |
| phpdbg | `which phpdbg` | Bundled with PHP since 5.6 |

Verify the right PHP: `which php`. The daemon inherits env from `dbg start` — do not prepend env vars per command.

## Backend: phpdbg

Canonical commands translate to phpdbg — see `_canonical-commands.md`. `dbg print <expr>` runs PHP in the current scope (maps to phpdbg's `ev`).

## PHP-specific breakpoints

| Canonical form | When |
|---|---|
| `dbg break file.php:42` | File and line |
| `dbg break my_function` | Function entry |
| `dbg break MyClass::method` | Method entry |
| `dbg break <loc> if <expr>` | Conditional |

## Type display

- **Arrays**: `dbg print print_r($array, true)` or `dbg print var_export($array, true)`.
- **Objects**: `dbg print get_object_vars($obj)` or `dbg print var_export($obj, true)`.
- **Large data**: `dbg print array_slice($big_array, 0, 5, true)`.
- **Class info**: `dbg print get_class($obj)`, `dbg print get_class_methods($obj)`.
- **Type check**: `dbg print get_debug_type($var)`.

Opcodes: `dbg raw print exec` shows opcodes for the current file — rarely needed.

## Known blind spots

| Symptom | Fix |
|---------|-----|
| `phpdbg` not found | Install PHP — phpdbg ships with standard PHP packages. |
| Breakpoint not hit | Path mismatch — use absolute paths. |
| `dbg print` returns opcodes | Wrap with `var_dump`/`var_export` for complex types. |
| Missing extensions | `php -m` to list; add `dl()` or config before `dbg start`. |
| Segfault on startup | `phpdbg -n` disables extensions — test in isolation. |
