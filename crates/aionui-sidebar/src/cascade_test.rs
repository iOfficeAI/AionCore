//! Path-1 cascade (design §4.3): deleting a conversation drops its `user_order`
//! rows. The orphan assertion is on the table row count, not API output.

use std::sync::Arc;

use aionui_common::OnConversationDelete;
use aionui_db::{
    IUserOrderStore, OrderItemRef, OrderItemType, OrderScene, SqlitePool, SqliteUserOrderStore, init_database_memory,
};

use super::UserOrderDeleteHook;

const USER: &str = "user-1";
const OTHER: &str = "user-2";

async fn seed_user(pool: &SqlitePool, id: &str) {
    sqlx::query("INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, 'x', 0, 0)")
        .bind(id)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

async fn count_rows(pool: &SqlitePool, user: &str, item_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM user_order WHERE user_id = ? AND item_id = ?")
        .bind(user)
        .bind(item_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn deleting_a_conversation_drops_its_pinned_row_and_is_user_scoped() {
    let db = init_database_memory().await.unwrap();
    seed_user(db.pool(), USER).await;
    seed_user(db.pool(), OTHER).await;
    let store: Arc<dyn IUserOrderStore> = Arc::new(SqliteUserOrderStore::new(db.pool().clone()));

    // Both users pin a conversation that happens to share the same id.
    let item = OrderItemRef::new(OrderItemType::Conversation, "c1");
    store.pin(USER, OrderScene::Pinned, &item).await.unwrap();
    store.pin(OTHER, OrderScene::Pinned, &item).await.unwrap();
    assert_eq!(count_rows(db.pool(), USER, "c1").await, 1);
    assert_eq!(count_rows(db.pool(), OTHER, "c1").await, 1);

    let hook = UserOrderDeleteHook::new(store.clone());
    hook.on_conversation_deleted(USER, "c1").await;

    // USER's row is gone; OTHER's identically-keyed row is untouched (BR-24).
    assert_eq!(
        count_rows(db.pool(), USER, "c1").await,
        0,
        "path-1 cascade removed the pinned row"
    );
    assert_eq!(count_rows(db.pool(), OTHER, "c1").await, 1, "cascade is user-scoped");

    // Idempotent: deleting again (or a never-pinned id) is a silent no-op.
    hook.on_conversation_deleted(USER, "c1").await;
    hook.on_conversation_deleted(USER, "never-pinned").await;
    assert_eq!(count_rows(db.pool(), USER, "c1").await, 0);
}
