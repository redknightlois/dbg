// See examples/README.md — same four algorithms across every language.
// Expected output:
//   factorial(5) = 120
//   fibonacci(10) = 55
//   ackermann(2, 3) = 9
//   collatz(27) = 111

#include <cstdint>
#include <iostream>

static std::uint64_t factorial(int n) {
    return n <= 1 ? 1 : static_cast<std::uint64_t>(n) * factorial(n - 1);
}

static std::uint64_t fibonacci(int n) {
    std::uint64_t a = 0, b = 1;
    for (int i = 0; i < n; ++i) {
        std::uint64_t next = a + b;   // ← fibonacci hot line
        a = b;
        b = next;
    }
    return a;
}

static std::uint64_t ackermann(std::uint64_t m, std::uint64_t n) {
    if (m == 0) return n + 1;   // ← ackermann recursion
    if (n == 0) return ackermann(m - 1, 1);
    return ackermann(m - 1, ackermann(m, n - 1));
}

static int collatz(std::uint64_t n) {
    int steps = 0;
    while (n != 1) {
        n = (n % 2 == 0) ? n / 2 : 3 * n + 1;   // ← collatz branch
        ++steps;
    }
    return steps;
}

int main() {
    std::cout << "factorial(5) = "   << factorial(5)       << '\n'
              << "fibonacci(10) = "  << fibonacci(10)      << '\n'
              << "ackermann(2, 3) = "<< ackermann(2, 3)    << '\n'
              << "collatz(27) = "    << collatz(27)        << '\n';
    return 0;
}
