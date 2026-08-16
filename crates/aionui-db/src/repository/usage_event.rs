use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::UsageEventRow;

/// Insert payload for one spend event. `id` is minted by the repository.
#[derive(Debug, Clone)]
pub struct InsertUsageEventParams<'a> {
    pub user_id: &'a str,
    pub conversation_id: &'a str,
    pub recorded_at: TimestampMs,
    pub fingerprint: &'a str,
    pub backend: &'a str,
    pub conversation_source: &'a str,
    pub conversation_name: Option<&'a str>,
    pub assistant_id: Option<&'a str>,
    pub assistant_name: Option<&'a str>,
    pub model_id: Option<&'a str>,
    pub turn_id: Option<&'a str>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub thought_tokens: i64,
    pub cached_read_tokens: i64,
    pub cached_write_tokens: i64,
    pub cost_delta: f64,
    pub session_cost_amount: Option<f64>,
    pub cost_currency: Option<&'a str>,
    pub event_source: &'a str,
}

#[async_trait::async_trait]
pub trait IUsageEventRepository: Send + Sync {
    /// Insert a spend event. Returns `Ok(None)` when the fingerprint already exists.
    async fn insert_if_new(&self, params: &InsertUsageEventParams<'_>) -> Result<Option<UsageEventRow>, DbError>;

    async fn list_for_user(
        &self,
        user_id: &str,
        since: Option<TimestampMs>,
        limit: i64,
    ) -> Result<Vec<UsageEventRow>, DbError>;

    async fn last_session_cost(&self, user_id: &str, conversation_id: &str) -> Result<Option<(f64, String)>, DbError>;

    async fn clear_for_user(&self, user_id: &str) -> Result<u64, DbError>;

    async fn prune_for_user(&self, user_id: &str, cutoff: TimestampMs, max_events: i64) -> Result<(), DbError>;
}
