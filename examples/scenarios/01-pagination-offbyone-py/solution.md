Line 26: `end = start + per_page + 1` should be `end = start + per_page`.

Python slice semantics are half-open, so the `+ 1` causes each page to overlap the next by one row. The `hit-trend` on `end` shows `6, 11, 16, 21, 26` while `start` shows `0, 5, 10, 15, 20` — gap of 5 between starts but window of 6 → one row of overlap per page.
