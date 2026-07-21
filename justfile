# Default: list available recipes
set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"]

cargo_script := if os_family() == "windows" { "powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File scripts/just/cargo.ps1" } else { "bash scripts/just/cargo.sh" }
build_script := if os_family() == "windows" { "powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File scripts/just/build.ps1" } else { "bash scripts/just/build.sh" }
install_script := if os_family() == "windows" { "powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File scripts/just/install.ps1" } else { "bash scripts/just/install.sh" }
migration_check_script := if os_family() == "windows" { "powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File scripts/migration/check-immutability.ps1" } else { "bash scripts/migration/check-immutability.sh" }
migration_check_test_script := if os_family() == "windows" { "powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File scripts/migration/check-immutability.test.ps1" } else { "bash scripts/migration/check-immutability.test.sh" }
auto_commit_script := if os_family() == "windows" { "powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File scripts/just/auto-commit-fixes.ps1" } else { "bash scripts/just/auto-commit-fixes.sh" }
update_aionrs_script := if os_family() == "windows" { "powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File scripts/just/update-aionrs.ps1" } else { "bash scripts/just/update-aionrs.sh" }
cat_config_script := if os_family() == "windows" { "powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File scripts/just/cat-config.ps1" } else { "bash scripts/just/cat-config.sh" }

default:
    @just --list

# Enable pre-commit hooks (run once after clone)
setup:
    git config core.hooksPath .githooks
    @echo "Git hooks enabled"

# Run cargo with optional local aionrs SDK patches.
_cargo *ARGS:
    @{{cargo_script}} {{ARGS}}

# Build in release mode and install to cargo bin
# Use `just build --force` to skip cache check
build *FLAGS: lint-fix fmt
    @{{build_script}} release {{FLAGS}}

# Build in debug mode
# Use `just build-debug --force` to skip cache check
build-debug *FLAGS:
    @{{build_script}} debug {{FLAGS}}

install:
    @{{install_script}} release

# Run all tests
test:
    @just _cargo nextest run --workspace

# Ensure already-shipped database migrations stay immutable
migration-check:
    @{{migration_check_script}}

# Test the migration immutability guard itself
migration-check-test:
    @{{migration_check_test_script}}

# Lint (warnings = errors)
lint:
    @just _cargo clippy --workspace -- -D warnings

lint-fix:
    @just _cargo fix --allow-dirty --allow-staged
    @just _cargo clippy --fix --workspace --allow-dirty --allow-staged -- -D warnings

# Format code
fmt:
    @cargo fmt --all

# Check formatting (CI)
fmt-check:
    @cargo fmt --all -- --check

# Lint + format check + migration check + test
check: migration-check lint fmt-check test

# Run the server (debug)
run *ARGS:
    @just _cargo run --bin aioncore -- {{ARGS}}

# Run the server (release)
run-release *ARGS:
    @just _cargo run --release --bin aioncore -- {{ARGS}}

# Pre-push gate: migration check, format, lint, auto-commit fixes, test, then push
push *ARGS: migration-check lint-fix fmt _auto-commit-fixes test
    git push {{ARGS}}

# Auto-commit any formatting/lint fixes if there are changes
_auto-commit-fixes:
    @{{auto_commit_script}}

# Update aionrs dependency (e.g. just update-aionrs or just update-aionrs v0.1.19)
update-aionrs *TAG:
    @{{update_aionrs_script}} {{TAG}}

# Security audit
audit:
    @cargo audit

# Clean build artifacts
clean:
    @cargo clean

# Decode dev config and copy to clipboard when possible
cat-config:
    @{{cat_config_script}}
