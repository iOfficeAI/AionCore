//! Grok ACP spend is not a `UsageUpdate` notification.
//!
//! Captured wire (`~/.grok/sessions/**/updates.jsonl`, 2026-08-17
//! grok-temp-aa8ec77a): Grok emits `_x.ai/session/update` with
//! `sessionUpdate: "turn_completed"` and a camelCase `usage` object. Standard
//! ACP `session/update` frames only carry `_meta.totalTokens` (context
//! occupancy) and are not spend. Official grok user-guide
//! `14-headless-mode.md`: `costUsdTicks` is 10^10 ticks per USD; ACP
//! `_meta.usage.inputTokens` is the full prompt sum.
//!
//! This module turns that captured object into the same `{used, size, _meta}`
//! AcpContextUsage frame StreamRelay already records.

use serde_json::{Map, Value};

/// Official grok scale: 1 USD = 10^10 ticks (`14-headless-mode.md`).
const USD_TICKS_PER_DOLLAR: f64 = 10_000_000_000.0;

/// Parse one raw JSON-RPC line. `None` unless it is Grok's turn-completed spend.
pub(crate) fn frame_from_jsonrpc_line(line: &str) -> Option<Value> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("method").and_then(Value::as_str) != Some("_x.ai/session/update") {
        return None;
    }
    let update = value.pointer("/params/update")?;
    if update.get("sessionUpdate").and_then(Value::as_str) != Some("turn_completed") {
        return None;
    }
    frame_from_usage_object(update.get("usage")?)
}

/// Prompt-response `_meta` dialect: nested `usage` (whole-prompt billing).
/// `totalTokens: 0` is Grok's compact / session-info replay marker — skip it.
pub(crate) fn frame_from_prompt_meta(meta: &Map<String, Value>) -> Option<Value> {
    if meta.get("totalTokens").and_then(Value::as_u64) == Some(0) {
        return None;
    }
    frame_from_usage_object(meta.get("usage")?)
}

pub(crate) fn frame_from_usage_object(usage: &Value) -> Option<Value> {
    let input = first_u64(usage, &["inputTokens", "input_tokens"]);
    let output = first_u64(usage, &["outputTokens", "output_tokens"]);
    let thought = first_u64(usage, &["reasoningTokens", "thought_tokens"]);
    let cached_read = first_u64(usage, &["cachedReadTokens", "cached_read_tokens"]);
    let cached_write = first_u64(
        usage,
        &["cacheCreationTokens", "cachedWriteTokens", "cached_write_tokens"],
    );
    let billed_total = first_u64(usage, &["totalTokens", "total_tokens"]);
    if billed_total == 0 && input + output + thought == 0 {
        return None;
    }

    let mut breakdown = serde_json::json!({
        "input_tokens": input,
        "output_tokens": output,
    });
    if thought > 0 {
        breakdown["thought_tokens"] = thought.into();
    }
    if cached_read > 0 {
        breakdown["cached_read_tokens"] = cached_read.into();
    }
    if cached_write > 0 {
        breakdown["cached_write_tokens"] = cached_write.into();
    }
    if let Some(model_id) = model_id_from_usage(usage) {
        breakdown["model_id"] = model_id.into();
    }

    let used = if billed_total > 0 {
        billed_total
    } else {
        input.saturating_add(output).saturating_add(thought)
    };
    let mut frame = serde_json::json!({
        "used": used,
        "size": 0,
        "_meta": breakdown,
    });
    if let Some(amount) = cost_usd_from_usage(usage) {
        frame["cost"] = serde_json::json!({ "amount": amount, "currency": "USD" });
    }
    Some(frame)
}

fn first_u64(value: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|key| {
            let raw = value.get(*key)?;
            raw.as_u64()
                .or_else(|| {
                    raw.as_f64()
                        .filter(|n| n.is_finite() && *n > 0.0)
                        .map(|n| n.round() as u64)
                })
                .filter(|n| *n > 0)
        })
        .unwrap_or(0)
}

fn cost_usd_from_usage(usage: &Value) -> Option<f64> {
    if let Some(ticks) = usage
        .get("costUsdTicks")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
    {
        return Some(ticks / USD_TICKS_PER_DOLLAR);
    }
    usage
        .get("cost")
        .and_then(|cost| cost.get("amount"))
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
}

fn model_id_from_usage(usage: &Value) -> Option<String> {
    usage
        .get("modelUsage")
        .and_then(Value::as_object)
        .and_then(|models| models.keys().next())
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Exact envelope from the 2026-08-17 WorkMate grok session `updates.jsonl`.
    const TURN_COMPLETED_LINE: &str = r#"{"timestamp":1786923282,"method":"_x.ai/session/update","params":{"sessionId":"01a00ced-5b1f-7c82-b7a6-d92a3842bd85","update":{"sessionUpdate":"turn_completed","prompt_id":"2078a2a5-c38d-488f-b7be-3bdc70c4f519","stop_reason":"end_turn","usage":{"inputTokens":14675,"outputTokens":119,"totalTokens":14794,"cachedReadTokens":11648,"cacheCreationTokens":0,"reasoningTokens":77,"modelCalls":1,"apiDurationMs":4579,"costUsdTicks":21406400,"modelUsage":{"grok-4.6-build":{"inputTokens":14675,"outputTokens":119,"totalTokens":14794,"cachedReadTokens":11648,"cacheCreationTokens":0,"reasoningTokens":77,"modelCalls":1,"apiDurationMs":4579,"costUsdTicks":21406400}},"numTurns":1}},"_meta":{"eventId":"01a00ced-5b1f-7c82-b7a6-d92a3842bd85-83","agentTimestampMs":1786923282125}}}"#;

    #[test]
    fn captured_turn_completed_is_spend() {
        let frame = frame_from_jsonrpc_line(TURN_COMPLETED_LINE).expect("grok spend");
        assert_eq!(frame["used"], 14794);
        assert_eq!(frame["_meta"]["input_tokens"], 14675);
        assert_eq!(frame["_meta"]["output_tokens"], 119);
        assert_eq!(frame["_meta"]["thought_tokens"], 77);
        assert_eq!(frame["_meta"]["cached_read_tokens"], 11648);
        assert_eq!(frame["_meta"]["model_id"], "grok-4.6-build");
        let amount = frame["cost"]["amount"].as_f64().expect("cost");
        assert!((amount - 0.00214064).abs() < 1e-12);
        assert_eq!(frame["cost"]["currency"], "USD");
    }

    #[test]
    fn occupancy_only_session_update_is_not_spend() {
        let line = r#"{"method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}},"_meta":{"totalTokens":4634}}}"#;
        assert!(frame_from_jsonrpc_line(line).is_none());
    }

    #[test]
    fn other_xai_updates_are_ignored() {
        let line = r#"{"method":"_x.ai/session/update","params":{"update":{"sessionUpdate":"auto_compact_completed","tokens_after":100}}}"#;
        assert!(frame_from_jsonrpc_line(line).is_none());
    }

    #[test]
    fn prompt_meta_usage_is_spend() {
        let meta = json!({
            "totalTokens": 15044,
            "usage": {
                "inputTokens": 14883,
                "outputTokens": 40,
                "reasoningTokens": 28,
                "costUsdTicks": 21307800
            }
        });
        let frame = frame_from_prompt_meta(meta.as_object().expect("object")).expect("spend");
        assert_eq!(frame["_meta"]["input_tokens"], 14883);
        assert_eq!(frame["_meta"]["output_tokens"], 40);
        assert_eq!(frame["_meta"]["thought_tokens"], 28);
    }

    #[test]
    fn zero_total_tokens_prompt_meta_is_compact_replay() {
        let meta = json!({
            "totalTokens": 0,
            "usage": { "inputTokens": 14883, "outputTokens": 40 }
        });
        assert!(frame_from_prompt_meta(meta.as_object().expect("object")).is_none());
    }

    #[test]
    fn all_zero_usage_is_not_spend() {
        assert!(
            frame_from_usage_object(&json!({
                "inputTokens": 0,
                "outputTokens": 0,
                "totalTokens": 0
            }))
            .is_none()
        );
    }
}
