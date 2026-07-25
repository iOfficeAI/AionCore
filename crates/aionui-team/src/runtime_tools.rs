use aionui_api_types::{
    TeamSessionBinding, TeamToolCall, TeamToolContextResponse, TeamToolErrorCode, TeamToolErrorPayload, TeamToolRole,
};
use serde_json::Value;

use crate::tool_executor::{TeamToolContext, TeamToolExecutor};
use crate::types::{TeamAgent, TeammateRole};

#[derive(Debug, Clone)]
pub struct ResolvedTeamToolContext {
    pub response: TeamToolContextResponse,
    pub context: Option<TeamToolContext>,
}

pub(crate) fn role_to_tool_role(role: TeammateRole) -> TeamToolRole {
    match role {
        TeammateRole::Lead => TeamToolRole::Lead,
        TeammateRole::Teammate => TeamToolRole::Teammate,
    }
}

pub(crate) fn error_payload(code: TeamToolErrorCode, message: impl Into<String>) -> TeamToolErrorPayload {
    TeamToolErrorPayload::new(code, message)
}

pub(crate) fn agent_for_conversation<'a>(
    agents: &'a [TeamAgent],
    conversation_id: &str,
    binding: &TeamSessionBinding,
) -> Result<&'a TeamAgent, TeamToolErrorPayload> {
    let agent = agents
        .iter()
        .find(|agent| agent.conversation_id == conversation_id)
        .ok_or_else(|| error_payload(TeamToolErrorCode::AgentNotFound, "caller agent not found in team"))?;
    if let Some(slot_id) = binding.slot_id.as_deref()
        && slot_id != agent.slot_id
    {
        return Err(error_payload(
            TeamToolErrorCode::PermissionDenied,
            "team binding slot_id does not match caller agent",
        ));
    }
    Ok(agent)
}

pub(crate) async fn execute_with_scheduler(
    scheduler: &crate::scheduler::TeammateManager,
    service: &std::sync::Weak<crate::service::TeamSessionService>,
    context: &TeamToolContext,
    call: TeamToolCall,
) -> Result<Value, TeamToolErrorPayload> {
    TeamToolExecutor::new(scheduler, service).execute(context, call).await
}
