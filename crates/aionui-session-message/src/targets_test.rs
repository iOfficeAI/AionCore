use super::*;

// The ranking rules of design §5.3 are asserted end-to-end against a real
// in-memory DB rather than against a pure ranking function. That is deliberate:
// the ranking now lives in the SQL query, and the bug these tests exist to
// prevent was precisely a ranking that only ever saw an already-truncated page.
// A pure-function test over a hand-supplied candidate list cannot catch that,
// because it hands the ranker the whole population by construction.

use aionui_db::models::ConversationRow;
use aionui_db::{IConversationRepository, SqliteConversationRepository, SqliteProjectStore, init_database_memory};

struct TargetsCtx {
    targets: MentionableTargets,
    repo: Arc<SqliteConversationRepository>,
}

async fn setup_targets_ctx() -> TargetsCtx {
    let db = init_database_memory().await.unwrap();
    for user in ["user_1", "user_2"] {
        sqlx::query(
            "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, 'hash', 'active', 0, 1, 1)",
        )
        .bind(user)
        .bind(user)
        .execute(db.pool())
        .await
        .unwrap();
    }
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let store: Arc<dyn aionui_db::IProjectStore> = Arc::new(SqliteProjectStore::new(db.pool().clone()));
    // Leaked so the shared in-memory pool outlives the test, as in the team
    // integration tests.
    std::mem::forget(db);
    let project_service = Arc::new(ProjectService::new(
        store,
        std::env::temp_dir().join("aionui-session-message-targets-test"),
    ));
    TargetsCtx {
        targets: MentionableTargets::new(repo.clone(), project_service),
        repo,
    }
}

/// Everything a ranking test needs to vary. Built through `Default` so each test
/// names only the fields it actually cares about.
struct NewConversation {
    user_id: &'static str,
    id: &'static str,
    name: &'static str,
    extra: &'static str,
    updated_at: TimestampMs,
    project_id: Option<&'static str>,
    pinned: bool,
}

impl Default for NewConversation {
    fn default() -> Self {
        Self {
            user_id: "user_1",
            id: "c1",
            name: "conversation",
            extra: "{}",
            updated_at: 1,
            project_id: None,
            pinned: false,
        }
    }
}

impl TargetsCtx {
    async fn insert(&self, new: NewConversation) {
        let row = ConversationRow {
            id: new.id.to_owned(),
            user_id: new.user_id.to_owned(),
            name: new.name.to_owned(),
            r#type: "acp".to_owned(),
            extra: new.extra.to_owned(),
            model: None,
            status: Some("finished".to_owned()),
            source: Some("aionui".to_owned()),
            channel_chat_id: None,
            pinned: new.pinned,
            pinned_at: None,
            created_at: 1,
            updated_at: new.updated_at,
            project_id: new.project_id.map(str::to_owned),
            folder_id: None,
            name_source: None,
        };
        self.repo.create(&row).await.unwrap();
    }

    /// Plain non-team conversation owned by `user_1`, all defaults.
    async fn insert_plain(&self, id: &'static str, name: &'static str) {
        self.insert(NewConversation {
            id,
            name,
            ..Default::default()
        })
        .await;
    }

    /// `count` filler conversations, newest first, occupying the top of the
    /// ranked order so anything else has to be lifted past them to be seen.
    async fn insert_recent_fillers(&self, count: i64) {
        for index in 0..count {
            let row = ConversationRow {
                id: format!("c_filler_{index}"),
                user_id: "user_1".to_owned(),
                name: format!("filler {index}"),
                r#type: "acp".to_owned(),
                extra: "{}".to_owned(),
                model: None,
                status: Some("finished".to_owned()),
                source: Some("aionui".to_owned()),
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                created_at: 1,
                updated_at: 1_000 + index,
                project_id: None,
                folder_id: None,
                name_source: None,
            };
            self.repo.create(&row).await.unwrap();
        }
    }

    async fn list(&self, current: &str, query: SessionMentionableQuery) -> Vec<String> {
        self.targets
            .list("user_1", current, &query)
            .await
            .unwrap()
            .items
            .into_iter()
            .map(|item| item.id)
            .collect()
    }
}

fn with_query(q: &str) -> SessionMentionableQuery {
    SessionMentionableQuery {
        q: Some(q.to_owned()),
        ..Default::default()
    }
}

// ── Ranking (design §5.3) ───────────────────────────────────────────

#[tokio::test]
async fn without_a_query_same_project_comes_first_then_modified_desc_within_each_group() {
    let ctx = setup_targets_ctx().await;
    ctx.insert(NewConversation {
        id: "c_current",
        name: "me",
        project_id: Some("proj_a"),
        ..Default::default()
    })
    .await;
    for (id, project, updated_at) in [
        ("c_other_old", Some("proj_b"), 10),
        ("c_same_old", Some("proj_a"), 20),
        ("c_other_new", Some("proj_b"), 90),
        ("c_same_new", Some("proj_a"), 30),
    ] {
        ctx.insert(NewConversation {
            id,
            name: id,
            project_id: project,
            updated_at,
            ..Default::default()
        })
        .await;
    }

    let ids = ctx.list("c_current", SessionMentionableQuery::default()).await;

    assert_eq!(ids, vec!["c_same_new", "c_same_old", "c_other_new", "c_other_old"]);
}

#[tokio::test]
async fn with_a_query_prefix_matches_outrank_contains_matches() {
    let ctx = setup_targets_ctx().await;
    ctx.insert(NewConversation {
        id: "c_current",
        name: "me",
        project_id: Some("proj_a"),
        ..Default::default()
    })
    .await;
    ctx.insert(NewConversation {
        id: "c_contains",
        name: "refactor-auth",
        project_id: Some("proj_a"),
        updated_at: 99,
        ..Default::default()
    })
    .await;
    ctx.insert(NewConversation {
        id: "c_prefix",
        name: "auth-module",
        project_id: Some("proj_b"),
        updated_at: 1,
        ..Default::default()
    })
    .await;

    let ids = ctx.list("c_current", with_query("auth")).await;

    // Prefix wins even though the contains match is same-project and far newer.
    assert_eq!(ids, vec!["c_prefix", "c_contains"]);
}

#[tokio::test]
async fn within_the_same_match_tier_same_project_then_modified_desc_applies() {
    let ctx = setup_targets_ctx().await;
    ctx.insert(NewConversation {
        id: "c_current",
        name: "me",
        project_id: Some("proj_a"),
        ..Default::default()
    })
    .await;
    ctx.insert(NewConversation {
        id: "c_other_project",
        name: "auth-b",
        project_id: Some("proj_b"),
        updated_at: 99,
        ..Default::default()
    })
    .await;
    ctx.insert(NewConversation {
        id: "c_same_project",
        name: "auth-a",
        project_id: Some("proj_a"),
        updated_at: 1,
        ..Default::default()
    })
    .await;

    let ids = ctx.list("c_current", with_query("auth")).await;

    assert_eq!(ids, vec!["c_same_project", "c_other_project"]);
}

#[tokio::test]
async fn a_query_that_matches_nothing_returns_nothing() {
    let ctx = setup_targets_ctx().await;
    ctx.insert_plain("c_auth", "auth").await;

    let ids = ctx.list("c_current", with_query("zzz")).await;

    assert!(ids.is_empty(), "{ids:?}");
}

#[tokio::test]
async fn matching_is_case_insensitive() {
    let ctx = setup_targets_ctx().await;
    ctx.insert_plain("c_auth", "refactor-Auth").await;
    // Present so the assertion proves a filter ran, rather than passing because
    // the only row in the table came back.
    ctx.insert_plain("c_unrelated", "something else").await;

    let ids = ctx.list("c_current", with_query("AUTH")).await;

    assert_eq!(ids, vec!["c_auth"]);
}

#[tokio::test]
async fn a_blank_query_is_treated_as_no_query_rather_than_matching_nothing() {
    let ctx = setup_targets_ctx().await;
    ctx.insert_plain("c_auth", "auth").await;

    let ids = ctx.list("c_current", with_query("   ")).await;

    assert_eq!(ids, vec!["c_auth"], "whitespace must not filter everything out");
}

#[tokio::test]
async fn same_project_priority_outranks_pinned() {
    // spec §5.3: same-project beats pinned. The pinned row here is also the
    // newer one, so pinned influencing the order in any way would surface it
    // first.
    let ctx = setup_targets_ctx().await;
    ctx.insert(NewConversation {
        id: "c_current",
        name: "me",
        project_id: Some("proj_a"),
        ..Default::default()
    })
    .await;
    ctx.insert(NewConversation {
        id: "c_pinned_other_project",
        name: "pinned",
        project_id: Some("proj_b"),
        updated_at: 99,
        pinned: true,
        ..Default::default()
    })
    .await;
    ctx.insert(NewConversation {
        id: "c_same_project",
        name: "same",
        project_id: Some("proj_a"),
        updated_at: 1,
        ..Default::default()
    })
    .await;

    let ids = ctx.list("c_current", SessionMentionableQuery::default()).await;

    assert_eq!(ids, vec!["c_same_project", "c_pinned_other_project"]);
}

// ── The regression these two exist for ──────────────────────────────
//
// Ranking used to run over one already-fetched page of the 20 most recently
// modified conversations, so anything below that window was unreachable no
// matter how well it matched.

#[tokio::test]
async fn a_search_reaches_a_match_far_outside_the_most_recent_page() {
    let ctx = setup_targets_ctx().await;
    ctx.insert_recent_fillers(40).await;
    // Oldest row in the table: under a page-then-rank implementation it is not
    // even a candidate.
    ctx.insert(NewConversation {
        id: "c_target",
        name: "auth rewrite",
        updated_at: 1,
        ..Default::default()
    })
    .await;

    let ids = ctx.list("c_current", with_query("auth")).await;

    assert_eq!(ids, vec!["c_target"]);
}

#[tokio::test]
async fn a_same_project_conversation_outranks_newer_rows_from_outside_the_page() {
    let ctx = setup_targets_ctx().await;
    ctx.insert(NewConversation {
        id: "c_current",
        name: "me",
        project_id: Some("proj_a"),
        updated_at: 5_000,
        ..Default::default()
    })
    .await;
    ctx.insert_recent_fillers(40).await;
    ctx.insert(NewConversation {
        id: "c_same_project_oldest",
        name: "related work",
        project_id: Some("proj_a"),
        updated_at: 1,
        ..Default::default()
    })
    .await;

    let ids = ctx.list("c_current", SessionMentionableQuery::default()).await;

    assert_eq!(
        ids.first().map(String::as_str),
        Some("c_same_project_oldest"),
        "the same-project row must be lifted above 40 newer rows: {ids:?}"
    );
}

#[tokio::test]
async fn like_metacharacters_in_a_query_match_literally() {
    // Unescaped, `%` would match every conversation and `_` any character —
    // the substring test this replaced treated both as ordinary text.
    let ctx = setup_targets_ctx().await;
    ctx.insert_plain("c_plain", "plain one").await;
    ctx.insert_plain("c_percent", "100% done").await;
    ctx.insert_plain("c_underscore", "a_b").await;
    ctx.insert_plain("c_any", "axb").await;

    assert_eq!(ctx.list("c_current", with_query("%")).await, vec!["c_percent"]);
    assert_eq!(ctx.list("c_current", with_query("a_b")).await, vec!["c_underscore"]);
}

// ── Hard filters (spec §5.3) ────────────────────────────────────────

#[tokio::test]
async fn the_list_never_contains_a_team_conversation() {
    let ctx = setup_targets_ctx().await;
    ctx.insert_plain("c_plain", "plain").await;
    ctx.insert(NewConversation {
        id: "c_team",
        name: "team chat",
        extra: r#"{"teamId":"team_1"}"#,
        ..Default::default()
    })
    .await;

    let ids = ctx.list("c_current", SessionMentionableQuery::default()).await;

    assert_eq!(ids, vec!["c_plain"], "team conversations must be hard-filtered");
}

#[tokio::test]
async fn the_list_excludes_the_current_conversation() {
    let ctx = setup_targets_ctx().await;
    ctx.insert_plain("c_current", "me").await;
    ctx.insert_plain("c_other", "them").await;

    let ids = ctx.list("c_current", SessionMentionableQuery::default()).await;

    assert_eq!(ids, vec!["c_other"]);
}

#[tokio::test]
async fn another_users_conversations_never_appear() {
    let ctx = setup_targets_ctx().await;
    ctx.insert(NewConversation {
        user_id: "user_2",
        id: "c_theirs",
        name: "theirs",
        ..Default::default()
    })
    .await;

    let ids = ctx.list("c_current", SessionMentionableQuery::default()).await;

    assert!(ids.is_empty(), "{ids:?}");
}

#[tokio::test]
async fn a_limit_above_the_cap_is_clamped_rather_than_returning_the_whole_table() {
    let ctx = setup_targets_ctx().await;
    ctx.insert_recent_fillers(60).await;

    let page = ctx
        .targets
        .list(
            "user_1",
            "c_current",
            &SessionMentionableQuery {
                limit: Some(10_000),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(page.items.len(), MAX_LIMIT as usize);
}

// ── Paging ──────────────────────────────────────────────────────────

#[tokio::test]
async fn a_page_stays_full_when_hard_filtered_rows_fill_the_scan_window() {
    // 12 team rows rank above every usable row, which is more than one scan
    // window (limit 5 + SCAN_SLACK 10 = 15) can absorb alongside a full page.
    // Before the scan loop existed, the hard filter simply shrank the page.
    let ctx = setup_targets_ctx().await;
    for index in 0..12 {
        let row = ConversationRow {
            id: format!("c_team_{index}"),
            user_id: "user_1".to_owned(),
            name: format!("team {index}"),
            r#type: "acp".to_owned(),
            extra: r#"{"teamId":"team_1"}"#.to_owned(),
            model: None,
            status: Some("finished".to_owned()),
            source: Some("aionui".to_owned()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 1,
            updated_at: 9_000 + index,
            project_id: None,
            folder_id: None,
            name_source: None,
        };
        ctx.repo.create(&row).await.unwrap();
    }
    ctx.insert_recent_fillers(6).await;

    let ids = ctx
        .list(
            "c_current",
            SessionMentionableQuery {
                limit: Some(5),
                ..Default::default()
            },
        )
        .await;

    assert_eq!(ids.len(), 5, "the page must not shrink to the survivors: {ids:?}");
    assert!(ids.iter().all(|id| id.starts_with("c_filler_")), "{ids:?}");
}

#[tokio::test]
async fn the_cursor_walks_the_whole_table_without_repeating_or_dropping_a_row() {
    let ctx = setup_targets_ctx().await;
    ctx.insert_recent_fillers(25).await;

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..5 {
        let page = ctx
            .targets
            .list(
                "user_1",
                "c_current",
                &SessionMentionableQuery {
                    limit: Some(10),
                    cursor: cursor.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        seen.extend(page.items.into_iter().map(|item| item.id));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    assert!(cursor.is_none(), "paging must terminate");
    let unique: std::collections::HashSet<&String> = seen.iter().collect();
    assert_eq!(unique.len(), seen.len(), "a row was returned twice: {seen:?}");
    assert_eq!(seen.len(), 25, "every row must be reachable: {seen:?}");
}

/// A page whose scan window is exhausted hands back no cursor, so the picker
/// stops instead of asking for a page that cannot exist.
#[tokio::test]
async fn an_exhausted_scan_hands_back_no_cursor() {
    let ctx = setup_targets_ctx().await;
    ctx.insert_plain("c_a", "a").await;
    ctx.insert(NewConversation {
        id: "c_b",
        name: "b",
        extra: r#"{"teamId":"team_1"}"#,
        updated_at: 99,
        ..Default::default()
    })
    .await;

    let page = ctx
        .targets
        .list(
            "user_1",
            "c_current",
            &SessionMentionableQuery {
                limit: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let ids: Vec<&str> = page.items.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids, vec!["c_a"]);
    assert!(page.next_cursor.is_none(), "{:?}", page.next_cursor);
}

// ── Cursor parsing ──────────────────────────────────────────────────

#[test]
fn an_absent_or_blank_cursor_starts_at_the_first_row() {
    assert_eq!(parse_cursor(None), 0);
    assert_eq!(parse_cursor(Some("   ")), 0);
}

#[test]
fn a_numeric_cursor_is_the_scan_offset() {
    assert_eq!(parse_cursor(Some("40")), 40);
}

#[test]
fn an_unparsable_cursor_restarts_rather_than_failing_the_picker() {
    // Notably what a cursor minted by the previous build looks like: it handed
    // back conversation ids, not offsets.
    assert_eq!(parse_cursor(Some("conv_019fd1e1")), 0);
}
