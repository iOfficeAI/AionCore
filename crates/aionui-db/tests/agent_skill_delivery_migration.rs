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
/// After 044 the remaining unverified keepers are pi and deepseek.
#[tokio::test]
async fn unverified_vendors_stay_null() {
    let pool = migrated_pool().await;
    let null_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_metadata \
         WHERE skill_delivery IS NULL \
         AND backend NOT IN ('opencode')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(null_count > 0, "unverified vendors must keep the safe NULL default");
}
