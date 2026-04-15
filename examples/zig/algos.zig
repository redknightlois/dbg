// Four classic algorithms shared across every examples/<lang>/.
// Expected output:
//   factorial(5) = 120
//   fibonacci(10) = 55
//   ackermann(2, 3) = 9
//   collatz(27) = 111

const std = @import("std");

fn factorial(n: u64) u64 {
    return if (n <= 1) 1 else n * factorial(n - 1);
}

fn fibonacci(n: u32) u64 {
    var a: u64 = 0;
    var b: u64 = 1;
    var i: u32 = 0;
    while (i < n) : (i += 1) {
        const next = a + b;   // ← fibonacci hot line
        a = b;
        b = next;
    }
    return a;
}

fn ackermann(m: u64, n: u64) u64 {
    if (m == 0) return n + 1;   // ← ackermann recursion
    if (n == 0) return ackermann(m - 1, 1);
    return ackermann(m - 1, ackermann(m, n - 1));
}

fn collatz(n_in: u64) u32 {
    var n = n_in;
    var steps: u32 = 0;
    while (n != 1) {
        n = if (n % 2 == 0) n / 2 else 3 * n + 1;   // ← collatz branch
        steps += 1;
    }
    return steps;
}

pub fn main() !void {
    const out = std.io.getStdOut().writer();
    try out.print("factorial(5) = {d}\n", .{factorial(5)});
    try out.print("fibonacci(10) = {d}\n", .{fibonacci(10)});
    try out.print("ackermann(2, 3) = {d}\n", .{ackermann(2, 3)});
    try out.print("collatz(27) = {d}\n", .{collatz(27)});
}
