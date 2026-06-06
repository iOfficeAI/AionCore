use std::sync::Arc;

use aionui_api_types::{ConversationRuntimeSummary, WebSocketMessage};
use aionui_common::{ErrorChain, now_ms};
use aionui_db::{ConversationRowUpdate, IConversationRepository};
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use tracing::{debug, error};

use crate::runtime_persistence::{RuntimePersistenceCoordinator, RuntimeWriteKind};

#[derive(Clone)]
pub struct RuntimeCompletionPublisher {
    repo: Arc<dyn IConversationRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
    persistence: RuntimePersistenceCoordinator,
}

impl RuntimeCompletionPublisher {
    pub fn new(
        repo: Arc<dyn IConversationRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
        persistence: RuntimePersistenceCoordinator,
    ) -> Self {
        Self {
            repo,
            broadcaster,
            persistence,
        }
    }

    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id))]
    pub async fn publish(&self, conversation_id: &str, runtime: Option<ConversationRuntimeSummary>) {
        if !self
            .persistence
            .allows(conversation_id, RuntimeWriteKind::ConversationFinished)
        {
            debug!(conversation_id, "turn completion skipped by runtime persistence policy");
            return;
        }

        match self.repo.get(conversation_id).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                debug!(
                    conversation_id,
                    "turn completion skipped because conversation row is missing"
                );
                return;
            }
            Err(error) => {
                error!(
                    conversation_id,
                    error = %ErrorChain(&error),
                    "turn completion skipped because conversation row lookup failed"
                );
                return;
            }
        }

        let update = ConversationRowUpdate {
            status: Some("finished".to_owned()),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        if let Err(error) = self.repo.update(conversation_id, &update).await {
            error!(
                conversation_id,
                error = %ErrorChain(&error),
                "Failed to update conversation status"
            );
            return;
        }

        let payload = json!({
            "conversation_id": conversation_id,
            "session_id": conversation_id,
            "status": "finished",
            "canSendMessage": true,
            "runtime": runtime,
        });
        self.broadcaster
            .broadcast(WebSocketMessage::new("turn.completed", payload));

        debug!(conversation_id, status = "finished", "Turn completed");
    }
}
