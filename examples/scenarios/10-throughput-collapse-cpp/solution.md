The per-batch `std::vector<Item> batch(this_batch)` allocates and *zero-initializes* 64 bytes × N. At batch=10K that's 640 KB per batch — fits in L2, allocator can satisfy from the freelist. At batch=100K that's 6.4 MB per batch — pushed into the OS allocator (mmap), zero-faulted page-by-page on first access, then released to the OS on `vector` destruction.

Profile with perf/callgrind: most of the time is in `__memset_avx2` (zero-init), `mmap`, and minor page faults — not in `process_batch`.

Fixes (any one):

1. Hoist the allocation out of the loop and `resize`/reuse:

```cpp
std::vector<Item> batch;
batch.reserve(batch_sz);
while (done < total) {
    std::size_t this_batch = std::min(batch_sz, total - done);
    batch.resize(this_batch);          // amortized, no realloc
    // ... fill and process ...
}
```

2. Skip default-init by using `std::make_unique_for_overwrite<Item[]>(this_batch)` (C++20) or a raw `new Item[this_batch]` — but that loses RAII. The hoist is cleaner.

3. Process in-place over a pre-allocated arena.

After the fix, batch=100000 should run in roughly the same wall-clock as batch=10000 (or faster).
