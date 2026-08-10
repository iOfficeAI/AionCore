use std::sync::Arc;

use aionui_conversation::ConversationService;

use crate::service::CronService;

#[derive(Clone)]
pub struct CronRouterState {
    pub cron_service: Arc<CronService>,
    pub conversation_service: ConversationService,
    /// Whether the desktop-only system-resume HTTP bridge is registered.
    /// Server identity modes must leave this disabled because HTTP headers are
    /// not a trustworthy internal-call boundary.
    pub allow_system_resume_http: bool,
}
