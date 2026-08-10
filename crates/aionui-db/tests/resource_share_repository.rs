//! Black-box integration tests for resource share collaboration.
//!
//! Covers grant/revoke and access resolution against an in-memory database,
//! including conversation readability for shared grantees.

use std::sync::Arc;

use aionui_db::{
    ConversationFilters, GrantShareParams, IConversationRepository, IResourceShareRepository, IUserRepository,
    ResourceAccess, SharePermission, ShareResourceType, SqliteConversationRepository, SqliteResourceShareRepository,
    SqliteUserRepository, init_database_memory, models::ConversationRow,
};

async fn setup() -> (
    Arc<dyn IResourceShareRepository>,
    Arc<dyn IConversationRepository>,
    Arc<dyn IUserRepository>,
    String,
    String,
) {
    let db = init_database_memory().await.unwrap();
    let share_repo: Arc<dyn IResourceShareRepository> = Arc::new(SqliteResourceShareRepository::new(db.pool().clone()));
    let conv_repo: Arc<dyn IConversationRepository> = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let user_repo: Arc<dyn IUserRepository> = Arc::new(SqliteUserRepository::new(db.pool().clone()));

    let owner = user_repo.create_user("owner_alice", "hash").await.unwrap();
    let grantee = user_repo.create_user("grantee_bob", "hash").await.unwrap();
    (share_repo, conv_repo, user_repo, owner.id, grantee.id)
}

fn make_conv(owner_id: &str) -> ConversationRow {
    let now = aionui_common::now_ms();
    ConversationRow {
        id: aionui_common::generate_prefixed_id("conv"),
        user_id: owner_id.to_owned(),
        name: "Shared chat".into(),
        r#type: "gemini".into(),
        extra: "{}".into(),
        model: None,
        status: Some("pending".into()),
        source: Some("aionui".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: now,
        updated_at: now,
        project_id: None,
        folder_id: None,
        name_source: None,
    }
}

#[tokio::test]
async fn shared_conversation_is_readable_but_not_writable_with_view() {
    let (shares, convs, _users, owner_id, grantee_id) = setup().await;
    let conv = make_conv(&owner_id);
    convs.create(&conv).await.unwrap();

    shares
        .grant(GrantShareParams {
            resource_type: ShareResourceType::Conversation,
            resource_id: &conv.id,
            owner_user_id: &owner_id,
            grantee_user_id: &grantee_id,
            permission: SharePermission::View,
            created_by: &owner_id,
        })
        .await
        .unwrap();

    assert_eq!(
        shares
            .resolve_access(ShareResourceType::Conversation, &conv.id, &grantee_id)
            .await
            .unwrap(),
        ResourceAccess::View
    );

    // List/get include shared conversations.
    let found = convs.get(&grantee_id, &conv.id).await.unwrap();
    assert!(found.is_some());
    let listed = convs
        .list_paginated(
            &grantee_id,
            &ConversationFilters {
                cursor: None,
                limit: 20,
                source: None,
                cron_job_id: None,
                pinned: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(listed.items.len(), 1);

    // View cannot update (write requires edit/owner).
    let err = convs
        .update(
            &grantee_id,
            &conv.id,
            &aionui_db::ConversationRowUpdate {
                name: Some("hacked".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, aionui_db::DbError::NotFound(_)));
}

#[tokio::test]
async fn edit_share_allows_conversation_update() {
    let (shares, convs, _users, owner_id, grantee_id) = setup().await;
    let conv = make_conv(&owner_id);
    convs.create(&conv).await.unwrap();

    shares
        .grant(GrantShareParams {
            resource_type: ShareResourceType::Conversation,
            resource_id: &conv.id,
            owner_user_id: &owner_id,
            grantee_user_id: &grantee_id,
            permission: SharePermission::Edit,
            created_by: &owner_id,
        })
        .await
        .unwrap();

    convs
        .update(
            &grantee_id,
            &conv.id,
            &aionui_db::ConversationRowUpdate {
                name: Some("renamed by grantee".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let updated = convs.get(&owner_id, &conv.id).await.unwrap().unwrap();
    assert_eq!(updated.name, "renamed by grantee");
}

#[tokio::test]
async fn revoke_restores_isolation() {
    let (shares, convs, _users, owner_id, grantee_id) = setup().await;
    let conv = make_conv(&owner_id);
    convs.create(&conv).await.unwrap();
    let share = shares
        .grant(GrantShareParams {
            resource_type: ShareResourceType::Conversation,
            resource_id: &conv.id,
            owner_user_id: &owner_id,
            grantee_user_id: &grantee_id,
            permission: SharePermission::Edit,
            created_by: &owner_id,
        })
        .await
        .unwrap();

    shares.revoke(&share.id).await.unwrap();
    assert!(convs.get(&grantee_id, &conv.id).await.unwrap().is_none());
    assert_eq!(
        shares
            .resolve_access(ShareResourceType::Conversation, &conv.id, &grantee_id)
            .await
            .unwrap(),
        ResourceAccess::None
    );
}
