pub mod governance;
pub mod role_prompt;
pub mod tools;

pub use governance::{TEAM_GOVERNANCE_PROMPT, with_team_governance};
pub use role_prompt::{
    AvailableAgentType, AvailableAssistant, LeadPromptParams, TeamPromptAgent, TeamPromptRole, TeammatePromptParams,
    build_lead_prompt, build_teammate_prompt,
};
pub use tools::{
    TeamToolDescriptor, TeamToolPermission, TeamToolSpec, authorize_team_tool, team_tool_specs,
    visible_team_tool_descriptors,
};
