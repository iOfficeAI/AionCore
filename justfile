# Default: list available recipes
default:
    @just --list

# Enable pre-commit hooks (run once after clone)
setup:
    git config core.hooksPath .githooks
    @echo "✅ Git hooks enabled"

# Run cargo with optional local aionrs SDK patches.
_cargo *ARGS:
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
        local crate expected_path pkgid
        for crate in "${crates[@]}"; do
            expected_path="$aionrs_root/crates/$crate"
            pkgid=$(cargo "${cargo_config[@]}" pkgid -p "$crate")

            if ! python3 -c 'import sys; from pathlib import Path; from urllib.parse import unquote, urlparse; pkgid=sys.argv[1]; expected=str(Path(sys.argv[2]).resolve()); ok=pkgid.startswith("path+file://") and str(Path(unquote(urlparse(pkgid[len("path+"):]).path)).resolve()) == expected; sys.exit(0 if ok else 1)' "$pkgid" "$expected_path"
            then
                echo "AIONRS patch was not used for $crate." >&2
                echo "  resolved: $pkgid" >&2
                echo "  expected: $expected_path" >&2
                exit 1
            fi
        done
    }

    if [[ -n "${AIONRS:-}" ]]; then
        if [[ ! -d "$AIONRS" ]]; then
            echo "AIONRS does not exist or is not a directory: $AIONRS" >&2
            exit 1
        fi

        aionrs_root=$(cd "$AIONRS" && pwd -P)
        crates=(
            aion-agent
            aion-providers
            aion-types
            aion-protocol
            aion-config
            aion-mcp
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
            -p aion-providers \
            -p aion-types \
            -p aion-protocol \
            -p aion-config \
            -p aion-mcp
        verify_local_aionrs_patch
    fi

    set +e
    if ((${#cargo_config[@]})); then
        cargo "${cargo_config[@]}" {{ARGS}}
    else
        cargo {{ARGS}}
    fi
    status=$?
    set -e
    exit "$status"

# Build in release mode and install to ~/.cargo/bin
# Use `just build --force` to skip cache check
build *FLAGS: lint-fix fmt
    #!/usr/bin/env bash
    set -euo pipefail
    just _cargo build --release
    new_sum=$(shasum -a 256 target/release/aioncore | cut -d' ' -f1)
    force=false
    for flag in {{FLAGS}}; do
        if [[ "$flag" == "--force" || "$flag" == "-f" ]]; then
            force=true
        fi
    done
    old_sum=""
    if [[ -f target/.build-sum ]] && [[ "$force" == "false" ]]; then
        old_sum=$(cat target/.build-sum)
    fi
    if [[ "$new_sum" == "$old_sum" ]]; then
        echo -e "\n⏭️  Binary unchanged — skipping install (sha256: ${new_sum:0:16}…)"
    else
        cp target/release/aioncore ~/.cargo/bin/
        codesign --force --sign - ~/.cargo/bin/aioncore
        echo "$new_sum" > target/.build-sum
        echo -e "\n✅ Build complete — sha256: ${new_sum:0:16}…"
    fi

# Build in debug mode
# Use `just build-debug --force` to skip cache check
build-debug *FLAGS:
    #!/usr/bin/env bash
    set -euo pipefail
    just _cargo build
    new_sum=$(shasum -a 256 target/debug/aioncore | cut -d' ' -f1)
    force=false
    for flag in {{FLAGS}}; do
        if [[ "$flag" == "--force" || "$flag" == "-f" ]]; then
            force=true
        fi
    done
    old_sum=""
    if [[ -f target/.build-debug-sum ]] && [[ "$force" == "false" ]]; then
        old_sum=$(cat target/.build-debug-sum)
    fi
    if [[ "$new_sum" == "$old_sum" ]]; then
        echo -e "\n⏭️  Debug binary unchanged (sha256: ${new_sum:0:16}…)"
    else
        echo "$new_sum" > target/.build-debug-sum
        echo -e "\n✅ Debug build complete — sha256: ${new_sum:0:16}…"
    fi

install:
    cp target/release/aioncore ~/.cargo/bin/
    codesign --force --sign - ~/.cargo/bin/aioncore

# Run all tests
test:
    just _cargo nextest run --workspace

# Ensure already-shipped database migrations stay immutable
migration-check:
    scripts/check-migration-immutability.sh

# Test the migration immutability guard itself
migration-check-test:
    scripts/check-migration-immutability.test.sh

# Lint (warnings = errors)
lint:
    just _cargo clippy --workspace -- -D warnings

lint-fix:
    just _cargo fix --allow-dirty --allow-staged
    just _cargo clippy --fix --workspace --allow-dirty --allow-staged -- -D warnings

# Format code
fmt:
    cargo fmt --all

# Check formatting (CI)
fmt-check:
    cargo fmt --all -- --check

# Lint + format check + migration check + test
check: migration-check lint fmt-check test

# Run the server (debug)
run *ARGS:
    just _cargo run --bin aioncore -- {{ARGS}}

# Run the server (release)
run-release *ARGS:
    just _cargo run --release --bin aioncore -- {{ARGS}}

# Pre-push gate: migration check, format, lint, auto-commit fixes, test, then push
push *ARGS: migration-check lint-fix fmt _auto-commit-fixes test
    git push {{ ARGS }}

# Auto-commit any formatting/lint fixes if there are changes
_auto-commit-fixes:
    #!/usr/bin/env bash
    if [ -n "$(git diff --name-only)" ]; then
        git add -A
        git commit -m "chore: apply auto-fixes (fmt + clippy)"
    fi

# Update aionrs dependency (e.g. just update-aionrs or just update-aionrs v0.1.19)
update-aionrs *TAG:
    #!/usr/bin/env bash
    set -euo pipefail
    tag="{{ TAG }}"
    if [ -z "$tag" ]; then
        tag=$(git ls-remote --tags https://github.com/iOfficeAI/aionrs.git | awk -F/ '{print $NF}' | grep -v '\\^{}' | sort -V | tail -1)
        echo "Using latest tag: $tag"
    fi
    sed -i '' "s|git = \"https://github.com/iOfficeAI/aionrs.git\", tag = \"[^\"]*\"|git = \"https://github.com/iOfficeAI/aionrs.git\", tag = \"$tag\"|g" Cargo.toml
    cargo check --workspace

# Security audit
audit:
    cargo audit

# Clean build artifacts
clean:
    cargo clean

# Decode dev config and copy to clipboard
cat-config:
    @base64 -D -i ~/.aionui-config-dev/aionui-config.txt | python3 -c 'import sys, urllib.parse; print(urllib.parse.unquote(sys.stdin.read()))' | pbcopy
