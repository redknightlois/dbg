# TTL cache returns stale misses

We have a small TTL cache. The test inserts a value with a 100ms TTL, sleeps 50ms, then reads it. The read should always succeed — there are 50ms of TTL left — but sometimes (or always, depending on the clock) we get `None`.

```
$ cargo run --bin broken
misses: 4/100
BUG: cache returned None for entries that should be live
```

Your task:
- Find the line in `src/main.rs` that's lying about whether an entry is "expired enough to return None". The bug isn't in the obvious expiry comparison — that one's fine.
- Fix it. Inserts and reads should both work normally; the constraint the original author was trying to enforce should still hold (don't just delete the line).

Constraints:
- The fix is a few characters. If you find yourself rewriting `get`, you misdiagnosed.
- Make the diagnosis from observation, not from staring at the source. Inspecting the actual values of the relevant `Duration`s on a failing call is faster than reasoning about it abstractly.

`solution.md` is a spoiler.
