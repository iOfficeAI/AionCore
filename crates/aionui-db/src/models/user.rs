use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum UserType {
    Local,
    Aionpro,
}

impl UserType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Aionpro => "aionpro",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum UserStatus {
    Active,
    Disabled,
}

impl UserStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
        }
    }
}

/// Row mapping for the `users` table.
///
/// All fields match the SQLite column names and types exactly.
/// Optional fields correspond to nullable columns.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: String,
    pub user_type: UserType,
    pub external_user_id: Option<String>,
    pub username: Option<String>,
    pub email: Option<String>,
    pub password_hash: Option<String>,
    pub avatar_path: Option<String>,
    pub jwt_secret: Option<String>,
    pub status: UserStatus,
    pub session_generation: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub last_login: Option<TimestampMs>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalUserProjection {
    pub username: Option<String>,
    pub email: Option<String>,
    pub avatar_path: Option<String>,
}
