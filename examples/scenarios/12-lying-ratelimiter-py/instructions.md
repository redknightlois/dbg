# Rate limiter is letting bursts through at window boundaries

A customer filed a ticket claiming our sliding-window rate limiter permits 2x the advertised rate near the boundary between two windows. We replicated it — `broken.py` fails the embedded customer repro.

```
$ python3 broken.py
first batch:  5/5 accepted
second batch: 4/5 accepted, 1/5 rejected
AssertionError: sliding-window guarantee violated: rejected only 1/5 requests
in the second batch (expected >= 4). This is fixed-window behavior — bursts at
the boundary.
```

The class is `SlidingWindowLimiter`. The docstrings and comments describe sliding-window semantics in detail — and several of us have read the file top-to-bottom and concluded the code is correct. So either the comments are wrong, the tests are wrong, or the code is doing something different from what it claims. Figure out which.

Your task:
- Identify the root cause and fix it. The docstrings specify the intended behavior authoritatively — that's what the limiter is *supposed* to do.
- The fix may be small or may require reworking the decision logic. We don't know; we haven't found it.
- After your fix, `python3 broken.py` should print `OK`.

Constraints:
- Don't relax the assertion. The customer's complaint is legitimate.
- Don't rewrite the whole class from scratch. If you can't localize the bug, say so and explain why.

`solution.md` is a spoiler.
