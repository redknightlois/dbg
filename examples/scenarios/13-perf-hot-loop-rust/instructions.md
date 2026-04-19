# Dedup stage is pegging a core

Our ingestion pipeline has a dedup step that collapses duplicate
events before we write them to object storage. It's been getting
slower as event batches grow — a 100-event batch is instant, an
80k batch now takes several seconds. CPU sits at 100 % on one
core the whole time; the rest of the pipeline is idle waiting.

Reproduce:

```
cd examples/scenarios/13-perf-hot-loop-rust
cargo build --release
time ./target/release/broken
```

Figure out where the time is actually going. A flamegraph / CPU
profile should pin the hot frame in seconds, not guesswork. Once
the offender is obvious, suggest a fix — something that drops the
wall time for 80k events under half a second.

No debugger breakpoints needed; this is a profiler-first problem.
