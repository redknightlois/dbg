# This classifier is slower than we'd expect

We have a classifier that filters 4M samples into a [lo, hi] range
and sums them. It's 12-line inner loop, nothing fancy, yet a single
invocation takes ~40 ms on an otherwise-idle box with `-O2` on. We
were hoping for something closer to raw-memory-bandwidth.

Reproduce:

```
cd examples/scenarios/14-callgrind-branch-mispredict-c
make
time ./broken
```

The CPU is pegged. Instrument with a cycle-accurate profiler
(callgrind, or similar) and figure out where the cycles are going —
we suspect it's not actually memory-bound. If the root cause is
what we think it is, propose a rewrite of the inner loop that gets
meaningfully closer to the theoretical ceiling.
