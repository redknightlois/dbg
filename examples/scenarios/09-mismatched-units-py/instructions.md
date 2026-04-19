# Fuel calculator off by a constant factor

QA filed a ticket: our fuel calculator overestimates every leg by roughly the same factor. For Boston→Denver (~1750 statute miles, 30 kt headwind) we report ~4025 L; the reference value is ~3500 L. The ratio (~1.151x) is suspicious — any ex-pilot will recognize it.

```
$ python3 broken.py
Boston→Denver: 4025 L
AssertionError: fuel estimate 4025 L outside reasonable range — check unit conversions
```

Find the bug, fix it, get the OK line.

Constraints:
- The fix is a multiplication by a constant. The interesting part isn't the patch — it's spotting *which line* the missing conversion belongs on. There's a leftover TODO that hints at it.
