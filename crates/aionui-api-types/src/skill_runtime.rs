//! Wire types for the agent-facing `aioncore skills` domain (channel A).
//!
//! Deliberately separate from `skill.rs`, which is the read-WRITE management
//! surface listing every importable skill. This domain is read-only and scoped
//! to ONE conversation's `extra.skills` snapshot: different semantics, different
//! authority, and merging them would let a runtime token reach the management
//! surface.
//!
//! The envelope mirrors the `session` CLI's shape (success flag + data + error +
//! meta) because the agent already knows how to read that, but it carries its
//! own error codes rather than borrowing the session domain's.

use serde::{Deserialize, Serialize};

pub const SKILL_RUNTIME_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillRuntimeErrorCode {
    /// The runtime token was missing, malformed, or not valid for this
    /// (user, conversation) pair.
    RuntimeAuthFailed,
    /// The conversation does not exist for this user.
    ConversationNotFound,
    /// The skill exists somewhere, but is not enabled in THIS conversation. A
    /// distinct code from `skill_not_found` on purpose: the agent should stop
    /// asking rather than retry, and an operator reading logs should be able to
    /// tell a snapshot mismatch from a missing file.
    SkillNotEnabled,
    /// Enabled in the snapshot, but no source directory resolves for this user.
    SkillNotFound,
    /// The requested relative path escaped the skill directory, or was absolute.
    InvalidPath,
    /// Malformed request (missing/unknown stdin field, unreadable file).
    SchemaValidationFailed,
    /// The CLI could not reach the backend at all.
    TransportUnavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRuntimeErrorPayload {
    pub code: SkillRuntimeErrorCode,
    pub message: String,
}

impl SkillRuntimeErrorPayload {
    pub fn new(code: SkillRuntimeErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRuntimeMeta {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRuntimeEnvelope<T> {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<SkillRuntimeErrorPayload>,
    pub meta: SkillRuntimeMeta,
}

impl<T> SkillRuntimeEnvelope<T> {
    pub fn success(data: T, command: Option<String>) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            meta: SkillRuntimeMeta {
                schema_version: SKILL_RUNTIME_SCHEMA_VERSION,
                command,
            },
        }
    }

    pub fn failure(error: SkillRuntimeErrorPayload, command: Option<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(error),
            meta: SkillRuntimeMeta {
                schema_version: SKILL_RUNTIME_SCHEMA_VERSION,
                command,
            },
        }
    }
}

/// One entry of `skills list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSkillListItem {
    /// The BARE snapshot name (`cron`). Note this is not necessarily what the
    /// agent's own CLI calls the skill: under plugin-based delivery it sees a
    /// prefixed name (`aionui:cron`).
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSkillListResponse {
    pub skills: Vec<RuntimeSkillListItem>,
}

/// `skills show <name>`: the full body PLUS the absolute skill root.
///
/// Both, not either: a read-only agent needs the content, while one that can run
/// commands needs the path so it can reach `references/` and `scripts/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSkillShowResponse {
    pub name: String,
    /// Frontmatter stripped, byte-identical to what the `[LOAD_SKILL]` channel
    /// injects (both go through `aionui_extension::extract_skill_body`).
    pub body: String,
    /// Absolute skill directory. Every relative reference inside `body` resolves
    /// against this, NOT against the agent's working directory.
    pub path: String,
}

/// `skills cat <name>/<relative-path>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSkillFileResponse {
    pub name: String,
    /// The relative path as requested, echoed back for correlation.
    pub path: String,
    pub content: String,
}

/// Query for the file read. A separate `path` parameter rather than extra route
/// segments so a nested `references/sub/x.md` needs no escaping games.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSkillFileQuery {
    pub path: String,
}
