#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
script="$script_dir/repair-2dev-team-migrations.sh"
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

legacy_034="0C16E8A14DFC245CE282ACA34D67E57BB203D56DCE37CF88477C134222B24BDF136A47469A7C66C46171ADA3B3464970"
legacy_035="4116AD16BBC20F216AB3E2D4171E50E4D81B0AB883D7CF2789980F16A05F3EE609BDF3E79352F6E257F29C46CD49C8D2"
legacy_036="120F4C07DCB71A82DBDFBB48ED00A684EE8DA73B345594CBB7E18B795BA2621E3D107CCBE988DEA0DC9CEB9033809D8D"
redo_038="6CD3280DE1A4A0A14B7F3C71DD7C2894B15DF1E470A27C08ACC52FFF1D15617FC5F21E84B9063CF2C428E4FC90B3C279"
redo_039="7864E5F3ACFEBDA8B1E5F02E0DF980AA5141EEB22F36FBDE9340F7EC53DBB019889812C27F743152B3DF6BEEDA406BD4"
redo_040="7896C825326AE86E3FFB2728B67FACCFC5A60B830174990FBDBC90B879BC1B77D5F1605C8898F32B191BBFC5122A096E"

target_039="$(shasum -a 384 "$repo_root/crates/aionui-db/migrations/039_ad_hoc_team_origin_conversation.sql" | awk '{print toupper($1)}')"
target_040="$(shasum -a 384 "$repo_root/crates/aionui-db/migrations/040_team_presets.sql" | awk '{print toupper($1)}')"
target_041="$(shasum -a 384 "$repo_root/crates/aionui-db/migrations/041_backfill_formal_team_leader_team_id.sql" | awk '{print toupper($1)}')"

fake_a="$(printf 'AA%.0s' {1..48})"
fake_b="$(printf 'BB%.0s' {1..48})"
fake_c="$(printf 'CC%.0s' {1..48})"

new_db() {
    local db="$1"
    sqlite3 "$db" <<'SQL'
CREATE TABLE _sqlx_migrations (
    version BIGINT PRIMARY KEY NOT NULL,
    description TEXT NOT NULL,
    installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    success BOOLEAN NOT NULL,
    checksum BLOB NOT NULL,
    execution_time BIGINT NOT NULL
);
SQL
}

insert_row() {
    local db="$1" version="$2" description="$3" checksum="$4" installed_on="$5" execution_time="$6"
    sqlite3 "$db" "INSERT INTO _sqlx_migrations(version,description,installed_on,success,checksum,execution_time) VALUES($version,'$description','$installed_on',1,X'$checksum',$execution_time);"
}

versions() {
    sqlite3 "$1" "SELECT group_concat(version, ',') FROM (SELECT version FROM _sqlx_migrations ORDER BY version);"
}

assert_row() {
    local db="$1" version="$2" description="$3" checksum="$4" installed_on="$5" execution_time="$6"
    local actual expected
    actual="$(sqlite3 -separator '|' "$db" "SELECT description,hex(checksum),typeof(checksum),length(checksum),installed_on,success,execution_time FROM _sqlx_migrations WHERE version=$version;")"
    expected="$description|$checksum|blob|48|$installed_on|1|$execution_time"
    [[ "$actual" == "$expected" ]] || {
        echo "row assertion failed for version $version" >&2
        echo "expected: $expected" >&2
        echo "actual:   $actual" >&2
        exit 1
    }
}

expect_fail() {
    local label="$1"
    shift
    local output="$tmpdir/failure-output.txt"
    if "$@" >"$output" 2>&1; then
        echo "expected failure: $label" >&2
        cat "$output" >&2
        exit 1
    fi
    echo "PASS reject: $label"
}

# ---------------------------------------------------------------------------
# Path A: fresh/current DB — official rows do not look like historical custom
# metadata. No repair and no backup should be created.
# ---------------------------------------------------------------------------
fresh="$tmpdir/path-a.sqlite"
new_db "$fresh"
insert_row "$fresh" 34 "official migration 34" "$fake_a" "2026-08-17 01:00:34" 34
insert_row "$fresh" 35 "official migration 35" "$fake_b" "2026-08-17 01:00:35" 35
insert_row "$fresh" 36 "official migration 36" "$fake_c" "2026-08-17 01:00:36" 36
insert_row "$fresh" 38 "aionrs fork capability" "$fake_a" "2026-08-17 01:00:38" 38
fresh_before="$(versions "$fresh")"
fresh_backup="$tmpdir/path-a.backup"
fresh_output="$(bash "$script" --apply --backup "$fresh_backup" "$fresh")"
[[ "$fresh_output" == *"No repair required"* ]]
[[ "$(versions "$fresh")" == "$fresh_before" ]]
[[ ! -e "$fresh_backup" ]]
echo "PASS path A: fresh/current DB requires no repair"

# ---------------------------------------------------------------------------
# Path B: redo window 038/039/040 -> 039/040/041. Verify dry-run, backup,
# high-to-low remap, target metadata, and preservation of non-rewritten fields.
# ---------------------------------------------------------------------------
redo="$tmpdir/path-b-redo.sqlite"
redo_backup="$tmpdir/path-b-redo.backup"
new_db "$redo"
insert_row "$redo" 38 "ad hoc team origin conversation" "$redo_038" "2026-07-30 10:00:38" 3800
insert_row "$redo" 39 "team presets" "$redo_039" "2026-07-30 10:00:39" 3900
insert_row "$redo" 40 "backfill formal team leader team id" "$redo_040" "2026-07-30 10:00:40" 4000
redo_before="$(sqlite3 "$redo" "SELECT group_concat(version || ':' || description || ':' || hex(checksum), '|') FROM (SELECT * FROM _sqlx_migrations ORDER BY version);")"
dry_output="$(bash "$script" "$redo")"
[[ "$dry_output" == *"Detected source window: redo"* ]]
[[ "$dry_output" == *"Dry-run only"* ]]
redo_after_dry="$(sqlite3 "$redo" "SELECT group_concat(version || ':' || description || ':' || hex(checksum), '|') FROM (SELECT * FROM _sqlx_migrations ORDER BY version);")"
[[ "$redo_after_dry" == "$redo_before" ]]

bash "$script" --apply --backup "$redo_backup" "$redo" >/dev/null
[[ -f "$redo_backup" ]]
[[ "$(versions "$redo")" == "39,40,41" ]]
assert_row "$redo" 39 "ad hoc team origin conversation" "$target_039" "2026-07-30 10:00:38" 3800
assert_row "$redo" 40 "team presets" "$target_040" "2026-07-30 10:00:39" 3900
assert_row "$redo" 41 "backfill formal team leader team id" "$target_041" "2026-07-30 10:00:40" 4000
echo "PASS path B: redo 038/039/040 remapped to 039/040/041"

# Second invocation after a successful repair is a no-op and must not make a
# pointless second backup.
repeat_backup="$tmpdir/repeat.backup"
repeat_output="$(bash "$script" --apply --backup "$repeat_backup" "$redo")"
[[ "$repeat_output" == *"No repair required"* ]]
[[ ! -e "$repeat_backup" ]]
[[ "$(versions "$redo")" == "39,40,41" ]]
echo "PASS idempotency: second run is a no-op"

# ---------------------------------------------------------------------------
# Path C: original legacy 034/035/036 -> 039/040/041.
# ---------------------------------------------------------------------------
legacy="$tmpdir/path-c-legacy.sqlite"
legacy_backup="$tmpdir/path-c-legacy.backup"
new_db "$legacy"
insert_row "$legacy" 34 "ad hoc team origin conversation" "$legacy_034" "2026-07-24 11:03:34" 3400
insert_row "$legacy" 35 "team presets" "$legacy_035" "2026-07-24 11:03:35" 3500
insert_row "$legacy" 36 "backfill formal team leader team id" "$legacy_036" "2026-07-24 11:03:36" 3600
bash "$script" --apply --backup "$legacy_backup" "$legacy" >/dev/null
[[ -f "$legacy_backup" ]]
[[ "$(versions "$legacy")" == "39,40,41" ]]
assert_row "$legacy" 39 "ad hoc team origin conversation" "$target_039" "2026-07-24 11:03:34" 3400
assert_row "$legacy" 40 "team presets" "$target_040" "2026-07-24 11:03:35" 3500
assert_row "$legacy" 41 "backfill formal team leader team id" "$target_041" "2026-07-24 11:03:36" 3600
echo "PASS path C: legacy 034/035/036 remapped to 039/040/041"

# ---------------------------------------------------------------------------
# Rejection: historical-looking description with the wrong checksum.
# ---------------------------------------------------------------------------
bad_checksum="$tmpdir/bad-checksum.sqlite"
new_db "$bad_checksum"
insert_row "$bad_checksum" 34 "ad hoc team origin conversation" "$fake_a" "2026-07-24 12:00:34" 34
insert_row "$bad_checksum" 35 "team presets" "$legacy_035" "2026-07-24 12:00:35" 35
insert_row "$bad_checksum" 36 "backfill formal team leader team id" "$legacy_036" "2026-07-24 12:00:36" 36
expect_fail "checksum mismatch" bash "$script" --apply --backup "$tmpdir/bad-checksum.backup" "$bad_checksum"
[[ ! -e "$tmpdir/bad-checksum.backup" ]]

# Rejection: source window is valid but target is occupied by unrelated data.
foreign_target="$tmpdir/foreign-target.sqlite"
new_db "$foreign_target"
insert_row "$foreign_target" 34 "ad hoc team origin conversation" "$legacy_034" "2026-07-24 13:00:34" 34
insert_row "$foreign_target" 35 "team presets" "$legacy_035" "2026-07-24 13:00:35" 35
insert_row "$foreign_target" 36 "backfill formal team leader team id" "$legacy_036" "2026-07-24 13:00:36" 36
insert_row "$foreign_target" 39 "foreign migration" "$fake_b" "2026-08-17 13:00:39" 39
expect_fail "target exists with non-source metadata" bash "$script" --apply --backup "$tmpdir/foreign-target.backup" "$foreign_target"
[[ ! -e "$tmpdir/foreign-target.backup" ]]

# Rejection: valid legacy source and an already-correct target row coexist.
# Even though the target metadata is valid, auto-repair must not overwrite or
# merge duplicate logical migrations.
collision="$tmpdir/collision.sqlite"
new_db "$collision"
insert_row "$collision" 34 "ad hoc team origin conversation" "$legacy_034" "2026-07-24 14:00:34" 34
insert_row "$collision" 35 "team presets" "$legacy_035" "2026-07-24 14:00:35" 35
insert_row "$collision" 36 "backfill formal team leader team id" "$legacy_036" "2026-07-24 14:00:36" 36
insert_row "$collision" 39 "ad hoc team origin conversation" "$target_039" "2026-08-17 14:00:39" 39
expect_fail "source/target collision" bash "$script" --apply --backup "$tmpdir/collision.backup" "$collision"
[[ ! -e "$tmpdir/collision.backup" ]]

echo "All 2dev Team migration repair tests passed"
