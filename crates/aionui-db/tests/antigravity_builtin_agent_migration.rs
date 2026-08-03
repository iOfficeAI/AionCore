//! Migration 034 seeds the Antigravity (agy CLI) builtin agent row.
//!
//! The assertions here pin the fields that silently change behaviour if wrong:
//! an `args` value would be passed to a CLI whose argv is built per turn, a
//! `yolo_id` would hand agy a mode it does not have, and a mis-shaped
//! `available_modes` leaves the UI's mode picker blank without any error.

use aionui_db::init_database_memory;
use sqlx::Row;

async fn migrated_pool() -> sqlx::SqlitePool {
    // Uses the crate's own initializer so the test exercises the same migration
    // path production does (a bare Migrator run misses its setup).
    let db = init_database_memory().await.expect("in-memory database");
    db.pool().clone()
}

#[tokio::test]
async fn seeds_antigravity_as_a_direct_cli_builtin() {
    let pool = migrated_pool().await;
    let row = sqlx::query(
        "SELECT name, backend, agent_type, agent_source, command, args, env, \
         native_skills_dirs, yolo_id, enabled, sort_order \
         FROM agent_metadata WHERE id = 'a9f3c21e'",
    )
    .fetch_one(&pool)
    .await
    .expect("antigravity builtin row must exist after migration 034");

    assert_eq!(row.get::<String, _>("name"), "Antigravity");
    assert_eq!(row.get::<String, _>("backend"), "antigravity");
    // Its own agent_type, not `acp`: agy does not speak ACP.
    assert_eq!(row.get::<String, _>("agent_type"), "antigravity");
    assert_eq!(row.get::<String, _>("agent_source"), "builtin");
    assert_eq!(row.get::<String, _>("command"), "agy");
    assert_eq!(row.get::<i64, _>("enabled"), 1);
    assert_eq!(row.get::<i64, _>("sort_order"), 3140);
    assert_eq!(row.get::<String, _>("native_skills_dirs"), r#"[".agents/skills"]"#);
}

#[tokio::test]
async fn args_stay_empty_because_agy_argv_is_per_turn() {
    // Bridged ACP rows store a static argv (e.g. ["-y","@scope/pkg@1.2.3"]).
    // agy's argv depends on the turn (-p <prompt> / --conversation / --add-dir),
    // so anything stored here would be wrong for every turn.
    let pool = migrated_pool().await;
    let args: String = sqlx::query_scalar("SELECT args FROM agent_metadata WHERE id = 'a9f3c21e'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(args, "[]");
}

#[tokio::test]
async fn yolo_id_is_the_sentinel_so_unattended_callers_can_ask_for_full_auto() {
    // agy has no full-auto MODE — measured on 1.1.9, all three of its modes are
    // refused the "command" permission without `--dangerously-skip-permissions`
    // and all three run commands with it, so the flag alone decides.
    //
    // A NULL here (the previous value) left "run unattended" indistinguishable
    // from "the user picked the default mode": a teammate or a scheduled run
    // would sit on approval prompts nobody is there to answer. The sentinel lets
    // them ask through the same channel every other agent uses; the backend
    // answers it by not installing its approval hook.
    let pool = migrated_pool().await;
    let yolo: Option<String> = sqlx::query_scalar("SELECT yolo_id FROM agent_metadata WHERE id = 'a9f3c21e'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(yolo.as_deref(), Some("yolo"));
}

#[tokio::test]
async fn available_modes_match_the_capability_projection_shape() {
    // The catalog write-back stores `{available_modes:[{id,name,..}],
    // current_mode_id}`. A bare array here would parse as "no modes" and the
    // picker would be empty until the first session finishes.
    let pool = migrated_pool().await;
    let raw: String = sqlx::query_scalar("SELECT available_modes FROM agent_metadata WHERE id = 'a9f3c21e'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let modes = v["available_modes"].as_array().expect("top-level available_modes key");
    let ids: Vec<&str> = modes.iter().map(|m| m["id"].as_str().unwrap()).collect();
    // agy's own axis is default / accept-edits / plan; `yolo` is AionUi's
    // sentinel, offered here so full auto is a deliberate choice rather than
    // something only teams and cron can reach.
    assert_eq!(ids, vec!["default", "accept-edits", "plan", "yolo"]);
    assert_eq!(v["current_mode_id"], "default");
    // The sentinel must never be the seeded current mode: that would silently
    // start every new conversation with approval prompts turned off.
    assert_ne!(v["current_mode_id"], "yolo");
}

#[tokio::test]
async fn the_row_agrees_with_the_hardcoded_agent_type_defaults() {
    // Two of the code paths that serve Antigravity read the compiled-in
    // `AgentType` defaults rather than this row: `native_skills_dirs` falls
    // through to `AgentType::native_skills_dirs()` (the row-reading branch is
    // ACP-only, and widening it would let a NULL column disable skills), and
    // team spawn falls back to `full_auto_mode_id()` when no row is in hand.
    // They are correct today only because the two sources agree — so pin that,
    // rather than leaving a silent divergence for whoever edits either one.
    use aionui_common::AgentType;

    let pool = migrated_pool().await;
    let row = sqlx::query("SELECT native_skills_dirs, yolo_id FROM agent_metadata WHERE id = 'a9f3c21e'")
        .fetch_one(&pool)
        .await
        .unwrap();

    let seeded_dirs: Vec<String> = serde_json::from_str(&row.get::<String, _>("native_skills_dirs")).unwrap();
    let compiled_dirs: Vec<String> = AgentType::Antigravity
        .native_skills_dirs()
        .expect("agy discovers skills natively")
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    assert_eq!(seeded_dirs, compiled_dirs);

    assert_eq!(
        row.get::<String, _>("yolo_id"),
        AgentType::Antigravity.full_auto_mode_id(Some("antigravity"))
    );
}
