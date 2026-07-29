use crate::MemoryError;

/// Trusted runtime metadata needed only to size Memory recall.
#[async_trait::async_trait]
pub trait RetrievalContextPort: Send + Sync {
    async fn context_capacity(&self, user_id: &str, conversation_id: &str) -> Result<Option<u32>, MemoryError>;
}

pub(crate) struct UnknownRetrievalContext;

#[async_trait::async_trait]
impl RetrievalContextPort for UnknownRetrievalContext {
    async fn context_capacity(&self, _user_id: &str, _conversation_id: &str) -> Result<Option<u32>, MemoryError> {
        Ok(None)
    }
}
