# Default: list available recipes
default:
    @just --list

# Enable pre-commit hooks (run once after clone)
setup:
    git config core.hooksPath .githooks
    @echo "✅ Git hooks enabled"

# Build in release mode and install to ~/.cargo/bin
# Use `just build --force` to skip cache check
build *FLAGS: lint-fix fmt
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release
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
    cargo build
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
    cargo nextest run --workspace

# Ensure already-shipped database migrations stay immutable
migration-check:
    scripts/check-migration-immutability.sh

# Test the migration immutability guard itself
migration-check-test:
    scripts/check-migration-immutability.test.sh

# Lint (warnings = errors)
lint:
    cargo clippy --workspace -- -D warnings

lint-fix:
    cargo fix --allow-dirty --allow-staged
    cargo clippy --fix --workspace --allow-dirty --allow-staged -- -D warnings

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
    cargo run --bin aioncore -- {{ARGS}}

# Run the server (release)
run-release *ARGS:
    cargo run --release --bin aioncore -- {{ARGS}}

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
