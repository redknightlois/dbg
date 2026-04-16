#!/usr/bin/env bash
# Fix GHC runtime linker on systems where /lib64 lacks standard libs.
# GHC's RTS hardcodes /lib64/ but Debian/Ubuntu puts libs in
# /lib/x86_64-linux-gnu/. This creates the missing symlinks.
#
# Usage: sudo bash fix-ghc-lib64.sh

set -euo pipefail

SRC="/lib/x86_64-linux-gnu"
DST="/lib64"

if [[ ! -d "$SRC" ]]; then
    echo "error: $SRC not found — this script is for Debian/Ubuntu layouts" >&2
    exit 1
fi

libs=(libc.so.6 libm.so.6 libdl.so.2 librt.so.1 libpthread.so.0
      libgcc_s.so.1 libresolv.so.2 libutil.so.1 libnuma.so.1)

linked=0
for lib in "${libs[@]}"; do
    if [[ -f "$SRC/$lib" && ! -e "$DST/$lib" ]]; then
        ln -s "$SRC/$lib" "$DST/$lib"
        echo "  linked $lib"
        ((linked++))
    fi
done

if ((linked == 0)); then
    echo "nothing to do — all symlinks already exist"
else
    echo "created $linked symlink(s) in $DST"
fi

# Verify
if command -v ghci &>/dev/null; then
    if echo ':quit' | ghci -v0 2>/dev/null; then
        echo "ghci: ok"
    else
        echo "ghci: still broken — may need additional libs" >&2
        exit 1
    fi
fi
