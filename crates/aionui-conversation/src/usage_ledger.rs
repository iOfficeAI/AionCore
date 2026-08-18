use std::sync::Arc;

use aionui_api_types::{UsageEventDto, UsageListResponse};
use aionui_common::now_ms;
use aionui_db::models::{ConversationRow, UsageEventRow};
use aionui_db::{IConversationRepository, IUsageEventRepository, InsertUsageEventParams};
use serde_json::Value;
use tracing::warn;

use crate::error::ConversationError;

const USAGE_RETENTION_MS: i64 = 180 * 24 * 60 * 60 * 1000;
const USAGE_MAX_EVENTS: i64 = 50_000;

pub struct ContextUsageSpend {
    pub total_tokens: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub thought_tokens: i64,
    pub cached_read_tokens: i64,
    pub cached_write_tokens: i64,
    pub session_cost_amount: Option<f64>,
    pub cost_currency: Option<String>,
    pub model_id: Option<String>,
}

fn non_negative_int(value: Option<&Value>) -> i64 {
    value
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n.round() as i64)
        .unwrap_or(0)
}

fn spend_counter(payload: &Value, meta: Option<&Value>, snake: &str, camel: &str) -> i64 {
    let nested = meta.and_then(|m| m.get("usage"));
    non_negative_int(
        meta.and_then(|m| m.get(snake))
            .or_else(|| nested.and_then(|u| u.get(camel)))
            .or_else(|| nested.and_then(|u| u.get(snake)))
            .or_else(|| payload.get(snake))
            .or_else(|| payload.get(camel)),
    )
}

/// Occupancy-only frames have `used`/`size` and no per-turn spend. Those are ignored.
pub fn spend_from_context_usage(payload: &Value) -> Option<ContextUsageSpend> {
    let meta = payload.get("_meta");
    let input_tokens = spend_counter(payload, meta, "input_tokens", "inputTokens");
    let output_tokens = spend_counter(payload, meta, "output_tokens", "outputTokens");
    let thought_tokens = spend_counter(payload, meta, "thought_tokens", "reasoningTokens");
    let cached_read_tokens = spend_counter(payload, meta, "cached_read_tokens", "cachedReadTokens");
    let cached_write_tokens = spend_counter(payload, meta, "cached_write_tokens", "cacheCreationTokens");
    let explicit_total_tokens = spend_counter(payload, meta, "total_tokens", "totalTokens");
    let cost = payload.get("cost");
    let session_cost_amount = cost
        .and_then(|c| c.get("amount"))
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0);
    let cost_currency = cost
        .and_then(|c| c.get("currency"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let model_id = meta
        .and_then(|m| m.get("model_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "unknown")
        .map(str::to_owned);

    if input_tokens + output_tokens + thought_tokens == 0 && session_cost_amount.is_none() {
        return None;
    }

    let cache_tokens = cached_read_tokens.saturating_add(cached_write_tokens);
    let input_includes_cache = cache_tokens > 0 && input_tokens >= cache_tokens;
    Some(ContextUsageSpend {
        total_tokens: if explicit_total_tokens > 0 {
            explicit_total_tokens
        } else {
            input_tokens
                .saturating_add(output_tokens)
                .saturating_add(if input_includes_cache { 0 } else { cache_tokens })
        },
        input_tokens,
        output_tokens,
        thought_tokens,
        cached_read_tokens,
        cached_write_tokens,
        session_cost_amount,
        cost_currency,
        model_id,
    })
}

fn extra_string(extra: &Value, key: &str) -> Option<String> {
    extra
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn conversation_backend(row: &ConversationRow, extra: &Value) -> String {
    if let Some(backend) = extra_string(extra, "backend") {
        return backend;
    }
    if row.r#type == "aionrs" {
        return "aionrs".into();
    }
    if !row.r#type.trim().is_empty() {
        return row.r#type.clone();
    }
    "unknown".into()
}

fn conversation_model_id(row: &ConversationRow, extra: &Value, hinted: Option<&str>) -> Option<String> {
    if let Some(model) = hinted.map(str::trim).filter(|s| !s.is_empty() && *s != "unknown") {
        return Some(model.to_owned());
    }
    if let Some(model) = extra_string(extra, "current_model_id") {
        return Some(model);
    }
    row.model.as_deref().and_then(|raw| {
        let value = serde_json::from_str::<Value>(raw).ok()?;
        value
            .get("use_model")
            .or_else(|| value.get("id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })
}

pub fn usage_event_fingerprint(turn_id: Option<&str>, spend: &ContextUsageSpend) -> String {
    if let Some(turn_id) = turn_id.map(str::trim).filter(|s| !s.is_empty()) {
        return format!("turn:{turn_id}");
    }
    format!(
        "counts:{}:{}:{}:{}:{}",
        spend.input_tokens,
        spend.output_tokens,
        spend.thought_tokens,
        spend.cached_read_tokens,
        spend.cached_write_tokens
    )
}

pub fn row_to_usage_dto(row: UsageEventRow) -> UsageEventDto {
    UsageEventDto {
        id: row.id,
        fingerprint: row.fingerprint,
        recorded_at: row.recorded_at,
        conversation_id: row.conversation_id,
        conversation_name: row.conversation_name,
        conversation_source: row.conversation_source,
        backend: row.backend,
        assistant_id: row.assistant_id,
        assistant_name: row.assistant_name,
        model_id: row.model_id,
        turn_id: row.turn_id,
        total_tokens: row.total_tokens,
        input_tokens: row.input_tokens,
        output_tokens: row.output_tokens,
        thought_tokens: row.thought_tokens,
        cached_read_tokens: row.cached_read_tokens,
        cached_write_tokens: row.cached_write_tokens,
        cost_delta: row.cost_delta,
        cost_currency: row.cost_currency,
        event_source: row.event_source,
    }
}

pub async fn record_context_usage_spend(
    usage_repo: &dyn IUsageEventRepository,
    conversation_repo: &dyn IConversationRepository,
    user_id: &str,
    conversation_id: &str,
    turn_id: Option<&str>,
    model_hint: Option<&str>,
    spend: &ContextUsageSpend,
) -> Result<Option<UsageEventRow>, ConversationError> {
    if user_id.trim().is_empty() || conversation_id.trim().is_empty() {
        return Ok(None);
    }

    let conversation = conversation_repo
        .get(user_id, conversation_id)
        .await
        .map_err(|e| ConversationError::internal(format!("Failed to load conversation for usage: {e}")))?;
    let Some(conversation) = conversation else {
        return Ok(None);
    };
    let extra = serde_json::from_str::<Value>(&conversation.extra).unwrap_or(Value::Null);
    let snapshot = conversation_repo
        .get_assistant_snapshot(user_id, conversation_id)
        .await
        .ok()
        .flatten();

    let previous_cost = usage_repo
        .last_session_cost(user_id, conversation_id)
        .await
        .map_err(|e| ConversationError::internal(format!("Failed to load previous usage cost: {e}")))?;
    let mut cost_delta = 0.0;
    let mut cost_currency = spend.cost_currency.clone();
    if let Some(amount) = spend.session_cost_amount {
        let previous_amount = match &previous_cost {
            Some((prev_amount, prev_currency))
                if cost_currency.as_deref().unwrap_or(prev_currency) == prev_currency.as_str() =>
            {
                *prev_amount
            }
            _ => 0.0,
        };
        cost_delta = (amount - previous_amount).max(0.0);
        if cost_currency.is_none() {
            cost_currency = previous_cost.map(|(_, currency)| currency);
        }
    }

    if spend.input_tokens + spend.output_tokens + spend.thought_tokens == 0 && cost_delta <= 0.0 {
        return Ok(None);
    }

    let fingerprint = usage_event_fingerprint(turn_id, spend);
    let backend = conversation_backend(&conversation, &extra);
    let event_source = if conversation.r#type == "aionrs" {
        "aionrs"
    } else {
        "acp"
    };
    let model_id = conversation_model_id(&conversation, &extra, model_hint.or(spend.model_id.as_deref()));
    let params = InsertUsageEventParams {
        user_id,
        conversation_id,
        recorded_at: now_ms(),
        fingerprint: &fingerprint,
        backend: &backend,
        conversation_source: conversation.source.as_deref().unwrap_or("aionui"),
        conversation_name: Some(conversation.name.as_str()).filter(|s| !s.is_empty()),
        assistant_id: snapshot.as_ref().map(|row| row.assistant_id.as_str()),
        assistant_name: None,
        model_id: model_id.as_deref(),
        turn_id,
        total_tokens: spend.total_tokens,
        input_tokens: spend.input_tokens,
        output_tokens: spend.output_tokens,
        thought_tokens: spend.thought_tokens,
        cached_read_tokens: spend.cached_read_tokens,
        cached_write_tokens: spend.cached_write_tokens,
        cost_delta,
        session_cost_amount: spend.session_cost_amount,
        cost_currency: cost_currency.as_deref(),
        event_source,
    };

    let inserted = usage_repo
        .insert_if_new(&params)
        .await
        .map_err(|e| ConversationError::internal(format!("Failed to record usage: {e}")))?;
    if inserted.is_some()
        && let Err(error) = usage_repo
            .prune_for_user(user_id, now_ms() - USAGE_RETENTION_MS, USAGE_MAX_EVENTS)
            .await
    {
        warn!(user_id, error = %error, "Failed to prune usage ledger");
    }
    Ok(inserted)
}

pub async fn list_usage_events(
    usage_repo: &dyn IUsageEventRepository,
    user_id: &str,
    since: Option<i64>,
    limit: Option<i64>,
) -> Result<UsageListResponse, ConversationError> {
    if let Err(error) = usage_repo
        .prune_for_user(user_id, now_ms() - USAGE_RETENTION_MS, USAGE_MAX_EVENTS)
        .await
    {
        warn!(user_id, error = %error, "Failed to prune usage ledger before listing");
    }
    let events = usage_repo
        .list_for_user(user_id, since, limit.unwrap_or(USAGE_MAX_EVENTS))
        .await
        .map_err(|e| ConversationError::internal(format!("Failed to list usage: {e}")))?;
    Ok(UsageListResponse {
        events: events.into_iter().map(row_to_usage_dto).collect(),
    })
}

pub async fn clear_usage_events(
    usage_repo: &dyn IUsageEventRepository,
    user_id: &str,
) -> Result<u64, ConversationError> {
    usage_repo
        .clear_for_user(user_id)
        .await
        .map_err(|e| ConversationError::internal(format!("Failed to clear usage: {e}")))
}

pub fn maybe_usage_repo(
    repo: &std::sync::RwLock<Option<Arc<dyn IUsageEventRepository>>>,
) -> Option<Arc<dyn IUsageEventRepository>> {
    repo.read().ok().and_then(|guard| guard.clone())
}

#[cfg(test)]
mod tests {
    use super::spend_from_context_usage;
    use serde_json::json;

    #[test]
    fn occupancy_only_frames_are_not_spend() {
        assert!(spend_from_context_usage(&json!({ "used": 12_000, "size": 200_000 })).is_none());
    }

    #[test]
    fn end_of_turn_meta_is_spend() {
        let spend = spend_from_context_usage(&json!({
            "used": 12_400,
            "size": 200_000,
            "_meta": { "input_tokens": 900, "output_tokens": 80 }
        }))
        .expect("spend");
        assert_eq!(spend.input_tokens, 900);
        assert_eq!(spend.output_tokens, 80);
        assert_eq!(spend.total_tokens, 980);
    }

    #[test]
    fn grok_nested_usage_is_spend() {
        let spend = spend_from_context_usage(&json!({
            "used": 14_794,
            "size": 0,
            "_meta": {
                "usage": {
                    "inputTokens": 14675,
                    "outputTokens": 119,
                    "reasoningTokens": 77,
                    "cachedReadTokens": 11648
                }
            }
        }))
        .expect("spend");
        assert_eq!(spend.input_tokens, 14675);
        assert_eq!(spend.output_tokens, 119);
        assert_eq!(spend.thought_tokens, 77);
        assert_eq!(spend.cached_read_tokens, 11648);
        assert_eq!(spend.total_tokens, 14794);
    }

    #[test]
    fn explicit_turn_total_is_preserved() {
        let spend = spend_from_context_usage(&json!({
            "used": 200_000,
            "size": 200_000,
            "_meta": {
                "total_tokens": 41_224,
                "input_tokens": 919,
                "output_tokens": 305,
                "cached_read_tokens": 20_000,
                "cached_write_tokens": 20_000
            }
        }))
        .expect("spend");
        assert_eq!(spend.total_tokens, 41_224);
    }
}
