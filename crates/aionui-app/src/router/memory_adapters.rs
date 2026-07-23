//! Application-owned adapters for the Memory domain's narrow ports.

use std::sync::Arc;

use aionui_api_types::AppOperationsModelHealth;
use aionui_common::{OnConversationDelete, ProviderWithModel};
use aionui_conversation::{
    CompletedTurnMemoryInput, ConversationMemoryPort, MemoryPortError,
    MemoryTurnOutcome as ConversationMemoryTurnOutcome, RecallMemoryInput,
};
use aionui_db::{IConversationRepository, IProviderRepository};
use aionui_memory::{AppOperationsReadinessPort, MemoryError, MemoryService, MemoryTurnOutcome, RetrievalContextPort};
use aionui_system::SettingsService;

#[derive(Clone)]
pub(crate) struct SettingsReadinessAdapter {
    settings: SettingsService,
}

impl SettingsReadinessAdapter {
    pub(crate) fn new(settings: SettingsService) -> Self {
        Self { settings }
    }
}

#[async_trait::async_trait]
impl AppOperationsReadinessPort for SettingsReadinessAdapter {
    async fn is_usable(&self) -> Result<bool, MemoryError> {
        self.settings
            .get_app_operations_model()
            .await
            .map(|resolved| resolved.health == AppOperationsModelHealth::Ready)
            .map_err(|_| MemoryError::Internal)
    }
}

#[derive(Clone)]
pub(crate) struct ConversationMemoryAdapter {
    service: Arc<MemoryService>,
}

impl ConversationMemoryAdapter {
    pub(crate) fn new(service: Arc<MemoryService>) -> Self {
        Self { service }
    }
}

#[derive(Clone)]
pub(crate) struct MemoryConversationDeleteAdapter {
    service: Arc<MemoryService>,
    conversations: Arc<dyn IConversationRepository>,
}

impl MemoryConversationDeleteAdapter {
    pub(crate) fn new(service: Arc<MemoryService>, conversations: Arc<dyn IConversationRepository>) -> Self {
        Self { service, conversations }
    }
}

#[async_trait::async_trait]
impl OnConversationDelete for MemoryConversationDeleteAdapter {
    async fn on_conversation_deleted(&self, conversation_id: &str) {
        let owner = match self.conversations.get(conversation_id).await {
            Ok(Some(conversation)) => conversation.user_id,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(
                    conversation_id,
                    error = %error,
                    "Memory conversation-delete owner lookup failed"
                );
                return;
            }
        };
        if let Err(error) = self.service.forget_conversation(&owner, conversation_id).await {
            tracing::warn!(
                user_id = owner,
                conversation_id,
                error = %error,
                "Memory conversation-delete cleanup failed"
            );
        }
    }
}

#[async_trait::async_trait]
impl ConversationMemoryPort for ConversationMemoryAdapter {
    async fn on_turn_completed(&self, input: CompletedTurnMemoryInput) -> Result<(), MemoryPortError> {
        let outcome = match input.outcome {
            ConversationMemoryTurnOutcome::Completed => MemoryTurnOutcome::Completed,
            ConversationMemoryTurnOutcome::Failed => MemoryTurnOutcome::Failed,
        };
        self.service
            .admit_turn_completed(&input.user_id, &input.conversation_id, &input.turn_id, outcome)
            .await
            .map(|_| ())
            .map_err(map_memory_port_error)
    }

    async fn build_recall_block(&self, input: RecallMemoryInput) -> Result<Option<String>, MemoryPortError> {
        self.service
            .build_recall_block(
                &input.user_id,
                &input.conversation_id,
                &input.prompt,
                &input.retrieval_id,
                &input.excluded_memory_ids,
            )
            .await
            .map_err(map_memory_port_error)
    }
}

fn map_memory_port_error(error: MemoryError) -> MemoryPortError {
    match error {
        MemoryError::InvalidInput | MemoryError::Forbidden | MemoryError::NotFound => MemoryPortError::Invalid,
        MemoryError::LeaseLost | MemoryError::StaleRevision | MemoryError::Conflict | MemoryError::Internal => {
            MemoryPortError::Unavailable
        }
    }
}

#[derive(Clone)]
pub(crate) struct TrustedRetrievalContextAdapter {
    conversations: Arc<dyn IConversationRepository>,
    providers: Arc<dyn IProviderRepository>,
}

impl TrustedRetrievalContextAdapter {
    pub(crate) fn new(
        conversations: Arc<dyn IConversationRepository>,
        providers: Arc<dyn IProviderRepository>,
    ) -> Self {
        Self {
            conversations,
            providers,
        }
    }
}

#[async_trait::async_trait]
impl RetrievalContextPort for TrustedRetrievalContextAdapter {
    async fn context_capacity(&self, user_id: &str, conversation_id: &str) -> Result<Option<u32>, MemoryError> {
        let Some(conversation) = self
            .conversations
            .get(conversation_id)
            .await
            .map_err(|_| MemoryError::Internal)?
            .filter(|conversation| conversation.user_id == user_id)
        else {
            return Ok(None);
        };
        let Some(binding) = conversation
            .model
            .as_deref()
            .and_then(|raw| serde_json::from_str::<ProviderWithModel>(raw).ok())
        else {
            return Ok(None);
        };
        let Some(provider) = self
            .providers
            .find_by_id(&binding.provider_id)
            .await
            .map_err(|_| MemoryError::Internal)?
        else {
            return Ok(None);
        };
        Ok(provider.context_limit.and_then(|limit| u32::try_from(limit).ok()))
    }
}

#[cfg(test)]
mod tests {
    use aionui_common::now_ms;
    use aionui_db::{
        CreateProviderParams, IConversationRepository, IProviderRepository, SqliteConversationRepository,
        SqliteProviderRepository, models::ConversationRow,
    };

    use super::*;

    #[tokio::test]
    async fn readiness_delegates_to_resolved_app_operations_health() {
        let database = aionui_db::init_database_memory().await.unwrap();
        let providers: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(database.pool().clone()));
        let settings = SettingsService::new(Arc::new(aionui_db::SqliteSettingsRepository::new(
            database.pool().clone(),
        )))
        .with_provider_repo(providers);
        let adapter = SettingsReadinessAdapter::new(settings);

        assert!(!adapter.is_usable().await.unwrap());
    }

    #[tokio::test]
    async fn trusted_capacity_uses_owned_conversation_provider_metadata_only() {
        let database = aionui_db::init_database_memory().await.unwrap();
        let conversations: Arc<dyn IConversationRepository> =
            Arc::new(SqliteConversationRepository::new(database.pool().clone()));
        let providers: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(database.pool().clone()));
        providers
            .create(CreateProviderParams {
                id: Some("provider-1"),
                platform: "openai",
                name: "Provider",
                base_url: "https://example.invalid",
                api_key_encrypted: "",
                models: r#"["model-1"]"#,
                enabled: true,
                capabilities: "[]",
                context_limit: Some(32_000),
                model_protocols: None,
                model_enabled: None,
                model_health: None,
                model_settings: "{}",
                bedrock_config: None,
                is_full_url: false,
            })
            .await
            .unwrap();
        conversations
            .create(&ConversationRow {
                id: "conversation-1".into(),
                user_id: "system_default_user".into(),
                name: "Conversation".into(),
                r#type: "acp".into(),
                extra: r#"{"context_limit":999999}"#.into(),
                model: Some(
                    serde_json::to_string(&ProviderWithModel {
                        provider_id: "provider-1".into(),
                        model: "model-1".into(),
                        use_model: None,
                    })
                    .unwrap(),
                ),
                status: Some("pending".into()),
                source: Some("aionui".into()),
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            })
            .await
            .unwrap();
        let adapter = TrustedRetrievalContextAdapter::new(conversations, providers);

        assert_eq!(
            adapter
                .context_capacity("system_default_user", "conversation-1")
                .await
                .unwrap(),
            Some(32_000)
        );
        assert_eq!(
            adapter
                .context_capacity("different-user", "conversation-1")
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn conversation_adapter_reports_failed_local_admission() {
        let adapter = ConversationMemoryAdapter::new(Arc::new(MemoryService::new()));

        let result = adapter
            .on_turn_completed(CompletedTurnMemoryInput {
                user_id: "user-1".into(),
                conversation_id: "conversation-1".into(),
                turn_id: "turn-1".into(),
                outcome: ConversationMemoryTurnOutcome::Completed,
            })
            .await;

        assert_eq!(result, Err(MemoryPortError::Unavailable));
    }
}
