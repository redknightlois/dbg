/*
 * Four classic algorithms, shared across every `examples/<lang>/`
 * directory. Expected output (identical in every port):
 *
 *   factorial(5) = 120
 *   fibonacci(10) = 55
 *   ackermann(2, 3) = 9
 *   collatz(27) = 111
 *
 * Good breakpoint targets:
 *   * fibonacci loop body (mutable locals a, b)      — dbg hit-trend
 *   * ackermann recursive branches                    — dbg stack
 *   * collatz while-loop body                         — long trend
 */
#include <stdio.h>
#include <stdint.h>

static uint64_t factorial(int n) {
    if (n <= 1) return 1;
    return (uint64_t)n * factorial(n - 1);
}

static uint64_t fibonacci(int n) {
    uint64_t a = 0, b = 1;
    for (int i = 0; i < n; i++) {
        uint64_t next = a + b; /* ← fibonacci hot line */
        a = b;
        b = next;
    }
    return a;
}

static uint64_t ackermann(uint64_t m, uint64_t n) {
    if (m == 0) return n + 1; /* ← ackermann recursion */
    if (n == 0) return ackermann(m - 1, 1);
    return ackermann(m - 1, ackermann(m, n - 1));
}

static int collatz(uint64_t n) {
    int steps = 0;
    while (n != 1) {
        if (n % 2 == 0) n = n / 2;
        else            n = 3 * n + 1; /* ← collatz odd branch */
        steps++;
    }
    return steps;
}

int main(void) {
    printf("factorial(5) = %llu\n",  (unsigned long long)factorial(5));
    printf("fibonacci(10) = %llu\n", (unsigned long long)fibonacci(10));
    printf("ackermann(2, 3) = %llu\n", (unsigned long long)ackermann(2, 3));
    printf("collatz(27) = %d\n",      collatz(27));
    return 0;
}
