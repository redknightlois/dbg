#!/usr/bin/env bash
# Drive every dbg canonical command against one language's example and
# capture the output. Produces a per-language log under
# examples/test-results/<lang>/ and a compact pass/skip/fail summary.
#
# Usage:
#   ./test-dbg-commands.sh <lang>         # one language
#   ./test-dbg-commands.sh all            # every available language
#
# Expects `./build.sh` has been run already so the targets exist.

set -u
set -o pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$ROOT/.." && pwd)"
DBG="$REPO/target/debug/dbg"
OUT="$ROOT/test-results"
mkdir -p "$OUT"

if [[ ! -x "$DBG" ]]; then
    echo "dbg binary not found at $DBG — run \`cargo build --bin dbg\` first" >&2
    exit 1
fi

# --------------------------------------------------------------
# Language matrix
#
# lang → (backend-type, target-path, source-file, fn-for-break)
#
# fn-for-break is the phrase we grep for in the source to locate the
# breakpoint line. Every language file has `← fibonacci hot line` as
# the marker at the `next = a + b` step.
# --------------------------------------------------------------

declare -A BACKEND TARGET SRC

BACKEND[python]="python"
TARGET[python]="$ROOT/python/algos.py"
SRC[python]="$ROOT/python/algos.py"

BACKEND[go]="go"
TARGET[go]="$ROOT/go/algos"
SRC[go]="$ROOT/go/main.go"

BACKEND[csharp]="dotnet"
TARGET[csharp]="$ROOT/csharp/bin/Debug/net8.0/algos.dll"
SRC[csharp]="$ROOT/csharp/Program.cs"

BACKEND[javascript]="javascript"
TARGET[javascript]="$ROOT/javascript/algos.js"
SRC[javascript]="$ROOT/javascript/algos.js"

BACKEND[java]="java"
TARGET[java]="Algos"
SRC[java]="$ROOT/java/Algos.java"

BACKEND[ruby]="ruby"
TARGET[ruby]="$ROOT/ruby/algos.rb"
SRC[ruby]="$ROOT/ruby/algos.rb"

BACKEND[php]="php"
TARGET[php]="$ROOT/php/algos.php"
SRC[php]="$ROOT/php/algos.php"

BACKEND[haskell]="haskell"
TARGET[haskell]="$ROOT/haskell/algos.hs"
SRC[haskell]="$ROOT/haskell/algos.hs"

BACKEND[ocaml]="ocaml"
TARGET[ocaml]="$ROOT/ocaml/algos"
SRC[ocaml]="$ROOT/ocaml/algos.ml"

BACKEND[c]="c"
TARGET[c]="$ROOT/c/algos"
SRC[c]="$ROOT/c/algos.c"

BACKEND[cpp]="cpp"
TARGET[cpp]="$ROOT/cpp/algos"
SRC[cpp]="$ROOT/cpp/algos.cpp"

BACKEND[rust]="rust"
TARGET[rust]="$ROOT/rust/target/debug/algos"
SRC[rust]="$ROOT/rust/src/main.rs"

BACKEND[zig]="zig"
TARGET[zig]="$ROOT/zig/algos"
SRC[zig]="$ROOT/zig/algos.zig"

ALL_LANGS=(python go csharp javascript java ruby php haskell ocaml c cpp rust zig)

# --------------------------------------------------------------
# Per-command test helpers
# --------------------------------------------------------------

# Run `dbg <args...>` with a hard timeout; append stdout+stderr to log.
# Prints "[ok]" or "[fail: <reason>]" on its own line so we can grep.
dbg_cmd() {
    local lang="$1"; shift
    local log="$OUT/$lang/session.log"
    local label="$1"; shift  # for display
    {
        echo
        echo "---- $ $DBG $* ($label) ----"
    } >> "$log"
    if timeout 20 "$DBG" "$@" >> "$log" 2>&1; then
        echo "[ok] $label" >> "$log"
        return 0
    else
        local rc=$?
        echo "[fail rc=$rc] $label" >> "$log"
        return 1
    fi
}

# Find the breakpoint line in a source file by grepping for the marker.
break_line() {
    local src="$1"
    grep -n "fibonacci hot line" "$src" 2>/dev/null | head -1 | cut -d: -f1
}

# --------------------------------------------------------------
# Single-language test run
# --------------------------------------------------------------

run_one() {
    local lang="$1"
    local backend="${BACKEND[$lang]}"
    local target="${TARGET[$lang]}"
    local src="${SRC[$lang]}"

    mkdir -p "$OUT/$lang"
    local log="$OUT/$lang/session.log"
    : > "$log"

    echo "================ $lang ================"
    {
        echo "# dbg canonical-command test: $lang"
        echo "# backend=$backend target=$target"
        echo "# source=$src"
        date
    } > "$log"

    # Precondition: target exists. For Java/C# the notion is looser;
    # a later `dbg start` will surface missing class/DLL errors.
    case "$lang" in
        java) # compiled class file alongside source
            [[ -f "$ROOT/java/Algos.class" ]] || { echo "  skip — Algos.class missing (run build.sh first)"; return 2; } ;;
        csharp)
            [[ -f "$target" ]] || { echo "  skip — $target missing"; return 2; } ;;
        python|ruby|php|haskell|ocaml|javascript)
            [[ -f "$src" ]]    || { echo "  skip — $src missing"; return 2; } ;;
        *)
            [[ -x "$target" ]] || { echo "  skip — $target not built"; return 2; } ;;
    esac

    local line
    line="$(break_line "$src")"
    if [[ -z "$line" ]]; then
        echo "  skip — couldn't locate fibonacci marker in $src"
        echo "SKIP: no marker" >> "$log"
        return 2
    fi
    # Use the absolute source path so debuggers that key breakpoints
    # on the fully-resolved file (pdb, jdb, ocamldebug) match cleanly.
    local loc="${src}:${line}"
    echo "  breakpoint: $loc"
    echo "# breakpoint loc: $loc" >> "$log"

    # --- clean slate
    "$DBG" kill >/dev/null 2>&1 || true
    sleep 0.3

    # --- start session with --break --run
    local ok=0 fail=0
    if (cd "$REPO" && timeout 30 "$DBG" start "$backend" "$target" --break "$loc" --run >> "$log" 2>&1); then
        ok=$((ok + 1))
        echo "    ✓ start"
    else
        fail=$((fail + 1))
        echo "    ✗ start (see $log)"
        echo "  giving up on $lang — daemon did not start" | tee -a "$log"
        "$DBG" kill >/dev/null 2>&1 || true
        return 1
    fi

    sleep 0.5

    # --- inspection vocabulary at the first hit
    local verbs=(
        "tool"
        "breaks"
        "stack"
        "locals"
    )
    for v in "${verbs[@]}"; do
        if dbg_cmd "$lang" "$v" "$v"; then ok=$((ok+1)); echo "    ✓ $v"
        else fail=$((fail+1)); echo "    ✗ $v"
        fi
    done

    # --- expression evaluation — name differs per language
    case "$lang" in
        python|javascript|haskell|ocaml|csharp|java|ruby|php) expr="a" ;;
        go)    expr="a" ;;
        *)     expr="a" ;;
    esac
    if dbg_cmd "$lang" "print $expr" print "$expr"; then ok=$((ok+1)); echo "    ✓ print $expr"
    else fail=$((fail+1)); echo "    ✗ print $expr"
    fi

    # --- step through the loop: `continue` 4 times to land at hits 2-5
    for i in 2 3 4 5; do
        if dbg_cmd "$lang" "continue#$i" continue; then ok=$((ok+1)); echo "    ✓ continue (hit #$i)"
        else fail=$((fail+1)); echo "    ✗ continue (hit #$i)"
        fi
        sleep 0.2
    done

    # --- cross-track queries
    for q in "hits $loc" "hit-diff $loc 1 3" "hit-trend $loc a" "cross fibonacci" "sessions"; do
        # shellcheck disable=SC2086
        if dbg_cmd "$lang" "$q" $q; then ok=$((ok+1)); echo "    ✓ $q"
        else fail=$((fail+1)); echo "    ✗ $q"
        fi
    done

    # --- tear down
    "$DBG" kill >/dev/null 2>&1 || true
    sleep 0.2

    echo "  $lang: $ok ok, $fail fail" | tee -a "$log"
    echo
    return 0
}

# --------------------------------------------------------------
# Drive
# --------------------------------------------------------------

if [[ $# -eq 0 || "$1" == "all" ]]; then
    langs=("${ALL_LANGS[@]}")
else
    langs=("$@")
fi

overall_ok=0
overall_skip=0
overall_fail=0
for l in "${langs[@]}"; do
    if [[ -z "${BACKEND[$l]:-}" ]]; then
        echo "unknown lang: $l (known: ${ALL_LANGS[*]})"
        overall_fail=$((overall_fail + 1))
        continue
    fi
    case "$(run_one "$l"; echo $?)" in
        *0) overall_ok=$((overall_ok + 1)) ;;
        *2) overall_skip=$((overall_skip + 1)) ;;
        *)  overall_fail=$((overall_fail + 1)) ;;
    esac
done

echo "================ summary ================"
echo "  $overall_ok ok, $overall_skip skipped, $overall_fail failed"
echo "  logs under $OUT/"
