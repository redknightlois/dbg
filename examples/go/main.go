// Four classic algorithms shared across every examples/<lang>/.
//
// Expected output:
//   factorial(5) = 120
//   fibonacci(10) = 55
//   ackermann(2, 3) = 9
//   collatz(27) = 111
package main

import "fmt"

func factorial(n uint64) uint64 {
	if n <= 1 {
		return 1
	}
	return n * factorial(n-1)
}

func fibonacci(n int) uint64 {
	var a, b uint64 = 0, 1
	for i := 0; i < n; i++ {
		next := a + b // ← fibonacci hot line
		a = b
		b = next
	}
	return a
}

func ackermann(m, n uint64) uint64 {
	if m == 0 { // ← ackermann recursion
		return n + 1
	}
	if n == 0 {
		return ackermann(m-1, 1)
	}
	return ackermann(m-1, ackermann(m, n-1))
}

func collatz(n uint64) int {
	steps := 0
	for n != 1 {
		if n%2 == 0 {
			n = n / 2
		} else {
			n = 3*n + 1 // ← collatz odd branch
		}
		steps++
	}
	return steps
}

func main() {
	fmt.Printf("factorial(5) = %d\n", factorial(5))
	fmt.Printf("fibonacci(10) = %d\n", fibonacci(10))
	fmt.Printf("ackermann(2, 3) = %d\n", ackermann(2, 3))
	fmt.Printf("collatz(27) = %d\n", collatz(27))
}
