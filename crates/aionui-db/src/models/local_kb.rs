//! Row models for M9 E15 local-kb (not experience library / KnowHub).

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct LocalKbSettingsRow {
    pub user_id: String,
    pub enabled: i64,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct LocalKbSourceRow {
    pub id: String,
    pub user_id: String,
    pub kind: String,
    pub label: String,
    pub origin_path: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct LocalKbDocumentRow {
    pub id: String,
    pub source_id: String,
    pub user_id: String,
    pub title: String,
    pub body_text: String,
    pub content_hash: String,
    pub relative_path: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone)]
pub struct UpsertLocalKbSettingsParams<'a> {
    pub user_id: &'a str,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct CreateLocalKbSourceParams<'a> {
    pub id: &'a str,
    pub user_id: &'a str,
    pub kind: &'a str,
    pub label: &'a str,
    pub origin_path: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct CreateLocalKbDocumentParams<'a> {
    pub id: &'a str,
    pub source_id: &'a str,
    pub user_id: &'a str,
    pub title: &'a str,
    pub body_text: &'a str,
    pub content_hash: &'a str,
    pub relative_path: Option<&'a str>,
}
