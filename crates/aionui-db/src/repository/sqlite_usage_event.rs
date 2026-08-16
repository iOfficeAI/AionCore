use aionui_common::{TimestampMs, generate_prefixed_id};
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::UsageEventRow;
use crate::repository::usage_event::{IUsageEventRepository, InsertUsageEventParams};

#[derive(Clone, Debug)]
pub struct SqliteUsageEventRepository {
    pool: SqlitePool,
}

impl SqliteUsageEventRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IUsageEventRepository for SqliteUsageEventRepository {
    async fn insert_if_new(&self, params: &InsertUsageEventParams<'_>) -> Result<Option<UsageEventRow>, DbError> {
        let id = generate_prefixed_id("usage");
        let result = sqlx::query(
            "INSERT OR IGNORE INTO usage_events (\
                id, user_id, conversation_id, recorded_at, fingerprint, backend, conversation_source, \
                conversation_name, assistant_id, assistant_name, model_id, turn_id, \
                input_tokens, output_tokens, thought_tokens, cached_read_tokens, cached_write_tokens, \
                cost_delta, session_cost_amount, cost_currency, event_source\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(params.user_id)
        .bind(params.conversation_id)
        .bind(params.recorded_at)
        .bind(params.fingerprint)
        .bind(params.backend)
        .bind(params.conversation_source)
        .bind(params.conversation_name)
        .bind(params.assistant_id)
        .bind(params.assistant_name)
        .bind(params.model_id)
        .bind(params.turn_id)
        .bind(params.input_tokens)
        .bind(params.output_tokens)
        .bind(params.thought_tokens)
        .bind(params.cached_read_tokens)
        .bind(params.cached_write_tokens)
        .bind(params.cost_delta)
        .bind(params.session_cost_amount)
        .bind(params.cost_currency)
        .bind(params.event_source)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Ok(None);
        }

        Ok(Some(UsageEventRow {
            id,
            user_id: params.user_id.to_owned(),
            conversation_id: params.conversation_id.to_owned(),
            recorded_at: params.recorded_at,
            fingerprint: params.fingerprint.to_owned(),
            backend: params.backend.to_owned(),
            conversation_source: params.conversation_source.to_owned(),
            conversation_name: params.conversation_name.map(str::to_owned),
            assistant_id: params.assistant_id.map(str::to_owned),
            assistant_name: params.assistant_name.map(str::to_owned),
            model_id: params.model_id.map(str::to_owned),
            turn_id: params.turn_id.map(str::to_owned),
            input_tokens: params.input_tokens,
            output_tokens: params.output_tokens,
            thought_tokens: params.thought_tokens,
            cached_read_tokens: params.cached_read_tokens,
            cached_write_tokens: params.cached_write_tokens,
            cost_delta: params.cost_delta,
            session_cost_amount: params.session_cost_amount,
            cost_currency: params.cost_currency.map(str::to_owned),
            event_source: params.event_source.to_owned(),
        }))
    }

    async fn list_for_user(
        &self,
        user_id: &str,
        since: Option<TimestampMs>,
        limit: i64,
    ) -> Result<Vec<UsageEventRow>, DbError> {
        let limit = limit.clamp(1, 5_000);
        let rows = if let Some(since) = since {
            sqlx::query_as::<_, UsageEventRow>(
                "SELECT id, user_id, conversation_id, recorded_at, fingerprint, backend, conversation_source, \
                 conversation_name, assistant_id, assistant_name, model_id, turn_id, \
                 input_tokens, output_tokens, thought_tokens, cached_read_tokens, cached_write_tokens, \
                 cost_delta, session_cost_amount, cost_currency, event_source \
                 FROM usage_events WHERE user_id = ? AND recorded_at >= ? \
                 ORDER BY recorded_at ASC LIMIT ?",
            )
            .bind(user_id)
            .bind(since)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, UsageEventRow>(
                "SELECT id, user_id, conversation_id, recorded_at, fingerprint, backend, conversation_source, \
                 conversation_name, assistant_id, assistant_name, model_id, turn_id, \
                 input_tokens, output_tokens, thought_tokens, cached_read_tokens, cached_write_tokens, \
                 cost_delta, session_cost_amount, cost_currency, event_source \
                 FROM usage_events WHERE user_id = ? \
                 ORDER BY recorded_at ASC LIMIT ?",
            )
            .bind(user_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows)
    }

    async fn last_session_cost(&self, user_id: &str, conversation_id: &str) -> Result<Option<(f64, String)>, DbError> {
        let row: Option<(f64, Option<String>)> = sqlx::query_as(
            "SELECT session_cost_amount, cost_currency FROM usage_events \
             WHERE user_id = ? AND conversation_id = ? AND session_cost_amount IS NOT NULL \
             ORDER BY recorded_at DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(amount, currency)| currency.map(|code| (amount, code))))
    }

    async fn clear_for_user(&self, user_id: &str) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM usage_events WHERE user_id = ?")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    async fn prune_for_user(&self, user_id: &str, cutoff: TimestampMs, max_events: i64) -> Result<(), DbError> {
        sqlx::query("DELETE FROM usage_events WHERE user_id = ? AND recorded_at < ?")
            .bind(user_id)
            .bind(cutoff)
            .execute(&self.pool)
            .await?;

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_events WHERE user_id = ?")
            .bind(user_id)
            .fetch_one(&self.pool)
            .await?;
        if count > max_events {
            let overflow = count - max_events;
            sqlx::query(
                "DELETE FROM usage_events WHERE id IN (\
                    SELECT id FROM usage_events WHERE user_id = ? ORDER BY recorded_at ASC LIMIT ?\
                )",
            )
            .bind(user_id)
            .bind(overflow)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }
}
