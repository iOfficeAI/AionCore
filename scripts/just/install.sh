#!/usr/bin/env bash
set -euo pipefail

mode="${1:-release}"
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) exe_suffix=".exe" ;;
    *) exe_suffix="" ;;
esac

case "$mode" in
    release) binary="target/release/aioncore$exe_suffix" ;;
    debug) binary="target/debug/aioncore$exe_suffix" ;;
    *) echo "unknown install mode: $mode" >&2; exit 1 ;;
esac

if [[ ! -f "$binary" ]]; then
    echo "binary not found: $binary" >&2
    exit 1
fi

cargo_home="${CARGO_HOME:-$HOME/.cargo}"
install_dir="$cargo_home/bin"
mkdir -p "$install_dir"
cp "$binary" "$install_dir/"

if [[ "$(uname -s)" == "Darwin" ]] && command -v codesign >/dev/null 2>&1; then
    codesign --force --sign - "$install_dir/$(basename "$binary")"
fi

echo "Installed $(basename "$binary") to $install_dir"
