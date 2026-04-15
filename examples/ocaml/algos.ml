(* Four classic algorithms shared across every examples/<lang>/.
   Expected output:
     factorial(5) = 120
     fibonacci(10) = 55
     ackermann(2, 3) = 9
     collatz(27) = 111
*)

let rec factorial n =
  if n <= 1 then 1 else n * factorial (n - 1)

let fibonacci n =
  let a = ref 0 in
  let b = ref 1 in
  for _ = 0 to n - 1 do
    let next = !a + !b in    (* ← fibonacci hot line *)
    a := !b;
    b := next
  done;
  !a

let rec ackermann m n =
  if m = 0 then n + 1    (* ← ackermann recursion *)
  else if n = 0 then ackermann (m - 1) 1
  else ackermann (m - 1) (ackermann m (n - 1))

let collatz n =
  let n = ref n in
  let steps = ref 0 in
  while !n <> 1 do
    n := (if !n mod 2 = 0 then !n / 2 else 3 * !n + 1);   (* ← collatz *)
    incr steps
  done;
  !steps

let () =
  Printf.printf "factorial(5) = %d\n"     (factorial 5);
  Printf.printf "fibonacci(10) = %d\n"    (fibonacci 10);
  Printf.printf "ackermann(2, 3) = %d\n"  (ackermann 2 3);
  Printf.printf "collatz(27) = %d\n"      (collatz 27)
