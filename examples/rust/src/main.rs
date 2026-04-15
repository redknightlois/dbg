//! Four classic algorithms shared across every `examples/<lang>/`.
//!
//! Expected output (identical across every port):
//!   factorial(5) = 120
//!   fibonacci(10) = 55
//!   ackermann(2, 3) = 9
//!   collatz(27) = 111

fn factorial(n: u64) -> u64 {
    if n <= 1 { 1 } else { n * factorial(n - 1) }
}

fn fibonacci(n: u32) -> u64 {
    let mut a: u64 = 0;
    let mut b: u64 = 1;
    for _ in 0..n {
        let next = a + b;           // ← fibonacci hot line
        a = b;
        b = next;
    }
    a
}

fn ackermann(m: u64, n: u64) -> u64 {
    match (m, n) {
        (0, _) => n + 1,            // ← ackermann recursion
        (_, 0) => ackermann(m - 1, 1),
        _      => ackermann(m - 1, ackermann(m, n - 1)),
    }
}

fn collatz(mut n: u64) -> u32 {
    let mut steps = 0u32;
    while n != 1 {
        n = if n % 2 == 0 { n / 2 } else { 3 * n + 1 };   // ← collatz branch
        steps += 1;
    }
    steps
}

fn main() {
    println!("factorial(5) = {}",     factorial(5));
    println!("fibonacci(10) = {}",    fibonacci(10));
    println!("ackermann(2, 3) = {}",  ackermann(2, 3));
    println!("collatz(27) = {}",      collatz(27));
}
