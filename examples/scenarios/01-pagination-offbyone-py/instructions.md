# Pagination drift

QA opened a ticket: walking every page of our paginated search no longer round-trips back to the input list. Rows are being duplicated across page boundaries. Reproduce with `python3 broken.py` and fix the underlying bug.

```
$ python3 broken.py
AssertionError: pagination drift: duplicated=[5, 11, 17] missing=[]
```

Constraints:
- The fix must be in `broken.py`. Don't paper over the symptom by deduplicating in `walk_all_pages`.
- We expect a one-character change once you find it. Anything bigger means you misdiagnosed.
- Don't open `broken.py` and stare at it — the file is small but the bug is the kind that's much faster to *observe* than to read out. Pick whatever investigation approach gets you the values of the relevant locals across consecutive iterations.

Don't peek at `solution.md` until you've located it.
