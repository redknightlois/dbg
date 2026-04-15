#!/usr/bin/env bash
# Install every toolchain needed to build the examples in this dir.
#
# Idempotent: installs what's missing and reports what's already there.
# Exits 0 when nothing is missing (either already present or just
# installed), non-zero when any language ended up `skipped` or `failed`.
#
# Privilege handling:
#   * Plain user:     apt-get calls go through `sudo -n`; home-dir
#                     installers (rustup, ghcup, choosenim, dotnet-
#                     install.sh, zig tarball) run as you.
#   * `sudo ./...`:   apt-get runs directly; home-dir installers drop
#                     back to $SUDO_USER via `sudo -u` so rustup &c.
#                     land under that user's $HOME, not /root.
#   * Plain root:     apt-get runs directly; home-dir installers
#                     write under /root (fine for containers / CI).
#
# Supported package managers: apt (Debian/Ubuntu) and brew (macOS).
# Other distros: apt steps fail cleanly and the script falls through
# to the upstream installer where one exists.
#
# Usage:
#   ./install-toolchains.sh              # install missing tools
#   sudo ./install-toolchains.sh         # same, using sudo for apt
#   ./install-toolchains.sh --dry-run    # report only, install nothing
#   ./install-toolchains.sh --check      # exit non-zero if anything missing
#   ./install-toolchains.sh rust go      # install only these languages

set -u
set -o pipefail

DRY_RUN=0
CHECK_ONLY=0
ONLY_LANGS=()

# ---------------------------------------------------------------
# Privilege detection.
#
# The script's job is to install every toolchain, regardless of how
# it was invoked. We detect three modes:
#
#   1. Plain root shell (EUID 0, no SUDO_USER)
#      → apt-get runs directly. Home-dir installers write to /root.
#
#   2. `sudo ./install-toolchains.sh` (EUID 0, SUDO_USER=<user>)
#      → apt-get runs directly. Home-dir installers drop privileges
#        to $SUDO_USER so rustup/ghcup/choosenim/dotnet land under
#        the invoking user's $HOME, not /root.
#
#   3. Regular user (EUID != 0)
#      → apt-get is prefixed with sudo (if present). Home-dir
#        installers run in-place.
#
# `HAS_SUDO=1` means we can escalate. In mode 1 (root without
# SUDO_USER), apt-get itself doesn't need sudo — but some downstream
# installers may want to drop back; we can't in that case and just
# run as root.
# ---------------------------------------------------------------

UID_REAL="${EUID:-$(id -u)}"
IS_ROOT=0
HAS_SUDO=0
TARGET_USER=""
TARGET_HOME=""

[[ "$UID_REAL" -eq 0 ]] && IS_ROOT=1

if command -v sudo >/dev/null 2>&1; then HAS_SUDO=1; fi

if (( IS_ROOT )) && [[ -n "${SUDO_USER:-}" ]]; then
    TARGET_USER="$SUDO_USER"
    TARGET_HOME="$(getent passwd "$SUDO_USER" 2>/dev/null | cut -d: -f6)"
    [[ -z "$TARGET_HOME" ]] && TARGET_HOME="/home/$SUDO_USER"
    echo "running under sudo — home-dir installers will drop to $TARGET_USER ($TARGET_HOME)"
elif (( IS_ROOT )); then
    TARGET_USER="root"
    TARGET_HOME="${HOME:-/root}"
    echo "running as plain root — home-dir installers will write under $TARGET_HOME"
else
    TARGET_USER="$(id -un)"
    TARGET_HOME="${HOME:-$(getent passwd "$TARGET_USER" | cut -d: -f6)}"
fi

# Run a home-dir installer as the "target" user. When invoked via
# sudo, `run_as_user` re-drops to $SUDO_USER so rustup et al. land
# under their $HOME, not /root. When not root, it's just a passthrough.
run_as_user() {
    if (( IS_ROOT )) && [[ "$TARGET_USER" != "root" ]]; then
        # -H to reset $HOME, -u to switch user, -n to fail rather than
        # prompt for a password (root->user doesn't need one anyway).
        run sudo -H -u "$TARGET_USER" -- "$@"
    else
        run "$@"
    fi
}

# Wrap a shell snippet the same way — for `curl | sh`-style installers.
run_as_user_sh() {
    local snippet="$1"
    if (( IS_ROOT )) && [[ "$TARGET_USER" != "root" ]]; then
        run sudo -H -u "$TARGET_USER" -- sh -c "$snippet"
    else
        run sh -c "$snippet"
    fi
}

while (( $# > 0 )); do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        --check)   CHECK_ONLY=1 ;;
        -h|--help)
            sed -n '2,16p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        -*)
            echo "unknown flag: $1" >&2
            exit 64
            ;;
        *) ONLY_LANGS+=("$1") ;;
    esac
    shift
done

# ---------------------------------------------------------------
# Status tracking
# ---------------------------------------------------------------

typeset -A STATUS
typeset -A NOTE

present() { STATUS["$1"]="present"; }
installed(){ STATUS["$1"]="installed"; }
skipped()  { STATUS["$1"]="skipped"; NOTE["$1"]="${2:-}"; }
failed()   { STATUS["$1"]="failed";  NOTE["$1"]="${2:-}"; }

want() {
    # Invoked by each language block to decide whether to bother.
    # Returns 0 if we should process this language, 1 otherwise.
    local lang="$1"
    if (( ${#ONLY_LANGS[@]} > 0 )); then
        local l
        for l in "${ONLY_LANGS[@]}"; do
            [[ "$l" == "$lang" ]] && return 0
        done
        return 1
    fi
    return 0
}

has() { command -v "$1" >/dev/null 2>&1; }

run() {
    # Print the command, then either execute or skip (dry-run).
    echo "  > $*"
    if (( DRY_RUN )); then
        return 0
    fi
    "$@"
}

# ---------------------------------------------------------------
# Distro / package manager detection
# ---------------------------------------------------------------

APT=0
BREW=0
if has apt-get; then APT=1; fi
if has brew;    then BREW=1; fi

apt_install() {
    if (( ! APT )); then
        return 1
    fi
    if (( IS_ROOT )); then
        run apt-get update -qq
        run apt-get install -y --no-install-recommends "$@"
    elif (( HAS_SUDO )); then
        # -n refuses to prompt for a password. If NOPASSWD isn't set
        # up, the user can `sudo -v` beforehand or re-invoke the
        # script via sudo.
        if ! sudo -n true 2>/dev/null; then
            echo "!! sudo needs a password; run either" >&2
            echo "   sudo -v && $0 $*" >&2
            echo "   sudo $0" >&2
            return 1
        fi
        run sudo -n apt-get update -qq
        run sudo -n apt-get install -y --no-install-recommends "$@"
    else
        echo "!! apt-get install needs root or sudo; not installed: $*" >&2
        return 1
    fi
}

brew_install() {
    if (( ! BREW )); then
        return 1
    fi
    run brew install "$@"
}

# ---------------------------------------------------------------
# Per-language installers
#
# Each function: if `has <bin>` just report present; else install via
# the best-available channel and report installed|failed.
# ---------------------------------------------------------------

install_c_cpp() {
    want c || return 0
    if has cc && has c++; then present c; present cpp; return 0; fi
    echo "==> installing C/C++ compiler (build-essential)"
    if apt_install build-essential; then
        installed c; installed cpp
    elif brew_install gcc; then
        installed c; installed cpp
    else
        skipped c   "no apt/brew — install gcc or clang manually"
        skipped cpp "same as C"
    fi
}

install_rust() {
    want rust || return 0
    if has cargo && has rustc; then present rust; return 0; fi
    echo "==> installing Rust (via rustup) for $TARGET_USER"
    if has curl; then
        run_as_user_sh "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal"
        # If rustup landed under the target user's $HOME, surface the
        # env helper for the rest of this script's PATH resolution.
        [[ -f "$TARGET_HOME/.cargo/env" ]] && source "$TARGET_HOME/.cargo/env"
        export PATH="$TARGET_HOME/.cargo/bin:$PATH"
        if has cargo; then installed rust; else failed rust "rustup ran but cargo missing on PATH"; fi
    else
        skipped rust "no curl; install from https://rustup.rs"
    fi
}

install_go() {
    want go || return 0
    if has go; then present go; return 0; fi
    echo "==> installing Go"
    if apt_install golang-go; then installed go
    elif brew_install go;    then installed go
    else
        skipped go "install manually from https://go.dev/dl/"
    fi
}

install_python() {
    want python || return 0
    if has python3; then present python; return 0; fi
    echo "==> installing Python 3"
    if apt_install python3; then installed python
    elif brew_install python; then installed python
    else skipped python "install python3 manually"
    fi
}

install_dotnet() {
    want csharp || return 0
    if has dotnet; then present csharp; return 0; fi
    echo "==> installing .NET SDK"
    if apt_install dotnet-sdk-8.0 2>/dev/null; then installed csharp
    elif brew_install --cask dotnet-sdk 2>/dev/null; then installed csharp
    elif has curl; then
        # Official install script — puts .NET under $TARGET_HOME/.dotnet.
        run_as_user_sh "curl -sSL https://dot.net/v1/dotnet-install.sh | bash -s -- --channel 8.0 --install-dir '$TARGET_HOME/.dotnet'"
        export PATH="$TARGET_HOME/.dotnet:$PATH"
        if has dotnet; then installed csharp; else failed csharp ".dotnet script ran but binary not found"; fi
    else
        skipped csharp "no installer available"
    fi
}

install_java() {
    want java || return 0
    if has javac && has java; then present java; return 0; fi
    echo "==> installing OpenJDK (needs javac, not just JRE)"
    if apt_install default-jdk; then installed java
    elif brew_install openjdk;    then installed java
    else skipped java "install JDK (with javac) manually"
    fi
}

install_node() {
    want javascript || return 0
    if has node; then present javascript; return 0; fi
    echo "==> installing Node.js"
    if apt_install nodejs; then installed javascript
    elif brew_install node; then installed javascript
    else skipped javascript "install Node from https://nodejs.org"
    fi
}

install_ruby() {
    want ruby || return 0
    if has ruby; then present ruby; return 0; fi
    echo "==> installing Ruby"
    if apt_install ruby-full; then installed ruby
    elif brew_install ruby;   then installed ruby
    else skipped ruby "install Ruby manually"
    fi
}

install_php() {
    want php || return 0
    if has php; then present php; return 0; fi
    echo "==> installing PHP"
    if apt_install php-cli; then installed php
    elif brew_install php;  then installed php
    else skipped php "install PHP manually"
    fi
}

install_zig() {
    want zig || return 0
    if has zig; then present zig; return 0; fi
    echo "==> installing Zig for $TARGET_USER"
    # Zig has no apt package; fetch the official tarball into
    # $TARGET_HOME/.local/zig-<ver> and symlink into .local/bin.
    local ZIG_VERSION="0.14.0"
    local arch
    case "$(uname -m)" in
        x86_64|amd64) arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) skipped zig "unsupported arch $(uname -m)"; return 0 ;;
    esac
    local os
    case "$(uname -s)" in
        Linux)  os="linux" ;;
        Darwin) os="macos" ;;
        *) skipped zig "unsupported OS $(uname -s)"; return 0 ;;
    esac
    if ! has curl; then skipped zig "curl required"; return 0; fi
    local url="https://ziglang.org/download/${ZIG_VERSION}/zig-${os}-${arch}-${ZIG_VERSION}.tar.xz"
    local dest="$TARGET_HOME/.local/zig-${ZIG_VERSION}"
    run_as_user mkdir -p "$TARGET_HOME/.local"
    run_as_user_sh "curl -sL '$url' | tar -xJ -C '$TARGET_HOME/.local'"
    run_as_user mv "$TARGET_HOME/.local/zig-${os}-${arch}-${ZIG_VERSION}" "$dest" 2>/dev/null || true
    run_as_user mkdir -p "$TARGET_HOME/.local/bin"
    run_as_user ln -sf "$dest/zig" "$TARGET_HOME/.local/bin/zig"
    export PATH="$TARGET_HOME/.local/bin:$PATH"
    if has zig; then installed zig; else failed zig "zig binary not on PATH after install"; fi
}

install_haskell() {
    want haskell || return 0
    if has ghc; then present haskell; return 0; fi
    echo "==> installing GHC (Haskell) for $TARGET_USER"
    if apt_install ghc; then installed haskell
    elif brew_install ghc; then installed haskell
    elif has curl; then
        # Canonical Haskell installer — runs under $TARGET_USER so
        # $HOME/.ghcup goes to the intended user, not /root.
        run_as_user_sh "curl --proto '=https' --tlsv1.2 -sSf https://get-ghcup.haskell.org | BOOTSTRAP_HASKELL_NONINTERACTIVE=1 BOOTSTRAP_HASKELL_MINIMAL=1 sh"
        [[ -d "$TARGET_HOME/.ghcup/bin" ]] && export PATH="$TARGET_HOME/.ghcup/bin:$PATH"
        if has ghc; then installed haskell; else failed haskell "ghcup ran but ghc missing"; fi
    else
        skipped haskell "install GHC manually"
    fi
}

install_ocaml() {
    want ocaml || return 0
    if has ocaml; then present ocaml; return 0; fi
    echo "==> installing OCaml"
    if apt_install ocaml; then installed ocaml
    elif brew_install ocaml; then installed ocaml
    else skipped ocaml "install OCaml manually"
    fi
}

install_nim() {
    want nim || return 0
    if has nim; then present nim; return 0; fi
    echo "==> installing Nim for $TARGET_USER"
    if apt_install nim; then installed nim
    elif brew_install nim; then installed nim
    elif has curl; then
        run_as_user_sh "curl https://nim-lang.org/choosenim/init.sh -sSf | sh -s -- -y"
        [[ -d "$TARGET_HOME/.nimble/bin" ]] && export PATH="$TARGET_HOME/.nimble/bin:$PATH"
        if has nim; then installed nim; else failed nim "choosenim ran but nim missing"; fi
    else
        skipped nim "install Nim manually"
    fi
}

# ---------------------------------------------------------------
# Drive installs
# ---------------------------------------------------------------

LANGS=(c cpp rust go python csharp java javascript ruby php zig haskell ocaml nim)

if (( CHECK_ONLY )); then
    # Just report what's missing, exit non-zero if anything is.
    missing=0
    for l in "${LANGS[@]}"; do
        case "$l" in
            c|cpp) has cc && has c++ && continue ;;
            rust)  has cargo && continue ;;
            python) has python3 && continue ;;
            csharp) has dotnet && continue ;;
            java) has javac && continue ;;
            javascript) has node && continue ;;
            haskell) has ghc && continue ;;
            *) has "$l" && continue ;;
        esac
        echo "missing: $l"
        missing=$((missing + 1))
    done
    exit $(( missing > 0 ? 1 : 0 ))
fi

echo "examples/install-toolchains.sh  (dry-run=$DRY_RUN, apt=$APT, brew=$BREW)"
echo

install_c_cpp
install_rust
install_go
install_python
install_dotnet
install_java
install_node
install_ruby
install_php
install_zig
install_haskell
install_ocaml
install_nim

echo
echo "---- summary ----"
any_skipped=0
for l in "${LANGS[@]}"; do
    st="${STATUS[$l]:-untouched}"
    note="${NOTE[$l]:-}"
    printf "  %-11s %s%s\n" "$l" "$st" "${note:+  ($note)}"
    case "$st" in
        skipped|failed) any_skipped=1 ;;
    esac
done

exit $any_skipped
