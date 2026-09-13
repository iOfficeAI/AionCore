use std::sync::Arc;

use aionui_ai_agent::runtime_token::RuntimeTokenService;

use crate::service::SkillRuntimeService;

/// Router state. Arc-wrapped dependencies only, constructed in `aionui-app`'s
/// `build_skill_runtime_state()` per the dependency-injection convention.
#[derive(Clone)]
pub struct SkillRuntimeRouterState {
    pub service: Arc<SkillRuntimeService>,
    pub runtime_token_service: Arc<RuntimeTokenService>,
}
