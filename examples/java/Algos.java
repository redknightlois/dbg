// Four classic algorithms shared across every examples/<lang>/.
// Expected output:
//   factorial(5) = 120
//   fibonacci(10) = 55
//   ackermann(2, 3) = 9
//   collatz(27) = 111

public final class Algos {

    static long factorial(int n) {
        return n <= 1 ? 1L : (long) n * factorial(n - 1);
    }

    static long fibonacci(int n) {
        long a = 0, b = 1;
        for (int i = 0; i < n; i++) {
            long next = a + b;     // ← fibonacci hot line
            a = b;
            b = next;
        }
        return a;
    }

    static long ackermann(long m, long n) {
        if (m == 0) return n + 1;  // ← ackermann recursion
        if (n == 0) return ackermann(m - 1, 1);
        return ackermann(m - 1, ackermann(m, n - 1));
    }

    static int collatz(long n) {
        int steps = 0;
        while (n != 1) {
            n = (n % 2 == 0) ? n / 2 : 3 * n + 1;   // ← collatz branch
            steps++;
        }
        return steps;
    }

    public static void main(String[] args) {
        System.out.println("factorial(5) = " + factorial(5));
        System.out.println("fibonacci(10) = " + fibonacci(10));
        System.out.println("ackermann(2, 3) = " + ackermann(2, 3));
        System.out.println("collatz(27) = " + collatz(27));
    }
}
