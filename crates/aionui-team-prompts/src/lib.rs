pub mod governance;
pub mod role_prompt;
pub mod team_tool_usage;
pub mod tools;

pub use governance::{TEAM_GOVERNANCE_PROMPT, with_team_governance};
pub use role_prompt::{
    AvailableAgentType, AvailableAssistant, LeadPromptParams, TeamPromptAgent, TeamPromptRole, TeammatePromptParams,
    build_lead_prompt, build_teammate_prompt,
};
pub use team_tool_usage::build_team_tool_usage;
pub use tools::{
    TeamToolDescriptor, TeamToolPermission, authorize_team_tool, team_tool_descriptors, visible_team_tool_descriptors,
};
