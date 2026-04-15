#!/usr/bin/env python3
"""Four classic algorithms shared across every examples/<lang>/.

Expected output:
    factorial(5) = 120
    fibonacci(10) = 55
    ackermann(2, 3) = 9
    collatz(27) = 111
"""

import sys


def factorial(n: int) -> int:
    return 1 if n <= 1 else n * factorial(n - 1)


def fibonacci(n: int) -> int:
    a, b = 0, 1
    for _ in range(n):
        nxt = a + b        # ← fibonacci hot line
        a = b
        b = nxt
    return a


def ackermann(m: int, n: int) -> int:
    if m == 0:             # ← ackermann recursion
        return n + 1
    if n == 0:
        return ackermann(m - 1, 1)
    return ackermann(m - 1, ackermann(m, n - 1))


def collatz(n: int) -> int:
    steps = 0
    while n != 1:
        if n % 2 == 0:
            n = n // 2
        else:
            n = 3 * n + 1   # ← collatz odd branch
        steps += 1
    return steps


def main() -> int:
    print(f"factorial(5) = {factorial(5)}")
    print(f"fibonacci(10) = {fibonacci(10)}")
    print(f"ackermann(2, 3) = {ackermann(2, 3)}")
    print(f"collatz(27) = {collatz(27)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
