//! Service-level tests over a real in-memory DB (no mocks): the D11 5-case
//! classification matrix, team folding + orphan downgrade (BR-8), display-only
//! path merge (case 3, asserts no DB write), the pinned anti-join / B1
//! double-render regression, cross-user scoping (BR-24), dangling project ids,
//! keyset paging continuity, and the ScopeGone 404.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use aionui_api_types::{SidebarItem, SidebarScope};
use aionui_db::{
    Database, ISidebarStore, IUserOrderStore, OrderItemRef, OrderItemType, SqlitePool, SqliteSidebarStore,
    SqliteUserOrderStore, init_database_memory,
};
use aionui_project::canonical;
use async_trait::async_trait;
use tempfile::TempDir;

use super::{SidebarError, SidebarService};
use crate::ports::RemoveProjectPorts;

const USER: &str = "user-1";
const OTHER: &str = "user-2";

// -- Fixture -----------------------------------------------------------------

struct Fixture {
    db: Database,
    _tmp: TempDir,
    service: SidebarService,
}

impl Fixture {
    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    /// A temp-session workspace path under
    /// `work_dir/conversations/<label>-temp-<leaf>` — the same shape the write
    /// side auto-assigns (`{label}-temp-{id}`), which classifies to chats
    /// (case 5). The `-temp-` marker is required: it is what
    /// `is_temp_session_workspace` keys on to stay root-agnostic.
    fn temp_workspace(&self, leaf: &str) -> String {
        self._tmp
            .path()
            .join("conversations")
            .join(format!("acp-temp-{leaf}"))
            .display()
            .to_string()
    }
}

async fn fixture() -> Fixture {
    let db = init_database_memory().await.unwrap();
    seed_user(db.pool(), USER).await;
    seed_user(db.pool(), OTHER).await;
    let tmp = tempfile::tempdir().unwrap();
    let sidebar: Arc<dyn ISidebarStore> = Arc::new(SqliteSidebarStore::new(db.pool().clone()));
    let user_order: Arc<dyn IUserOrderStore> = Arc::new(SqliteUserOrderStore::new(db.pool().clone()));
    let service = SidebarService::new(sidebar, user_order, tmp.path().to_path_buf());
    Fixture { db, _tmp: tmp, service }
}

// -- Seed helpers (raw SQL; mirrors sqlite_sidebar_test.rs) -------------------

async fn seed_user(pool: &SqlitePool, id: &str) {
    sqlx::query("INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, 'x', 0, 0)")
        .bind(id)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn insert_conv(pool: &SqlitePool, user: &str, id: &str, project_id: Option<&str>, extra: &str, updated_at: i64) {
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, project_id, archived_at, created_at, updated_at) \
         VALUES (?, ?, ?, 'acp', ?, ?, NULL, ?, ?)",
    )
    .bind(id)
    .bind(user)
    .bind(id)
    .bind(extra)
    .bind(project_id)
    .bind(updated_at)
    .bind(updated_at)
    .execute(pool)
    .await
    .unwrap();
}

/// A conversation whose workspace lives in `extra.$.workspace`.
async fn insert_conv_ws(pool: &SqlitePool, user: &str, id: &str, workspace: &str, updated_at: i64) {
    let extra = serde_json::json!({ "workspace": workspace }).to_string();
    insert_conv(pool, user, id, None, &extra, updated_at).await;
}

/// A conversation that is a team member (`extra.$.teamId`).
async fn insert_member(pool: &SqlitePool, user: &str, id: &str, team_id: &str, updated_at: i64) {
    let extra = serde_json::json!({ "teamId": team_id }).to_string();
    insert_conv(pool, user, id, None, &extra, updated_at).await;
}

async fn insert_team(
    pool: &SqlitePool,
    user: &str,
    id: &str,
    workspace: &str,
    project_id: Option<&str>,
    updated_at: i64,
) {
    sqlx::query(
        "INSERT INTO teams (id, user_id, name, workspace, project_id, archived_at, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, NULL, ?, ?)",
    )
    .bind(id)
    .bind(user)
    .bind(id)
    .bind(workspace)
    .bind(project_id)
    .bind(updated_at)
    .bind(updated_at)
    .execute(pool)
    .await
    .unwrap();
}

/// Standard project with a workspace-root folder whose canonical is `canon`.
async fn insert_std_project(pool: &SqlitePool, user: &str, project_id: &str, name: &str, canon: &str, uri: &str) {
    sqlx::query("INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES (?, ?, ?, 'standard', 0, 0)")
        .bind(project_id)
        .bind(user)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    let folder_id = format!("f-{project_id}");
    sqlx::query("INSERT INTO folders (folder_id, resource_uri, resource_canonical, created_at, updated_at) VALUES (?, ?, ?, 0, 0)")
        .bind(&folder_id)
        .bind(uri)
        .bind(canon)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO project_explorer (pe_id, project_id, folder_id, role, order_index, created_at, updated_at) \
         VALUES (?, ?, ?, 'workspace', 0, 0, 0)",
    )
    .bind(format!("pe-{project_id}"))
    .bind(project_id)
    .bind(&folder_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_temp_project(pool: &SqlitePool, user: &str, project_id: &str) {
    sqlx::query("INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES (?, ?, 'Temp', 'temp', 0, 0)")
        .bind(project_id)
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
}

// -- Assertion helpers -------------------------------------------------------

fn canon_of(path: &str) -> String {
    let uri = canonical::to_file_uri(Path::new(path)).unwrap();
    canonical::canonicalize(&uri).unwrap().as_str().to_owned()
}

fn file_uri(path: &str) -> String {
    canonical::to_file_uri(Path::new(path)).unwrap()
}

fn conv_ids(items: &[SidebarItem]) -> Vec<String> {
    let mut ids: Vec<String> = items
        .iter()
        .filter_map(|i| match i {
            SidebarItem::Conversation { conversation } => Some(conversation.id.clone()),
            SidebarItem::Team(_) => None,
        })
        .collect();
    ids.sort();
    ids
}

fn team_ids(items: &[SidebarItem]) -> Vec<String> {
    let mut ids: Vec<String> = items
        .iter()
        .filter_map(|i| match i {
            SidebarItem::Team(t) => Some(t.team_id.clone()),
            SidebarItem::Conversation { .. } => None,
        })
        .collect();
    ids.sort();
    ids
}

fn find_project<'a>(
    resp: &'a aionui_api_types::SidebarResponse,
    project_id: &str,
) -> &'a aionui_api_types::SidebarGroup {
    resp.groups
        .iter()
        .find(|g| matches!(&g.scope, SidebarScope::Project { project_id: p, .. } if p == project_id))
        .unwrap_or_else(|| panic!("no project group {project_id}"))
}

fn find_chats(resp: &aionui_api_types::SidebarResponse) -> &aionui_api_types::SidebarGroup {
    resp.groups
        .iter()
        .find(|g| matches!(g.scope, SidebarScope::Chats))
        .expect("no chats group")
}

fn find_dir<'a>(resp: &'a aionui_api_types::SidebarResponse, name: &str) -> Option<&'a aionui_api_types::SidebarGroup> {
    resp.groups
        .iter()
        .find(|g| matches!(&g.scope, SidebarScope::Dir { name: n, .. } if n == name))
}

fn find_pinned(resp: &aionui_api_types::SidebarResponse) -> Option<&aionui_api_types::SidebarGroup> {
    resp.groups.iter().find(|g| matches!(g.scope, SidebarScope::Pinned))
}

// -- D11 classification matrix (conversations) -------------------------------

#[tokio::test]
async fn classifies_five_cases_for_conversations() {
    let fx = fixture().await;
    let pool = fx.pool();
    insert_std_project(
        pool,
        USER,
        "proj-std",
        "Std",
        &canon_of("/repo/std"),
        &file_uri("/repo/std"),
    )
    .await;
    insert_temp_project(pool, USER, "proj-temp").await;

    // case 1: bound to standard project.
    insert_conv(pool, USER, "c1", Some("proj-std"), "{}", 100).await;
    // case 2: bound to temp project => chats.
    insert_conv(pool, USER, "c2", Some("proj-temp"), "{}", 90).await;
    // case 3: unbound, workspace canonicalizes onto the standard project root.
    insert_conv_ws(pool, USER, "c3", "/repo/std", 80).await;
    // case 4: unbound, non-temp path with no matching project => pseudo-dir.
    insert_conv_ws(pool, USER, "c4", "/repo/other", 70).await;
    // case 5: unbound, temp-session workspace => chats.
    insert_conv_ws(pool, USER, "c5", &fx.temp_workspace("sess-a"), 60).await;

    let resp = fx.service.first_screen(USER, Some(50), &[]).await.unwrap();

    // case 1 + case 3 land in the same standard-project group (path merge).
    assert_eq!(conv_ids(&find_project(&resp, "proj-std").items), vec!["c1", "c3"]);
    // case 4 -> its own dir group.
    assert_eq!(
        conv_ids(&find_dir(&resp, "other").expect("dir group").items),
        vec!["c4"]
    );
    // case 2 + case 5 -> chats.
    assert_eq!(conv_ids(&find_chats(&resp).items), vec!["c2", "c5"]);
    // temp project never becomes its own project group.
    assert!(
        !resp
            .groups
            .iter()
            .any(|g| matches!(&g.scope, SidebarScope::Project { project_id, .. } if project_id == "proj-temp"))
    );
}

// Historical-debt regression: a conversation whose temp workspace was baked
// under a PREVIOUS data-dir root (user migrated their conversation directory
// across releases) must still land in chats — not spawn a pseudo-dir project
// group. Before the root-agnostic fix, the old root failed the temp check
// (it != the current work_dir) so the path fell through to `GroupKey::Dir`,
// rendering the temp session as a project. The stable `-temp-` leaf marker is
// what lets us recognize it regardless of the root prefix.
#[tokio::test]
async fn migrated_root_temp_workspace_still_classifies_to_chats() {
    let fx = fixture().await;
    let pool = fx.pool();

    // Absolute temp path under a DIFFERENT (older) root than the fixture
    // work_dir, carrying the `-temp-` leaf marker.
    let migrated = "/old-data/aionui/conversations/users/u1/2025/01/02/acp-temp-migrated";
    insert_conv_ws(pool, USER, "c-mig", migrated, 60).await;

    let resp = fx.service.first_screen(USER, Some(50), &[]).await.unwrap();

    assert_eq!(conv_ids(&find_chats(&resp).items), vec!["c-mig"]);
    assert!(
        !resp.groups.iter().any(|g| matches!(g.scope, SidebarScope::Dir { .. })),
        "migrated temp workspace must not spawn a pseudo-dir group"
    );
}

#[tokio::test]
async fn path_merge_does_not_write_the_db() {
    let fx = fixture().await;
    let pool = fx.pool();
    insert_std_project(
        pool,
        USER,
        "proj-std",
        "Std",
        &canon_of("/repo/std"),
        &file_uri("/repo/std"),
    )
    .await;
    insert_conv_ws(pool, USER, "c3", "/repo/std", 80).await;

    let resp = fx.service.first_screen(USER, Some(50), &[]).await.unwrap();
    assert_eq!(
        conv_ids(&find_project(&resp, "proj-std").items),
        vec!["c3"],
        "displayed under the project"
    );

    // BR-17/27: the merge is display-only; project_id stays NULL on disk.
    let pid: Option<String> = sqlx::query_scalar("SELECT project_id FROM conversations WHERE id = 'c3'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(pid, None, "path merge must not persist a project binding");
}

#[tokio::test]
async fn dangling_project_id_falls_through_to_path() {
    let fx = fixture().await;
    let pool = fx.pool();
    insert_std_project(
        pool,
        USER,
        "proj-std",
        "Std",
        &canon_of("/repo/std"),
        &file_uri("/repo/std"),
    )
    .await;
    // project_id points at a project that does not exist -> treat as NULL, then
    // classify by workspace path (which merges onto proj-std).
    let extra = serde_json::json!({ "workspace": "/repo/std" }).to_string();
    insert_conv(pool, USER, "ghost", Some("no-such-proj"), &extra, 80).await;

    let resp = fx.service.first_screen(USER, Some(50), &[]).await.unwrap();
    assert_eq!(conv_ids(&find_project(&resp, "proj-std").items), vec!["ghost"]);
}

// -- Teams: isomorphic classification + folding + orphan downgrade -----------

#[tokio::test]
async fn teams_classify_and_fold_members_orphans_downgrade() {
    let fx = fixture().await;
    let pool = fx.pool();
    insert_std_project(
        pool,
        USER,
        "proj-std",
        "Std",
        &canon_of("/repo/std"),
        &file_uri("/repo/std"),
    )
    .await;

    // Live team bound to the standard project.
    insert_team(pool, USER, "T1", "", Some("proj-std"), 200).await;
    // Two members of T1: folded into the team row, not independent.
    insert_member(pool, USER, "m1", "T1", 150).await;
    insert_member(pool, USER, "m2", "T1", 160).await;
    // Orphan member: teamId names no live team -> downgrades to an independent
    // conversation, classified by its own (temp) workspace => chats (BR-8).
    let orphan_extra = serde_json::json!({ "teamId": "ghost-team", "workspace": fx.temp_workspace("o") }).to_string();
    insert_conv(pool, USER, "orphan", None, &orphan_extra, 140).await;

    let resp = fx.service.first_screen(USER, Some(50), &[]).await.unwrap();

    let proj = find_project(&resp, "proj-std");
    assert_eq!(team_ids(&proj.items), vec!["T1"], "team lands in the project group");
    assert!(
        conv_ids(&proj.items).is_empty(),
        "folded members are not independent rows"
    );

    // T1 aggregates its members (created_at asc); orphan is not a member.
    let team = proj
        .items
        .iter()
        .find_map(|i| match i {
            SidebarItem::Team(t) => Some(t),
            _ => None,
        })
        .unwrap();
    assert_eq!(team.member_conversation_ids, vec!["m1", "m2"]);

    // Orphan surfaces as an independent conversation in chats.
    assert_eq!(conv_ids(&find_chats(&resp).items), vec!["orphan"]);
}

// -- Pinned anti-join / B1 double-render -------------------------------------

#[tokio::test]
async fn pinned_item_leaves_its_natural_group() {
    let fx = fixture().await;
    let pool = fx.pool();
    insert_conv_ws(pool, USER, "c4", "/repo/other", 70).await;

    // Before pin: c4 lives in its dir group, no pinned group.
    let before = fx.service.first_screen(USER, Some(50), &[]).await.unwrap();
    assert!(find_pinned(&before).is_none());
    assert_eq!(conv_ids(&find_dir(&before, "other").unwrap().items), vec!["c4"]);

    fx.service.pin(USER, "pinned", "conversation", "c4").await.unwrap();

    let after = fx.service.first_screen(USER, Some(50), &[]).await.unwrap();
    // Pinned group carries c4 with pinned=true.
    let pinned = find_pinned(&after).expect("pinned group present");
    assert_eq!(conv_ids(&pinned.items), vec!["c4"]);
    let is_pinned = pinned.items.iter().any(
        |i| matches!(i, SidebarItem::Conversation { conversation } if conversation.id == "c4" && conversation.pinned),
    );
    assert!(is_pinned, "DTO pinned flag overridden to true");
    // B1: c4 no longer double-renders in its natural (dir) group. The dir group
    // may still surface (its pinned member keeps contributing latest_activity),
    // but must not re-list c4 among its items.
    let dir_convs = find_dir(&after, "other")
        .map(|g| conv_ids(&g.items))
        .unwrap_or_default();
    assert!(
        !dir_convs.contains(&"c4".to_owned()),
        "pinned item must not appear in its natural group"
    );
}

#[tokio::test]
async fn pin_unpin_is_idempotent_and_validated() {
    let fx = fixture().await;
    insert_conv_ws(fx.pool(), USER, "c1", "/repo/other", 70).await;

    // Unknown scene / item_type -> 400, never a silent no-op.
    assert!(matches!(
        fx.service.pin(USER, "bogus", "conversation", "c1").await,
        Err(SidebarError::BadRequest(_))
    ));
    assert!(matches!(
        fx.service.pin(USER, "pinned", "bogus", "c1").await,
        Err(SidebarError::BadRequest(_))
    ));

    // Two pins collapse to one row.
    fx.service.pin(USER, "pinned", "conversation", "c1").await.unwrap();
    fx.service.pin(USER, "pinned", "conversation", "c1").await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_order WHERE user_id = ? AND item_id = 'c1'")
        .bind(USER)
        .fetch_one(fx.pool())
        .await
        .unwrap();
    assert_eq!(count, 1, "pin is idempotent");

    // Unpin removes; unpinning again is a no-op.
    fx.service.unpin(USER, "pinned", "conversation", "c1").await.unwrap();
    fx.service.unpin(USER, "pinned", "conversation", "c1").await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_order WHERE user_id = ? AND item_id = 'c1'")
        .bind(USER)
        .fetch_one(fx.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn pinned_group_hides_a_conversation_that_became_a_live_team_member() {
    // Path-4 read-side defense (design §4.3): if a pinned conversation is a live
    // team member (folded into its team), it must not also surface as an
    // independent row in the pinned group. The pinned row still exists on disk —
    // this is read-side defense, not a table-level cascade.
    let fx = fixture().await;
    let pool = fx.pool();
    insert_team(pool, USER, "T1", "", None, 200).await;
    insert_member(pool, USER, "m1", "T1", 150).await;

    // Pin the member conversation directly (simulating dirty data / a future
    // write path — the normal UI never exposes a member as an independent row).
    fx.service.pin(USER, "pinned", "conversation", "m1").await.unwrap();
    let row_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_order WHERE user_id = ? AND item_id = 'm1'")
        .bind(USER)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(row_count, 1, "the pinned row is present on disk");

    let resp = fx.service.first_screen(USER, Some(50), &[]).await.unwrap();
    let pinned = find_pinned(&resp);
    let pinned_convs = pinned.map(|g| conv_ids(&g.items)).unwrap_or_default();
    assert!(
        !pinned_convs.contains(&"m1".to_owned()),
        "a live team member must not render as an independent pinned row"
    );
}

// -- Cross-user scoping (BR-24) ----------------------------------------------

#[tokio::test]
async fn first_screen_is_user_scoped() {
    let fx = fixture().await;
    let pool = fx.pool();
    // Another user owns a project + conversation on a workspace USER also uses.
    insert_std_project(
        pool,
        OTHER,
        "proj-foreign",
        "Foreign",
        &canon_of("/repo/shared"),
        &file_uri("/repo/shared"),
    )
    .await;
    insert_conv(pool, OTHER, "foreign-c", Some("proj-foreign"), "{}", 100).await;
    // USER's own single conversation.
    insert_conv_ws(pool, USER, "mine", "/repo/mine", 50).await;

    let resp = fx.service.first_screen(USER, Some(50), &[]).await.unwrap();
    assert!(
        !resp
            .groups
            .iter()
            .any(|g| matches!(&g.scope, SidebarScope::Project { project_id, .. } if project_id == "proj-foreign"))
    );
    let all_conv: Vec<String> = resp.groups.iter().flat_map(|g| conv_ids(&g.items)).collect();
    assert_eq!(all_conv, vec!["mine"], "no cross-user leakage");
}

// -- Keyset paging continuity ------------------------------------------------

#[tokio::test]
async fn chats_paging_is_continuous_no_dup_no_miss() {
    let fx = fixture().await;
    let pool = fx.pool();
    // Five chats conversations (temp workspaces), distinct updated_at.
    for (i, ts) in [("a", 50), ("b", 40), ("c", 30), ("d", 20), ("e", 10)] {
        insert_conv_ws(pool, USER, i, &fx.temp_workspace(i), ts).await;
    }

    // First screen with a window of 2 over chats.
    let resp = fx.service.first_screen(USER, Some(2), &[]).await.unwrap();
    let chats = find_chats(&resp);
    assert!(chats.has_more);
    let mut seen = conv_ids(&chats.items);
    assert_eq!(seen.len(), 2);
    let mut cursor = chats.next_cursor.clone();

    // Page the rest via the items endpoint.
    while let Some(c) = cursor {
        let page = fx.service.items(USER, "chats", Some(&c), Some(2)).await.unwrap();
        seen.extend(conv_ids(&page.items));
        cursor = page.next_cursor;
        if !page.has_more {
            break;
        }
    }
    seen.sort();
    assert_eq!(seen, vec!["a", "b", "c", "d", "e"], "every chat seen exactly once");
}

// -- ScopeGone 404 -----------------------------------------------------------

#[tokio::test]
async fn items_on_missing_scope_is_scope_gone() {
    let fx = fixture().await;
    insert_conv_ws(fx.pool(), USER, "c1", "/repo/other", 70).await;

    let err = fx
        .service
        .items(USER, "project:no-such", None, Some(10))
        .await
        .unwrap_err();
    assert!(matches!(err, SidebarError::ScopeGone), "stale project scope -> 404");

    // A syntactically bad scope is a 400, not a 404.
    let err = fx.service.items(USER, "bogus", None, Some(10)).await.unwrap_err();
    assert!(matches!(err, SidebarError::BadRequest(_)));
}

// -- remove_project (BR-19 / D13 "所见即所删") --------------------------------

/// A `RemoveProjectPorts` that performs the same real deletes production does
/// (conversation row + its `user_order` rows via the same store; team + folded
/// members; project record), so a test can assert both the returned counts and
/// the on-disk orphan cleanup. `fail` ids/records make a single port call error
/// to exercise best-effort orchestration.
struct FakePorts {
    pool: SqlitePool,
    user_order: Arc<dyn IUserOrderStore>,
    fail: HashSet<String>,
    deleted_convs: Mutex<Vec<String>>,
    deleted_teams: Mutex<Vec<String>>,
    project_deleted: Mutex<bool>,
}

impl FakePorts {
    fn new(pool: SqlitePool, user_order: Arc<dyn IUserOrderStore>, fail: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            pool,
            user_order,
            fail: fail.iter().map(|s| s.to_string()).collect(),
            deleted_convs: Mutex::new(Vec::new()),
            deleted_teams: Mutex::new(Vec::new()),
            project_deleted: Mutex::new(false),
        })
    }
}

#[async_trait]
impl RemoveProjectPorts for FakePorts {
    async fn delete_conversation(&self, user_id: &str, conversation_id: &str) -> Result<(), String> {
        if self.fail.contains(conversation_id) {
            return Err(format!("forced failure deleting {conversation_id}"));
        }
        sqlx::query("DELETE FROM conversations WHERE id = ? AND user_id = ?")
            .bind(conversation_id)
            .bind(user_id)
            .execute(&self.pool)
            .await
            .unwrap();
        // Mirror the production path-1 cascade so orphan rows are cleaned up.
        self.user_order
            .remove_item(
                user_id,
                &OrderItemRef::new(OrderItemType::Conversation, conversation_id),
            )
            .await
            .unwrap();
        self.deleted_convs.lock().unwrap().push(conversation_id.to_owned());
        Ok(())
    }

    async fn remove_team(&self, user_id: &str, team_id: &str) -> Result<(), String> {
        if self.fail.contains(team_id) {
            return Err(format!("forced failure removing team {team_id}"));
        }
        // Member conversations are folded into the team and go with it.
        let members: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM conversations WHERE user_id = ? AND json_extract(extra, '$.teamId') = ?",
        )
        .bind(user_id)
        .bind(team_id)
        .fetch_all(&self.pool)
        .await
        .unwrap();
        for member in &members {
            sqlx::query("DELETE FROM conversations WHERE id = ?")
                .bind(member)
                .execute(&self.pool)
                .await
                .unwrap();
            self.user_order
                .remove_item(user_id, &OrderItemRef::new(OrderItemType::Conversation, member))
                .await
                .unwrap();
        }
        sqlx::query("DELETE FROM teams WHERE id = ? AND user_id = ?")
            .bind(team_id)
            .bind(user_id)
            .execute(&self.pool)
            .await
            .unwrap();
        self.user_order
            .remove_item(user_id, &OrderItemRef::new(OrderItemType::Team, team_id))
            .await
            .unwrap();
        self.deleted_teams.lock().unwrap().push(team_id.to_owned());
        Ok(())
    }

    async fn delete_project_record(&self, user_id: &str, project_id: &str) -> Result<(), String> {
        if self.fail.contains(project_id) {
            return Err(format!("forced failure deleting project {project_id}"));
        }
        sqlx::query("DELETE FROM project_explorer WHERE project_id = ?")
            .bind(project_id)
            .execute(&self.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM projects WHERE project_id = ? AND user_id = ?")
            .bind(project_id)
            .bind(user_id)
            .execute(&self.pool)
            .await
            .unwrap();
        *self.project_deleted.lock().unwrap() = true;
        Ok(())
    }
}

/// A `user_order` store over the same pool the service uses — `SqliteUserOrderStore`
/// is stateless over the pool, so a fresh instance behaves identically.
fn uo_store(pool: &SqlitePool) -> Arc<dyn IUserOrderStore> {
    Arc::new(SqliteUserOrderStore::new(pool.clone()))
}

async fn conv_exists(pool: &SqlitePool, id: &str) -> bool {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    n > 0
}

async fn user_order_count(pool: &SqlitePool, user: &str, item_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM user_order WHERE user_id = ? AND item_id = ?")
        .bind(user)
        .bind(item_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The delete set is the render construct: the bound conversation, the
/// path-merged unbound conversation, and the bound team (with its folded
/// members) all go; unrelated dir/chats rows stay. Pinned rows are included
/// (pinning only hoists display) and their `user_order` rows are cleaned up.
#[tokio::test]
async fn remove_project_deletes_the_visible_construct_and_orphan_rows() {
    let fx = fixture().await;
    let pool = fx.pool();
    insert_std_project(
        pool,
        USER,
        "proj-std",
        "Std",
        &canon_of("/repo/std"),
        &file_uri("/repo/std"),
    )
    .await;

    insert_conv(pool, USER, "c1", Some("proj-std"), "{}", 100).await; // bound (case 1)
    insert_conv_ws(pool, USER, "c3", "/repo/std", 90).await; // path-merged (case 3)
    insert_team(pool, USER, "T1", "", Some("proj-std"), 200).await; // bound team
    insert_member(pool, USER, "m1", "T1", 150).await;
    insert_member(pool, USER, "m2", "T1", 160).await;
    insert_conv_ws(pool, USER, "c4", "/repo/other", 70).await; // dir group -> keep
    insert_conv_ws(pool, USER, "c5", &fx.temp_workspace("s"), 60).await; // chats -> keep

    // Pin a conversation and the team so we can assert orphan cleanup.
    fx.service.pin(USER, "pinned", "conversation", "c1").await.unwrap();
    fx.service.pin(USER, "pinned", "team", "T1").await.unwrap();

    let ports = FakePorts::new(pool.clone(), uo_store(pool), &[]);
    fx.service.set_remove_project_ports(ports.clone());

    // Dry run reports the set without touching anything.
    let preview = fx.service.remove_project(USER, "proj-std", true).await.unwrap();
    assert_eq!(preview.teams_deleted, 1);
    assert_eq!(preview.conversations_deleted, 2, "c1 + c3");
    assert!(conv_exists(pool, "c1").await, "dry run must not delete");
    assert_eq!(ports.deleted_convs.lock().unwrap().len(), 0);

    // The preview names the delete set with pinned flags so the confirm dialog can
    // list *which* items go. Pinned members (c1, T1) were hoisted into the top
    // pinned group (B1 anti-join) — the frontend cannot reconstruct them, so the
    // names must ride the preview.
    let team_items: Vec<_> = preview
        .items
        .iter()
        .filter(|i| i.kind == aionui_api_types::RemoveProjectItemKind::Team)
        .collect();
    assert_eq!(team_items.len(), 1);
    assert_eq!(team_items[0].name, "T1");
    assert!(team_items[0].pinned, "T1 was pinned");

    let mut conv_items: Vec<_> = preview
        .items
        .iter()
        .filter(|i| i.kind == aionui_api_types::RemoveProjectItemKind::Conversation)
        .collect();
    conv_items.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(
        conv_items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
        ["c1", "c3"]
    );
    let c1 = conv_items.iter().find(|i| i.name == "c1").unwrap();
    let c3 = conv_items.iter().find(|i| i.name == "c3").unwrap();
    assert!(c1.pinned, "c1 was pinned");
    assert!(!c3.pinned, "c3 was not pinned");

    // Live delete matches the preview exactly.
    let result = fx.service.remove_project(USER, "proj-std", false).await.unwrap();
    assert_eq!(result.teams_deleted, preview.teams_deleted);
    assert_eq!(result.conversations_deleted, preview.conversations_deleted);
    assert!(
        result.items.is_empty(),
        "live delete omits the name list (preview already showed it)"
    );

    // The whole visible construct is gone: bound conv, merged conv, team, members.
    assert!(!conv_exists(pool, "c1").await);
    assert!(!conv_exists(pool, "c3").await);
    assert!(!conv_exists(pool, "m1").await);
    assert!(!conv_exists(pool, "m2").await);
    let team_left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM teams WHERE id = 'T1'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(team_left, 0);
    let proj_left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE project_id = 'proj-std'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(proj_left, 0, "project record removed");

    // Unrelated rows survive.
    assert!(conv_exists(pool, "c4").await, "dir-group conv untouched");
    assert!(conv_exists(pool, "c5").await, "chats conv untouched");

    // Orphan cleanup: no dangling user_order rows for the removed conv/team.
    assert_eq!(user_order_count(pool, USER, "c1").await, 0, "pinned conv row gone");
    assert_eq!(user_order_count(pool, USER, "T1").await, 0, "pinned team row gone");
}

/// A failing port call does not abort the sweep: siblings still delete and the
/// reported count reflects only the successes.
#[tokio::test]
async fn remove_project_is_best_effort_on_port_failure() {
    let fx = fixture().await;
    let pool = fx.pool();
    insert_std_project(
        pool,
        USER,
        "proj-std",
        "Std",
        &canon_of("/repo/std"),
        &file_uri("/repo/std"),
    )
    .await;
    insert_conv(pool, USER, "c1", Some("proj-std"), "{}", 100).await;
    insert_conv(pool, USER, "c2", Some("proj-std"), "{}", 90).await;

    // Deleting c1 fails; c2 must still be removed.
    let ports = FakePorts::new(pool.clone(), uo_store(pool), &["c1"]);
    fx.service.set_remove_project_ports(ports.clone());

    let result = fx.service.remove_project(USER, "proj-std", false).await.unwrap();
    assert_eq!(result.conversations_deleted, 1, "only c2 counted");
    assert!(conv_exists(pool, "c1").await, "failed delete left c1 in place");
    assert!(!conv_exists(pool, "c2").await, "sibling delete still ran");
    assert!(
        *ports.project_deleted.lock().unwrap(),
        "project record delete still attempted"
    );
}

/// A missing or non-standard target is a 404 (`ScopeGone`), before any port is
/// touched.
#[tokio::test]
async fn remove_project_on_missing_or_nonstandard_scope_is_scope_gone() {
    let fx = fixture().await;
    let pool = fx.pool();
    insert_temp_project(pool, USER, "proj-temp").await;

    let ports = FakePorts::new(pool.clone(), uo_store(pool), &[]);
    fx.service.set_remove_project_ports(ports.clone());

    // Unknown project id.
    let err = fx.service.remove_project(USER, "no-such", false).await.unwrap_err();
    assert!(matches!(err, SidebarError::ScopeGone));
    // A temp project is not a removable standard-project scope.
    let err = fx.service.remove_project(USER, "proj-temp", false).await.unwrap_err();
    assert!(matches!(err, SidebarError::ScopeGone));

    // Neither attempt invoked a port.
    assert!(ports.deleted_convs.lock().unwrap().is_empty());
    assert!(!*ports.project_deleted.lock().unwrap());
}
