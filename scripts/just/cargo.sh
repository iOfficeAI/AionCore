#!/usr/bin/env bash
set -euo pipefail

cargo_config=()
restore_cargo_lock=false
cargo_lock_snapshot=""
aionrs_root=""

restore_local_lockfile() {
    local status=$?

    if [[ -n "$cargo_lock_snapshot" && -f "$cargo_lock_snapshot" ]]; then
        if [[ "$restore_cargo_lock" == "true" || "$status" -ne 0 ]]; then
            cp "$cargo_lock_snapshot" Cargo.lock || status=$?
        fi
    fi
    if [[ -n "$cargo_lock_snapshot" ]]; then
        rm -f "$cargo_lock_snapshot"
    fi

    return "$status"
}
trap restore_local_lockfile EXIT

verify_local_aionrs_patch() {
    local metadata_file
    metadata_file=$(mktemp)
    cargo "${cargo_config[@]}" metadata --format-version 1 > "$metadata_file"

    python3 - "$aionrs_root" "$metadata_file" "${crates[@]}" <<'PY'
import json
import sys
from pathlib import Path

aionrs_root = Path(sys.argv[1]).resolve()
metadata_path = Path(sys.argv[2])
crates = sys.argv[3:]
metadata = json.loads(metadata_path.read_text())
packages = {package["name"]: package for package in metadata["packages"]}

for crate in crates:
    package = packages.get(crate)
    expected = (aionrs_root / "crates" / crate).resolve()
    if not package:
        print(f"AIONRS patch was not used for {crate}.", file=sys.stderr)
        print("  resolved: package not found", file=sys.stderr)
        print(f"  expected: {expected}", file=sys.stderr)
        sys.exit(1)

    actual = Path(package["manifest_path"]).resolve().parent
    if actual != expected:
        print(f"AIONRS patch was not used for {crate}.", file=sys.stderr)
        print(f"  resolved: {actual}", file=sys.stderr)
        print(f"  expected: {expected}", file=sys.stderr)
        sys.exit(1)
PY

    rm -f "$metadata_file"
}

if [[ -n "${AIONRS:-}" ]]; then
    if [[ ! -d "$AIONRS" ]]; then
        echo "AIONRS does not exist or is not a directory: $AIONRS" >&2
        exit 1
    fi

    aionrs_root=$(cd "$AIONRS" && pwd -P)
    crates=(
        aion-agent
        aion-compact
        aion-config
        aion-mcp
        aion-memory
        aion-process
        aion-protocol
        aion-providers
        aion-skills
        aion-tools
        aion-types
    )

    for crate in "${crates[@]}"; do
        crate_dir="$aionrs_root/crates/$crate"
        if [[ ! -f "$crate_dir/Cargo.toml" ]]; then
            echo "AIONRS is missing $crate: $crate_dir/Cargo.toml" >&2
            exit 1
        fi

        toml_path=${crate_dir//\\/\\\\}
        toml_path=${toml_path//\"/\\\"}
        cargo_config+=(--config "patch.'https://github.com/iOfficeAI/aionrs.git'.$crate.path = \"$toml_path\"")
    done

    echo "Using local aionrs SDK: $aionrs_root" >&2

    if [[ -f Cargo.lock ]]; then
        cargo_lock_snapshot=$(mktemp)
        cp Cargo.lock "$cargo_lock_snapshot"

        if git diff --quiet -- Cargo.lock && git diff --cached --quiet -- Cargo.lock; then
            restore_cargo_lock=true
        else
            echo "Cargo.lock already has changes; leaving successful AIONRS lockfile updates in place." >&2
        fi
    fi

    echo "Resolving Cargo.lock against local aionrs SDK" >&2
    cargo "${cargo_config[@]}" update \
        -p aion-agent \
        -p aion-compact \
        -p aion-config \
        -p aion-mcp \
        -p aion-memory \
        -p aion-process \
        -p aion-protocol \
        -p aion-providers \
        -p aion-skills \
        -p aion-tools \
        -p aion-types
    verify_local_aionrs_patch
fi

if ((${#cargo_config[@]})); then
    cargo "${cargo_config[@]}" "$@"
else
    cargo "$@"
fi
