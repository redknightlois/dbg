# Invoice renderer is unreasonably slow per request

The invoice service renders 200 invoices per dashboard refresh.
It's taking 10+ seconds when it should be microseconds — the
actual format string is one f-string.

Reproduce:

```
cd examples/scenarios/15-pstats-expensive-setup-py
time python3 broken.py
```

The team has been staring at `fmt()` looking for inefficiency.
I don't think it's in there — can you profile the whole loop with
cProfile / pstats and tell me where the tottime actually piles up?
If you find the smoking gun, suggest the smallest fix that keeps
the `fmt()` API stateless-looking from the caller's side.
