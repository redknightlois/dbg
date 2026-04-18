# What this scenario is testing

The source is written to *lie convincingly*. The class docstring, method docstrings, variable names (`window_start`, `_advance_window`, "slide the window forward"), and even the `stats()` invariant ("the two should always agree") all describe a sliding-window limiter. But the decision path (`current_count < self.limit` gated by `_advance_window`'s step-reset) implements a fixed-window limiter. The `events` deque is a decoy — it's maintained correctly but never consulted for the accept/reject decision.

Reading the file carefully *reinforces* the wrong mental model. You come away thinking "yes, this is sliding-window with a safety counter." You'd be wrong.

## What exposes the lie

Printing `limiter.stats()` on any failing request shows `events_in_window=N, accepted_in_window=M` with `N != M`. The docstring claims they must agree. The divergence is the smoking gun. This is essentially free from a debugger (breakpoint on `allow` return, dump `stats()`) and invisible from source inspection alone.

Other runtime tells:
- Logging the sequence of `window_start` values shows discrete jumps of `window_seconds`, not continuous sliding.
- Logging `(now, len(events), current_count)` across the workload shows `current_count` resetting while `len(events)` stays high — classic fixed-window fingerprint.

## The actual fix

Replace the `current_count`-based decision with one that uses `len(self.events)`:

```python
def allow(self, now: float) -> bool:
    self._evict_stale(now)
    if len(self.events) < self.limit:
        self.events.append(now)
        return True
    return False
```

That's the entire sliding-window algorithm. `_advance_window`, `window_start`, and `current_count` are all dead weight — delete them. The lying invariant in `stats()` becomes trivially true because `accepted_in_window` just *is* `len(self.events)` now.

## Why this is interesting as a dbg test

Every bug we've tried so far has failed to provoke debugger use because the source was short and honest — careful reading plus symptom-shape matching won. This scenario inverts that: the source is actively misleading, so reading wastes time and reading *more carefully* wastes more time. Runtime observation — specifically, checking whether a documented invariant actually holds — is the shortest path to the answer.

An agent that reaches for `dbg break broken.py:<allow return>` + inspect locals, or even just `print(limiter.stats())` inside the loop, solves this fast. An agent that trusts the comments walks in circles.
