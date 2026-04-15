# examples/

The same four classic algorithms — **factorial, fibonacci, ackermann, collatz** — implemented in every language `dbg` supports. They share identical inputs, identical outputs, and identical breakpoint targets so you can exercise `dbg` across language boundaries with a single mental model.

## Scripts

| Script | Purpose |
|---|---|
| `./install-toolchains.sh` | Install every toolchain referenced below. Handles root / sudo / plain user correctly; home-dir installers always drop to the invoking user. |
| `./build.sh` | Build every language whose toolchain is available; cleanly skips the rest. |
| `./install-toolchains.sh --check` | Exit non-zero if any toolchain is missing (CI gate). |
| `./install-toolchains.sh --dry-run` | Print what would be installed without touching anything. |
| `./install-toolchains.sh rust go` | Install only these languages. |
| `./build.sh rust go` | Build only these languages. |

## Expected output (every language)

```
factorial(5) = 120
fibonacci(10) = 55
ackermann(2, 3) = 9
collatz(27) = 111
```

If a language prints something different, the port is broken. This is by design — every example is a drop-in reference implementation of the same four functions.

## Breakpoint targets

Each file has three commented breakpoint lines so the canonical `dbg break <file>:<line>` hits cleanly:

| Marker | Where | Why you'd break here |
|---|---|---|
| `← fibonacci hot line` | iterative inner loop | Locals `a`, `b`, `next` change every iteration — perfect for `dbg hit-trend` on a numeric series. |
| `← collatz branch` | odd-branch update | Longer run (27 takes 111 steps) — good for `dbg hit-trend` showing a non-monotonic sequence. |
| `← ackermann recursion` | implicit — break at the function entry | Heavy recursion → `dbg stack` grows deep; good for `dbg hit-diff` between early and late calls. |

Example session (any language):

```
$ dbg start rust examples/rust                       # or c, go, python, csharp, …
$ dbg break src/main.rs:17                           # fibonacci hot line
$ dbg run                                             # hits the breakpoint
$ dbg continue                                        # hit #2
$ dbg continue                                        # hit #3
... 10 times total ...
$ dbg hits src/main.rs:17                            # every captured row
$ dbg hit-trend src/main.rs:17 a                     # sparkline of `a`
$ dbg at-hit disasm                                  # LLDB disasm of fib
$ dbg cross fibonacci                                # unified view: hits + disasm
$ dbg kill
$ dbg sessions                                        # your saved session
```

## Language matrix

| Language | Directory | Build command | Backend | Source entry |
|---|---|---|---|---|
| C | `c/` | `cc -g -O0 algos.c -o algos` | lldb | `c/algos.c` |
| C++ | `cpp/` | `c++ -g -O0 -std=c++17 algos.cpp -o algos` | lldb | `cpp/algos.cpp` |
| Rust | `rust/` | `cargo build` | lldb | `rust/src/main.rs` |
| Go | `go/` | `go build -gcflags='all=-N -l' -o algos .` | delve | `go/main.go` |
| Python | `python/` | (interpreted) | pdb | `python/algos.py` |
| C# (.NET) | `csharp/` | `dotnet build -c Debug` | netcoredbg | `csharp/Program.cs` |
| Java | `java/` | `javac -g Algos.java` | jdb | `java/Algos.java` |
| JavaScript (Node) | `javascript/` | (interpreted) | node-inspect | `javascript/algos.js` |
| Ruby | `ruby/` | (interpreted) | rdbg | `ruby/algos.rb` |
| PHP | `php/` | (interpreted) | phpdbg | `php/algos.php` |
| Zig | `zig/` | `zig build-exe -O Debug algos.zig` | lldb | `zig/algos.zig` |
| Haskell | `haskell/` | `ghc -O0 algos.hs` | ghci | `haskell/algos.hs` |
| OCaml | `ocaml/` | `ocamlc -g algos.ml -o algos` | ocamldebug | `ocaml/algos.ml` |

The backend column points to the `dbg` backend that drives each language — see `skills/adapters/_canonical-commands.md` for what maps to what.

## Verifying parity

After `./build.sh`, run each built binary and confirm the four lines match:

```bash
for dir in c cpp rust/target/debug go python csharp/bin/Debug/net8.0 \
           javascript ruby php zig haskell ocaml; do
    case "$dir" in
        python)      python3 python/algos.py ;;
        javascript)  node javascript/algos.js ;;
        ruby)        ruby ruby/algos.rb ;;
        php)         php php/algos.php ;;
        rust/target/debug) ./rust/target/debug/algos ;;
        csharp/bin/Debug/net8.0) dotnet csharp/bin/Debug/net8.0/algos.dll ;;
        *)           "./$dir/algos" ;;
    esac
done
```

Any divergence means a port drifted — fix the source, not the expected output.
