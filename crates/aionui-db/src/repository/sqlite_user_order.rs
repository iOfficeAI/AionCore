use aionui_common::now_ms;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{OrderItemType, OrderScene, UserOrderRow};
use crate::repository::user_order::{IUserOrderStore, OrderItemRef, PinOutcome, PinnedCursor};

/// Gap between adjacent pins; a fresh top pin claims `min - PIN_GAP`.
const PIN_GAP: i64 = 1000;

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

#[cfg(test)]
#[path = "sqlite_user_order_test.rs"]
mod sqlite_user_order_test;
