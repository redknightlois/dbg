# RSS keeps growing, OOM kills us every 18 hours

Our request handler has been showing slow RSS growth in production —
graph is linear in request count, kernel eventually OOMs the
process. No visible leak in `valgrind --leak-check=full` because
the allocations are still reachable at exit.

Reproduce:

```
cd examples/scenarios/16-massif-unbounded-heap-c
make
./broken
```

The program exits fine; the issue is the shape of the heap over
time, not a leaked-after-exit block. Please run a heap-profile
(massif or equivalent) across the run and tell me which call path
is growing without bound. Propose a bounded-cache fix — we don't
want to lose the hit-rate we have now, so any eviction is fine as
long as the resident set is capped.
