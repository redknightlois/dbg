# Wrong dispatch order

Production complaint from the dispatch team: low-priority orders are being dispatched ahead of high-priority ones. The sort is in `broken.cpp`. Reproduce, find the bug, fix it.

```
$ c++ -g -O0 -std=c++17 broken.cpp -o broken && ./broken
dispatch order:
  A p=1 d=1000
  E p=2 d=800
  C p=3 d=500
  D p=5 d=1500
  B p=5 d=2000
BUG: front of queue has priority 1 (expected 5)
```

Constraints:
- The intended order is **priority DESC, then deadline ASC**. The implementation gets at least one of those wrong.
- The fix is one operator. If you find yourself restructuring the comparator, back up.
- Convince us via observation, not by reading. Inspect the comparator's inputs and return value across a few calls — the inversion is glaring once you see actual values, easy to miss when reading the source.

`solution.md` is a spoiler. Don't open it first.
