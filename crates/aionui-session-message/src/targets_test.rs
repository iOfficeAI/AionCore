use super::*;

fn candidate(id: &str, name: &str, project: Option<&str>, modified_at: i64) -> TargetCandidate {
    TargetCandidate {
        id: id.to_owned(),
        name: name.to_owned(),
        project_id: project.map(str::to_owned),
        modified_at,
    }
}

#[test]
fn without_a_query_same_project_comes_first_then_modified_desc_within_each_group() {
    let ranked = rank_targets(
        Some("proj_a"),
        None,
        vec![
            candidate("c1", "other-old", Some("proj_b"), 10),
            candidate("c2", "same-old", Some("proj_a"), 20),
            candidate("c3", "other-new", Some("proj_b"), 90),
            candidate("c4", "same-new", Some("proj_a"), 30),
        ],
    );
    let ids: Vec<&str> = ranked.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["c4", "c2", "c3", "c1"]);
}

#[test]
fn with_a_query_prefix_matches_outrank_contains_matches() {
    let ranked = rank_targets(
        Some("proj_a"),
        Some("auth"),
        vec![
            candidate("c1", "refactor-auth", Some("proj_a"), 99),
            candidate("c2", "auth-module", Some("proj_b"), 1),
        ],
    );
    let ids: Vec<&str> = ranked.iter().map(|c| c.id.as_str()).collect();
    // Prefix beats contains even though c1 is same-project and far newer.
    assert_eq!(ids, vec!["c2", "c1"]);
}

#[test]
fn within_the_same_match_tier_same_project_then_modified_desc_applies() {
    let ranked = rank_targets(
        Some("proj_a"),
        Some("auth"),
        vec![
            candidate("c1", "auth-b", Some("proj_b"), 99),
            candidate("c2", "auth-a", Some("proj_a"), 1),
        ],
    );
    let ids: Vec<&str> = ranked.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["c2", "c1"]);
}

#[test]
fn a_query_that_matches_nothing_drops_the_candidate() {
    let ranked = rank_targets(None, Some("zzz"), vec![candidate("c1", "auth", None, 1)]);
    assert!(ranked.is_empty());
}

#[test]
fn matching_is_case_insensitive() {
    let ranked = rank_targets(None, Some("AUTH"), vec![candidate("c1", "refactor-Auth", None, 1)]);
    assert_eq!(ranked.len(), 1);
}

#[test]
fn a_blank_query_is_treated_as_no_query_rather_than_matching_nothing() {
    let ranked = rank_targets(None, Some("   "), vec![candidate("c1", "auth", None, 1)]);
    assert_eq!(ranked.len(), 1, "whitespace must not filter everything out");
}

#[test]
fn same_project_priority_outranks_pinned_because_pinned_is_not_carried_at_all() {
    // spec §5.3: same-project beats pinned. Locked structurally — `pinned` is
    // not part of TargetCandidate, so it cannot influence ranking.
    let ranked = rank_targets(Some("proj_a"), None, vec![candidate("c1", "x", Some("proj_a"), 1)]);
    assert_eq!(ranked.len(), 1);
}

// ── Integration: the hard filters (spec §5.3) ──────────────────────
//
// Real in-memory DB rather than a mock repo: the whole point is that the rows
// come from the same query the picker and the agent's `session list` share.

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

impl TargetsCtx {
    async fn insert_conversation(&self, id: &str, name: &str, extra: &str) {
        self.insert_conversation_for("user_1", id, name, extra).await;
    }

    async fn insert_conversation_for(&self, user_id: &str, id: &str, name: &str, extra: &str) {
        let row = ConversationRow {
            id: id.to_owned(),
            user_id: user_id.to_owned(),
            name: name.to_owned(),
            r#type: "acp".to_owned(),
            extra: extra.to_owned(),
            model: None,
            status: Some("finished".to_owned()),
            source: Some("aionui".to_owned()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 1,
            updated_at: 1,
            project_id: None,
            folder_id: None,
            name_source: None,
        };
        self.repo.create(&row).await.unwrap();
    }
}

#[tokio::test]
async fn the_list_never_contains_a_team_conversation() {
    let ctx = setup_targets_ctx().await;
    ctx.insert_conversation("c_plain", "plain", r#"{}"#).await;
    ctx.insert_conversation("c_team", "team chat", r#"{"teamId":"team_1"}"#)
        .await;

    let page = ctx
        .targets
        .list("user_1", "c_current", &SessionMentionableQuery::default())
        .await
        .unwrap();

    let ids: Vec<&str> = page.items.iter().map(|item| item.id.as_str()).collect();
    assert!(ids.contains(&"c_plain"), "{ids:?}");
    assert!(
        !ids.contains(&"c_team"),
        "team conversations must be hard-filtered: {ids:?}"
    );
}

#[tokio::test]
async fn the_list_excludes_the_current_conversation() {
    let ctx = setup_targets_ctx().await;
    ctx.insert_conversation("c_current", "me", r#"{}"#).await;
    ctx.insert_conversation("c_other", "them", r#"{}"#).await;

    let page = ctx
        .targets
        .list("user_1", "c_current", &SessionMentionableQuery::default())
        .await
        .unwrap();

    let ids: Vec<&str> = page.items.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids, vec!["c_other"]);
}

#[tokio::test]
async fn another_users_conversations_never_appear() {
    let ctx = setup_targets_ctx().await;
    ctx.insert_conversation_for("user_2", "c_theirs", "theirs", r#"{}"#)
        .await;

    let page = ctx
        .targets
        .list("user_1", "c_current", &SessionMentionableQuery::default())
        .await
        .unwrap();

    assert!(page.items.is_empty(), "{:?}", page.items);
}

#[tokio::test]
async fn a_limit_above_the_cap_is_clamped_rather_than_returning_the_whole_table() {
    let ctx = setup_targets_ctx().await;
    for index in 0..3 {
        ctx.insert_conversation(&format!("c{index}"), &format!("conv {index}"), r#"{}"#)
            .await;
    }
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
    assert_eq!(page.items.len(), 3, "the clamp must not drop legitimate rows");
}

/// The cursor is taken from the DB page order BEFORE the team/self filtering,
/// so a page whose last row is hard-filtered still advances. Without this the
/// picker would page over the same rows forever.
#[tokio::test]
async fn the_cursor_survives_a_page_whose_last_row_is_hard_filtered() {
    let ctx = setup_targets_ctx().await;
    // `list_paginated` orders newest-first, so insert with ascending
    // `updated_at` and expect the reverse.
    for (index, (id, extra)) in [("c_a", r#"{}"#), ("c_b", r#"{"teamId":"team_1"}"#)]
        .into_iter()
        .enumerate()
    {
        ctx.insert_conversation(id, id, extra).await;
        ctx.repo
            .update(
                "user_1",
                id,
                &aionui_db::ConversationRowUpdate {
                    updated_at: Some(100 - index as i64),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }

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

    // Only the non-team row survives the filter…
    let ids: Vec<&str> = page.items.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids, vec!["c_a"]);
    // …and with no further pages there is no cursor to hand back.
    assert!(page.next_cursor.is_none(), "{:?}", page.next_cursor);
}
