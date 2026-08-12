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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum SiteRole {
    Admin,
    Member,
}

impl SiteRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }
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
    pub site_role: SiteRole,
    pub must_change_password: bool,
    pub session_generation: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub last_login: Option<TimestampMs>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditActor {
    pub user_id: Option<String>,
    pub username: Option<String>,
}

impl AuditActor {
    pub fn system() -> Self {
        Self {
            user_id: None,
            username: None,
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AdminAuditRecord {
    pub id: String,
    pub occurred_at: TimestampMs,
    pub actor_user_id: Option<String>,
    pub actor_username: Option<String>,
    pub action: String,
    pub target_user_id: Option<String>,
    pub target_username: Option<String>,
    pub details: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalUserProjection {
    pub username: Option<String>,
    pub email: Option<String>,
    pub avatar_path: Option<String>,
}
