use async_trait::async_trait;
use thiserror::Error;

const HISTORICAL_MEMORY_OPEN: &str = "<historical_memory trust=\"untrusted\"";
const HISTORICAL_MEMORY_CLOSE: &str = "</historical_memory>";
const MAX_MEMORY_BLOCK_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTurnOutcome {
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedTurnMemoryInput {
    pub user_id: String,
    pub conversation_id: String,
    pub turn_id: String,
    pub outcome: MemoryTurnOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallMemoryInput {
    pub user_id: String,
    pub conversation_id: String,
    pub prompt: String,
    pub retrieval_id: String,
    pub excluded_memory_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum MemoryPortError {
    #[error("Memory is unavailable")]
    Unavailable,
    #[error("Memory request is invalid")]
    Invalid,
}

#[async_trait]
pub trait ConversationMemoryPort: Send + Sync {
    /// Called after the durable conversation completion and event publication.
    /// Implementations must treat duplicate `(conversation_id, turn_id)` delivery idempotently.
    async fn on_turn_completed(&self, input: CompletedTurnMemoryInput) -> Result<(), MemoryPortError>;

    /// Resets all Memory derived from a conversation before canonical messages
    /// and artifacts are destroyed. The default keeps helper-owned conversation
    /// services isolated from the application Memory lifecycle.
    async fn on_conversation_reset(&self, _user_id: &str, _conversation_id: &str) -> Result<(), MemoryPortError> {
        Ok(())
    }

    /// Returns only a canonical, code-owned historical block. Callers validate the
    /// envelope and degrade to the unchanged user prompt on any invalid response.
    async fn build_recall_block(&self, input: RecallMemoryInput) -> Result<Option<String>, MemoryPortError>;
}

#[derive(Debug, Default)]
pub struct NoopConversationMemoryPort;

#[async_trait]
impl ConversationMemoryPort for NoopConversationMemoryPort {
    async fn on_turn_completed(&self, _input: CompletedTurnMemoryInput) -> Result<(), MemoryPortError> {
        Ok(())
    }

    async fn build_recall_block(&self, _input: RecallMemoryInput) -> Result<Option<String>, MemoryPortError> {
        Ok(None)
    }
}

pub(crate) fn assemble_agent_prompt(prompt: &str, block: Option<&str>) -> String {
    let Some(block) = block.filter(|block| valid_canonical_block(block)) else {
        return prompt.to_owned();
    };
    // Build options carry higher-priority system, User Context, conversation,
    // and pin context. This agent-bound payload therefore places historical
    // Memory immediately before the current user prompt.
    format!("{block}\n\n{prompt}")
}

fn valid_canonical_block(block: &str) -> bool {
    block.len() <= MAX_MEMORY_BLOCK_BYTES
        && block.starts_with(HISTORICAL_MEMORY_OPEN)
        && block.ends_with(HISTORICAL_MEMORY_CLOSE)
        && block.matches(HISTORICAL_MEMORY_OPEN).count() == 1
        && block.matches(HISTORICAL_MEMORY_CLOSE).count() == 1
}

#[cfg(test)]
mod tests {
    use super::assemble_agent_prompt;

    #[test]
    fn prompt_assembly_places_one_canonical_memory_block_before_the_current_prompt() {
        let block = "<historical_memory trust=\"untrusted\" policy_version=\"v1\">\n- fact\n</historical_memory>";
        let assembled = assemble_agent_prompt("current prompt", Some(block));
        assert_eq!(assembled, format!("{block}\n\ncurrent prompt"));
        assert_eq!(assembled.matches("<historical_memory trust=\"untrusted\"").count(), 1);
    }

    #[test]
    fn invalid_or_duplicate_memory_blocks_fall_back_to_the_original_prompt() {
        assert_eq!(assemble_agent_prompt("original", Some("renderer memory")), "original");
        assert_eq!(
            assemble_agent_prompt(
                "original",
                Some(
                    "<historical_memory trust=\"untrusted\">one</historical_memory><historical_memory trust=\"untrusted\">two</historical_memory>",
                ),
            ),
            "original",
        );
    }
}
