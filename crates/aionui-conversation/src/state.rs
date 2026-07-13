use std::sync::Arc;

use crate::service::ConversationService;
use aionui_ai_agent::{ActiveLeaseRegistry, IWorkerTaskManager};

/// Shared state for conversation route handlers.
#[derive(Clone)]
pub struct ConversationRouterState {
    pub service: ConversationService,
    pub task_manager: Arc<dyn IWorkerTaskManager>,
    pub active_leases: Arc<ActiveLeaseRegistry>,
}
