#!/usr/bin/env bash
# Build every language example whose toolchain is available on this
# machine. Skips cleanly when a toolchain is missing (run
# `./install-toolchains.sh` to fill the gaps).
#
# Exit 0 if every present toolchain built its example successfully;
# non-zero if any present toolchain failed.
#
# Usage:
#   ./build.sh              # build all available
#   ./build.sh rust go      # build only these

set -u
set -o pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

ONLY_LANGS=("$@")
want() {
    (( ${#ONLY_LANGS[@]} == 0 )) && return 0
    local l
    for l in "${ONLY_LANGS[@]}"; do [[ "$l" == "$1" ]] && return 0; done
    return 1
}

has() { command -v "$1" >/dev/null 2>&1; }

# Accumulate results for the summary line. Explicit `=()` is safer
# than `declare -a X Y Z` under `set -u` on older bash versions.
BUILT=()
SKIPPED=()
FAILED=()
built()   { BUILT+=("$1"); }
skipped() { SKIPPED+=("$1  — $2"); }
failed()  { FAILED+=("$1  — $2"); }

# Run a build; on failure print the last ~20 lines of the log.
run_build() {
    local lang="$1"; shift
    local log="$ROOT/.build-$lang.log"
    echo "==> $lang"
    if "$@" >"$log" 2>&1; then
        built "$lang"
    else
        echo "   FAILED — last 20 lines of $log:"
        tail -n 20 "$log" | sed 's/^/   | /'
        failed "$lang" "see $log"
    fi
}

# ------------------------------------------------------------------
# Per-language build steps
# ------------------------------------------------------------------

build_c() {
    want c || return 0
    has cc || { skipped c "cc not found"; return 0; }
    run_build c cc -g -O0 -o "$ROOT/c/algos" "$ROOT/c/algos.c"
}

build_cpp() {
    want cpp || return 0
    has c++ || { skipped cpp "c++ not found"; return 0; }
    run_build cpp c++ -g -O0 -std=c++17 -o "$ROOT/cpp/algos" "$ROOT/cpp/algos.cpp"
}

build_rust() {
    want rust || return 0
    has cargo || { skipped rust "cargo not found"; return 0; }
    run_build rust cargo build --manifest-path "$ROOT/rust/Cargo.toml" --quiet
}

build_go() {
    want go || return 0
    has go || { skipped go "go not found"; return 0; }
    # -N -l → disable optimization + inlining so line numbers match source.
    run_build go bash -c "cd '$ROOT/go' && go build -gcflags='all=-N -l' -o algos ."
}

build_python() {
    want python || return 0
    has python3 || { skipped python "python3 not found"; return 0; }
    run_build python python3 -m py_compile "$ROOT/python/algos.py"
}

build_csharp() {
    want csharp || return 0
    has dotnet || { skipped csharp "dotnet not found"; return 0; }
    run_build csharp dotnet build "$ROOT/csharp/Algos.csproj" -c Debug --nologo -v quiet
}

build_java() {
    want java || return 0
    has javac || { skipped java "javac not found (need JDK, not just JRE)"; return 0; }
    run_build java bash -c "cd '$ROOT/java' && javac -g Algos.java"
}

build_javascript() {
    want javascript || return 0
    has node || { skipped javascript "node not found"; return 0; }
    # No build step for node; just syntax-check with --check.
    run_build javascript node --check "$ROOT/javascript/algos.js"
}

build_ruby() {
    want ruby || return 0
    has ruby || { skipped ruby "ruby not found"; return 0; }
    run_build ruby ruby -c "$ROOT/ruby/algos.rb"
}

build_php() {
    want php || return 0
    has php || { skipped php "php not found"; return 0; }
    run_build php php -l "$ROOT/php/algos.php"
}

build_zig() {
    want zig || return 0
    has zig || { skipped zig "zig not found"; return 0; }
    run_build zig bash -c "cd '$ROOT/zig' && zig build-exe -O Debug algos.zig"
}

build_haskell() {
    want haskell || return 0
    has ghc || { skipped haskell "ghc not found"; return 0; }
    run_build haskell bash -c "cd '$ROOT/haskell' && ghc -O0 -o algos algos.hs"
}

build_ocaml() {
    want ocaml || return 0
    has ocaml || { skipped ocaml "ocaml not found"; return 0; }
    run_build ocaml bash -c "cd '$ROOT/ocaml' && ocamlc -g algos.ml -o algos"
}

# ------------------------------------------------------------------
# Drive
# ------------------------------------------------------------------

build_c; build_cpp; build_rust; build_go
build_python; build_csharp; build_java; build_javascript
build_ruby; build_php; build_zig; build_haskell; build_ocaml

echo
echo "---- build summary ----"
# `${arr[@]+"${arr[@]}"}` is the canonical set -u-safe way to expand
# a possibly-empty bash array without tripping "unbound variable".
if (( ${#BUILT[@]} > 0 )); then
    printf "  built:   %s\n" "${BUILT[*]}"
fi
for s in ${SKIPPED[@]+"${SKIPPED[@]}"}; do printf "  skipped: %s\n" "$s"; done
for f in ${FAILED[@]+"${FAILED[@]}"};   do printf "  FAILED:  %s\n" "$f"; done
printf "  total: %d built, %d skipped, %d failed\n" \
    "${#BUILT[@]}" "${#SKIPPED[@]}" "${#FAILED[@]}"

exit $(( ${#FAILED[@]} > 0 ? 1 : 0 ))
