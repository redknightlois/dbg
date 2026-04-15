// Four classic algorithms shared across every examples/<lang>/.
// Expected output:
//   factorial(5) = 120
//   fibonacci(10) = 55
//   ackermann(2, 3) = 9
//   collatz(27) = 111

using System;

namespace DbgExample;

internal static class Algos
{
    static ulong Factorial(int n)
        => n <= 1 ? 1UL : (ulong)n * Factorial(n - 1);

    static ulong Fibonacci(int n)
    {
        ulong a = 0, b = 1;
        for (int i = 0; i < n; i++)
        {
            ulong next = a + b;   // ← fibonacci hot line
            a = b;
            b = next;
        }
        return a;
    }

    static ulong Ackermann(ulong m, ulong n)
    {
        if (m == 0) return n + 1;   // ← ackermann recursion
        if (n == 0) return Ackermann(m - 1, 1);
        return Ackermann(m - 1, Ackermann(m, n - 1));
    }

    static int Collatz(ulong n)
    {
        int steps = 0;
        while (n != 1)
        {
            n = (n % 2 == 0) ? n / 2 : 3 * n + 1;   // ← collatz branch
            steps++;
        }
        return steps;
    }

    public static void Main()
    {
        Console.WriteLine($"factorial(5) = {Factorial(5)}");
        Console.WriteLine($"fibonacci(10) = {Fibonacci(10)}");
        Console.WriteLine($"ackermann(2, 3) = {Ackermann(2, 3)}");
        Console.WriteLine($"collatz(27) = {Collatz(27)}");
    }
}
