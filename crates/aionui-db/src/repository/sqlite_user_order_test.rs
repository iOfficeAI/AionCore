use super::{PIN_GAP, SqliteUserOrderStore};
use crate::init_database_memory;
use crate::models::{OrderItemType, OrderScene};
use crate::repository::user_order::{IUserOrderStore, OrderItemRef, PinOutcome, PinnedCursor};

const USER: &str = "user-1";
const OTHER_USER: &str = "user-2";

async fn store() -> (SqliteUserOrderStore, crate::Database) {
    let db = init_database_memory().await.unwrap();
    let store = SqliteUserOrderStore::new(db.pool().clone());
    (store, db)
}

fn conv(id: &str) -> OrderItemRef {
    OrderItemRef::new(OrderItemType::Conversation, id)
}

fn team(id: &str) -> OrderItemRef {
    OrderItemRef::new(OrderItemType::Team, id)
}

#[tokio::test]
async fn pin_inserts_first_row_at_base_key() {
    let (store, _db) = store().await;
    let outcome = store.pin(USER, OrderScene::Pinned, &conv("c1")).await.unwrap();
    assert_eq!(outcome, PinOutcome::Inserted);

    let rows = store.list_pinned(USER, OrderScene::Pinned, None, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].item_id, "c1");
    assert_eq!(rows[0].order_key, PIN_GAP);
}

#[tokio::test]
async fn pin_stacks_newest_on_top() {
    let (store, _db) = store().await;
    store.pin(USER, OrderScene::Pinned, &conv("c1")).await.unwrap();
    store.pin(USER, OrderScene::Pinned, &conv("c2")).await.unwrap();
    store.pin(USER, OrderScene::Pinned, &team("t1")).await.unwrap();

    // Ascending order_key => most-recently pinned first.
    let rows = store.list_pinned(USER, OrderScene::Pinned, None, 10).await.unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r.item_id.as_str()).collect();
    assert_eq!(ids, vec!["t1", "c2", "c1"]);
    assert_eq!(rows[0].order_key, PIN_GAP - 2 * PIN_GAP);
    assert_eq!(rows[1].order_key, PIN_GAP - PIN_GAP);
    assert_eq!(rows[2].order_key, PIN_GAP);
}

#[tokio::test]
async fn pin_is_idempotent() {
    let (store, _db) = store().await;
    assert_eq!(
        store.pin(USER, OrderScene::Pinned, &conv("c1")).await.unwrap(),
        PinOutcome::Inserted
    );
    assert_eq!(
        store.pin(USER, OrderScene::Pinned, &conv("c1")).await.unwrap(),
        PinOutcome::AlreadyPinned
    );

    let rows = store.list_pinned(USER, OrderScene::Pinned, None, 10).await.unwrap();
    assert_eq!(rows.len(), 1, "duplicate pin must not add a second row");
    assert_eq!(rows[0].order_key, PIN_GAP, "order_key preserved on no-op");
}

#[tokio::test]
async fn unpin_reports_removal_then_noop() {
    let (store, _db) = store().await;
    store.pin(USER, OrderScene::Pinned, &conv("c1")).await.unwrap();

    assert!(store.unpin(USER, OrderScene::Pinned, &conv("c1")).await.unwrap());
    assert!(
        !store.unpin(USER, OrderScene::Pinned, &conv("c1")).await.unwrap(),
        "second unpin is an idempotent no-op"
    );
    assert!(
        store
            .list_pinned(USER, OrderScene::Pinned, None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn list_pinned_paginates_by_keyset() {
    let (store, _db) = store().await;
    // Pin c1..c5; keys are 1000, 0, -1000, -2000, -3000 (newest lowest).
    for id in ["c1", "c2", "c3", "c4", "c5"] {
        store.pin(USER, OrderScene::Pinned, &conv(id)).await.unwrap();
    }

    let page1 = store.list_pinned(USER, OrderScene::Pinned, None, 2).await.unwrap();
    let ids1: Vec<&str> = page1.iter().map(|r| r.item_id.as_str()).collect();
    assert_eq!(ids1, vec!["c5", "c4"]);

    let last = page1.last().unwrap();
    let cursor = PinnedCursor {
        order_key: last.order_key,
        item_type: OrderItemType::parse(&last.item_type).unwrap(),
        item_id: last.item_id.clone(),
    };
    let page2 = store
        .list_pinned(USER, OrderScene::Pinned, Some(&cursor), 2)
        .await
        .unwrap();
    let ids2: Vec<&str> = page2.iter().map(|r| r.item_id.as_str()).collect();
    assert_eq!(
        ids2,
        vec!["c3", "c2"],
        "keyset resumes strictly after cursor, no repeat"
    );
}

#[tokio::test]
async fn pinned_reads_ride_the_scene_index_no_full_scan() {
    // BR-25: the hot pinned reads must ride `idx_user_order_scene`
    // (user_id, scene, order_key) and never degrade into a full-table scan.
    // The keyset predicate is deliberately expanded lexicographically (see the
    // comment in `list_pinned`) precisely so the leading `order_key` range stays
    // index-driven; this test pins that guarantee.
    use sqlx::Row;

    let (store, db) = store().await;
    for id in ["c1", "c2", "c3"] {
        store.pin(USER, OrderScene::Pinned, &conv(id)).await.unwrap();
    }

    let plan_detail = |sql: &'static str, binds: Vec<String>| {
        let pool = db.pool().clone();
        async move {
            let mut query = sqlx::query(sql);
            for bind in &binds {
                query = query.bind(bind);
            }
            let rows = query.fetch_all(&pool).await.unwrap();
            rows.iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect::<Vec<_>>()
                .join(" | ")
        }
    };

    // Base (first-screen) read: WHERE user_id, scene + ORDER BY order_key.
    let base = plan_detail(
        "EXPLAIN QUERY PLAN \
         SELECT user_id, scene, item_type, item_id, order_key FROM user_order \
         WHERE user_id = ? AND scene = ? \
         ORDER BY order_key ASC, item_type ASC, item_id ASC \
         LIMIT ?",
        vec![USER.to_owned(), OrderScene::Pinned.as_str().to_owned(), "10".to_owned()],
    )
    .await;
    assert!(
        base.contains("idx_user_order_scene"),
        "base pinned read must use idx_user_order_scene, got plan: {base}"
    );
    assert!(
        !base.contains("SCAN user_order"),
        "base pinned read must not full-scan user_order, got plan: {base}"
    );

    // Keyset continuation read: the expanded (order_key > ? OR ...) predicate.
    let keyset = plan_detail(
        "EXPLAIN QUERY PLAN \
         SELECT user_id, scene, item_type, item_id, order_key FROM user_order \
         WHERE user_id = ? AND scene = ? AND ( \
            order_key > ? OR \
            (order_key = ? AND (item_type > ? OR (item_type = ? AND item_id > ?))) \
         ) \
         ORDER BY order_key ASC, item_type ASC, item_id ASC \
         LIMIT ?",
        vec![
            USER.to_owned(),
            OrderScene::Pinned.as_str().to_owned(),
            "0".to_owned(),
            "0".to_owned(),
            "conversation".to_owned(),
            "conversation".to_owned(),
            "c2".to_owned(),
            "10".to_owned(),
        ],
    )
    .await;
    assert!(
        keyset.contains("idx_user_order_scene"),
        "keyset pinned read must use idx_user_order_scene, got plan: {keyset}"
    );
    assert!(
        !keyset.contains("SCAN user_order"),
        "keyset pinned read must not full-scan user_order, got plan: {keyset}"
    );
}

#[tokio::test]
async fn pinned_refs_returns_all_typed_refs() {
    let (store, _db) = store().await;
    store.pin(USER, OrderScene::Pinned, &conv("c1")).await.unwrap();
    store.pin(USER, OrderScene::Pinned, &team("t1")).await.unwrap();

    let mut refs = store.pinned_refs(USER, OrderScene::Pinned).await.unwrap();
    refs.sort_by(|a, b| a.item_id.cmp(&b.item_id));
    assert_eq!(refs, vec![conv("c1"), team("t1")]);
}

#[tokio::test]
async fn remove_item_deletes_single_ref() {
    let (store, _db) = store().await;
    store.pin(USER, OrderScene::Pinned, &conv("c1")).await.unwrap();
    store.pin(USER, OrderScene::Pinned, &conv("c2")).await.unwrap();

    store.remove_item(USER, &conv("c1")).await.unwrap();
    let ids: Vec<String> = store
        .pinned_refs(USER, OrderScene::Pinned)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.item_id)
        .collect();
    assert_eq!(ids, vec!["c2"]);
    // Idempotent: removing a gone item is fine.
    store.remove_item(USER, &conv("c1")).await.unwrap();
}

#[tokio::test]
async fn remove_items_batch_is_atomic() {
    let (store, _db) = store().await;
    for id in ["c1", "c2", "c3"] {
        store.pin(USER, OrderScene::Pinned, &conv(id)).await.unwrap();
    }
    store.pin(USER, OrderScene::Pinned, &team("t1")).await.unwrap();

    store
        .remove_items(USER, &[conv("c1"), conv("c3"), team("t1")])
        .await
        .unwrap();
    let ids: Vec<String> = store
        .pinned_refs(USER, OrderScene::Pinned)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.item_id)
        .collect();
    assert_eq!(ids, vec!["c2"]);

    // Empty batch is a no-op.
    store.remove_items(USER, &[]).await.unwrap();
    assert_eq!(store.pinned_refs(USER, OrderScene::Pinned).await.unwrap().len(), 1);
}

#[tokio::test]
async fn rows_are_scoped_per_user() {
    let (store, _db) = store().await;
    store.pin(USER, OrderScene::Pinned, &conv("c1")).await.unwrap();

    // A different user sees nothing and cannot unpin another user's row.
    assert!(
        store
            .list_pinned(OTHER_USER, OrderScene::Pinned, None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!store.unpin(OTHER_USER, OrderScene::Pinned, &conv("c1")).await.unwrap());
    assert_eq!(
        store
            .list_pinned(USER, OrderScene::Pinned, None, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn concurrent_pins_serialize_into_distinct_rows() {
    let (store, _db) = store().await;
    // Two concurrent pins on the same empty scene. BEGIN IMMEDIATE serializes
    // the read-min-then-insert, so both land as distinct rows (no lost write,
    // no duplicate key) with different order_keys.
    let a = {
        let store = store.clone();
        tokio::spawn(async move { store.pin(USER, OrderScene::Pinned, &conv("c1")).await })
    };
    let b = {
        let store = store.clone();
        tokio::spawn(async move { store.pin(USER, OrderScene::Pinned, &conv("c2")).await })
    };
    a.await.unwrap().unwrap();
    b.await.unwrap().unwrap();

    let rows = store.list_pinned(USER, OrderScene::Pinned, None, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].order_key, rows[1].order_key, "keys must not collide");
}
