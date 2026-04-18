"""Sliding-window rate limiter.

Enforces at most N accepted requests per rolling W-second window.

Unlike fixed-window limiters — which permit bursts of up to 2N
across a window boundary because the counter resets on the jump —
this implementation slides the window continuously, so the limit
holds at every instant. Concretely:

    at any time `t`, the number of accepted requests in the interval
    (t - window_seconds, t] must not exceed `limit`.

We maintain a deque of every accepted-request timestamp and evict
entries that have aged out. The decision to accept is made against
the live window content, so there is no boundary-jump burst window.

See `tests/test_sliding_window.py` for the invariants this must
satisfy. The workload at the bottom of this file reproduces a
failure mode filed by a customer: their benchmark claims 2x the
advertised rate gets through at the window boundary.
"""

from __future__ import annotations
from collections import deque


class SlidingWindowLimiter:
    """Rate limiter enforcing `limit` requests per rolling `window_seconds`."""

    def __init__(self, limit: int, window_seconds: float) -> None:
        self.limit = limit
        self.window_seconds = window_seconds
        # Every accepted request timestamp, in FIFO order. Used by
        # `_evict_stale` to keep the window content current.
        self.events: deque[float] = deque()
        # Monotonic start of the current window; slides forward as
        # time passes so that `window_start + window_seconds` is the
        # window's right edge at the moment of decision.
        self.window_start: float = 0.0
        # Accepted-count within the current window. Grows with each
        # acceptance, resets when the window has fully advanced past
        # the old contents.
        self.current_count: int = 0

    def _evict_stale(self, now: float) -> None:
        """Drop events that have aged out of the window.

        After this returns, `self.events` contains exactly the
        timestamps of accepted requests within `(now - window, now]`.
        """
        cutoff = now - self.window_seconds
        while self.events and self.events[0] <= cutoff:
            self.events.popleft()

    def _advance_window(self, now: float) -> None:
        """Slide the window forward so its right edge is at `now`.

        Called before every decision so that the counter reflects
        only requests still inside the window.
        """
        if self.window_start == 0.0:
            # First call — anchor the window at the first request.
            self.window_start = now
            return
        # Slide forward until `now` lies within the current window.
        while now - self.window_start >= self.window_seconds:
            self.window_start += self.window_seconds
            self.current_count = 0

    def allow(self, now: float) -> bool:
        """Accept or reject a request arriving at time `now`.

        Returns True if the request fits within the sliding window's
        remaining capacity. On acceptance, records the timestamp and
        increments the in-window counter.
        """
        self._evict_stale(now)
        self._advance_window(now)
        # Decide against the live sliding-window content, not the
        # fixed-window counter. `_advance_window` resets
        # `current_count` at boundaries — using it for the decision
        # would permit a boundary-jump burst. `self.events` contains
        # exactly the accepted timestamps in (now - window, now].
        if len(self.events) < self.limit:
            self.events.append(now)
            self.current_count = len(self.events)
            return True
        return False

    def stats(self) -> dict:
        """Introspection helper. `events_in_window` is the live
        deque size (true rolling count); `accepted_in_window` is
        the counter used for the accept/reject decision. The two
        should always agree — if they drift, something is wrong."""
        return {
            "events_in_window": len(self.events),
            "accepted_in_window": self.current_count,
            "limit": self.limit,
            "window_start": self.window_start,
        }


# --- workload ------------------------------------------------------

def scenario() -> None:
    """Customer repro:

        limit=5, window=1.0s
        t=0.10 .. 0.90: send 5 requests across the window (all accept)
        t=1.05 .. 1.45: send 5 more spanning the boundary

    A correct sliding-window limiter accepts only 2 of the second batch:
      * t=1.05 (5 events in (0.05, 1.05] → reject)
      * t=1.15 (4 events in (0.15, 1.15] → accept)
      * t=1.25 (5 events in (0.25, 1.25] → reject)
      * t=1.35 (4 events in (0.35, 1.35] → accept)
      * t=1.45 (5 events in (0.45, 1.45] → reject)
    A fixed-window limiter anchored at ~1.10 resets its counter and
    accepts 4 of them — the boundary burst. The test below asserts
    we reject >= 3 of the second batch (tight enough to catch the
    fixed-window bug, loose enough to allow the real sliding-window
    acceptances at t=1.15 and t=1.35).
    """
    limiter = SlidingWindowLimiter(limit=5, window_seconds=1.0)

    first_batch = [0.10, 0.30, 0.50, 0.70, 0.90]
    second_batch = [1.05, 1.15, 1.25, 1.35, 1.45]

    accepted_first = sum(1 for t in first_batch if limiter.allow(t))
    assert accepted_first == 5, f"first batch should all accept, got {accepted_first}"

    accepted_second = sum(1 for t in second_batch if limiter.allow(t))
    rejected_second = len(second_batch) - accepted_second

    print(f"first batch:  5/5 accepted")
    print(f"second batch: {accepted_second}/5 accepted, {rejected_second}/5 rejected")
    print(f"limiter stats: {limiter.stats()}")

    # A correct sliding-window limiter rejects >= 3 of the second batch
    # (see the walk-through in the docstring above). A fixed-window
    # implementation resets its counter at the boundary and rejects
    # at most 1 — the smoking gun.
    if rejected_second < 3:
        raise AssertionError(
            f"sliding-window guarantee violated: rejected only "
            f"{rejected_second}/5 requests in the second batch "
            f"(expected >= 4). This is fixed-window behavior — "
            f"bursts at the boundary."
        )
    print("OK")


if __name__ == "__main__":
    scenario()
