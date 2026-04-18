Inside `summarize`, the max-tracking branch uses `<` instead of `>`:

```go
if data[j] < max {     // wrong
    max = data[j]
}
```

Should be:

```go
if data[j] > max {
    max = data[j]
}
```

This is a copy-paste-from-min bug. The "max" ends up tracking the minimum-so-far again, then gets clobbered each time a smaller value appears, so the reported max is essentially a second min.

This scenario is intentionally framed as a naive bug report — no debugger hint, no profiler hint, no mention of `dbg`. The question being measured is whether the agent reaches for `dbg break broken.go:N + dbg locals` (or even `dbg run` to see the failing case in context) versus just reading the source. Both paths solve it; the latter is faster *for this particular bug* because the source is short. Track the choice as the metric.
