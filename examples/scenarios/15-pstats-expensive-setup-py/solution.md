`dbg hotspots` (via pstats) surfaces `_load_templates` and
`re.compile` dominating cumulative time — fmt() itself is fast.

Fix: move `_patterns` to a class-level cache or a module-level
`@functools.lru_cache`. The easiest drop-in: make `_load_templates`
a `@staticmethod @lru_cache(maxsize=1)` — Renderer() instantiation
amortizes to ~free after the first call.
