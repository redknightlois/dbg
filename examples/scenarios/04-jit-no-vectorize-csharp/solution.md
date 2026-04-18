The redundant `if (i >= xs.Length) break;` line inside `SumSlow`'s loop body. The .NET JIT's auto-vectorizer requires a single, statically-analyzable exit condition; a second early-exit branch in the body forces it to fall back to scalar codegen.

`dbg disasm Program:SumFast` will show `vaddps`/`vmovups` over a 32-byte stride. `dbg disasm Program:SumSlow` will show `addss` (scalar single-precision add) one element at a time.

Fix: delete the redundant `if (i >= xs.Length) break;` line. The loop-bound check `i <= xs.Length - 1` (or the more idiomatic `i < xs.Length`) is sufficient and the JIT proves it.

Equivalently: rewrite as `for (int i = 0; i < xs.Length; i++)` like `SumFast`. The JIT recognizes that exact shape as a vectorizable bound.
