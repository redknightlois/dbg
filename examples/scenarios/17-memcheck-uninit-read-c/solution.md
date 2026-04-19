Memcheck flags an uninitialized-value read in `main` at the
`if (c.flags & 0x1)` line. `Config.flags` is never set in `load()`
— its value is whatever was on the stack, which is usually zero
but can be nonzero after certain call paths.

Fix: initialize `c.flags = 0;` in `load()`, or switch to
`Config c = { .retries=3, .timeout_ms=500, .flags=0 };` to catch
the whole struct in one place.
