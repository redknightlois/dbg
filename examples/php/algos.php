<?php
// Four classic algorithms shared across every examples/<lang>/.
// Expected output:
//   factorial(5) = 120
//   fibonacci(10) = 55
//   ackermann(2, 3) = 9
//   collatz(27) = 111

declare(strict_types=1);

function factorial(int $n): int {
    return $n <= 1 ? 1 : $n * factorial($n - 1);
}

function fibonacci(int $n): int {
    $a = 0;
    $b = 1;
    for ($i = 0; $i < $n; $i++) {
        $next = $a + $b;     // ← fibonacci hot line
        $a = $b;
        $b = $next;
    }
    return $a;
}

function ackermann(int $m, int $n): int {
    if ($m === 0) return $n + 1;    // ← ackermann recursion
    if ($n === 0) return ackermann($m - 1, 1);
    return ackermann($m - 1, ackermann($m, $n - 1));
}

function collatz(int $n): int {
    $steps = 0;
    while ($n !== 1) {
        $n = ($n % 2 === 0) ? intdiv($n, 2) : 3 * $n + 1;  // ← collatz branch
        $steps++;
    }
    return $steps;
}

echo "factorial(5) = ",    factorial(5),      PHP_EOL;
echo "fibonacci(10) = ",   fibonacci(10),     PHP_EOL;
echo "ackermann(2, 3) = ", ackermann(2, 3),   PHP_EOL;
echo "collatz(27) = ",     collatz(27),       PHP_EOL;
