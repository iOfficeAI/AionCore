use aionui_db::{
    ConversationFilters, ConversationRowUpdate, IConversationRepository, MessagePageCursor, MessagePageDirection,
    MessagePageParams, MessageRowUpdate, SqliteConversationRepository, init_database_memory, models::ConversationRow,
    models::MessageRow,
};

const USER_ID: &str = "system_default_user";

async fn setup() -> (SqliteConversationRepository, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteConversationRepository::new(db.pool().clone());
    (repo, db)
}

fn make_conversation(suffix: &str) -> ConversationRow {
    let now = aionui_common::now_ms();
    ConversationRow {
        id: aionui_common::generate_prefixed_id("conv"),
        user_id: USER_ID.to_string(),
        name: format!("Conversation {suffix}"),
        r#type: "gemini".to_string(),
        extra: r#"{"workspace":"/home/user/project"}"#.to_string(),
        model: Some(r#"{"providerId":"prov_1","model":"claude-sonnet-4-20250514"}"#.to_string()),
        status: Some("pending".to_string()),
        source: Some("aionui".to_string()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: now,
        updated_at: now,
    }
}

fn make_message(conv_id: &str, content: &str) -> MessageRow {
    let now = aionui_common::now_ms();
    MessageRow {
        id: aionui_common::generate_prefixed_id("msg"),
        conversation_id: conv_id.to_string(),
        msg_id: Some(aionui_common::generate_prefixed_id("cmsg")),
        r#type: "text".to_string(),
        content: format!(r#"{{"content":"{content}"}}"#),
        position: Some("right".to_string()),
        status: Some("finish".to_string()),
        hidden: false,
        created_at: now,
    }
}

fn make_artifact(conv_id: &str, artifact_id: &str) -> aionui_db::ConversationArtifactRow {
    aionui_db::ConversationArtifactRow {
        id: artifact_id.to_string(),
        conversation_id: conv_id.to_string(),
        cron_job_id: Some("cron_1".to_string()),
        kind: "skill_suggest".to_string(),
        status: "pending".to_string(),
        payload: serde_json::json!({
            "cron_job_id": "cron_1",
            "name": "daily-report",
            "description": "Daily report",
            "skillContent": "---\nname: daily-report\n---\nUse it."
        })
        .to_string(),
        created_at: 1000,
        updated_at: 1000,
    }
}

// ── Conversation CRUD ───────────────────────────────────────────────

#[tokio::test]
async fn create_get_update_delete_lifecycle() {
    let (repo, _db) = setup().await;

    // Create
    let conv = make_conversation("lifecycle");
    repo.create(&conv).await.unwrap();

    // Get
    let found = repo.get(&conv.id).await.unwrap().unwrap();
    assert_eq!(found.name, "Conversation lifecycle");
    assert_eq!(found.status.as_deref(), Some("pending"));

    // Update
    let now = aionui_common::now_ms();
    repo.update(
        &conv.id,
        &ConversationRowUpdate {
            name: Some("Updated Name".to_string()),
            status: Some("running".to_string()),
            updated_at: Some(now),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let updated = repo.get(&conv.id).await.unwrap().unwrap();
    assert_eq!(updated.name, "Updated Name");
    assert_eq!(updated.status.as_deref(), Some("running"));

    // Delete
    repo.delete(&conv.id).await.unwrap();
    assert!(repo.get(&conv.id).await.unwrap().is_none());
}

#[tokio::test]
async fn delete_conversation_cascades_messages() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("cascade");
    repo.create(&conv).await.unwrap();

    // Insert messages
    for i in 0..3 {
        let msg = make_message(&conv.id, &format!("msg {i}"));
        repo.insert_message(&msg).await.unwrap();
    }

    // Verify messages exist
    let msgs = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 50,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    assert_eq!(msgs.items.len(), 3);

    // Delete conversation → messages cascade
    repo.delete(&conv.id).await.unwrap();

    let msgs = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 50,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    assert!(msgs.items.is_empty());
}

// ── Cursor pagination ───────────────────────────────────────────────

#[tokio::test]
async fn cursor_pagination_walks_through_all_items() {
    let (repo, _db) = setup().await;

    // Create 7 conversations with distinct updated_at
    for i in 0..7 {
        let mut c = make_conversation(&format!("{i}"));
        c.updated_at = (i + 1) as i64 * 1000;
        repo.create(&c).await.unwrap();
    }

    // Page 1: no cursor, limit 3
    let p1 = repo
        .list_paginated(
            USER_ID,
            &ConversationFilters {
                limit: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(p1.items.len(), 3);
    assert!(p1.has_more);
    assert_eq!(p1.total, 7);

    // Page 2
    let cursor = p1.items.last().unwrap().id.clone();
    let p2 = repo
        .list_paginated(
            USER_ID,
            &ConversationFilters {
                cursor: Some(cursor),
                limit: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(p2.items.len(), 3);
    assert!(p2.has_more);

    // Page 3
    let cursor = p2.items.last().unwrap().id.clone();
    let p3 = repo
        .list_paginated(
            USER_ID,
            &ConversationFilters {
                cursor: Some(cursor),
                limit: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(p3.items.len(), 1);
    assert!(!p3.has_more);

    // All 7 items collected, no duplicates
    let mut all_ids: Vec<_> = p1
        .items
        .iter()
        .chain(p2.items.iter())
        .chain(p3.items.iter())
        .map(|c| c.id.clone())
        .collect();
    all_ids.sort();
    all_ids.dedup();
    assert_eq!(all_ids.len(), 7);
}

// ── Filter combinations ─────────────────────────────────────────────

#[tokio::test]
async fn filter_by_source_and_pinned_combined() {
    let (repo, _db) = setup().await;

    let mut c1 = make_conversation("aionui-pinned");
    c1.source = Some("aionui".to_string());
    c1.pinned = true;
    c1.pinned_at = Some(aionui_common::now_ms());
    repo.create(&c1).await.unwrap();

    let mut c2 = make_conversation("telegram-pinned");
    c2.source = Some("telegram".to_string());
    c2.pinned = true;
    c2.pinned_at = Some(aionui_common::now_ms());
    repo.create(&c2).await.unwrap();

    let mut c3 = make_conversation("aionui-unpinned");
    c3.source = Some("aionui".to_string());
    c3.pinned = false;
    repo.create(&c3).await.unwrap();

    // Filter: source=aionui AND pinned=true
    let result = repo
        .list_paginated(
            USER_ID,
            &ConversationFilters {
                source: Some("aionui".to_string()),
                pinned: Some(true),
                limit: 20,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].id, c1.id);
}

#[tokio::test]
async fn filter_by_cron_job_id() {
    let (repo, _db) = setup().await;

    let mut c1 = make_conversation("cron-a");
    c1.extra = r#"{"cronJobId":"cron_123","workspace":"/p"}"#.to_string();
    repo.create(&c1).await.unwrap();

    let mut c2 = make_conversation("cron-b");
    c2.extra = r#"{"cronJobId":"cron_456","workspace":"/q"}"#.to_string();
    repo.create(&c2).await.unwrap();

    let mut c3 = make_conversation("no-cron");
    c3.extra = r#"{"workspace":"/r"}"#.to_string();
    repo.create(&c3).await.unwrap();

    let result = repo
        .list_paginated(
            USER_ID,
            &ConversationFilters {
                cron_job_id: Some("cron_123".to_string()),
                limit: 20,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].id, c1.id);
}

#[tokio::test]
async fn filter_by_cron_job_id_accepts_snake_case_extra() {
    let (repo, _db) = setup().await;

    let mut c1 = make_conversation("cron-snake");
    c1.extra = r#"{"cron_job_id":"cron_123","workspace":"/p"}"#.to_string();
    repo.create(&c1).await.unwrap();

    let result = repo
        .list_paginated(
            USER_ID,
            &ConversationFilters {
                cron_job_id: Some("cron_123".to_string()),
                limit: 20,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].id, c1.id);
}

// ── Extended queries ────────────────────────────────────────────────

#[tokio::test]
async fn find_by_source_and_chat_integration() {
    let (repo, _db) = setup().await;

    let mut c = make_conversation("telegram");
    c.source = Some("telegram".to_string());
    c.channel_chat_id = Some("group:789".to_string());
    c.r#type = "acp".to_string();
    repo.create(&c).await.unwrap();

    let found = repo
        .find_by_source_and_chat(USER_ID, "telegram", "group:789", "acp")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, c.id);
}

#[tokio::test]
async fn list_by_cron_job_returns_matching() {
    let (repo, _db) = setup().await;

    let mut c1 = make_conversation("cron1");
    c1.extra = r#"{"cronJobId":"job_x","workspace":"/a"}"#.to_string();
    repo.create(&c1).await.unwrap();

    let mut c2 = make_conversation("cron2");
    c2.extra = r#"{"cronJobId":"job_x","workspace":"/b"}"#.to_string();
    repo.create(&c2).await.unwrap();

    let mut c3 = make_conversation("cron3");
    c3.extra = r#"{"cronJobId":"job_y","workspace":"/c"}"#.to_string();
    repo.create(&c3).await.unwrap();

    let result = repo.list_by_cron_job(USER_ID, "job_x").await.unwrap();
    assert_eq!(result.len(), 2);
}

#[tokio::test]
async fn list_by_cron_job_accepts_mixed_key_formats() {
    let (repo, _db) = setup().await;

    let mut c1 = make_conversation("cron-old");
    c1.extra = r#"{"cronJobId":"job_x","workspace":"/a"}"#.to_string();
    repo.create(&c1).await.unwrap();

    let mut c2 = make_conversation("cron-new");
    c2.extra = r#"{"cron_job_id":"job_x","workspace":"/b"}"#.to_string();
    repo.create(&c2).await.unwrap();

    let result = repo.list_by_cron_job(USER_ID, "job_x").await.unwrap();
    assert_eq!(result.len(), 2);
}

#[tokio::test]
async fn list_associated_finds_same_workspace() {
    let (repo, _db) = setup().await;

    let mut c1 = make_conversation("ws1");
    c1.extra = r#"{"workspace":"/shared"}"#.to_string();
    repo.create(&c1).await.unwrap();

    let mut c2 = make_conversation("ws2");
    c2.extra = r#"{"workspace":"/shared"}"#.to_string();
    repo.create(&c2).await.unwrap();

    let mut c3 = make_conversation("ws3");
    c3.extra = r#"{"workspace":"/different"}"#.to_string();
    repo.create(&c3).await.unwrap();

    let assoc = repo.list_associated(USER_ID, &c1.id).await.unwrap();
    assert_eq!(assoc.len(), 1);
    assert_eq!(assoc[0].id, c2.id);
}

#[tokio::test]
async fn list_associated_returns_empty_when_no_workspace() {
    let (repo, _db) = setup().await;

    let mut c = make_conversation("no-ws");
    c.extra = r#"{"setting":"value"}"#.to_string();
    repo.create(&c).await.unwrap();

    let assoc = repo.list_associated(USER_ID, &c.id).await.unwrap();
    assert!(assoc.is_empty());
}

// ── Message operations ──────────────────────────────────────────────

#[tokio::test]
async fn initial_latest_returns_latest_limit_in_ascending_order() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("msgs");
    repo.create(&conv).await.unwrap();

    for i in 0..10 {
        let mut msg = make_message(&conv.id, &format!("item {i}"));
        msg.created_at = (i + 1) as i64 * 1000;
        repo.insert_message(&msg).await.unwrap();
    }

    let p1 = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 3,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    assert_eq!(p1.items.len(), 3);
    assert_eq!(
        p1.items.iter().map(|m| m.created_at).collect::<Vec<_>>(),
        vec![8000, 9000, 10000]
    );
    assert!(p1.has_more_before);
    assert!(!p1.has_more_after);
}

#[tokio::test]
async fn before_pages_walk_history_without_duplicates() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("before");
    repo.create(&conv).await.unwrap();

    for i in 0..6 {
        let mut msg = make_message(&conv.id, &format!("item {i}"));
        msg.id = format!("msg-{i}");
        msg.created_at = (i + 1) as i64 * 1000;
        repo.insert_message(&msg).await.unwrap();
    }

    let latest = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 3,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    let older = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 3,
                direction: MessagePageDirection::Before {
                    cursor: MessagePageCursor::from(&latest.items[0]),
                },
            },
        )
        .await
        .unwrap();

    let mut ids = latest
        .items
        .iter()
        .chain(older.items.iter())
        .map(|m| m.id.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 6);
    assert_eq!(
        older.items.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["msg-0", "msg-1", "msg-2"]
    );
    assert!(!older.has_more_before);
    assert!(older.has_more_after);
}

#[tokio::test]
async fn after_returns_newer_rows_with_same_timestamp_tie_breaker() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("after");
    repo.create(&conv).await.unwrap();

    for id in ["msg-a", "msg-b", "msg-c", "msg-d", "msg-e"] {
        let mut msg = make_message(&conv.id, id);
        msg.id = id.to_string();
        msg.created_at = 1000;
        repo.insert_message(&msg).await.unwrap();
    }

    let page = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 2,
                direction: MessagePageDirection::After {
                    cursor: MessagePageCursor {
                        created_at: 1000,
                        id: "msg-b".to_string(),
                    },
                },
            },
        )
        .await
        .unwrap();

    assert_eq!(
        page.items.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["msg-c", "msg-d"]
    );
    assert!(page.has_more_before);
    assert!(page.has_more_after);
}

#[tokio::test]
async fn created_at_id_ordering_is_scoped_per_conversation() {
    let (repo, _db) = setup().await;
    let conv_a = make_conversation("seq-a");
    let conv_b = make_conversation("seq-b");
    repo.create(&conv_a).await.unwrap();
    repo.create(&conv_b).await.unwrap();

    let mut a1 = make_message(&conv_a.id, "a1");
    a1.id = "a1".to_string();
    a1.created_at = 1000;
    repo.insert_message(&a1).await.unwrap();

    let mut a2 = make_message(&conv_a.id, "a2");
    a2.id = "a2".to_string();
    a2.created_at = 2000;
    repo.insert_message(&a2).await.unwrap();

    let mut b1 = make_message(&conv_b.id, "b1");
    b1.id = "b1".to_string();
    b1.created_at = 1000;
    repo.insert_message(&b1).await.unwrap();

    let page_a = repo
        .list_messages_page(
            &conv_a.id,
            &MessagePageParams {
                limit: 10,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    let page_b = repo
        .list_messages_page(
            &conv_b.id,
            &MessagePageParams {
                limit: 10,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        page_a.items.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
        vec![r#"{"content":"a1"}"#, r#"{"content":"a2"}"#]
    );
    assert_eq!(
        page_b.items.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
        vec![r#"{"content":"b1"}"#]
    );
}

#[tokio::test]
async fn concurrent_insert_message_does_not_require_sequence_allocation() {
    let dir = tempfile::tempdir().unwrap();
    let db = aionui_db::init_database(&dir.path().join("messages.db")).await.unwrap();
    let repo = SqliteConversationRepository::new(db.pool().clone());
    let conv = make_conversation("seq-race");
    repo.create(&conv).await.unwrap();

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(48));
    let mut handles = Vec::new();
    for i in 0..48 {
        let repo = repo.clone();
        let conv_id = conv.id.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut msg = make_message(&conv_id, &format!("concurrent {i}"));
            msg.id = format!("msg-concurrent-{i}");
            msg.msg_id = Some(format!("cmsg-concurrent-{i}"));
            repo.insert_message(&msg).await
        }));
    }

    for handle in handles {
        handle.await.unwrap().unwrap();
    }

    let page = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 100,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();

    assert_eq!(page.items.len(), 48);
    let mut ids = page.items.iter().map(|m| m.id.as_str()).collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids.len(), 48);
    assert_eq!(ids[0], "msg-concurrent-0");
}

#[tokio::test]
async fn anchor_window_contains_anchor_and_sets_flags() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("anchor");
    repo.create(&conv).await.unwrap();

    for i in 1..=7 {
        let mut msg = make_message(&conv.id, &format!("item {i}"));
        msg.id = format!("msg-{i}");
        msg.created_at = i as i64 * 1000;
        repo.insert_message(&msg).await.unwrap();
    }

    let page = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 5,
                direction: MessagePageDirection::Anchor {
                    message_id: "msg-4".to_string(),
                },
            },
        )
        .await
        .unwrap();

    assert_eq!(
        page.items.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["msg-2", "msg-3", "msg-4", "msg-5", "msg-6"]
    );
    assert!(page.has_more_before);
    assert!(page.has_more_after);
}

#[tokio::test]
async fn anchor_rejects_legacy_artifact_rows() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("anchor-artifact");
    repo.create(&conv).await.unwrap();

    repo.insert_message(&MessageRow {
        id: "legacy-cron".into(),
        conversation_id: conv.id.clone(),
        msg_id: None,
        r#type: "cron_trigger".into(),
        content: "{}".into(),
        position: Some("center".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at: 1000,
    })
    .await
    .unwrap();

    let err = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 5,
                direction: MessagePageDirection::Anchor {
                    message_id: "legacy-cron".to_string(),
                },
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err, aionui_db::DbError::NotFound(_)));
}

#[tokio::test]
async fn upsert_preserves_existing_created_at() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("upsert-seq");
    repo.create(&conv).await.unwrap();

    let mut msg = make_message(&conv.id, "first");
    msg.id = "msg-stable".to_string();
    repo.upsert_message(&msg).await.unwrap();

    let mut updated = msg.clone();
    updated.content = r#"{"content":"updated"}"#.to_string();
    updated.created_at = msg.created_at + 5000;
    repo.upsert_message(&updated).await.unwrap();

    let page = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 10,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();

    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].created_at, msg.created_at);
    assert_eq!(page.items[0].content, r#"{"content":"updated"}"#);
}

#[tokio::test]
async fn update_message_fields() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("msg-update");
    repo.create(&conv).await.unwrap();

    let msg = make_message(&conv.id, "original");
    repo.insert_message(&msg).await.unwrap();

    repo.update_message(
        &msg.id,
        &MessageRowUpdate {
            content: Some(r#"{"content":"modified"}"#.to_string()),
            hidden: Some(true),
            status: Some(Some("error".to_string())),
        },
    )
    .await
    .unwrap();

    let msgs = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 50,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    let updated = &msgs.items[0];
    assert_eq!(updated.content, r#"{"content":"modified"}"#);
    assert!(updated.hidden);
    assert_eq!(updated.status.as_deref(), Some("error"));
}

#[tokio::test]
async fn delete_messages_by_conversation_clears_all() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("msg-delete");
    repo.create(&conv).await.unwrap();

    for i in 0..5 {
        let msg = make_message(&conv.id, &format!("msg {i}"));
        repo.insert_message(&msg).await.unwrap();
    }

    repo.delete_messages_by_conversation(&conv.id).await.unwrap();

    let result = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 50,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    assert!(result.items.is_empty());
}

#[tokio::test]
async fn get_message_by_msg_id_triple() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("msg-find");
    repo.create(&conv).await.unwrap();

    let mut msg = make_message(&conv.id, "findable");
    msg.msg_id = Some("unique_msg_123".to_string());
    msg.r#type = "tool_call".to_string();
    repo.insert_message(&msg).await.unwrap();

    // Match
    let found = repo
        .get_message_by_msg_id(&conv.id, "unique_msg_123", "tool_call")
        .await
        .unwrap();
    assert!(found.is_some());

    // Wrong type → None
    let not_found = repo
        .get_message_by_msg_id(&conv.id, "unique_msg_123", "text")
        .await
        .unwrap();
    assert!(not_found.is_none());

    // Wrong conv → None
    let not_found = repo
        .get_message_by_msg_id("other_conv", "unique_msg_123", "tool_call")
        .await
        .unwrap();
    assert!(not_found.is_none());
}

// ── Message search ──────────────────────────────────────────────────

#[tokio::test]
async fn search_messages_across_conversations() {
    let (repo, _db) = setup().await;

    let c1 = make_conversation("search1");
    repo.create(&c1).await.unwrap();
    let c2 = make_conversation("search2");
    repo.create(&c2).await.unwrap();

    let msg1 = make_message(&c1.id, "Rust 代码审查报告");
    repo.insert_message(&msg1).await.unwrap();

    let msg2 = make_message(&c2.id, "Python 代码审查总结");
    repo.insert_message(&msg2).await.unwrap();

    let msg3 = make_message(&c1.id, "unrelated content");
    repo.insert_message(&msg3).await.unwrap();

    let result = repo.search_messages(USER_ID, "审查", 1, 20).await.unwrap();
    assert_eq!(result.total, 2);
    assert_eq!(result.items.len(), 2);

    // Verify conversation names are included
    let names: Vec<_> = result.items.iter().map(|r| &r.conversation_name).collect();
    assert!(names.contains(&&"Conversation search1".to_string()));
    assert!(names.contains(&&"Conversation search2".to_string()));
}

#[tokio::test]
async fn search_messages_empty_result() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("empty-search");
    repo.create(&conv).await.unwrap();

    let msg = make_message(&conv.id, "hello world");
    repo.insert_message(&msg).await.unwrap();

    let result = repo
        .search_messages(USER_ID, "nonexistent_keyword", 1, 20)
        .await
        .unwrap();
    assert!(result.items.is_empty());
    assert_eq!(result.total, 0);
    assert!(!result.has_more);
}

#[tokio::test]
async fn search_messages_pagination() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("search-page");
    repo.create(&conv).await.unwrap();

    for i in 0..5 {
        let mut msg = make_message(&conv.id, &format!("searchable item {i}"));
        msg.created_at = (i + 1) as i64 * 1000;
        repo.insert_message(&msg).await.unwrap();
    }

    let p1 = repo.search_messages(USER_ID, "searchable", 1, 2).await.unwrap();
    assert_eq!(p1.items.len(), 2);
    assert_eq!(p1.total, 5);
    assert!(p1.has_more);

    let p2 = repo.search_messages(USER_ID, "searchable", 2, 2).await.unwrap();
    assert_eq!(p2.items.len(), 2);
    assert!(p2.has_more);

    let p3 = repo.search_messages(USER_ID, "searchable", 3, 2).await.unwrap();
    assert_eq!(p3.items.len(), 1);
    assert!(!p3.has_more);
}

// ── Pinned update flow ──────────────────────────────────────────────

#[tokio::test]
async fn pin_and_unpin_conversation() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("pin-test");
    repo.create(&conv).await.unwrap();

    // Pin
    let pin_time = aionui_common::now_ms();
    repo.update(
        &conv.id,
        &ConversationRowUpdate {
            pinned: Some(true),
            pinned_at: Some(Some(pin_time)),
            updated_at: Some(pin_time),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let pinned = repo.get(&conv.id).await.unwrap().unwrap();
    assert!(pinned.pinned);
    assert_eq!(pinned.pinned_at, Some(pin_time));

    // Unpin
    let now = aionui_common::now_ms();
    repo.update(
        &conv.id,
        &ConversationRowUpdate {
            pinned: Some(false),
            pinned_at: Some(None),
            updated_at: Some(now),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let unpinned = repo.get(&conv.id).await.unwrap().unwrap();
    assert!(!unpinned.pinned);
    assert!(unpinned.pinned_at.is_none());
}

// ── Error cases ─────────────────────────────────────────────────────

#[tokio::test]
async fn update_nonexistent_conversation_returns_not_found() {
    let (repo, _db) = setup().await;
    let err = repo
        .update(
            "nonexistent_id",
            &ConversationRowUpdate {
                name: Some("x".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, aionui_db::DbError::NotFound(_)));
}

#[tokio::test]
async fn delete_nonexistent_conversation_returns_not_found() {
    let (repo, _db) = setup().await;
    let err = repo.delete("nonexistent_id").await.unwrap_err();
    assert!(matches!(err, aionui_db::DbError::NotFound(_)));
}

#[tokio::test]
async fn list_associated_nonexistent_returns_not_found() {
    let (repo, _db) = setup().await;
    let err = repo.list_associated(USER_ID, "nonexistent_id").await.unwrap_err();
    assert!(matches!(err, aionui_db::DbError::NotFound(_)));
}

#[tokio::test]
async fn update_message_nonexistent_returns_not_found() {
    let (repo, _db) = setup().await;
    let err = repo
        .update_message(
            "nonexistent_id",
            &MessageRowUpdate {
                hidden: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, aionui_db::DbError::NotFound(_)));
}

// ── Extra field update ──────────────────────────────────────────────

#[tokio::test]
async fn update_extra_replaces_json() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("extra-update");
    repo.create(&conv).await.unwrap();

    let now = aionui_common::now_ms();
    repo.update(
        &conv.id,
        &ConversationRowUpdate {
            extra: Some(r#"{"workspace":"/new","flag":true}"#.to_string()),
            updated_at: Some(now),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let found = repo.get(&conv.id).await.unwrap().unwrap();
    assert_eq!(found.extra, r#"{"workspace":"/new","flag":true}"#);
}

#[tokio::test]
async fn get_messages_excludes_legacy_cron_and_skill_suggest_rows() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("message-filter");
    repo.create(&conv).await.unwrap();

    repo.insert_message(&make_message(&conv.id, "visible")).await.unwrap();

    for (id, ty) in [("legacy-cron", "cron_trigger"), ("legacy-skill", "skill_suggest")] {
        repo.insert_message(&MessageRow {
            id: id.into(),
            conversation_id: conv.id.clone(),
            msg_id: None,
            r#type: ty.into(),
            content: "{}".into(),
            position: Some("center".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: 2000,
        })
        .await
        .unwrap();
    }

    let rows = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 50,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    assert_eq!(rows.items.len(), 1);
    assert_eq!(rows.items[0].r#type, "text");
}

#[tokio::test]
async fn list_legacy_cron_trigger_messages_returns_only_trigger_rows() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("legacy-cron-trigger");
    repo.create(&conv).await.unwrap();

    repo.insert_message(&MessageRow {
        id: aionui_common::generate_prefixed_id("msg"),
        conversation_id: conv.id.clone(),
        msg_id: Some("legacy-trigger".into()),
        r#type: "cron_trigger".into(),
        content: r#"{"cron_job_id":"cron_1","cron_job_name":"Daily Report"}"#.into(),
        position: Some("center".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at: 1000,
    })
    .await
    .unwrap();
    repo.insert_message(&make_message(&conv.id, "plain text"))
        .await
        .unwrap();

    let rows = repo.list_legacy_cron_trigger_messages(&conv.id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].r#type, "cron_trigger");
}

#[tokio::test]
async fn artifact_upsert_list_and_mark_saved() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("artifact-row");
    repo.create(&conv).await.unwrap();

    let artifact_id = format!("{}:skill_suggest:cron_1", conv.id);
    let inserted = repo
        .upsert_artifact(&make_artifact(&conv.id, &artifact_id))
        .await
        .unwrap();
    assert_eq!(inserted.status, "pending");

    let listed = repo.list_artifacts(&conv.id).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, artifact_id);

    let dismissed = repo
        .update_artifact_status(&conv.id, &artifact_id, "dismissed", 2000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dismissed.status, "dismissed");
    assert_eq!(dismissed.updated_at, 2000);

    let saved = repo.mark_skill_suggest_artifacts_saved("cron_1", 3000).await.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].status, "saved");
    assert_eq!(saved[0].updated_at, 3000);
}

#[tokio::test]
async fn delete_artifacts_by_conversation_removes_rows() {
    let (repo, _db) = setup().await;
    let conv = make_conversation("artifact-delete");
    repo.create(&conv).await.unwrap();

    let artifact_id = format!("{}:skill_suggest:cron_1", conv.id);
    repo.upsert_artifact(&make_artifact(&conv.id, &artifact_id))
        .await
        .unwrap();

    repo.delete_artifacts_by_conversation(&conv.id).await.unwrap();

    let listed = repo.list_artifacts(&conv.id).await.unwrap();
    assert!(listed.is_empty());
}

// ── User isolation ──────────────────────────────────────────────────

#[tokio::test]
async fn list_paginated_scoped_to_user() {
    let (repo, db) = setup().await;

    // Create a second user
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('user_2', 'other', 'hash', 1000, 1000)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let c1 = make_conversation("user1-conv");
    repo.create(&c1).await.unwrap();

    let mut c2 = make_conversation("user2-conv");
    c2.user_id = "user_2".to_string();
    repo.create(&c2).await.unwrap();

    // User 1 only sees their own
    let result = repo
        .list_paginated(
            USER_ID,
            &ConversationFilters {
                limit: 20,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].user_id, USER_ID);
}
