#!/usr/bin/env ruby
# Four classic algorithms shared across every examples/<lang>/.
# Expected output:
#   factorial(5) = 120
#   fibonacci(10) = 55
#   ackermann(2, 3) = 9
#   collatz(27) = 111

def factorial(n)
  n <= 1 ? 1 : n * factorial(n - 1)
end

def fibonacci(n)
  a, b = 0, 1
  n.times do
    nxt = a + b       # ← fibonacci hot line
    a = b
    b = nxt
  end
  a
end

def ackermann(m, n)
  return n + 1              if m == 0   # ← ackermann recursion
  return ackermann(m - 1, 1) if n == 0
  ackermann(m - 1, ackermann(m, n - 1))
end

def collatz(n)
  steps = 0
  until n == 1
    n = (n % 2 == 0) ? n / 2 : 3 * n + 1   # ← collatz branch
    steps += 1
  end
  steps
end

puts "factorial(5) = #{factorial(5)}"
puts "fibonacci(10) = #{fibonacci(10)}"
puts "ackermann(2, 3) = #{ackermann(2, 3)}"
puts "collatz(27) = #{collatz(27)}"
