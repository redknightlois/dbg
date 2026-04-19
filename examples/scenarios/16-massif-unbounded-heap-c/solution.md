Massif's heap-xtree attributes nearly all live bytes to
`add_to_cache` (via `calloc(4096,1)` for the meta blob). The
snapshot-over-time graph climbs linearly across the 4000-request
run — classic unbounded cache.

Fix: replace the linked list with a bounded LRU (or any size-capped
map). Evict on insert when `count >= MAX_ENTRIES`. A size cap of
~512 tenants is usually plenty given real hit-rate.
