//! Migration 043 adds `agent_metadata.skill_delivery` and seeds per-vendor values.
//!
//! Assertions are on the RAW JSON, deliberately. At this layer the column is an
//! opaque string (see `models/agent_metadata.rs`) — `aionui-api-types` owns the
//! schema and its tolerant parser is unit-tested there. Keeping this test
//! serde_json-only avoids coupling the data layer to the DTO layer.
//!
//! Rows are addressed by `backend`, not by seed id: the id table is spread
//! across 001 / 003 / 034, and `backend` is the label the runtime keys on.

use aionui_db::init_database_memory;

async fn migrated_pool() -> sqlx::SqlitePool {
    // The crate's own initializer, so the test exercises the same migration path
    // production does (a bare Migrator run misses its setup).
    let db = init_database_memory().await.expect("in-memory database");
    db.pool().clone()
}

async fn delivery_json(pool: &sqlx::SqlitePool, backend: &str) -> serde_json::Value {
    let raw: Option<String> = sqlx::query_scalar("SELECT skill_delivery FROM agent_metadata WHERE backend = ? LIMIT 1")
        .bind(backend)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("the {backend} row must exist after 001/003/034: {e}"));
    let raw = raw.unwrap_or_else(|| panic!("{backend} must carry a seeded skill_delivery"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{backend} skill_delivery must be valid JSON: {e}"))
}

#[tokio::test]
async fn claude_gets_layer_one_argv_delivery_with_allow_dir_args() {
    let pool = migrated_pool().await;
    let delivery = delivery_json(&pool, "claude").await;

    assert_eq!(delivery["mode"], "argv");
    assert_eq!(
        delivery["args"],
        serde_json::json!(["--plugin-dir", "{skill_view_dir}"])
    );
    // Not optional: spec §10.2 #9 measured that a `--plugin-dir` registered
    // skill still fails claude's path check when the agent Reads its
    // supplementary files, under AionUi's real default permission mode.
    assert_eq!(
        delivery["allow_dir_args"],
        serde_json::json!(["--add-dir", "{skill_dir}"])
    );
}

/// codebuddy is deliberately NOT on layer 1 yet. Its pinned build (2.138.0)
/// documents `--plugin-dir` and accepts it at the argv level, but whether the
/// flag actually makes skills discoverable is unprobed (it needs an
/// authenticated account). Declaring `argv` on that basis would be a real
/// regression: `argv` also switches injection to LIGHT, so an inert flag would
/// leave codebuddy with no skills at all. This test pins the conservative
/// choice so a future promotion is a conscious edit.
#[tokio::test]
async fn codebuddy_stays_injected_until_its_layer_one_behavior_is_probed() {
    let pool = migrated_pool().await;
    let delivery = delivery_json(&pool, "codebuddy").await;

    assert_eq!(delivery["mode"], "injected");
    assert_eq!(
        delivery["allow_dir_args"],
        serde_json::json!(["--add-dir", "{skill_dir}"])
    );
}

#[tokio::test]
async fn codex_gets_protocol_delivery() {
    let pool = migrated_pool().await;
    let delivery = delivery_json(&pool, "codex").await;

    assert_eq!(delivery["mode"], "protocol");
    // Verified: codex-cli 0.146.0 self-generated schema
    // `v2/SkillsExtraRootsSetParams.json`.
    assert_eq!(delivery["method"], "skills/extraRoots/set");
}

/// agy gets no allow-listing, and that is a MEASURED result rather than an
/// omission: our argv always passes `--dangerously-skip-permissions`, and a live
/// probe under exactly that argv read a file outside the cwd with its directory
/// not allow-listed. Declaring the flag anyway would add one argument per skill
/// for no effect, and would read as a guarantee the measurement contradicts.
#[tokio::test]
async fn antigravity_is_injected_with_no_allow_listing() {
    let pool = migrated_pool().await;
    let delivery = delivery_json(&pool, "antigravity").await;

    assert_eq!(delivery["mode"], "injected");
    assert_eq!(delivery["allow_dir_args"], serde_json::json!([]));
}

#[tokio::test]
async fn opencode_is_injected() {
    let pool = migrated_pool().await;
    assert_eq!(delivery_json(&pool, "opencode").await["mode"], "injected");
}

/// The column must stay nullable with NO CHECK: it is an open extension point.
/// A CHECK would turn "ship a new mode as data" back into "write a migration",
/// and would hard-fail a registry insert carrying a newer mode on an older DB —
/// converting a degradable problem into an outage.
#[tokio::test]
async fn the_db_layer_accepts_an_unknown_mode() {
    let pool = migrated_pool().await;
    sqlx::query("UPDATE agent_metadata SET skill_delivery = ? WHERE backend = 'opencode'")
        .bind(r#"{"mode":"future_mode_v9"}"#)
        .execute(&pool)
        .await
        .expect("the DB must not constrain skill_delivery values");
}

/// An unverified vendor must keep the safe NULL default (read as `injected`).
/// Asserted so a later migration cannot quietly opt an unprobed vendor into
/// layer 1 — G4 requires a new vendor to be zero-intrusion by default.
#[tokio::test]
async fn unverified_vendors_stay_null() {
    let pool = migrated_pool().await;
    let null_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_metadata \
         WHERE skill_delivery IS NULL \
         AND backend NOT IN ('claude','codex','codebuddy','antigravity','opencode')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(null_count > 0, "unverified vendors must keep the safe NULL default");
}
