//! Local-kb repository trait (M9 E15). Separate from skill experience library.

use async_trait::async_trait;

use crate::error::DbError;
use crate::models::{
    CreateLocalKbDocumentParams, CreateLocalKbSourceParams, LocalKbDocumentRow, LocalKbSettingsRow, LocalKbSourceRow,
    UpsertLocalKbSettingsParams,
};

#[async_trait]
pub trait ILocalKbRepository: Send + Sync {
    async fn get_settings(&self, user_id: &str) -> Result<Option<LocalKbSettingsRow>, DbError>;

    async fn upsert_settings(&self, params: UpsertLocalKbSettingsParams<'_>) -> Result<LocalKbSettingsRow, DbError>;

    async fn list_sources(&self, user_id: &str) -> Result<Vec<LocalKbSourceRow>, DbError>;

    async fn create_source(&self, params: CreateLocalKbSourceParams<'_>) -> Result<LocalKbSourceRow, DbError>;

    async fn delete_source(&self, user_id: &str, source_id: &str) -> Result<bool, DbError>;

    async fn create_document(&self, params: CreateLocalKbDocumentParams<'_>) -> Result<LocalKbDocumentRow, DbError>;

    async fn list_documents_for_source(
        &self,
        user_id: &str,
        source_id: &str,
    ) -> Result<Vec<LocalKbDocumentRow>, DbError>;

    /// Substring search over title + body for one user. Caller must enforce enabled.
    async fn search_documents(
        &self,
        user_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<LocalKbDocumentRow>, DbError>;

    async fn get_document(&self, user_id: &str, document_id: &str) -> Result<Option<LocalKbDocumentRow>, DbError>;
}
