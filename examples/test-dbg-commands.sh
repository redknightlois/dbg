#!/usr/bin/env bash
# Drive every dbg canonical command against one language's example and
# capture the output. Produces per-target logs under
# examples/test-results/<lang>/ and a compact pass/skip/fail summary.
#
# Usage:
#   ./test-dbg-commands.sh <lang>                       # one language, both targets
#   ./test-dbg-commands.sh all                          # every available language, both targets
#   ./test-dbg-commands.sh --target fibonacci all       # only fibonacci across all langs
#   ./test-dbg-commands.sh --target ackermann all       # only ackermann across all langs
#   ./test-dbg-commands.sh --target both <lang>         # both (explicit, default)
#
# Env overrides:
#   OUT_DIR=/some/path  override examples/test-results
#
# Expects `./build.sh` has been run already so the targets exist.

set -u
set -o pipefail

ONLY_TARGET="both"
if [[ "${1:-}" == "--target" ]]; then
    shift
    ONLY_TARGET="${1:-both}"
    shift || true
    case "$ONLY_TARGET" in
        fibonacci|ackermann|both) ;;
        *) echo "invalid --target: $ONLY_TARGET (expected fibonacci|ackermann|both)" >&2; exit 2 ;;
    esac
fi

ROOT="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$ROOT/.." && pwd)"
DBG="${DBG:-$REPO/target/debug/dbg}"
OUT="${OUT_DIR:-$ROOT/test-results}"
mkdir -p "$OUT"

if [[ ! -x "$DBG" ]]; then
    echo "dbg binary not found at $DBG — run \`cargo build --bin dbg\` first" >&2
    exit 1
fi

# --------------------------------------------------------------
# Language matrix
#
# lang → (backend-type, target-path, source-file)
#
# Each source has two marker comments we grep for to locate a
# breakpoint line:
#   - `← fibonacci hot line`   (at the `next = a + b` step)
#   - `← ackermann recursion`  (at the entry of ackermann)
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

# Run `dbg <args...>` with a hard timeout; append stdout+stderr to the
# per-target log. Prints "[ok]" or "[fail: <reason>]" on its own line
# so we can grep.
#
# We capture the run's output to a tempfile so we can scan it for
# dbg-printed error markers. `dbg` sometimes exits 0 but prints
# `error: …`, `[error: …]`, `Command failed: …`, or an LLDB-style
# `WARNING: Unable to resolve breakpoint …` — those still count as
# failures for the purpose of the harness summary.
#
# Signature: dbg_cmd <lang> <target> <label> <dbg-args...>
dbg_cmd() {
    local lang="$1"; shift
    local target="$1"; shift
    local log="$OUT/$lang/$target.log"
    local label="$1"; shift  # for display
    local tmp
    tmp="$(mktemp)"
    {
        echo
        echo "---- $ $DBG $* ($label) ----"
    } >> "$log"
    local rc=0
    timeout 20 "$DBG" "$@" >"$tmp" 2>&1 || rc=$?
    cat "$tmp" >> "$log"
    if [[ $rc -ne 0 ]]; then
        echo "[fail rc=$rc] $label" >> "$log"
        rm -f "$tmp"
        return 1
    fi
    # rc==0 but the output may still indicate a dbg-reported error.
    if grep -qE '^(error:|\[error|Command failed:|WARNING:.*[Uu]nable to resolve)' "$tmp"; then
        local errmsg
        errmsg="$(grep -m1 -E '^(error:|\[error|Command failed:|WARNING:.*[Uu]nable to resolve)' "$tmp")"
        echo "[fail rc=0 $errmsg] $label" >> "$log"
        rm -f "$tmp"
        return 1
    fi
    echo "[ok] $label" >> "$log"
    rm -f "$tmp"
    return 0
}

# Find the breakpoint line in a source file by grepping for a marker.
break_line() {
    local src="$1"
    local marker="$2"
    grep -n "$marker" "$src" 2>/dev/null | head -1 | cut -d: -f1
}

# --------------------------------------------------------------
# Single-target test run
#
# Returns:
#   0  — target executed (ok/fail counts in the caller-visible vars)
#   2  — target skipped (e.g. marker missing)
#   1  — fatal (daemon didn't start)
#
# Writes `ok` and `fail` counts to the globals TARGET_OK / TARGET_FAIL.
# --------------------------------------------------------------

run_target() {
    local lang="$1"
    local target_name="$2"   # fibonacci | ackermann
    local marker="$3"        # grep phrase in source
    local trend_var="$4"     # expression name (a / n)
    local fn_name="$5"       # function name for `cross <fn>`

    local backend="${BACKEND[$lang]}"
    local target="${TARGET[$lang]}"
    local src="${SRC[$lang]}"

    TARGET_OK=0
    TARGET_FAIL=0

    local log="$OUT/$lang/$target_name.log"
    : > "$log"
    {
        echo "# dbg canonical-command test: $lang / $target_name"
        echo "# backend=$backend target=$target"
        echo "# source=$src marker=\"$marker\" trend_var=$trend_var"
        date
    } > "$log"

    local line
    line="$(break_line "$src" "$marker")"
    if [[ -z "$line" ]]; then
        echo "    skip $target_name — no $target_name marker"
        echo "SKIP: no $target_name marker" >> "$log"
        return 2
    fi
    # Use the absolute source path so debuggers that key breakpoints
    # on the fully-resolved file (pdb, jdb, ocamldebug) match cleanly.
    local loc="${src}:${line}"
    echo "    breakpoint ($target_name): $loc"
    echo "# breakpoint loc: $loc" >> "$log"

    # --- clean slate
    "$DBG" kill >/dev/null 2>&1 || true
    sleep 0.3

    # --- start session with --break --run
    # For Java, start from the directory containing the .class files
    # so jdb can find the class. For everything else, start from $REPO.
    local start_dir="$REPO"
    case "$lang" in
        java) start_dir="$ROOT/java" ;;
    esac

    if (cd "$start_dir" && timeout 30 "$DBG" start "$backend" "$target" --break "$loc" --run >> "$log" 2>&1); then
        TARGET_OK=$((TARGET_OK + 1))
        echo "      ✓ start"
    else
        TARGET_FAIL=$((TARGET_FAIL + 1))
        echo "      ✗ start (see $log)"
        echo "  giving up on $lang/$target_name — daemon did not start" | tee -a "$log"
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
        if dbg_cmd "$lang" "$target_name" "$v" "$v"; then TARGET_OK=$((TARGET_OK+1)); echo "      ✓ $v"
        else TARGET_FAIL=$((TARGET_FAIL+1)); echo "      ✗ $v"
        fi
    done

    # --- expression evaluation — variable depends on target
    if dbg_cmd "$lang" "$target_name" "print $trend_var" print "$trend_var"; then
        TARGET_OK=$((TARGET_OK+1)); echo "      ✓ print $trend_var"
    else
        TARGET_FAIL=$((TARGET_FAIL+1)); echo "      ✗ print $trend_var"
    fi

    # --- step through the loop: `continue` 4 times to land at hits 2-5.
    # Follow each continue with `locals` so backends with
    # auto_capture_locals=false still get locals_json backfilled
    # (enables sparklines via the retroactive-update path).
    for i in 2 3 4 5; do
        if dbg_cmd "$lang" "$target_name" "continue#$i" continue; then
            TARGET_OK=$((TARGET_OK+1)); echo "      ✓ continue (hit #$i)"
        else
            TARGET_FAIL=$((TARGET_FAIL+1)); echo "      ✗ continue (hit #$i)"
        fi
        sleep 0.2
        dbg_cmd "$lang" "$target_name" "locals#$i" locals >/dev/null 2>&1 || true
        sleep 0.1
    done

    # --- cross-track queries
    for q in "hits $loc" "hit-diff $loc 1 3" "hit-trend $loc $trend_var" "cross $fn_name" "sessions"; do
        # shellcheck disable=SC2086
        if dbg_cmd "$lang" "$target_name" "$q" $q; then
            TARGET_OK=$((TARGET_OK+1)); echo "      ✓ $q"
        else
            TARGET_FAIL=$((TARGET_FAIL+1)); echo "      ✗ $q"
        fi
    done

    # --- tear down
    "$DBG" kill >/dev/null 2>&1 || true
    sleep 0.2

    echo "    $lang/$target_name: $TARGET_OK ok, $TARGET_FAIL fail" >> "$log"
    return 0
}

# --------------------------------------------------------------
# Single-language test run — drives both targets
# --------------------------------------------------------------

run_one() {
    local lang="$1"
    local backend="${BACKEND[$lang]}"
    local target="${TARGET[$lang]}"
    local src="${SRC[$lang]}"

    mkdir -p "$OUT/$lang"

    echo "================ $lang ================"

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

    local lang_ok=0 lang_fail=0
    local fib_ok=0 fib_fail=0 fib_ran=0 fib_fatal=0
    local ack_ok=0 ack_fail=0 ack_ran=0 ack_fatal=0

    # --- fibonacci target (existing behavior)
    if [[ "$ONLY_TARGET" == "fibonacci" || "$ONLY_TARGET" == "both" ]]; then
        echo "  -- fibonacci --"
        TARGET_OK=0; TARGET_FAIL=0
        run_target "$lang" "fibonacci" "fibonacci hot line" "a" "fibonacci"
        local fib_rc=$?
        case $fib_rc in
            0) fib_ran=1 ;;
            1) fib_fatal=1 ;;
            2) : ;;  # skipped (missing marker)
        esac
        if [[ $fib_rc -ne 2 ]]; then
            fib_ok=$TARGET_OK
            fib_fail=$TARGET_FAIL
            lang_ok=$((lang_ok + fib_ok))
            lang_fail=$((lang_fail + fib_fail))
        fi

        # --- clean daemon state between targets
        "$DBG" kill >/dev/null 2>&1 || true
        sleep 0.3
    fi

    # --- ackermann target
    if [[ "$ONLY_TARGET" == "ackermann" || "$ONLY_TARGET" == "both" ]]; then
        echo "  -- ackermann --"
        TARGET_OK=0; TARGET_FAIL=0
        run_target "$lang" "ackermann" "ackermann recursion" "n" "ackermann"
        local ack_rc=$?
        case $ack_rc in
            0) ack_ran=1 ;;
            1) ack_fatal=1 ;;
            2) : ;;
        esac
        if [[ $ack_rc -ne 2 ]]; then
            ack_ok=$TARGET_OK
            ack_fail=$TARGET_FAIL
            lang_ok=$((lang_ok + ack_ok))
            lang_fail=$((lang_fail + ack_fail))
        fi
    fi

    # Format per-target sub-summary; show "skip" if the target didn't run.
    local fib_str ack_str
    if [[ $fib_ran -eq 1 ]]; then
        fib_str="$fib_ok/$((fib_ok+fib_fail))"
    elif [[ $fib_fatal -eq 1 ]]; then
        fib_str="fail"
    else
        fib_str="skip"
    fi
    if [[ $ack_ran -eq 1 ]]; then
        ack_str="$ack_ok/$((ack_ok+ack_fail))"
    elif [[ $ack_fatal -eq 1 ]]; then
        ack_str="fail"
    else
        ack_str="skip"
    fi

    echo "  $lang: $lang_ok ok, $lang_fail fail (fib: $fib_str, ack: $ack_str)"
    echo

    # Decide overall rc for this language:
    #  * any target ran to completion → 0 (ok)
    #  * at least one target failed fatally → classify as toolchain-skip
    #    only if ALL attempted targets failed at start with a "no
    #    debugger executable found" signature; else propagate as fail.
    #  * else (every target skipped due to missing marker) → 2.
    local any_ran=$(( fib_ran + ack_ran ))
    local any_fatal=$(( fib_fatal + ack_fatal ))
    if [[ $any_ran -gt 0 ]]; then
        return 0
    fi
    if [[ $any_fatal -gt 0 ]]; then
        # Did every fatal failure come from a missing debugger binary?
        # Look for the signature dbg emits in that case. If yes → skip
        # (toolchain absent). Otherwise → fail.
        local log_dir="$OUT/$lang"
        if grep -qiE 'no debugger executable|not found in PATH|no such file|could not find|command not found|missing dependencies|install missing dependencies' \
                 "$log_dir"/*.log 2>/dev/null; then
            return 2
        fi
        return 1
    fi
    return 2
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
    run_one "$l"
    case $? in
        0) overall_ok=$((overall_ok + 1)) ;;
        2) overall_skip=$((overall_skip + 1)) ;;
        *) overall_fail=$((overall_fail + 1)) ;;
    esac
done

echo "================ summary ================"
echo "  $overall_ok ok, $overall_skip skipped, $overall_fail failed"
echo "  logs under $OUT/"
