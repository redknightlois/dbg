# Event loop stalls during request bursts

The Node service's p99 latency goes to ~5s during short request
bursts. Steady-state it's fine. The team thinks it's GC; I'd like
a second opinion from an actual CPU profile.

Reproduce:

```
cd examples/scenarios/20-nodeprof-sync-blocker-js
time node broken.js
```

The script simulates the bursty path (processes 20k jobs in a
tight loop). Please capture a V8 / Node CPU profile, identify
which synchronous frame is starving the loop, and propose the
smallest fix. No need for a full async rewrite if there's a
cheaper change that restores throughput.
