# Dispatch throughput halves under GC pressure

The request dispatcher is limited to ~40k RPS even on an unloaded
box. CPU is only 60 % used — we suspect GC pauses are eating the
rest but haven't confirmed.

Reproduce:

```
cd examples/scenarios/18-dotnettrace-gc-hot-alloc-csharp
dotnet run -c Release
```

Please collect a .NET trace (dotnet-trace), look at the GC events
and allocation tick counters, and identify which method is
responsible for the gen0 pressure. A flamegraph on allocation
bytes would tell us fastest. Propose a rewrite that keeps the
same key-building behavior but drops allocation rate by an order
of magnitude — we want to stay on the heap-free side of things
in the request path.
