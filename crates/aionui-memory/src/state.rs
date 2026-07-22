//! Router state for the Memory domain.

use std::sync::Arc;

use crate::service::MemoryService;

/// Dependencies supplied by application composition when Memory routes are added.
#[derive(Clone)]
pub struct MemoryRouterState {
    pub service: Arc<MemoryService>,
}
