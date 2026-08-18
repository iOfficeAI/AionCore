#!/usr/bin/env bash
set -euo pipefail

# Repair SQLx migration metadata written by two historical second-development
# Team migration windows so AionCore v0.1.67 can safely apply its official
# migrations and the rebased custom 039/040/041 migrations.
#
# Historical windows:
#   legacy: 034/035/036 -> 039/040/041
#   redo:   038/039/040 -> 039/040/041
#
# Safety model:
#   - dry-run by default; --apply is required to modify the database
#   - a SQLite backup is created before any mutation
#   - source rows are recognized by BOTH SQLx description and SHA-384 checksum
#   - version, description and checksum are changed together in BEGIN IMMEDIATE
#   - installed_on/success/execution_time are preserved
#   - post-check verifies source release and exact target metadata
#
# SQLx 0.8.6 computes checksum as Sha384::digest(sql.as_bytes()) and stores the
# 48 raw digest bytes in a BLOB column. This script keeps known historical
# checksums as hex literals for comparison and writes them with SQLite X'...'.

usage() {
    cat <<'EOF'
Usage: repair-2dev-team-migrations.sh [--apply] [--backup PATH] DATABASE

Repairs historical second-development Team migration metadata before running
AionCore v0.1.67 migrations.

Default behavior is dry-run only. Pass --apply to modify the database.
When changes are required, --apply always creates a backup; --backup PATH may
be used to choose its location.

Recognized source windows:
  legacy: 034 -> 039, 035 -> 040, 036 -> 041
  redo:   038 -> 039, 039 -> 040, 040 -> 041

After a successful repair, start the normal AionCore migrator so official
migrations and any still-pending custom migrations can run normally.
EOF
}

fail() {
    echo "ERROR: $*" >&2
    exit 1
}

apply=0
backup_path=''
database=''

while (($# > 0)); do
    case "$1" in
        --apply)
            apply=1
            ;;
        --backup)
            shift
            (($# > 0)) || { usage >&2; exit 2; }
            backup_path="$1"
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        -*)
            echo "Unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
        *)
            [[ -z "$database" ]] || { echo "Only one database may be supplied" >&2; usage >&2; exit 2; }
            database="$1"
            ;;
    esac
    shift
done

[[ -n "$database" ]] || { usage >&2; exit 2; }
[[ -f "$database" ]] || fail "Database does not exist: $database"
command -v sqlite3 >/dev/null 2>&1 || fail "sqlite3 is required"
command -v shasum >/dev/null 2>&1 || fail "shasum is required"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

# Target metadata is derived from the actual worktree migration files on every
# run. Do not replace these with hard-coded target checksums.
target_versions=(39 40 41)
target_files=(
    "$repo_root/crates/aionui-db/migrations/039_ad_hoc_team_origin_conversation.sql"
    "$repo_root/crates/aionui-db/migrations/040_team_presets.sql"
    "$repo_root/crates/aionui-db/migrations/041_backfill_formal_team_leader_team_id.sql"
)

declare -a target_descriptions=()
declare -a target_checksums=()

description_from_filename() {
    local name stem description
    name="$(basename "$1")"
    stem="${name%.sql}"
    description="${stem#*_}"
    printf '%s' "${description//_/ }"
}

sha384_hex_file() {
    shasum -a 384 "$1" | awk '{print toupper($1)}'
}

for i in "${!target_files[@]}"; do
    file="${target_files[$i]}"
    [[ -f "$file" ]] || fail "Target migration file is missing: $file"
    target_descriptions+=("$(description_from_filename "$file")")
    target_checksums+=("$(sha384_hex_file "$file")")
done

# Historical source metadata whitelist, calculated from exact git blobs:
#   integrate/ad-hoc-team-latest:034/035/036
#   ce1a8bf3:038/039/040
# Description values follow SQLx source.rs: remove .sql and replace '_' with ' '.
legacy_versions=(34 35 36)
legacy_descriptions=(
    "ad hoc team origin conversation"
    "team presets"
    "backfill formal team leader team id"
)
legacy_checksums=(
    "0C16E8A14DFC245CE282ACA34D67E57BB203D56DCE37CF88477C134222B24BDF136A47469A7C66C46171ADA3B3464970"
    "4116AD16BBC20F216AB3E2D4171E50E4D81B0AB883D7CF2789980F16A05F3EE609BDF3E79352F6E257F29C46CD49C8D2"
    "120F4C07DCB71A82DBDFBB48ED00A684EE8DA73B345594CBB7E18B795BA2621E3D107CCBE988DEA0DC9CEB9033809D8D"
)

redo_versions=(38 39 40)
redo_descriptions=(
    "ad hoc team origin conversation"
    "team presets"
    "backfill formal team leader team id"
)
redo_checksums=(
    "6CD3280DE1A4A0A14B7F3C71DD7C2894B15DF1E470A27C08ACC52FFF1D15617FC5F21E84B9063CF2C428E4FC90B3C279"
    "7864E5F3ACFEBDA8B1E5F02E0DF980AA5141EEB22F36FBDE9340F7EC53DBB019889812C27F743152B3DF6BEEDA406BD4"
    "7896C825326AE86E3FFB2728B67FACCFC5A60B830174990FBDBC90B879BC1B77D5F1605C8898F32B191BBFC5122A096E"
)

# Verify the SQLx metadata table exists and has the complete expected columns.
table_exists="$(sqlite3 "$database" "PRAGMA query_only=ON; SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations';")"
[[ "$table_exists" == '1' ]] || fail "_sqlx_migrations table is missing"

schema_columns="$(sqlite3 -separator ',' "$database" "PRAGMA query_only=ON; SELECT group_concat(name, ',') FROM pragma_table_info('_sqlx_migrations');")"
for required in version description installed_on success checksum execution_time; do
    case ",$schema_columns," in
        *",$required,"*) ;;
        *) fail "_sqlx_migrations is missing required column: $required" ;;
    esac
done

# Full-row reader. In addition to all six SQLx columns, include SQLite type and
# length for checksum so a text-encoded digest cannot be mistaken for the BLOB.
read_row() {
    local version="$1"
    sqlite3 -separator $'\t' "$database" \
        "PRAGMA query_only=ON; SELECT version,description,installed_on,success,hex(checksum),execution_time,typeof(checksum),length(checksum) FROM _sqlx_migrations WHERE version=$version;"
}

row_count() {
    local version="$1"
    sqlite3 "$database" "PRAGMA query_only=ON; SELECT COUNT(*) FROM _sqlx_migrations WHERE version=$version;"
}

print_row() {
    local label="$1" version="$2" row
    row="$(read_row "$version")"
    if [[ -z "$row" ]]; then
        echo "$label version=$version: absent"
    else
        echo "$label version=$version: $row"
    fi
}

# classify_window sets CLASSIFY_RESULT to match|none or exits on suspicious
# partial/mismatched historical metadata. This avoids false positives on the
# official 034..038 migrations while still rejecting a row that looks like a
# custom migration by description/checksum but is not an exact whitelist hit.
CLASSIFY_RESULT='none'
classify_window() {
    local kind="$1"
    local exact=0 suspicious=0
    local -a versions descriptions checksums

    if [[ "$kind" == 'legacy' ]]; then
        versions=(34 35 36)
        descriptions=("${legacy_descriptions[@]}")
        checksums=("${legacy_checksums[@]}")
    else
        versions=(38 39 40)
        descriptions=("${redo_descriptions[@]}")
        checksums=("${redo_checksums[@]}")
    fi

    for i in "${!versions[@]}"; do
        local version="${versions[$i]}"
        local expected_desc="${descriptions[$i]}"
        local expected_checksum="${checksums[$i]}"
        local count row rv rd ri rs rc re rt rl
        count="$(row_count "$version")"
        [[ "$count" -le 1 ]] || fail "Duplicate _sqlx_migrations rows for version $version"
        [[ "$count" == '1' ]] || continue
        row="$(read_row "$version")"
        IFS=$'\t' read -r rv rd ri rs rc re rt rl <<< "$row"

        if [[ "$rd" == "$expected_desc" && "$rc" == "$expected_checksum" && "$rt" == 'blob' && "$rl" == '48' && "$rs" == '1' ]]; then
            ((exact += 1))
        elif [[ "$rd" == "$expected_desc" || "$rc" == "$expected_checksum" ]]; then
            ((suspicious += 1))
            echo "Suspicious $kind source row:" >&2
            print_row "  actual" "$version" >&2
            echo "  expected description=$expected_desc checksum=$expected_checksum type=blob length=48 success=1" >&2
        fi
    done

    if ((suspicious > 0)); then
        fail "$kind migration window contains description/checksum metadata that is not an exact whitelist match"
    fi
    if ((exact == ${#versions[@]})); then
        CLASSIFY_RESULT='match'
        return
    fi
    if ((exact > 0)); then
        fail "$kind migration window is incomplete or partially remapped; refusing automatic repair"
    fi
    CLASSIFY_RESULT='none'
}

classify_window legacy
legacy_result="$CLASSIFY_RESULT"
classify_window redo
redo_result="$CLASSIFY_RESULT"

if [[ "$legacy_result" == 'match' && "$redo_result" == 'match' ]]; then
    fail "Both historical migration windows are present; database requires manual inspection"
fi

source_kind=''
declare -a source_versions=()
declare -a source_descriptions=()
declare -a source_checksums=()

if [[ "$legacy_result" == 'match' ]]; then
    source_kind='legacy'
    source_versions=(34 35 36)
    source_descriptions=("${legacy_descriptions[@]}")
    source_checksums=("${legacy_checksums[@]}")
elif [[ "$redo_result" == 'match' ]]; then
    source_kind='redo'
    source_versions=(38 39 40)
    source_descriptions=("${redo_descriptions[@]}")
    source_checksums=("${redo_checksums[@]}")
fi

# Validate target versions whenever they are not part of the recognized source
# window. A foreign row at 039/040/041 is never overwritten automatically.
target_exact() {
    local idx="$1"
    local version="${target_versions[$idx]}" row rv rd ri rs rc re rt rl
    [[ "$(row_count "$version")" == '1' ]] || return 1
    row="$(read_row "$version")"
    IFS=$'\t' read -r rv rd ri rs rc re rt rl <<< "$row"
    [[ "$rd" == "${target_descriptions[$idx]}" \
        && "$rc" == "${target_checksums[$idx]}" \
        && "$rt" == 'blob' \
        && "$rl" == '48' \
        && "$rs" == '1' ]]
}

if [[ -z "$source_kind" ]]; then
    found_target=0
    for i in "${!target_versions[@]}"; do
        version="${target_versions[$i]}"
        count="$(row_count "$version")"
        [[ "$count" -le 1 ]] || fail "Duplicate _sqlx_migrations rows for target version $version"
        if [[ "$count" == '1' ]]; then
            found_target=1
            if ! target_exact "$i"; then
                print_row "Foreign target" "$version" >&2
                fail "Target migration version $version exists but does not match the current 2dev target metadata"
            fi
        fi
    done

    if ((found_target == 1)); then
        echo "No repair required: existing 039/040/041 rows that are present already match the current target metadata"
    else
        echo "No repair required: no recognized legacy/redo 2dev Team migration window is present"
    fi
    exit 0
fi

# For a recognized source window, ensure destination occupancy is safe before
# backup/mutation. Redo source rows at 039/040 are intentionally allowed here;
# they are moved high-to-low. Any other pre-existing destination is a collision.
source_contains_version() {
    local needle="$1"
    for value in "${source_versions[@]}"; do
        [[ "$value" == "$needle" ]] && return 0
    done
    return 1
}

for i in "${!target_versions[@]}"; do
    target="${target_versions[$i]}"
    count="$(row_count "$target")"
    [[ "$count" -le 1 ]] || fail "Duplicate _sqlx_migrations rows for target version $target"
    if [[ "$count" == '1' ]] && ! source_contains_version "$target"; then
        if target_exact "$i"; then
            print_row "Existing target" "$target" >&2
            fail "Collision: source window $source_kind exists while target version $target is already populated with the target migration"
        else
            print_row "Foreign target" "$target" >&2
            fail "Target version $target already exists with non-source/non-target metadata"
        fi
    fi
done

echo "Detected source window: $source_kind"
echo "SQLx metadata rows to be remapped (full columns + checksum type/length):"
for source in "${source_versions[@]}"; do
    print_row "  source" "$source"
done

echo "Target metadata derived from current worktree migration files:"
for i in "${!target_versions[@]}"; do
    echo "  ${target_versions[$i]} description='${target_descriptions[$i]}' checksum=${target_checksums[$i]}"
done

# Preserve non-rewritten metadata for post-check.
declare -a preserved_installed_on=()
declare -a preserved_success=()
declare -a preserved_execution_time=()
for i in "${!source_versions[@]}"; do
    source="${source_versions[$i]}"
    row="$(read_row "$source")"
    IFS=$'\t' read -r rv rd ri rs rc re rt rl <<< "$row"
    preserved_installed_on[$i]="$ri"
    preserved_success[$i]="$rs"
    preserved_execution_time[$i]="$re"
done

if ((apply == 0)); then
    echo "Dry-run only; no database changes made (pass --apply to execute)"
    exit 0
fi

if [[ -z "$backup_path" ]]; then
    backup_path="${database}.pre-2dev-team-migration-repair-$(date +%Y%m%d%H%M%S).bak"
fi
[[ "$backup_path" != *$'\n'* ]] || fail "Backup path must not contain a newline"
mkdir -p "$(dirname "$backup_path")"
backup_arg="${backup_path//\\/\\\\}"
backup_arg="${backup_arg//\"/\\\"}"
sqlite3 "$database" ".backup \"$backup_arg\""
[[ -f "$backup_path" ]] || fail "SQLite backup was not created: $backup_path"
echo "Backup written: $backup_path"

# Build an atomic transaction. The TEMP guard table converts any concurrent
# metadata change or destination collision after preflight into a CHECK failure.
sql='BEGIN IMMEDIATE; CREATE TEMP TABLE _repair_2dev_guard(ok INTEGER NOT NULL CHECK(ok=1));'

# Move high-to-low. Logical migration 3 -> target 041, 2 -> 040, 1 -> 039.
for ((i=${#source_versions[@]}-1; i>=0; i--)); do
    source="${source_versions[$i]}"
    target="${target_versions[$i]}"
    source_desc="${source_descriptions[$i]}"
    source_checksum="${source_checksums[$i]}"
    target_desc="${target_descriptions[$i]}"
    target_checksum="${target_checksums[$i]}"

    # Descriptions are generated from controlled migration filenames. Refuse an
    # unexpected quote rather than interpolating an unsafe SQL literal.
    [[ "$source_desc" != *"'"* && "$target_desc" != *"'"* ]] || fail "Unexpected quote in migration description"

    sql+=" INSERT INTO _repair_2dev_guard SELECT CASE WHEN EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version=$source AND description='$source_desc' AND checksum=X'$source_checksum' AND success=1 AND typeof(checksum)='blob' AND length(checksum)=48) THEN 1 ELSE 0 END;"
    sql+=" INSERT INTO _repair_2dev_guard SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version=$target) THEN 1 ELSE 0 END;"
    sql+=" UPDATE _sqlx_migrations SET version=$target, description='$target_desc', checksum=X'$target_checksum' WHERE version=$source AND description='$source_desc' AND checksum=X'$source_checksum' AND success=1;"
done

sql+=' DROP TABLE _repair_2dev_guard; COMMIT;'
sqlite3 "$database" "$sql"

# Post-check: every historical source metadata identity is released and every
# target row has exact metadata from the current migration file while
# non-rewritten columns match the historical source row. In the redo path,
# numeric versions 039/040 are both old source numbers and new target numbers,
# so checking `version` absence alone would be incorrect.
for i in "${!source_versions[@]}"; do
    source="${source_versions[$i]}"
    target="${target_versions[$i]}"
    old_identity_count="$(sqlite3 "$database" "PRAGMA query_only=ON; SELECT COUNT(*) FROM _sqlx_migrations WHERE version=$source AND description='${source_descriptions[$i]}' AND checksum=X'${source_checksums[$i]}';")"
    [[ "$old_identity_count" == '0' ]] || fail "Post-check failed: historical source metadata still exists at version $source"
    target_exact "$i" || {
        print_row "Post-check target" "$target" >&2
        fail "Post-check failed: target version $target metadata does not match current migration file"
    }
    row="$(read_row "$target")"
    IFS=$'\t' read -r rv rd ri rs rc re rt rl <<< "$row"
    [[ "$ri" == "${preserved_installed_on[$i]}" ]] || fail "Post-check failed: installed_on changed for $source -> $target"
    [[ "$rs" == "${preserved_success[$i]}" ]] || fail "Post-check failed: success changed for $source -> $target"
    [[ "$re" == "${preserved_execution_time[$i]}" ]] || fail "Post-check failed: execution_time changed for $source -> $target"
done

echo "2dev Team migration metadata repair completed and verified"
echo "Next step: run the normal AionCore migrator so official/pending migrations can execute"
