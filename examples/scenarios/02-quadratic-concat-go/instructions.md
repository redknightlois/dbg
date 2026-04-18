# Report endpoint is too slow

Performance ticket: our CSV report endpoint takes ~30s to format 50k rows. Product expected this to be sub-second based on the size of the output. The handler is in `broken.go`. Find the bottleneck and fix it.

```
$ go run broken.go
formatted 50000 rows, 2538890 bytes, in 28.4s
```

Constraints:
- The functions in `broken.go` look unremarkable. Don't trust your guess about which line is slow — *measure*. We've been burned by intuition on this before.
- The fix is idiomatic Go and roughly five lines. If you find yourself rewriting the algorithm, you went too far.
- Report the runtime after your fix. We expect at least a 50x improvement.

`solution.md` has the answer; don't open it before you've isolated the line.
