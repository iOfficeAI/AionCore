use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageEventDto {
    pub id: String,
    pub recorded_at: TimestampMs,
    pub conversation_id: String,
    pub conversation_name: Option<String>,
    pub conversation_source: String,
    pub backend: String,
    pub assistant_id: Option<String>,
    pub assistant_name: Option<String>,
    pub model_id: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub thought_tokens: i64,
    pub cached_read_tokens: i64,
    pub cached_write_tokens: i64,
    pub cost_delta: f64,
    pub cost_currency: Option<String>,
    pub event_source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UsageListQuery {
    pub since: Option<TimestampMs>,
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageListResponse {
    pub events: Vec<UsageEventDto>,
}
