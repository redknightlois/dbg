-- Four classic algorithms shared across every examples/<lang>/.
-- Expected output:
--   factorial(5) = 120
--   fibonacci(10) = 55
--   ackermann(2, 3) = 9
--   collatz(27) = 111

module Main where

import Data.Word (Word64)

factorial :: Word64 -> Word64
factorial n | n <= 1    = 1
            | otherwise = n * factorial (n - 1)

fibonacci :: Int -> Word64
fibonacci = go 0 1
  where
    go a _ 0 = a
    go a b n = go b (a + b) (n - 1)   -- ← fibonacci hot line

ackermann :: Word64 -> Word64 -> Word64
ackermann 0 n = n + 1   -- ← ackermann recursion
ackermann m 0 = ackermann (m - 1) 1
ackermann m n = ackermann (m - 1) (ackermann m (n - 1))

collatz :: Word64 -> Int
collatz = go 0
  where
    go k 1 = k
    go k n
      | even n    = go (k + 1) (n `div` 2)
      | otherwise = go (k + 1) (3 * n + 1)   -- ← collatz branch

main :: IO ()
main = do
  putStrLn $ "factorial(5) = "    ++ show (factorial 5)
  putStrLn $ "fibonacci(10) = "   ++ show (fibonacci 10)
  putStrLn $ "ackermann(2, 3) = " ++ show (ackermann 2 3)
  putStrLn $ "collatz(27) = "     ++ show (collatz 27)
