//! Memory domain business operations.

use std::sync::Arc;

use aionui_api_types::MemoryUpdateInput;

use crate::{EvidenceBuildRequest, MemoryError, evidence::EvidenceBuilder};

/// Domain service that owns Memory business-operation entry points.
#[derive(Clone)]
pub struct MemoryService {
    evidence_builder: Arc<EvidenceBuilder>,
}

impl Default for MemoryService {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryService {
    /// Creates the public Memory business-operation entry point.
    pub fn new() -> Self {
        Self {
            evidence_builder: Arc::new(EvidenceBuilder),
        }
    }

    /// Reconstructs validated, sanitized evidence for the registered Memory task.
    pub fn build_evidence(&self, request: EvidenceBuildRequest) -> Result<MemoryUpdateInput, MemoryError> {
        self.evidence_builder.build(request)
    }
}

#[cfg(test)]
mod tests {
    use aionui_db::models::ConversationRow;

    use super::MemoryService;
    use crate::EvidenceBuildRequest;

    #[test]
    fn exposes_evidence_building_through_the_public_service() {
        let service = MemoryService::new();
        let output = service
            .build_evidence(EvidenceBuildRequest {
                conversation: ConversationRow {
                    id: "conversation-1".into(),
                    user_id: "user-1".into(),
                    name: "Conversation".into(),
                    r#type: "acp".into(),
                    extra: "{}".into(),
                    model: None,
                    status: Some("finished".into()),
                    source: Some("aionui".into()),
                    channel_chat_id: None,
                    pinned: false,
                    pinned_at: None,
                    created_at: 1,
                    updated_at: 1,
                },
                messages: Vec::new(),
                previous_summary: None,
                summary_cursor: None,
                claimed_turn_ids: Vec::new(),
                existing_entries: Vec::new(),
            })
            .unwrap();

        assert_eq!(output.conversation.id, "conversation-1");
    }
}
