# Sliding-window stats are wrong

`broken.go` computes mean / min / max for every sliding window of size 10 over a 200-element series. It runs and exits with an error:

```
$ go run broken.go
window 1: data[10]=84.323 exceeds reported max=53.117
window 2: data[10]=84.323 exceeds reported max=53.117
window 3: data[10]=84.323 exceeds reported max=53.117
BUG: 327 invariant violations across 191 windows
```

Fix it. Make sure the `OK` line prints when you're done.
