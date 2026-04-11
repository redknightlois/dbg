#!/bin/sh
set -e

main() {
    echo "Installing dbg — Agent Debug Toolkit"
    echo ""

    # Ensure cargo is available
    if ! command -v cargo >/dev/null 2>&1; then
        # Try sourcing env in case rustup is installed but not in current shell
        if [ -f "$HOME/.cargo/env" ]; then
            . "$HOME/.cargo/env"
        fi
    fi

    if ! command -v cargo >/dev/null 2>&1; then
        if command -v rustup >/dev/null 2>&1; then
            echo "rustup found but cargo missing. Installing default toolchain..."
            rustup default stable
        else
            echo "Rust not found. Installing via rustup..."
            curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --quiet
        fi
        . "$HOME/.cargo/env"
        echo ""
    fi

    # Install dbg-cli
    echo "Installing dbg-cli from crates.io..."
    cargo install dbg-cli
    echo ""

    # Verify the binary exists
    DBG_BIN="$HOME/.cargo/bin/dbg"
    if [ ! -f "$DBG_BIN" ]; then
        echo "Error: dbg binary not found at $DBG_BIN"
        exit 1
    fi

    echo "Done. dbg is ready."
    echo ""

    # If not in PATH, show how to activate
    case ":$PATH:" in
        *":$HOME/.cargo/bin:"*)
            ;;
        *)
            echo "To use dbg in this shell, run:"
            echo "  export PATH=\"\$HOME/.cargo/bin:\$PATH\""
            echo ""
            echo "To make it permanent (optional):"
            echo "  echo 'export PATH=\"\$HOME/.cargo/bin:\$PATH\"' >> ~/.bashrc"
            echo ""
            ;;
    esac

    echo "Next: connect to your agent:"
    echo "  dbg --init claude    # or: dbg --init codex"
}

main
