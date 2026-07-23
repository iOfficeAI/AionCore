#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
script="$repo_root/scripts/migration/check-immutability.sh"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

run_in_repo() {
    local cwd="$1"
    local expected_status="$2"
    local expected_text="$3"
    shift 3

    set +e
    output="$(cd "$cwd" && "$@" 2>&1)"
    status=$?
    set -e

    if [[ "$status" != "$expected_status" ]]; then
        echo "expected status $expected_status, got $status" >&2
        echo "$output" >&2
        exit 1
    fi

    if [[ -n "$expected_text" && "$output" != *"$expected_text"* ]]; then
        echo "expected output to contain: $expected_text" >&2
        echo "$output" >&2
        exit 1
    fi
}

init_case_repo() {
    local name="$1"
    local dir="$tmpdir/$name"

    mkdir -p "$dir/crates/aionui-db/migrations"
    (
        cd "$dir"
        git init -q -b main
        git config user.email test@example.com
        git config user.name "Migration Test"
        printf '%s\n' '-- 001 initial' > crates/aionui-db/migrations/001_initial_schema.sql
        printf '%s\n' '-- 002 data fix' > crates/aionui-db/migrations/002_data_fix.sql
        printf '%s\n' '-- auxiliary sql' > crates/aionui-db/migrations/manual_fixture.sql
        git add crates/aionui-db/migrations
        git commit -q -m "seed migrations"
        git checkout -q -b feature
    )

    printf '%s\n' "$dir"
}

modified_repo="$(init_case_repo modified)"
printf '%s\n' '-- modified' >> "$modified_repo/crates/aionui-db/migrations/001_initial_schema.sql"
run_in_repo "$modified_repo" 1 "Existing migration files from main must not be modified or deleted" \
    env AIONCORE_MIGRATION_BASE_REF=main bash "$script"

deleted_repo="$(init_case_repo deleted)"
rm "$deleted_repo/crates/aionui-db/migrations/002_data_fix.sql"
run_in_repo "$deleted_repo" 1 "Existing migration files from main must not be modified or deleted" \
    env AIONCORE_MIGRATION_BASE_REF=main bash "$script"

auxiliary_repo="$(init_case_repo auxiliary)"
printf '%s\n' '-- modified auxiliary sql' >> "$auxiliary_repo/crates/aionui-db/migrations/manual_fixture.sql"
run_in_repo "$auxiliary_repo" 1 "Existing migration files from main must not be modified or deleted" \
    env AIONCORE_MIGRATION_BASE_REF=main bash "$script"

added_repo="$(init_case_repo added)"
printf '%s\n' '-- 003 new migration' > "$added_repo/crates/aionui-db/migrations/003_new_change.sql"
run_in_repo "$added_repo" 0 "Migration immutability check passed" \
    env AIONCORE_MIGRATION_BASE_REF=main bash "$script"

duplicate_repo="$(init_case_repo duplicate)"
printf '%s\n' '-- duplicate 002 migration' > "$duplicate_repo/crates/aionui-db/migrations/002_duplicate_change.sql"
run_in_repo "$duplicate_repo" 1 "Duplicate database migration versions are not allowed" \
    env AIONCORE_MIGRATION_BASE_REF=main bash "$script"

override_repo="$(init_case_repo override)"
printf '%s\n' '-- modified with explicit override' >> "$override_repo/crates/aionui-db/migrations/001_initial_schema.sql"
run_in_repo "$override_repo" 0 "skipping migration immutability check" \
    env AIONCORE_MIGRATION_BASE_REF=main AIONCORE_ALLOW_MAIN_MIGRATION_EDIT=1 bash "$script"

echo "Migration immutability script tests passed"
