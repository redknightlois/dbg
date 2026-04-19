# Normalize kernel scales worse than expected — compute-bound?

Our `normalize(out, mean, scale, x)` kernel is on the critical
path of the model. A teammate wants to rewrite it with FMA to
"cut the compute". Before we do that, I want to know whether the
kernel is actually compute-bound or if we're chasing the wrong
bottleneck.

Reproduce:

```
cd examples/scenarios/23-gdbg-cuda-bandwidth-bound
make
./broken
```

Please collect ncu hardware counters (or gdbg's equivalent) and
tell me:
1. Is this compute-bound, memory-bound, or something else?
2. What's the achieved DRAM throughput as a fraction of peak?
3. If the kernel is already near ceiling, what structural change
   would actually help? (e.g. kernel fusion with upstream pass).

I'd rather not ship an FMA rewrite that moves the needle zero.
