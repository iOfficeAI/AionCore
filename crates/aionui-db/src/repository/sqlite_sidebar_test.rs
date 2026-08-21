use super::SqliteSidebarStore;
use crate::init_database_memory;
use crate::repository::sidebar::{ArchiveScope, ISidebarStore};
use sqlx::SqlitePool;

const USER: &str = "user-1";
const OTHER_USER: &str = "user-2";

async fn store() -> (SqliteSidebarStore, crate::Database) {
    let db = init_database_memory().await.unwrap();
    seed_user(db.pool(), USER).await;
    seed_user(db.pool(), OTHER_USER).await;
    let store = SqliteSidebarStore::new(db.pool().clone());
    (store, db)
}

async fn seed_user(pool: &SqlitePool, id: &str) {
    sqlx::query("INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, 'x', 0, 0)")
        .bind(id)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

/// Insert a conversation with explicit project_id, extra json, and archived_at.
#[allow(clippy::too_many_arguments)]
async fn insert_conv(
    pool: &SqlitePool,
    user: &str,
    id: &str,
    project_id: Option<&str>,
    extra: &str,
    updated_at: i64,
    archived_at: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, project_id, archived_at, created_at, updated_at) \
         VALUES (?, ?, ?, 'chat', ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(user)
    .bind(id)
    .bind(extra)
    .bind(project_id)
    .bind(archived_at)
    .bind(updated_at)
    .bind(updated_at)
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_team(
    pool: &SqlitePool,
    user: &str,
    id: &str,
    workspace: &str,
    project_id: Option<&str>,
    updated_at: i64,
    archived_at: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO teams (id, user_id, name, workspace, project_id, archived_at, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(user)
    .bind(id)
    .bind(workspace)
    .bind(project_id)
    .bind(archived_at)
    .bind(updated_at)
    .bind(updated_at)
    .execute(pool)
    .await
    .unwrap();
}

/// Insert a project plus its workspace-root folder + explorer entry.
#[allow(clippy::too_many_arguments)]
async fn insert_project_with_workspace(
    pool: &SqlitePool,
    user: &str,
    project_id: &str,
    name: &str,
    kind: &str,
    folder_id: &str,
    canonical: &str,
    uri: &str,
) {
    sqlx::query(
        "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES (?, ?, ?, ?, 0, 0)",
    )
    .bind(project_id)
    .bind(user)
    .bind(name)
    .bind(kind)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO folders (folder_id, resource_uri, resource_canonical, created_at, updated_at) VALUES (?, ?, ?, 0, 0)",
    )
    .bind(folder_id)
    .bind(uri)
    .bind(canonical)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO project_explorer (pe_id, project_id, folder_id, role, order_index, created_at, updated_at) \
         VALUES (?, ?, ?, 'workspace', 0, 0, 0)",
    )
    .bind(format!("pe-{project_id}"))
    .bind(project_id)
    .bind(folder_id)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn thin_conversations_extract_fields_and_fold_blanks() {
    let (store, db) = store().await;
    let pool = db.pool();
    // Bound conversation, has workspace + teamId.
    insert_conv(
        pool,
        USER,
        "c1",
        Some("proj-A"),
        r#"{"workspace":"/repo/a","teamId":"t9"}"#,
        100,
        None,
    )
    .await;
    // Unbound (empty project_id folds to None), blank workspace + whitespace teamId fold to None.
    insert_conv(
        pool,
        USER,
        "c2",
        Some(""),
        r#"{"workspace":"","teamId":"  "}"#,
        50,
        None,
    )
    .await;
    // No extra keys at all.
    insert_conv(pool, USER, "c3", None, "{}", 10, None).await;

    let mut rows = store.list_conversations_thin(USER, ArchiveScope::Active).await.unwrap();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(rows.len(), 3);

    assert_eq!(rows[0].id, "c1");
    assert_eq!(rows[0].project_id.as_deref(), Some("proj-A"));
    assert_eq!(rows[0].workspace.as_deref(), Some("/repo/a"));
    assert_eq!(rows[0].team_id.as_deref(), Some("t9"));
    assert_eq!(rows[0].updated_at, 100);

    assert_eq!(rows[1].id, "c2");
    assert_eq!(rows[1].project_id, None, "empty project_id folds to None");
    assert_eq!(rows[1].workspace, None, "empty workspace folds to None");
    assert_eq!(rows[1].team_id, None, "whitespace teamId folds to None");

    assert_eq!(rows[2].id, "c3");
    assert_eq!(rows[2].workspace, None);
    assert_eq!(rows[2].team_id, None);
}

#[tokio::test]
async fn thin_conversations_exclude_archived_and_other_users() {
    let (store, db) = store().await;
    let pool = db.pool();
    insert_conv(pool, USER, "live", None, "{}", 10, None).await;
    insert_conv(pool, USER, "archived", None, "{}", 20, Some(999)).await;
    insert_conv(pool, OTHER_USER, "foreign", None, "{}", 30, None).await;

    let active = store.list_conversations_thin(USER, ArchiveScope::Active).await.unwrap();
    let ids: Vec<&str> = active.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["live"], "archived and cross-user rows excluded");

    // The scope flip selects the complementary slice: same user, archived only.
    let archived = store
        .list_conversations_thin(USER, ArchiveScope::Archived)
        .await
        .unwrap();
    let ids: Vec<&str> = archived.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["archived"],
        "archived scope selects only archived, still user-scoped"
    );
}

#[tokio::test]
async fn thin_teams_fold_blank_workspace_and_exclude_archived() {
    let (store, db) = store().await;
    let pool = db.pool();
    insert_team(pool, USER, "t1", "/team/ws", Some("proj-A"), 100, None).await;
    insert_team(pool, USER, "t2", "", None, 50, None).await;
    insert_team(pool, USER, "t3", "/x", None, 10, Some(1)).await;

    let mut rows = store.list_teams_thin(USER, ArchiveScope::Active).await.unwrap();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["t1", "t2"], "archived team excluded");
    assert_eq!(rows[0].workspace.as_deref(), Some("/team/ws"));
    assert_eq!(rows[0].project_id.as_deref(), Some("proj-A"));
    assert_eq!(rows[1].workspace, None, "empty team workspace folds to None");

    // Scope flip: only the archived team, still user-scoped.
    let archived = store.list_teams_thin(USER, ArchiveScope::Archived).await.unwrap();
    let ids: Vec<&str> = archived.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["t3"], "archived scope selects only archived team");
}

#[tokio::test]
async fn list_user_projects_joins_workspace_folder_and_carries_kind() {
    let (store, db) = store().await;
    let pool = db.pool();
    insert_project_with_workspace(
        pool,
        USER,
        "proj-A",
        "Alpha",
        "standard",
        "f-A",
        "file:///repo/a",
        "file:///repo/a",
    )
    .await;
    // Temp project with no workspace entry: still enumerated, canonical/uri None.
    sqlx::query("INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('proj-B', ?, 'Beta', 'temp', 7, 0)")
        .bind(USER)
        .execute(pool)
        .await
        .unwrap();

    let metas = store.list_user_projects(USER).await.unwrap();
    assert_eq!(metas.len(), 2, "both the user's projects enumerated");

    let a = metas.iter().find(|m| m.project_id == "proj-A").unwrap();
    assert_eq!(a.name, "Alpha");
    assert_eq!(a.kind, "standard");
    assert_eq!(a.workspace_canonical.as_deref(), Some("file:///repo/a"));

    let b = metas.iter().find(|m| m.project_id == "proj-B").unwrap();
    assert_eq!(b.kind, "temp", "temp kind carried for case-2 classification");
    assert_eq!(b.workspace_canonical, None, "no workspace entry => None");
    assert_eq!(b.created_at, 7, "created_at carried for empty-group ordering");
}

#[tokio::test]
async fn list_user_projects_is_user_scoped() {
    let (store, db) = store().await;
    let pool = db.pool();
    // OTHER_USER owns a project whose workspace canonical USER might also point at.
    insert_project_with_workspace(
        pool,
        OTHER_USER,
        "proj-foreign",
        "Foreign",
        "standard",
        "f-F",
        "file:///repo/shared",
        "file:///repo/shared",
    )
    .await;
    insert_project_with_workspace(
        pool,
        USER,
        "proj-mine",
        "Mine",
        "standard",
        "f-M",
        "file:///repo/mine",
        "file:///repo/mine",
    )
    .await;

    let metas = store.list_user_projects(USER).await.unwrap();
    let ids: Vec<&str> = metas.iter().map(|m| m.project_id.as_str()).collect();
    assert_eq!(ids, vec!["proj-mine"], "another user's project never surfaces (BR-24)");
}

#[tokio::test]
async fn hydrate_conversations_is_scoped_and_batched() {
    let (store, db) = store().await;
    let pool = db.pool();
    insert_conv(pool, USER, "c1", Some("proj-A"), r#"{"workspace":"/a"}"#, 100, None).await;
    insert_conv(pool, USER, "c2", None, "{}", 50, None).await;
    insert_conv(pool, USER, "archived", None, "{}", 40, Some(1)).await;
    insert_conv(pool, OTHER_USER, "foreign", None, "{}", 30, None).await;

    let ids_arg = ["c1".to_string(), "c2".into(), "archived".into(), "foreign".into()];
    let rows = store
        .hydrate_conversations(USER, &ids_arg, ArchiveScope::Active)
        .await
        .unwrap();
    let mut ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, vec!["c1", "c2"], "archived + cross-user dropped from window");
    let c1 = rows.iter().find(|r| r.id == "c1").unwrap();
    assert_eq!(c1.project_id.as_deref(), Some("proj-A"));

    // Same id set under the archived scope hydrates only the archived row.
    let rows = store
        .hydrate_conversations(USER, &ids_arg, ArchiveScope::Archived)
        .await
        .unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["archived"], "archived scope hydrates only archived rows");
}

#[tokio::test]
async fn empty_inputs_short_circuit() {
    let (store, _db) = store().await;
    assert!(
        store.list_user_projects(USER).await.unwrap().is_empty(),
        "no projects => empty"
    );
    assert!(
        store
            .hydrate_conversations(USER, &[], ArchiveScope::Active)
            .await
            .unwrap()
            .is_empty()
    );
}
