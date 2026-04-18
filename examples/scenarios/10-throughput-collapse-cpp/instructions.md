# Throughput collapses at large batch sizes

We process 10M items in batches. Larger batches should be at least as fast as smaller ones (fewer iterations of outer overhead). Instead, going from 10K to 100K per batch makes the *total* run substantially slower.

```
$ c++ -g -O2 -std=c++17 broken.cpp -o broken && ./broken
batch=10000 total=10000000 time=420ms sink=...
batch=100000 total=10000000 time=11800ms sink=...
```

Your task:
- Explain *why*. The per-batch processing inside `process_batch` is identical in both runs — same arithmetic, same total amount of work. So the cost has to be elsewhere.
- Once you have a concrete diagnosis, propose the smallest possible code change that recovers throughput at the larger batch size.

Constraints:
- "Just use smaller batches" is not the fix. The customer's pipeline gives us 100K-item chunks; we need to process them efficiently.
- Hand-wavy answers ("memory pressure?") don't count. Show what's actually expensive.
