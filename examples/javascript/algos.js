// Four classic algorithms shared across every examples/<lang>/.
// Expected output:
//   factorial(5) = 120
//   fibonacci(10) = 55
//   ackermann(2, 3) = 9
//   collatz(27) = 111

'use strict';

function factorial(n) {
    return n <= 1 ? 1n : BigInt(n) * factorial(n - 1);
}

function fibonacci(n) {
    let a = 0n, b = 1n;
    for (let i = 0; i < n; i++) {
        const next = a + b;       // ← fibonacci hot line
        a = b;
        b = next;
    }
    return a;
}

function ackermann(m, n) {
    if (m === 0) return n + 1;    // ← ackermann recursion
    if (n === 0) return ackermann(m - 1, 1);
    return ackermann(m - 1, ackermann(m, n - 1));
}

function collatz(n) {
    let steps = 0;
    while (n !== 1) {
        n = (n % 2 === 0) ? n / 2 : 3 * n + 1;   // ← collatz branch
        steps++;
    }
    return steps;
}

function main() {
    console.log(`factorial(5) = ${factorial(5)}`);
    console.log(`fibonacci(10) = ${fibonacci(10)}`);
    console.log(`ackermann(2, 3) = ${ackermann(2, 3)}`);
    console.log(`collatz(27) = ${collatz(27)}`);
}

main();
