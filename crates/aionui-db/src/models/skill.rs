use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `skills` table.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SkillRow {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub path: String,
    pub source: String,
    pub enabled: bool,
    pub deleted_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row mapping for the `skill_import_records` table.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SkillImportRecordRow {
    pub id: String,
    pub operation_id: String,
    pub source_label: String,
    pub source_path: Option<String>,
    pub source_name: String,
    pub skill_id: Option<String>,
    pub skill_name: Option<String>,
    pub status: String,
    pub error_code: Option<String>,
    pub error_path: Option<String>,
    pub actual_bytes: Option<i64>,
    pub limit_bytes: Option<i64>,
    pub line: Option<i64>,
    pub column: Option<i64>,
    pub created_at: TimestampMs,
}
