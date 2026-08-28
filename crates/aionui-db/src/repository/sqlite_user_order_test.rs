use super::{PIN_GAP, SqliteUserOrderStore};
use crate::init_database_memory;
use crate::models::{OrderItemType, OrderScene};
use crate::repository::user_order::{IUserOrderStore, MoveOutcome, OrderItemRef, PinOutcome, PinnedCursor};

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

// -- move_item -----------------------------------------------------------------

/// Force one row to a specific `order_key`, so a test can seed a degenerate
/// layout (e.g. an exhausted neighbour gap) that the public write path never
/// produces on its own.
async fn set_key(db: &crate::Database, item: &OrderItemRef, order_key: i64) {
    sqlx::query(
        "UPDATE user_order SET order_key = ? WHERE user_id = ? AND scene = ? AND item_type = ? AND item_id = ?",
    )
    .bind(order_key)
    .bind(USER)
    .bind(OrderScene::Pinned.as_str())
    .bind(item.item_type.as_str())
    .bind(&item.item_id)
    .execute(db.pool())
    .await
    .unwrap();
}

async fn ids(store: &SqliteUserOrderStore) -> Vec<String> {
    store
        .list_pinned(USER, OrderScene::Pinned, None, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.item_id)
        .collect()
}

#[tokio::test]
async fn move_to_top_places_below_current_min() {
    let (store, _db) = store().await;
    // Pin c1,c2,c3 → keys 1000, 0, -1000 → order [c3, c2, c1].
    for id in ["c1", "c2", "c3"] {
        store.pin(USER, OrderScene::Pinned, &conv(id)).await.unwrap();
    }

    // Move the bottom row (c1) to the top.
    let outcome = store
        .move_item(USER, OrderScene::Pinned, &conv("c1"), None)
        .await
        .unwrap();
    assert_eq!(outcome, MoveOutcome::Moved);

    assert_eq!(ids(&store).await, vec!["c1", "c3", "c2"]);
    let rows = store.list_pinned(USER, OrderScene::Pinned, None, 10).await.unwrap();
    // min was -1000, so the new top is -2000, strictly below every other row.
    assert_eq!(rows[0].order_key, -2000);
}

#[tokio::test]
async fn move_after_anchor_lands_at_midpoint() {
    let (store, _db) = store().await;
    for id in ["c1", "c2", "c3"] {
        store.pin(USER, OrderScene::Pinned, &conv(id)).await.unwrap();
    }
    // order [c3(-1000), c2(0), c1(1000)]. Move c3 to directly after c2.
    store
        .move_item(USER, OrderScene::Pinned, &conv("c3"), Some(&conv("c2")))
        .await
        .unwrap();

    // Between c2(0) and c1(1000) → 500.
    assert_eq!(ids(&store).await, vec!["c2", "c3", "c1"]);
    let rows = store.list_pinned(USER, OrderScene::Pinned, None, 10).await.unwrap();
    assert_eq!(rows[1].item_id, "c3");
    assert_eq!(rows[1].order_key, 500);
}

#[tokio::test]
async fn move_after_last_row_appends_one_gap_below() {
    let (store, _db) = store().await;
    for id in ["c1", "c2", "c3"] {
        store.pin(USER, OrderScene::Pinned, &conv(id)).await.unwrap();
    }
    // order [c3(-1000), c2(0), c1(1000)]. Move c2 to after the last row (c1).
    store
        .move_item(USER, OrderScene::Pinned, &conv("c2"), Some(&conv("c1")))
        .await
        .unwrap();

    assert_eq!(ids(&store).await, vec!["c3", "c1", "c2"]);
    let rows = store.list_pinned(USER, OrderScene::Pinned, None, 10).await.unwrap();
    // No successor of c1(1000) once c2 is excluded → key = 1000 + PIN_GAP.
    assert_eq!(rows[2].item_id, "c2");
    assert_eq!(rows[2].order_key, 1000 + PIN_GAP);
}

#[tokio::test]
async fn move_after_immediate_predecessor_is_a_stable_noop_order() {
    let (store, _db) = store().await;
    for id in ["c1", "c2", "c3"] {
        store.pin(USER, OrderScene::Pinned, &conv(id)).await.unwrap();
    }
    // order [c3, c2, c1]. Move c2 to after c3 (its current predecessor). The
    // moved-self exclusion means c3's successor is c1, not c2 — so c2 stays
    // between them and the visible order is unchanged.
    store
        .move_item(USER, OrderScene::Pinned, &conv("c2"), Some(&conv("c3")))
        .await
        .unwrap();
    assert_eq!(ids(&store).await, vec!["c3", "c2", "c1"]);
}

#[tokio::test]
async fn move_missing_item_reports_not_found() {
    let (store, _db) = store().await;
    store.pin(USER, OrderScene::Pinned, &conv("c1")).await.unwrap();

    let outcome = store
        .move_item(USER, OrderScene::Pinned, &conv("ghost"), None)
        .await
        .unwrap();
    assert_eq!(outcome, MoveOutcome::MovedNotFound);
    // Table untouched.
    assert_eq!(ids(&store).await, vec!["c1"]);
}

#[tokio::test]
async fn move_after_missing_anchor_reports_not_found() {
    let (store, _db) = store().await;
    store.pin(USER, OrderScene::Pinned, &conv("c1")).await.unwrap();
    store.pin(USER, OrderScene::Pinned, &conv("c2")).await.unwrap();

    let outcome = store
        .move_item(USER, OrderScene::Pinned, &conv("c1"), Some(&conv("ghost")))
        .await
        .unwrap();
    assert_eq!(outcome, MoveOutcome::AfterNotFound);
    // order unchanged [c2, c1].
    assert_eq!(ids(&store).await, vec!["c2", "c1"]);
}

#[tokio::test]
async fn move_mixes_conversation_and_team_rows() {
    let (store, _db) = store().await;
    store.pin(USER, OrderScene::Pinned, &conv("c1")).await.unwrap();
    store.pin(USER, OrderScene::Pinned, &team("t1")).await.unwrap();
    // order [t1(0), c1(1000)]. Move t1 to after c1 → t1 goes to the bottom.
    store
        .move_item(USER, OrderScene::Pinned, &team("t1"), Some(&conv("c1")))
        .await
        .unwrap();
    assert_eq!(ids(&store).await, vec!["c1", "t1"]);
}

#[tokio::test]
async fn move_rebalances_when_neighbour_gap_is_exhausted() {
    let (store, db) = store().await;
    for id in ["a", "b", "m"] {
        store.pin(USER, OrderScene::Pinned, &conv(id)).await.unwrap();
    }
    // Seed a degenerate layout: a(1000), b(1001) are adjacent (gap 1), m parked
    // at the bottom. Moving m to after a cannot fit an integer midpoint, so the
    // whole scene must rebalance first.
    set_key(&db, &conv("a"), 1000).await;
    set_key(&db, &conv("b"), 1001).await;
    set_key(&db, &conv("m"), 5000).await;

    store
        .move_item(USER, OrderScene::Pinned, &conv("m"), Some(&conv("a")))
        .await
        .unwrap();

    // Rebalance respaced [a, b, m] → 1000, 2000, 3000; then m lands between the
    // fresh a(1000) and b(2000) at 1500.
    assert_eq!(ids(&store).await, vec!["a", "m", "b"]);
    let rows = store.list_pinned(USER, OrderScene::Pinned, None, 10).await.unwrap();
    let by_id = |id: &str| rows.iter().find(|r| r.item_id == id).unwrap().order_key;
    assert_eq!(by_id("a"), 1000);
    assert_eq!(by_id("m"), 1500);
    assert_eq!(by_id("b"), 2000, "b respaced away from its exhausted 1001 slot");
    // Soft invariant: no two rows share a key after the move.
    let mut keys: Vec<i64> = rows.iter().map(|r| r.order_key).collect();
    keys.sort_unstable();
    let unique = {
        let mut k = keys.clone();
        k.dedup();
        k.len()
    };
    assert_eq!(unique, keys.len(), "keys must stay distinct after rebalance");
}

#[tokio::test]
async fn concurrent_moves_serialize_without_key_collision() {
    let (store, _db) = store().await;
    for id in ["c1", "c2", "c3"] {
        store.pin(USER, OrderScene::Pinned, &conv(id)).await.unwrap();
    }
    // Two concurrent moves on the same scene. BEGIN IMMEDIATE serializes the
    // read-neighbours → compute → write, so neither reads a stale layout and
    // both commit as Moved.
    let a = {
        let store = store.clone();
        tokio::spawn(async move { store.move_item(USER, OrderScene::Pinned, &conv("c1"), None).await })
    };
    let b = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .move_item(USER, OrderScene::Pinned, &conv("c2"), Some(&conv("c3")))
                .await
        })
    };
    assert_eq!(a.await.unwrap().unwrap(), MoveOutcome::Moved);
    assert_eq!(b.await.unwrap().unwrap(), MoveOutcome::Moved);

    let rows = store.list_pinned(USER, OrderScene::Pinned, None, 10).await.unwrap();
    assert_eq!(rows.len(), 3, "no rows lost");
    let mut keys: Vec<i64> = rows.iter().map(|r| r.order_key).collect();
    keys.sort_unstable();
    let deduped = {
        let mut k = keys.clone();
        k.dedup();
        k.len()
    };
    assert_eq!(deduped, keys.len(), "concurrent moves must not collide keys");
}
