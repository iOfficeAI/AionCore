//! RouterState. Holds Arc-wrapped dependencies only; construction happens in
//! `aionui-app`'s `build_*_state()` (AGENTS.md DI convention).

use std::sync::Arc;

use aionui_ai_agent::RuntimeTokenService;

use crate::service::SessionMessageService;
use crate::targets::MentionableTargets;

#[derive(Clone)]
pub struct SessionMessageRouterState {
    pub service: Arc<SessionMessageService>,
    pub targets: Arc<MentionableTargets>,
    pub runtime_token_service: Arc<RuntimeTokenService>,
}
