"""Recursive Collatz step counter.

`collatz_steps(n)` should return the number of steps to reach 1.
For n=27 the answer is 111. We get the right answer for small n
but blow the stack for n >= 113. The Collatz sequence for 113
peaks at 9232, which doesn't justify a stack overflow on its own —
something in the recursion is wrong.

Run:
    python3 broken.py 27          # OK -> 111
    python3 broken.py 113         # RecursionError
"""

import sys

sys.setrecursionlimit(2000)


def collatz_steps(n: int, depth: int = 0) -> int:
    if n == 1:
        return depth
    if n % 2 == 0:
        # even branch: halve
        return collatz_steps(n // 2, depth + 1)
    else:
        # odd branch: 3n + 1, then recurse from the *result of the next halving*
        # — pre-collapsing one step here as an "optimization".
        nxt = 3 * n + 1
        # 3n+1 is always even when n is odd, so the next step halves nxt.
        # Collapse the odd step and the following even step into a single
        # recursion that advances depth by 2.
        return collatz_steps(nxt // 2, depth + 2)


def main(argv: list[str]) -> int:
    n = int(argv[1]) if len(argv) > 1 else 27
    steps = collatz_steps(n)
    print(f"collatz({n}) = {steps} steps")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
