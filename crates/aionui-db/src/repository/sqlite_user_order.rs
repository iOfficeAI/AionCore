use aionui_common::now_ms;
use sqlx::{SqliteConnection, SqlitePool};

use crate::error::DbError;
use crate::models::{OrderItemType, OrderScene, UserOrderRow};
use crate::repository::user_order::{IUserOrderStore, MoveOutcome, OrderItemRef, PinOutcome, PinnedCursor};

/// Gap between adjacent pins; a fresh top pin claims `min - PIN_GAP`, and a
/// rebalance spaces rows `PIN_GAP` apart.
const PIN_GAP: i64 = 1000;

/// Minimum neighbour gap a `move` needs to fit an integer midpoint with room to
/// spare (D4). A smaller gap triggers a whole-scene rebalance rather than
/// risking a midpoint that ties an existing key.
const REBALANCE_THRESHOLD: i64 = 16;

const USER_ORDER_COLS: &str = "user_id, scene, item_type, item_id, order_key, created_at, updated_at";

/// SQLite-backed implementation of [`IUserOrderStore`].
#[derive(Clone, Debug)]
pub struct SqliteUserOrderStore {
    pool: SqlitePool,
}

impl SqliteUserOrderStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IUserOrderStore for SqliteUserOrderStore {
    async fn pin(&self, user_id: &str, scene: OrderScene, item: &OrderItemRef) -> Result<PinOutcome, DbError> {
        // BEGIN IMMEDIATE claims the writer lock up front so the
        // read-min-then-insert is atomic: two concurrent pins can't both read
        // the same MIN and insert colliding top rows (the second queues on the
        // busy handler). Same pattern as `insert_message_once`.
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;

        let result: Result<PinOutcome, DbError> = async {
            let exists: i64 = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM user_order \
                 WHERE user_id = ? AND scene = ? AND item_type = ? AND item_id = ?)",
            )
            .bind(user_id)
            .bind(scene.as_str())
            .bind(item.item_type.as_str())
            .bind(&item.item_id)
            .fetch_one(&mut *connection)
            .await?;
            if exists != 0 {
                return Ok(PinOutcome::AlreadyPinned);
            }

            // Empty scene → NULL MIN → start at PIN_GAP; otherwise one gap above
            // the current top. order_key is not unique, so no collision handling
            // is needed here.
            let min_key: Option<i64> =
                sqlx::query_scalar("SELECT MIN(order_key) FROM user_order WHERE user_id = ? AND scene = ?")
                    .bind(user_id)
                    .bind(scene.as_str())
                    .fetch_one(&mut *connection)
                    .await?;
            let order_key = min_key.map(|min| min - PIN_GAP).unwrap_or(PIN_GAP);

            let now = now_ms();
            sqlx::query(
                "INSERT INTO user_order (user_id, scene, item_type, item_id, order_key, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(user_id)
            .bind(scene.as_str())
            .bind(item.item_type.as_str())
            .bind(&item.item_id)
            .bind(order_key)
            .bind(now)
            .bind(now)
            .execute(&mut *connection)
            .await?;
            Ok(PinOutcome::Inserted)
        }
        .await;

        match result {
            Ok(outcome) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(outcome)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn unpin(&self, user_id: &str, scene: OrderScene, item: &OrderItemRef) -> Result<bool, DbError> {
        let result = sqlx::query(
            "DELETE FROM user_order \
             WHERE user_id = ? AND scene = ? AND item_type = ? AND item_id = ?",
        )
        .bind(user_id)
        .bind(scene.as_str())
        .bind(item.item_type.as_str())
        .bind(&item.item_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn move_item(
        &self,
        user_id: &str,
        scene: OrderScene,
        moved: &OrderItemRef,
        after: Option<&OrderItemRef>,
    ) -> Result<MoveOutcome, DbError> {
        // BEGIN IMMEDIATE up front: read neighbours → compute key → write, all
        // under the writer lock, so concurrent moves (multi-window / multi-
        // instance) cannot both read stale neighbours and race their writes.
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;

        let result: Result<MoveOutcome, DbError> = async {
            if row_order_key(&mut connection, user_id, scene, moved).await?.is_none() {
                return Ok(MoveOutcome::MovedNotFound);
            }

            let new_key = match after {
                // Move to top: one gap below the current minimum. `moved` exists,
                // so MIN is non-null; the result is strictly below every row,
                // including `moved` itself, so it becomes the new top.
                None => min_order_key(&mut connection, user_id, scene).await?.unwrap_or(PIN_GAP) - PIN_GAP,
                Some(after) => {
                    let Some(left) = row_order_key(&mut connection, user_id, scene, after).await? else {
                        return Ok(MoveOutcome::AfterNotFound);
                    };
                    // Successor of `after` excluding `moved` (its stale slot must
                    // not count as the neighbour we squeeze into).
                    match successor_key(&mut connection, user_id, scene, left, after, moved).await? {
                        None => left + PIN_GAP,
                        Some(right) if right - left >= REBALANCE_THRESHOLD => left + (right - left) / 2,
                        Some(_) => {
                            // Gap exhausted: respace the whole scene 1000 apart,
                            // then recompute against the fresh keys. After a
                            // rebalance neighbour gaps are >= PIN_GAP, so the
                            // midpoint is guaranteed strictly between them.
                            rebalance_scene(&mut connection, user_id, scene).await?;
                            let left = row_order_key(&mut connection, user_id, scene, after)
                                .await?
                                .ok_or_else(|| DbError::NotFound("after row vanished during rebalance".into()))?;
                            match successor_key(&mut connection, user_id, scene, left, after, moved).await? {
                                None => left + PIN_GAP,
                                Some(right) => left + (right - left) / 2,
                            }
                        }
                    }
                }
            };

            sqlx::query(
                "UPDATE user_order SET order_key = ?, updated_at = ? \
                 WHERE user_id = ? AND scene = ? AND item_type = ? AND item_id = ?",
            )
            .bind(new_key)
            .bind(now_ms())
            .bind(user_id)
            .bind(scene.as_str())
            .bind(moved.item_type.as_str())
            .bind(&moved.item_id)
            .execute(&mut *connection)
            .await?;
            Ok(MoveOutcome::Moved)
        }
        .await;

        match result {
            Ok(outcome) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(outcome)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn list_pinned(
        &self,
        user_id: &str,
        scene: OrderScene,
        after: Option<&PinnedCursor>,
        limit: i64,
    ) -> Result<Vec<UserOrderRow>, DbError> {
        // Keyset on (order_key, item_type, item_id). Expanded lexicographic form
        // (rather than a row-value tuple) so the leading order_key range uses
        // idx_user_order_scene.
        let rows = match after {
            None => {
                sqlx::query_as::<_, UserOrderRow>(&format!(
                    "SELECT {USER_ORDER_COLS} FROM user_order \
                     WHERE user_id = ? AND scene = ? \
                     ORDER BY order_key ASC, item_type ASC, item_id ASC \
                     LIMIT ?"
                ))
                .bind(user_id)
                .bind(scene.as_str())
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
            Some(cursor) => {
                sqlx::query_as::<_, UserOrderRow>(&format!(
                    "SELECT {USER_ORDER_COLS} FROM user_order \
                     WHERE user_id = ? AND scene = ? AND ( \
                        order_key > ? OR \
                        (order_key = ? AND (item_type > ? OR (item_type = ? AND item_id > ?))) \
                     ) \
                     ORDER BY order_key ASC, item_type ASC, item_id ASC \
                     LIMIT ?"
                ))
                .bind(user_id)
                .bind(scene.as_str())
                .bind(cursor.order_key)
                .bind(cursor.order_key)
                .bind(cursor.item_type.as_str())
                .bind(cursor.item_type.as_str())
                .bind(&cursor.item_id)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows)
    }

    async fn pinned_refs(&self, user_id: &str, scene: OrderScene) -> Result<Vec<OrderItemRef>, DbError> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT item_type, item_id FROM user_order WHERE user_id = ? AND scene = ?")
                .bind(user_id)
                .bind(scene.as_str())
                .fetch_all(&self.pool)
                .await?;
        // Skip rows whose item_type is out of the enum (defensive; the write
        // path only ever stores known values).
        Ok(rows
            .into_iter()
            .filter_map(|(item_type, item_id)| {
                OrderItemType::parse(&item_type).map(|item_type| OrderItemRef { item_type, item_id })
            })
            .collect())
    }

    async fn remove_item(&self, user_id: &str, item: &OrderItemRef) -> Result<(), DbError> {
        sqlx::query("DELETE FROM user_order WHERE user_id = ? AND item_type = ? AND item_id = ?")
            .bind(user_id)
            .bind(item.item_type.as_str())
            .bind(&item.item_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn remove_items(&self, user_id: &str, items: &[OrderItemRef]) -> Result<(), DbError> {
        if items.is_empty() {
            return Ok(());
        }
        // One transaction so the cascade is atomic: either every referenced
        // row is gone or none are (a mid-batch failure rolls back).
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;

        let result: Result<(), DbError> = async {
            for item in items {
                sqlx::query("DELETE FROM user_order WHERE user_id = ? AND item_type = ? AND item_id = ?")
                    .bind(user_id)
                    .bind(item.item_type.as_str())
                    .bind(&item.item_id)
                    .execute(&mut *connection)
                    .await?;
            }
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(())
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }
}

/// The `order_key` of one `(item_type, item_id)` row in a scene, or `None` when
/// the row is absent (stale window). Used by `move_item` to resolve the `moved`
/// and `after` anchors under the write lock.
async fn row_order_key(
    conn: &mut SqliteConnection,
    user_id: &str,
    scene: OrderScene,
    item: &OrderItemRef,
) -> Result<Option<i64>, DbError> {
    let key: Option<i64> = sqlx::query_scalar(
        "SELECT order_key FROM user_order \
         WHERE user_id = ? AND scene = ? AND item_type = ? AND item_id = ?",
    )
    .bind(user_id)
    .bind(scene.as_str())
    .bind(item.item_type.as_str())
    .bind(&item.item_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(key)
}

/// The smallest `order_key` in a scene, or `None` when the scene is empty.
async fn min_order_key(conn: &mut SqliteConnection, user_id: &str, scene: OrderScene) -> Result<Option<i64>, DbError> {
    let min_key: Option<i64> =
        sqlx::query_scalar("SELECT MIN(order_key) FROM user_order WHERE user_id = ? AND scene = ?")
            .bind(user_id)
            .bind(scene.as_str())
            .fetch_one(&mut *conn)
            .await?;
    Ok(min_key)
}

/// The `order_key` of `after`'s immediate successor in `(order_key, item_type,
/// item_id)` order, **excluding `moved`** (its stale slot must not count as the
/// neighbour). `None` when `after` is the last row (ignoring `moved`).
///
/// The `moved` exclusion is what lets a drag land right after its old neighbour:
/// were `moved` still counted, dragging it one slot down would pick `moved`
/// itself as the successor and wedge the new key against its own stale value.
async fn successor_key(
    conn: &mut SqliteConnection,
    user_id: &str,
    scene: OrderScene,
    after_key: i64,
    after: &OrderItemRef,
    moved: &OrderItemRef,
) -> Result<Option<i64>, DbError> {
    // Expanded lexicographic ">" on the (order_key, item_type, item_id) triple,
    // mirroring the keyset in `list_pinned`, so the leading order_key range uses
    // idx_user_order_scene. The trailing NOT(item_type=? AND item_id=?) drops
    // `moved` from the candidate set.
    let key: Option<i64> = sqlx::query_scalar(
        "SELECT order_key FROM user_order \
         WHERE user_id = ? AND scene = ? \
           AND (order_key > ? OR (order_key = ? AND (item_type > ? OR (item_type = ? AND item_id > ?)))) \
           AND NOT (item_type = ? AND item_id = ?) \
         ORDER BY order_key ASC, item_type ASC, item_id ASC \
         LIMIT 1",
    )
    .bind(user_id)
    .bind(scene.as_str())
    .bind(after_key)
    .bind(after_key)
    .bind(after.item_type.as_str())
    .bind(after.item_type.as_str())
    .bind(&after.item_id)
    .bind(moved.item_type.as_str())
    .bind(&moved.item_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(key)
}

/// Respace an entire scene's `order_key`s to `PIN_GAP, 2*PIN_GAP, …` in their
/// current `(order_key, item_type, item_id)` order, restoring uniform gaps when
/// a `move` has exhausted the room between two neighbours. Runs inside the
/// caller's `BEGIN IMMEDIATE` transaction, so the read-then-rewrite is atomic.
async fn rebalance_scene(conn: &mut SqliteConnection, user_id: &str, scene: OrderScene) -> Result<(), DbError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT item_type, item_id FROM user_order \
         WHERE user_id = ? AND scene = ? \
         ORDER BY order_key ASC, item_type ASC, item_id ASC",
    )
    .bind(user_id)
    .bind(scene.as_str())
    .fetch_all(&mut *conn)
    .await?;

    let now = now_ms();
    for (index, (item_type, item_id)) in rows.into_iter().enumerate() {
        let order_key = (index as i64 + 1) * PIN_GAP;
        sqlx::query(
            "UPDATE user_order SET order_key = ?, updated_at = ? \
             WHERE user_id = ? AND scene = ? AND item_type = ? AND item_id = ?",
        )
        .bind(order_key)
        .bind(now)
        .bind(user_id)
        .bind(scene.as_str())
        .bind(&item_type)
        .bind(&item_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "sqlite_user_order_test.rs"]
mod sqlite_user_order_test;
