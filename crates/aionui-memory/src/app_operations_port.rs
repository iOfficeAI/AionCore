use crate::MemoryError;

/// Content-free view of the shared App Operations role's current usability.
#[async_trait::async_trait]
pub trait AppOperationsReadinessPort: Send + Sync {
    async fn is_usable(&self) -> Result<bool, MemoryError>;
}
