//! Repository traits for the assistants and assistant_overrides tables.

use crate::error::DbError;
use crate::models::{
    AssistantDefinitionRow, AssistantOverlayRow, AssistantOverrideRow, AssistantPreferenceRow, AssistantRow,
    CreateAssistantParams, UpdateAssistantParams, UpsertAssistantDefinitionParams, UpsertAssistantOverlayParams,
    UpsertAssistantPreferenceParams, UpsertOverrideParams,
};

/// CRUD access for user-authored assistant rows.
///
/// Object-safe via `async_trait` to support `Arc<dyn IAssistantRepository>`.
#[async_trait::async_trait]
pub trait IAssistantRepository: Send + Sync {
    /// Return all user-authored assistants, ordered by `updated_at` descending.
    async fn list(&self) -> Result<Vec<AssistantRow>, DbError>;

    /// Look up a single assistant by id.
    async fn get(&self, id: &str) -> Result<Option<AssistantRow>, DbError>;

    /// Insert a new assistant row. Primary-key conflict surfaces as
    /// `DbError::Conflict`.
    async fn create(&self, params: &CreateAssistantParams<'_>) -> Result<AssistantRow, DbError>;

    /// Partial update of an existing assistant row. Returns `Ok(None)` if
    /// no row matches.
    async fn update(&self, id: &str, params: &UpdateAssistantParams<'_>) -> Result<Option<AssistantRow>, DbError>;

    /// Delete an assistant row by id. Returns `true` if a row was removed.
    async fn delete(&self, id: &str) -> Result<bool, DbError>;

    /// Insert or replace by id. Exists for callers outside of the
    /// migration/import path; the import endpoint must use `create` and
    /// skip on conflict per spec §6.3.
    async fn upsert(&self, params: &CreateAssistantParams<'_>) -> Result<AssistantRow, DbError>;
}

/// Per-assistant user state (enabled flag, sort order, last-used timestamp).
#[async_trait::async_trait]
pub trait IAssistantOverrideRepository: Send + Sync {
    /// Fetch the override row for a given assistant id, if any.
    async fn get(&self, assistant_id: &str) -> Result<Option<AssistantOverrideRow>, DbError>;

    /// Fetch all override rows.
    async fn get_all(&self) -> Result<Vec<AssistantOverrideRow>, DbError>;

    /// Insert or update the override row for an assistant.
    async fn upsert(&self, params: &UpsertOverrideParams<'_>) -> Result<AssistantOverrideRow, DbError>;

    /// Delete the override row for an assistant. Returns `true` if a row was
    /// removed.
    async fn delete(&self, assistant_id: &str) -> Result<bool, DbError>;

    /// Remove override rows whose `assistant_id` is not in `valid_ids`.
    /// Returns the number of rows deleted.
    async fn delete_orphans(&self, valid_ids: &[&str]) -> Result<u64, DbError>;
}

/// Runtime assistant definitions across builtin / user / generated / extension sources.
#[async_trait::async_trait]
pub trait IAssistantDefinitionRepository: Send + Sync {
    async fn list(&self) -> Result<Vec<AssistantDefinitionRow>, DbError>;
    async fn get_by_assistant_id(&self, assistant_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError>;
    async fn get_by_assistant_id_including_deleted(
        &self,
        assistant_id: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_assistant_id(assistant_id).await
    }
    async fn get_by_id(&self, id: &str) -> Result<Option<AssistantDefinitionRow>, DbError>;
    async fn get_by_source_ref(
        &self,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError>;
    async fn get_by_source_ref_including_deleted(
        &self,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_source_ref(source, source_ref).await
    }
    async fn upsert(&self, params: &UpsertAssistantDefinitionParams<'_>) -> Result<AssistantDefinitionRow, DbError>;
    async fn soft_delete(&self, id: &str, deleted_at: i64) -> Result<bool, DbError>;
}

/// Runtime per-user assistant overlay used by the current app version.
#[async_trait::async_trait]
pub trait IAssistantOverlayRepository: Send + Sync {
    async fn get(&self, assistant_definition_id: &str) -> Result<Option<AssistantOverlayRow>, DbError>;
    async fn list(&self) -> Result<Vec<AssistantOverlayRow>, DbError>;
    async fn upsert(&self, params: &UpsertAssistantOverlayParams<'_>) -> Result<AssistantOverlayRow, DbError>;
    async fn delete(&self, assistant_definition_id: &str) -> Result<bool, DbError>;
}

/// Assistant-scoped "auto remember last" preferences.
#[async_trait::async_trait]
pub trait IAssistantPreferenceRepository: Send + Sync {
    async fn get(&self, assistant_definition_id: &str) -> Result<Option<AssistantPreferenceRow>, DbError>;
    async fn upsert(&self, params: &UpsertAssistantPreferenceParams<'_>) -> Result<AssistantPreferenceRow, DbError>;
    async fn delete(&self, assistant_definition_id: &str) -> Result<bool, DbError>;
}
