//! Focused migration tests for 039-042 ad-hoc and formal team data changes.

use aionui_db::init_database_memory;

#[tokio::test]
async fn migrations_039_through_042_apply_and_support_origin_and_presets() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();

    sqlx::query(
        "INSERT INTO teams (id, user_id, name, workspace, workspace_mode, agents, agents_version, origin_conversation_id, created_at, updated_at) \
         VALUES ('t1', 'u1', 'AdHoc', '/tmp', 'shared', '[]', '1.0.1', 'conv-1', 1, 1)",
    )
    .execute(pool)
    .await
    .unwrap();

    let origin: (Option<String>,) = sqlx::query_as("SELECT origin_conversation_id FROM teams WHERE id = 't1'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(origin.0.as_deref(), Some("conv-1"));

    let dup = sqlx::query(
        "INSERT INTO teams (id, user_id, name, workspace, workspace_mode, agents, agents_version, origin_conversation_id, created_at, updated_at) \
         VALUES ('t2', 'u1', 'Dup', '/tmp', 'shared', '[]', '1.0.1', 'conv-1', 1, 1)",
    )
    .execute(pool)
    .await;
    assert!(dup.is_err(), "duplicate origin_conversation_id must fail");

    sqlx::query(
        "INSERT INTO team_presets (id, user_id, name, description, expertise_tags, example_prompts, leader, members, version, created_at, updated_at) \
         VALUES ('p1', 'u1', 'Preset', 'd', '[]', '[]', '{}', '[]', 1, 1, 1)",
    )
    .execute(pool)
    .await
    .unwrap();
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM team_presets")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(count.0, 1);

    let versions: Vec<(i64,)> =
        sqlx::query_as("SELECT version FROM _sqlx_migrations WHERE version >= 39 ORDER BY version")
            .fetch_all(pool)
            .await
            .unwrap();
    let vs: Vec<i64> = versions.into_iter().map(|v| v.0).collect();
    assert_eq!(vs, vec![39, 40, 41, 42]);
}

#[tokio::test]
async fn migration_042_removes_only_orphaned_team_conversation_fields() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();

    sqlx::query(
        "INSERT INTO teams \
         (id, user_id, name, workspace, workspace_mode, agents, agents_version, created_at, updated_at) \
         VALUES ('team-live', 'system_default_user', 'Live', '/tmp', 'shared', '[]', '1.0.1', 1, 1)",
    )
    .execute(pool)
    .await
    .unwrap();
    for (id, extra) in [
        (
            "orphan",
            r#"{"teamId":"team-deleted","team_id":"team-deleted","slot_id":"slot-1","role":"lead","team_mcp_stdio_config":{"command":"core"},"workspace":"/keep"}"#,
        ),
        (
            "live",
            r#"{"teamId":"team-live","slot_id":"slot-2","role":"lead","workspace":"/keep-live"}"#,
        ),
        ("ordinary", r#"{"workspace":"/ordinary"}"#),
    ] {
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, extra) \
             VALUES (?, 'system_default_user', ?, 'acp', 'pending', 1, 1, ?)",
        )
        .bind(id)
        .bind(id)
        .bind(extra)
        .execute(pool)
        .await
        .unwrap();
    }

    let repair_sql = std::fs::read_to_string("migrations/042_remove_orphaned_team_conversation_bindings.sql").unwrap();
    sqlx::raw_sql(&repair_sql).execute(pool).await.unwrap();

    let orphan_extra: String = sqlx::query_scalar("SELECT extra FROM conversations WHERE id = 'orphan'")
        .fetch_one(pool)
        .await
        .unwrap();
    let orphan: serde_json::Value = serde_json::from_str(&orphan_extra).unwrap();
    assert_eq!(orphan, serde_json::json!({ "workspace": "/keep" }));

    let live_extra: String = sqlx::query_scalar("SELECT extra FROM conversations WHERE id = 'live'")
        .fetch_one(pool)
        .await
        .unwrap();
    let live: serde_json::Value = serde_json::from_str(&live_extra).unwrap();
    assert_eq!(live["teamId"], "team-live");
    assert_eq!(live["slot_id"], "slot-2");
    assert_eq!(live["workspace"], "/keep-live");
}

#[tokio::test]
async fn migration_041_is_idempotent_on_repeat_update() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();

    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, status, created_at, updated_at, extra) \
         VALUES ('lead-conv', 'u1', 'Lead', 'pending', 1, 1, '{}')",
    )
    .execute(pool)
    .await
    .ok();

    for _ in 0..2 {
        let res = sqlx::query(
            r#"
UPDATE conversations
SET extra = json_set(extra, '$.team_id', t.team_id)
FROM (
    SELECT
        teams.id AS team_id,
        json_extract(agent.value, '$.conversation_id') AS conversation_id
    FROM teams
    JOIN json_each(teams.agents) AS agent
    WHERE teams.origin_conversation_id IS NULL
      AND json_extract(agent.value, '$.role') = 'lead'
) AS t
WHERE conversations.id = t.conversation_id
  AND (
      json_extract(conversations.extra, '$.team_id') IS NULL
      OR json_type(conversations.extra, '$.team_id') = 'null'
  );
"#,
        )
        .execute(pool)
        .await;
        assert!(res.is_ok(), "041 backfill should be re-runnable: {res:?}");
    }
}
