# Collatz step counter blows the stack

`broken.py` computes the Collatz step count for an integer. It returns the right answer for small inputs but raises `RecursionError` for `n=113` even though the actual Collatz sequence for 113 only peaks at 9232 — there's no way the *true* recursion depth justifies a stack overflow with the default limit of 2000.

```
$ python3 broken.py 27
collatz(27) = 111 steps
$ python3 broken.py 113
RecursionError: maximum recursion depth exceeded
```

Your task:
- Figure out *why* this implementation is doing exponentially more work than the algorithm requires. The bug isn't a typo or a wrong constant — it's a structural mistake in the recursion.
- Fix it. The function should still be recursive (don't rewrite it as a loop) and should still produce 111 for n=27.
- Show your reasoning: how many calls *should* this function make for a small input like n=15, vs. how many it actually makes?

Constraints:
- Reading the file alone might not be enough — the structural mistake looks innocent in isolation. Counting actual invocations as you walk through small inputs makes the pattern obvious.
- Don't just bump `setrecursionlimit`. That's hiding the problem.

`solution.md` is a spoiler.
