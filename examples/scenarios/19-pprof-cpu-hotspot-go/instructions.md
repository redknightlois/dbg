# Log ingestor is 10x slower than wc

We stream JSON-lines logs through a small extractor that pulls
the `user_id` field from every line. It's limping — a file `wc`
finishes in half a second takes 5+ seconds to process. The team
assumes JSON parsing is the cost; we've been eyeing a swap to
a faster JSON library.

Reproduce:

```
cd examples/scenarios/19-pprof-cpu-hotspot-go
go build -o broken .
time ./broken
```

Before we rewrite the parser, please run a CPU profile (pprof)
and show me where the cycles actually go. I don't want to spend
two weeks on a JSON library swap if the slow path is somewhere
else entirely. Propose the minimal change that restores wc-class
throughput.
