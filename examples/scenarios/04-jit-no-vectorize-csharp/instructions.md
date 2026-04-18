# Hot loop is 5x slower than its twin

Benchmark regression: `SumSlow` benchmarks ~5x slower than `SumFast`, even though they compute the same thing over the same array. We suspect the JIT is doing something different for one of them — but we want *evidence*, not a guess.

```
$ dotnet run -c Release
SumFast: 21.4 ms  (sum=...)
SumSlow: 108.7 ms (sum=...)
ratio  : 5.08x
```

Your task:
- Confirm or refute the hypothesis that the JIT codegen for the two methods diverges. Show concrete evidence (instruction-level, not just timing).
- If they do diverge, identify which line of `Program.cs` is responsible — i.e. which line, if removed or changed, would let the JIT produce equivalent code for both.
- Write up a short note with the evidence and the offending line.

Constraints:
- A timing benchmark alone doesn't count. We already have one and it doesn't tell us *why*.
- The fix is one line. Bigger means you misread the situation.

`solution.md` is a spoiler.
