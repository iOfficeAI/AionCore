//! Router state carrying the assistant service for axum handlers.

use std::sync::Arc;

use aionui_db::GovernanceService;

use crate::service::AssistantService;

/// Shared state injected into `/api/assistants/*` handlers.
#[derive(Clone)]
pub struct AssistantRouterState {
    pub service: Arc<AssistantService>,
    /// Optional governance for M9 E12 access-scope catalog filtering.
    pub governance: Option<Arc<GovernanceService>>,
}
