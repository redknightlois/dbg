# LRU cache corrupts under load

We have a hand-rolled LRU cache (doubly-linked list + dict). It works fine for small inputs and the unit tests pass. Under a moderate workload (1000 mixed put/get ops over a 200-key universe, capacity 50), the linked list becomes structurally inconsistent — forward and reverse traversals don't agree on which keys are in the list — and the run aborts.

```
$ python3 broken.py
Traceback (most recent call last):
  ...
  File "broken.py", line 101, in list_keys_reverse
    assert cur is not None
AssertionError
```

Your task:
- Find the bug and fix it. The fix is in `LruCache`.
- The cache logic is spread across several short methods that all touch `prev`/`next`. Each method, in isolation, looks correct — every individual line is a textbook DLL operation. Reading them and trying to spot the bug is harder than it sounds; we wasted half a day on it ourselves.
- The diagnostic helpers `list_keys()` and `list_keys_reverse()` are there for you to use.

Constraints:
- The fix is a one-line change (specifically: a reordering, not new logic).
- Don't add defensive None-guards or change the data structure. The bug is upstream of the symptom.

`solution.md` is a spoiler.
