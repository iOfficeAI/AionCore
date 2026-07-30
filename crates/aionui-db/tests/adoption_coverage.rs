//! AionPro adoption coverage gate.
//!
//! `adopt_system_default_data` re-owns the local default user's data to the
//! machine's first external (AionPro) account by convention: any table with a
//! `user_id` or `owner_user_id` column is an ownership root and gets adopted.
//! A table whose ownership column uses any OTHER name would be silently
//! skipped — no error, no log (this is exactly how `project_explorer` was
//! missed before `owner_user_id` support landed).
//!
//! This test turns that silent gap into a hard gate: EVERY table in the live
//! schema must fall into exactly one declared category. A migration adding an
//! unclassified table (including ones merged in from main) fails this test
//! until someone makes a conscious adoption decision for it.

use aionui_db::init_database_memory;

/// Ownership follows the parent chain — the row's user is derived by joining
/// through the declared parent column to an owned root, so adoption of the
/// root covers these rows. Each entry is (table, parent column, owned root).
const PARENT_SCOPED: &[(&str, &str, &str)] = &[
    ("acp_session", "conversation_id", "conversations"),
    ("conversation_artifacts", "conversation_id", "conversations"),
    ("conversation_assistant_snapshots", "conversation_id", "conversations"),
    // `cron_job_runs.owner_id` is the scheduler's run-lease holder, NOT a
    // user reference — ownership flows from `job_id`.
    ("cron_job_runs", "job_id", "cron_jobs"),
    ("mailbox", "team_id", "teams"),
    ("messages", "conversation_id", "conversations"),
    ("team_tasks", "team_id", "teams"),
];

/// Deliberately machine-global tables — adoption must NOT touch them.
/// Every entry needs a rationale.
const GLOBAL_TABLES: &[(&str, &str)] = &[
    // Canonical filesystem-path registry shared by all users by design
    // (project-bind: folders are reused globally by resource_canonical).
    ("folders", "global canonical path registry"),
];

/// Tables whose `user_id` column is NOT a Core-user ownership column — it
/// foreign-keys a non-Core identity table (a channel/platform user). The row's
/// Core owner is reached by joining that parent to its adopted root, so the
/// misleading `user_id` must be excluded from the ownership-root heuristic.
/// Without this, "has a `user_id` column" would classify the table as an
/// ownership root by accident and the gate would pass without a conscious
/// decision. Each entry: (table, the non-Core table its `user_id` points at).
const NON_CORE_USER_ID_TABLES: &[(&str, &str)] = &[
    // assistant_sessions.user_id -> assistant_users.id (a channel/platform
    // user). Core ownership flows through assistant_users.owner_user_id, which
    // adoption re-owns; adoption's UPDATE on this `user_id` never matches
    // 'system_default_user' (it holds assistant_users UUIDs), so it is a
    // harmless no-op.
    ("assistant_sessions", "assistant_users"),
];

/// Identity / infrastructure tables outside the ownership model.
const INFRA_TABLES: &[&str] = &["users", "_sqlx_migrations"];

async fn table_names(pool: &sqlx::SqlitePool) -> Vec<String> {
    sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn has_column(pool: &sqlx::SqlitePool, table: &str, column: &str) -> bool {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?")
        .bind(table)
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap();
    count > 0
}

#[tokio::test]
async fn every_table_is_classified_for_aionpro_adoption() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();

    let mut unclassified = Vec::new();
    let mut overlaps = Vec::new();

    for table in table_names(pool).await {
        let non_core_user_id = NON_CORE_USER_ID_TABLES.iter().any(|(name, _)| *name == table);
        let owned_by_user_id = has_column(pool, &table, "user_id").await && table != "users" && !non_core_user_id;
        let owned_by_owner_user_id = has_column(pool, &table, "owner_user_id").await;
        let parent_scoped = PARENT_SCOPED.iter().any(|(name, _, _)| *name == table);
        let global = GLOBAL_TABLES.iter().any(|(name, _)| *name == table);
        let infra = INFRA_TABLES.contains(&table.as_str());

        let categories = [
            owned_by_user_id,
            owned_by_owner_user_id,
            parent_scoped,
            global,
            infra,
            non_core_user_id,
        ]
        .iter()
        .filter(|hit| **hit)
        .count();

        match categories {
            0 => unclassified.push(table),
            1 => {}
            _ => overlaps.push(table),
        }
    }

    assert!(
        overlaps.is_empty(),
        "tables classified in more than one adoption category (fix the declarations): {overlaps:?}"
    );
    assert!(
        unclassified.is_empty(),
        "NEW TABLE(S) WITHOUT AN AIONPRO ADOPTION DECISION: {unclassified:?}\n\
         Every table must either carry an ownership column (`user_id` / `owner_user_id`,\n\
         adopted automatically), or be declared in this test as parent-scoped (with its\n\
         parent column), machine-global (with a rationale), or infrastructure.\n\
         See crates/aionui-db/tests/adoption_coverage.rs."
    );

    db.close().await;
}

#[tokio::test]
async fn parent_scoped_declarations_match_the_live_schema() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();
    let tables = table_names(pool).await;

    for (table, parent_column, owned_root) in PARENT_SCOPED {
        assert!(
            tables.iter().any(|name| name == table),
            "parent-scoped declaration references missing table {table}"
        );
        assert!(
            has_column(pool, table, parent_column).await,
            "parent-scoped table {table} lost its declared parent column {parent_column}"
        );
        assert!(
            has_column(pool, owned_root, "user_id").await,
            "parent-scoped table {table} points at root {owned_root}, which is not user-owned"
        );
        // A parent-scoped table must NOT quietly grow its own ownership column
        // (that would double-classify it and hide it from this gate's intent).
        assert!(
            !has_column(pool, table, "user_id").await && !has_column(pool, table, "owner_user_id").await,
            "{table} now has an ownership column — reclassify it as an ownership root"
        );
    }

    for (table, _) in GLOBAL_TABLES {
        assert!(
            tables.iter().any(|name| name == table),
            "global-table declaration references missing table {table}"
        );
    }

    for (table, parent) in NON_CORE_USER_ID_TABLES {
        assert!(
            tables.iter().any(|name| name == table),
            "non-core-user-id declaration references missing table {table}"
        );
        assert!(
            has_column(pool, table, "user_id").await,
            "{table} was declared as carrying a non-core user_id but has no user_id column"
        );
        // The parent its user_id points at must itself be an adopted root —
        // owned via owner_user_id — so Core ownership genuinely flows through it.
        assert!(
            has_column(pool, parent, "owner_user_id").await,
            "{table}.user_id points at {parent}, which is not an owner_user_id root"
        );
    }

    db.close().await;
}

// ── upgrade adoption behavior ──────────────────────────────────────────

use aionui_db::{
    ExternalUserProjection, IProjectStore, IUserRepository, SqliteProjectStore, SqliteUserRepository, UserType,
};

async fn seed_default_user_project(pool: &sqlx::SqlitePool) -> (String, String) {
    let store = SqliteProjectStore::new(pool.clone());
    let folder = store
        .upsert_folder("file:///tmp/adopt-ws", "file:///tmp/adopt-ws")
        .await
        .unwrap();
    let (project, entry) = store
        .create_project_with_workspace_entry(
            "system_default_user",
            &folder.folder_id,
            "adopt-ws",
            aionui_db::ProjectKind::Standard,
        )
        .await
        .unwrap();
    (project.project_id, entry.pe_id)
}

#[tokio::test]
async fn first_external_user_adopts_owner_user_id_tables_too() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();
    let repo = SqliteUserRepository::new(pool.clone());

    // Pre-upgrade machine data: a conversation (user_id table) and a
    // project + explorer workspace pair (user_id + owner_user_id tables).
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES ('conv-legacy', 'system_default_user', 'Legacy', 'acp', '{}', 'pending', 1, 1)",
    )
    .execute(pool)
    .await
    .unwrap();
    let (project_id, pe_id) = seed_default_user_project(pool).await;

    // Channel binding owned by the pre-upgrade local user (`owner_user_id`
    // table with a coexisting platform identity column).
    sqlx::query(
        "INSERT INTO assistant_users (id, platform_user_id, platform_type, display_name, authorized_at, owner_user_id)
         VALUES ('au-legacy', 'tg-123', 'telegram', 'TG User', 1, 'system_default_user')",
    )
    .execute(pool)
    .await
    .unwrap();

    let user = repo
        .ensure_external_user(
            UserType::Aionpro,
            "ext-upgrade-1",
            ExternalUserProjection {
                username: Some("Pro".into()),
                email: None,
                avatar_path: None,
            },
        )
        .await
        .unwrap();
    let moved = repo.adopt_system_default_data(&user.id).await.unwrap();
    assert!(
        moved >= 4,
        "conversation + project + explorer entry + channel binding must move, moved={moved}"
    );

    let binding_owner: String = sqlx::query_scalar("SELECT owner_user_id FROM assistant_users WHERE id = 'au-legacy'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(binding_owner, user.id, "channel platform binding must be adopted");

    let conv_owner: String = sqlx::query_scalar("SELECT user_id FROM conversations WHERE id = 'conv-legacy'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(conv_owner, user.id);

    // The projects/project_explorer PAIR must move together — a split pair
    // (project adopted, explorer entry left behind) is exactly the bug this
    // extension fixes.
    let project_owner: String = sqlx::query_scalar("SELECT user_id FROM projects WHERE project_id = ?")
        .bind(&project_id)
        .fetch_one(pool)
        .await
        .unwrap();
    let entry_owner: String = sqlx::query_scalar("SELECT owner_user_id FROM project_explorer WHERE pe_id = ?")
        .bind(&pe_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(project_owner, user.id);
    assert_eq!(entry_owner, user.id, "explorer entry must be adopted with its project");

    db.close().await;
}

#[tokio::test]
async fn adoption_window_closes_with_second_external_user() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();
    let repo = SqliteUserRepository::new(pool.clone());
    let (project_id, _pe_id) = seed_default_user_project(pool).await;

    let first = repo
        .ensure_external_user(
            UserType::Aionpro,
            "ext-a",
            ExternalUserProjection {
                username: Some("A".into()),
                email: None,
                avatar_path: None,
            },
        )
        .await
        .unwrap();
    let second = repo
        .ensure_external_user(
            UserType::Aionpro,
            "ext-b",
            ExternalUserProjection {
                username: Some("B".into()),
                email: None,
                avatar_path: None,
            },
        )
        .await
        .unwrap();

    // Two external users exist: neither may adopt, in either order.
    assert_eq!(repo.adopt_system_default_data(&first.id).await.unwrap(), 0);
    assert_eq!(repo.adopt_system_default_data(&second.id).await.unwrap(), 0);

    let project_owner: String = sqlx::query_scalar("SELECT user_id FROM projects WHERE project_id = ?")
        .bind(&project_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(project_owner, "system_default_user", "closed window must move nothing");

    db.close().await;
}
