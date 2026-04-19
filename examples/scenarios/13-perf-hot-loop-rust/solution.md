`dedup_push` does a linear scan per insert, so the batch is O(n²).
A perf/flamegraph pins `dedup_push` (inlined) or its `iter()` cmp
frame as ~95 % of self-time.

Fix: keep a `HashSet<u64>` alongside the `Vec`, or replace the
function with `buf.contains(&x)` swapped for a `HashSet::insert`
returning whether the key was new. 80k events drops from seconds
to ~1 ms.
