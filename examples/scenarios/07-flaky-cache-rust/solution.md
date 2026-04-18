The "10ms guard" line:

```rust
let remaining = expires_at.duration_since(Instant::now());
```

`Instant::now()` is called *again* here, after the first `Instant::now()` used in the expiry comparison. Between the two calls, scheduler latency or even just monotonic-clock granularity can mean the second `now()` is past `expires_at`, so `duration_since` returns `Duration::ZERO` (Rust's `Duration::duration_since` saturates at zero rather than wrapping or panicking). That ZERO is `< 10ms`, so we return `None` — even though we *just* validated the entry was live.

Two valid fixes:

1. Compute `now` once and reuse it:

```rust
let now = Instant::now();
if now >= *expires_at { return None; }
let remaining = expires_at.duration_since(now);
if remaining < Duration::from_millis(10) { return None; }
```

2. Use `checked_duration_since` and treat `None` as expired explicitly, but the root cause is calling `now()` twice.
