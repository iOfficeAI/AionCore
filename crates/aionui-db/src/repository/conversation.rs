use aionui_common::{PaginatedResult, TimestampMs};
use serde::{Deserialize, Serialize};

use crate::error::DbError;
use crate::models::{
    ConversationArtifactRow, ConversationAssistantSnapshotRow, ConversationRow, MessageRow,
    UpsertConversationAssistantSnapshotParams,
};

/// Conversation + message data access abstraction.
///
/// Covers conversation CRUD, extended queries (source/chat, cron-job,
/// associated workspace), and message operations (list, insert, update,
/// delete, search).
///
/// Object-safe via `async_trait` to support `Arc<dyn IConversationRepository>`.
#[async_trait::async_trait]
pub trait IConversationRepository: Send + Sync {
    // ── Conversation CRUD ───────────────────────────────────────────

    /// Returns a conversation by ID, or `None` if not found.
    async fn get(&self, id: &str) -> Result<Option<ConversationRow>, DbError>;

    /// Inserts a new conversation row.
    async fn create(&self, row: &ConversationRow) -> Result<(), DbError>;

    /// Partially updates a conversation. Returns `DbError::NotFound` if ID is missing.
    async fn update(&self, id: &str, updates: &ConversationRowUpdate) -> Result<(), DbError>;

    /// Deletes a conversation (messages cascade via FK).
    /// Returns `DbError::NotFound` if ID is missing.
    async fn delete(&self, id: &str) -> Result<(), DbError>;

    /// Lists conversations with cursor-based pagination and optional filters.
    async fn list_paginated(
        &self,
        user_id: &str,
        filters: &ConversationFilters,
    ) -> Result<PaginatedResult<ConversationRow>, DbError>;

    // ── Extended queries ────────────────────────────────────────────

    /// Finds a conversation by source, channel chat ID, and agent type.
    async fn find_by_source_and_chat(
        &self,
        user_id: &str,
        source: &str,
        chat_id: &str,
        agent_type: &str,
    ) -> Result<Option<ConversationRow>, DbError>;

    /// Lists conversations whose `extra.cronJobId` matches.
    async fn list_by_cron_job(&self, user_id: &str, cron_job_id: &str) -> Result<Vec<ConversationRow>, DbError>;

    /// Lists conversations sharing the same `extra.workspace` value.
    /// The conversation identified by `conversation_id` is excluded.
    async fn list_associated(&self, user_id: &str, conversation_id: &str) -> Result<Vec<ConversationRow>, DbError>;

    /// Returns the persisted assistant snapshot for a conversation, if any.
    async fn get_assistant_snapshot(
        &self,
        _conversation_id: &str,
    ) -> Result<Option<ConversationAssistantSnapshotRow>, DbError> {
        Ok(None)
    }

    /// Inserts or updates a persisted assistant snapshot for a conversation.
    async fn upsert_assistant_snapshot(
        &self,
        _params: &UpsertConversationAssistantSnapshotParams<'_>,
    ) -> Result<Option<ConversationAssistantSnapshotRow>, DbError> {
        Ok(None)
    }

    /// Deletes the assistant snapshot bound to a conversation.
    async fn delete_assistant_snapshot(&self, _conversation_id: &str) -> Result<bool, DbError> {
        Ok(false)
    }

    // ── Message operations ──────────────────────────────────────────

    /// Returns paginated messages for a conversation, ordered by `created_at`.
    async fn get_messages(
        &self,
        conv_id: &str,
        page: u32,
        page_size: u32,
        order: SortOrder,
    ) -> Result<PaginatedResult<MessageRow>, DbError>;

    /// Returns a single message scoped to a conversation.
    async fn get_message(&self, _conv_id: &str, _message_id: &str) -> Result<Option<MessageRow>, DbError> {
        Ok(None)
    }

    /// Inserts a new message row.
    async fn insert_message(&self, message: &MessageRow) -> Result<(), DbError>;

    /// Partially updates a message. Returns `DbError::NotFound` if ID is missing.
    async fn update_message(&self, id: &str, updates: &MessageRowUpdate) -> Result<(), DbError>;

    /// Deletes all messages belonging to a conversation.
    async fn delete_messages_by_conversation(&self, conv_id: &str) -> Result<(), DbError>;

    /// Finds a message by (conversation_id, msg_id, type) triple.
    async fn get_message_by_msg_id(
        &self,
        conv_id: &str,
        msg_id: &str,
        msg_type: &str,
    ) -> Result<Option<MessageRow>, DbError>;

    /// Lists stale assistant-side runtime messages that were left in a
    /// non-terminal state by a previous process.
    async fn list_stale_runtime_messages(&self) -> Result<Vec<MessageRow>, DbError> {
        Ok(Vec::new())
    }

    /// Full-text search across messages, joining conversation name.
    async fn search_messages(
        &self,
        user_id: &str,
        keyword: &str,
        page: u32,
        page_size: u32,
    ) -> Result<PaginatedResult<MessageSearchRow>, DbError>;

    /// Returns persisted conversation artifacts ordered by `created_at`.
    async fn list_artifacts(&self, _conversation_id: &str) -> Result<Vec<ConversationArtifactRow>, DbError> {
        Ok(Vec::new())
    }

    /// Returns a conversation artifact by ID scoped to a conversation.
    async fn get_artifact(
        &self,
        _conversation_id: &str,
        _artifact_id: &str,
    ) -> Result<Option<ConversationArtifactRow>, DbError> {
        Ok(None)
    }

    /// Inserts or updates a conversation artifact by primary key.
    async fn upsert_artifact(&self, artifact: &ConversationArtifactRow) -> Result<ConversationArtifactRow, DbError> {
        Ok(artifact.clone())
    }

    /// Updates artifact status and returns the updated row if found.
    async fn update_artifact_status(
        &self,
        _conversation_id: &str,
        _artifact_id: &str,
        _status: &str,
        _updated_at: TimestampMs,
    ) -> Result<Option<ConversationArtifactRow>, DbError> {
        Ok(None)
    }

    /// Marks all skill suggestion artifacts for a cron job as saved.
    async fn mark_skill_suggest_artifacts_saved(
        &self,
        _cron_job_id: &str,
        _updated_at: TimestampMs,
    ) -> Result<Vec<ConversationArtifactRow>, DbError> {
        Ok(Vec::new())
    }

    /// Deletes all artifacts belonging to a conversation.
    async fn delete_artifacts_by_conversation(&self, _conversation_id: &str) -> Result<(), DbError> {
        Ok(())
    }

    /// Returns legacy persisted cron trigger rows so callers can synthesize
    /// artifact cards for historical conversations created before artifact migration.
    async fn list_legacy_cron_trigger_messages(&self, _conversation_id: &str) -> Result<Vec<MessageRow>, DbError> {
        Ok(Vec::new())
    }
}

// ── Supporting types ────────────────────────────────────────────────

/// Sort direction for message listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

impl SortOrder {
    pub fn as_sql(&self) -> &'static str {
        match self {
            SortOrder::Asc => "ASC",
            SortOrder::Desc => "DESC",
        }
    }
}

/// Filters for paginated conversation listing.
#[derive(Debug, Clone, Default)]
pub struct ConversationFilters {
    /// Cursor: the ID of the last conversation from the previous page.
    pub cursor: Option<String>,
    /// Max items per page (default 20).
    pub limit: u32,
    /// Filter by conversation source.
    pub source: Option<String>,
    /// Filter by `extra.cronJobId`.
    pub cron_job_id: Option<String>,
    /// Filter by pinned status.
    pub pinned: Option<bool>,
}

impl ConversationFilters {
    pub fn effective_limit(&self) -> u32 {
        if self.limit == 0 { 20 } else { self.limit }
    }
}

/// Partial update payload for a conversation row.
///
/// `None` = keep existing value; `Some(v)` = set to `v`.
#[derive(Debug, Clone, Default)]
pub struct ConversationRowUpdate {
    pub name: Option<String>,
    pub pinned: Option<bool>,
    pub pinned_at: Option<Option<TimestampMs>>,
    pub model: Option<Option<String>>,
    pub extra: Option<String>,
    pub status: Option<String>,
    pub updated_at: Option<TimestampMs>,
}

/// Partial update payload for a message row.
#[derive(Debug, Clone, Default)]
pub struct MessageRowUpdate {
    pub content: Option<String>,
    pub status: Option<Option<String>>,
    pub hidden: Option<bool>,
}

/// A single result row from cross-conversation message search.
/// Includes full conversation fields for building nested response.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MessageSearchRow {
    // Message fields
    pub message_id: String,
    #[sqlx(rename = "type")]
    pub r#type: String,
    pub content: String,
    pub created_at: TimestampMs,
    // Conversation fields
    pub conversation_id: String,
    pub conversation_name: String,
    pub conversation_type: String,
    pub conversation_extra: String,
    pub conversation_model: Option<String>,
    pub conversation_status: Option<String>,
    pub conversation_source: Option<String>,
    pub conversation_channel_chat_id: Option<String>,
    pub conversation_pinned: bool,
    pub conversation_pinned_at: Option<TimestampMs>,
    pub conversation_created_at: TimestampMs,
    pub conversation_updated_at: TimestampMs,
}
