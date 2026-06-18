use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamToolPermission {
    AnyTeamAgent,
    LeadOnly,
}

#[derive(Debug, Clone, Serialize)]
pub struct TeamToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct TeamToolSpec {
    pub name: &'static str,
    pub permission: TeamToolPermission,
    pub description: &'static str,
    pub input_schema: Value,
}

pub const TEAM_SPAWN_AGENT_DESCRIPTION: &str = r#"Create a new teammate agent to join the team.

Use this only when one of the following is true:
- The user explicitly approved the proposed teammate lineup in a previous message
- The user explicitly instructed you to create a specific teammate immediately

Before calling this tool in the normal planning flow:
- Start with one short sentence explaining why additional teammates would help
- Tell the user which teammate(s) you recommend
- Present the proposal as a table with: name, responsibility, recommended agent type/backend, and recommended model
- Include each teammate's responsibility, recommended agent type/backend, and model
- Ask whether to create them as proposed or change any names, responsibilities, or agent types
- In that approval question, remind the user that they can later ask you to replace or adjust any teammate if the lineup is not working well
- Do NOT call this tool in that same turn; wait for explicit approval in a later user message

When calling this tool, provide the model parameter if a specific model was recommended and approved.

The new agent will be created and added to the team. You can then assign tasks and send messages to it."#;

pub const TEAM_LIST_MODELS_DESCRIPTION: &str =
    "Query available models for team agent types. Returns the real-time model list that matches the frontend model selector.

Use this to:
- Check what models are available before spawning an agent with a specific model
- See all available agent types and their models at once
- Verify a model ID is valid for a given agent type

Pass agent_type to query a specific backend, or omit it to see all.";

pub const TEAM_DESCRIBE_ASSISTANT_DESCRIPTION: &str =
    "Get detailed information about a preset assistant before spawning it as a teammate.

Returns the preset's full description, enabled skills, and example tasks so you can
judge whether it fits the user's request. Use this when two or more presets look
relevant from the one-line catalog in your system prompt.

Only works on preset assistants listed in \"Available Preset Assistants for Spawning\".
After confirming a match, call team_spawn_agent with the same custom_agent_id.";

pub fn team_tool_specs() -> Vec<TeamToolSpec> {
    vec![
        TeamToolSpec {
            name: "team_send_message",
            permission: TeamToolPermission::AnyTeamAgent,
            description: "Send a message to a teammate or broadcast to all (to=\"*\").",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "Target agent slot_id or \"*\" for broadcast" },
                    "message": { "type": "string", "description": "Message content" }
                },
                "required": ["to", "message"]
            }),
        },
        TeamToolSpec {
            name: "team_spawn_agent",
            permission: TeamToolPermission::LeadOnly,
            description: TEAM_SPAWN_AGENT_DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Agent display name" },
                    "agent_type": { "type": "string", "description": "Agent type/backend to use (e.g. \"claude\", \"codex\", \"codebuddy\", \"gemini\"). Query team_list_models first to see available options." },
                    "model": { "type": "string", "description": "Specific model ID to use (e.g. \"claude-sonnet-4\"). Must be a valid model for the chosen agent_type. Query team_list_models to see available models." },
                    "custom_agent_id": { "type": "string", "description": "Preset assistant ID to spawn (from the Available Preset Assistants catalog). When set, agent_type is derived from the preset's backend." },
                    "backend": { "type": "string", "description": "Legacy alias for agent_type. Prefer agent_type." },
                    "role": { "type": "string", "description": "Agent role (default: 'teammate')" }
                },
                "required": ["name"]
            }),
        },
        TeamToolSpec {
            name: "team_task_create",
            permission: TeamToolPermission::AnyTeamAgent,
            description: "Create a new task on the team task board.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "subject": { "type": "string", "description": "Task subject" },
                    "description": { "type": "string", "description": "Task description" },
                    "owner": { "type": "string", "description": "Owning agent slotId" },
                    "blocked_by": { "type": "array", "items": { "type": "string" }, "description": "Task IDs this task depends on" }
                },
                "required": ["subject"]
            }),
        },
        TeamToolSpec {
            name: "team_task_update",
            permission: TeamToolPermission::AnyTeamAgent,
            description: "Update an existing task on the team task board.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "Task ID to update" },
                    "status": { "type": "string", "description": "New status: pending, in_progress, completed, deleted" },
                    "description": { "type": "string", "description": "New description" },
                    "owner": { "type": "string", "description": "New owning agent slotId" },
                    "blocked_by": { "type": "array", "items": { "type": "string" }, "description": "New dependency list" }
                },
                "required": ["task_id"]
            }),
        },
        TeamToolSpec {
            name: "team_task_list",
            permission: TeamToolPermission::AnyTeamAgent,
            description: "List all tasks on the team task board.",
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        TeamToolSpec {
            name: "team_members",
            permission: TeamToolPermission::AnyTeamAgent,
            description: "List all team members with their roles and current status.",
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        TeamToolSpec {
            name: "team_rename_agent",
            permission: TeamToolPermission::LeadOnly,
            description: "Rename a team member. Lead only.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "slot_id": { "type": "string", "description": "Agent slot_id to rename" },
                    "new_name": { "type": "string", "description": "New display name" }
                },
                "required": ["slot_id", "new_name"]
            }),
        },
        TeamToolSpec {
            name: "team_shutdown_agent",
            permission: TeamToolPermission::LeadOnly,
            description: "Initiate shutdown of a teammate. Lead only. Sends a shutdown_request to the target agent.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "slot_id": { "type": "string", "description": "Agent slot_id to shut down" },
                    "reason": { "type": "string", "description": "Reason for shutdown" }
                },
                "required": ["slot_id"]
            }),
        },
        TeamToolSpec {
            name: "team_list_models",
            permission: TeamToolPermission::AnyTeamAgent,
            description: TEAM_LIST_MODELS_DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "agent_type": { "type": "string", "description": "Agent type/backend to query (e.g. \"gemini\", \"claude\", \"codex\"). Shows all when omitted." }
                }
            }),
        },
        TeamToolSpec {
            name: "team_describe_assistant",
            permission: TeamToolPermission::AnyTeamAgent,
            description: TEAM_DESCRIBE_ASSISTANT_DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "custom_agent_id": { "type": "string", "description": "The preset assistant ID from the \"Available Preset Assistants\" catalog (e.g., \"word-creator\")." },
                    "locale": { "type": "string", "description": "Locale like \"zh-CN\" or \"en-US\". Defaults to the user's current UI language when omitted." }
                },
                "required": ["custom_agent_id"]
            }),
        },
    ]
}

pub fn visible_team_tool_descriptors(is_lead: bool) -> Vec<TeamToolDescriptor> {
    team_tool_specs()
        .into_iter()
        .filter(|spec| is_lead || spec.permission != TeamToolPermission::LeadOnly)
        .map(|spec| TeamToolDescriptor {
            name: spec.name.to_owned(),
            description: spec.description.to_owned(),
            input_schema: spec.input_schema,
        })
        .collect()
}

pub fn authorize_team_tool(is_lead: bool, tool_name: &str) -> Result<(), String> {
    let Some(spec) = team_tool_specs().into_iter().find(|spec| spec.name == tool_name) else {
        return Err(format!("Unknown tool: {tool_name}"));
    };
    if spec.permission == TeamToolPermission::LeadOnly && !is_lead {
        return Err(format!("Only Lead can use {tool_name}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_lead_tools_list_hides_lead_only_tools() {
        let names: Vec<String> = visible_team_tool_descriptors(false)
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert!(!names.contains(&"team_spawn_agent".to_owned()));
        assert!(!names.contains(&"team_rename_agent".to_owned()));
        assert!(!names.contains(&"team_shutdown_agent".to_owned()));
        assert!(names.contains(&"team_send_message".to_owned()));
    }

    #[test]
    fn authorization_rejects_non_lead_rename() {
        let err = authorize_team_tool(false, "team_rename_agent").unwrap_err();
        assert!(err.contains("Only Lead"));
    }

    #[test]
    fn permission_table_matches_contract() {
        let permissions: Vec<(&str, TeamToolPermission)> = team_tool_specs()
            .iter()
            .map(|spec| (spec.name, spec.permission))
            .collect();
        assert_eq!(
            permissions,
            vec![
                ("team_send_message", TeamToolPermission::AnyTeamAgent),
                ("team_spawn_agent", TeamToolPermission::LeadOnly),
                ("team_task_create", TeamToolPermission::AnyTeamAgent),
                ("team_task_update", TeamToolPermission::AnyTeamAgent),
                ("team_task_list", TeamToolPermission::AnyTeamAgent),
                ("team_members", TeamToolPermission::AnyTeamAgent),
                ("team_rename_agent", TeamToolPermission::LeadOnly),
                ("team_shutdown_agent", TeamToolPermission::LeadOnly),
                ("team_list_models", TeamToolPermission::AnyTeamAgent),
                ("team_describe_assistant", TeamToolPermission::AnyTeamAgent),
            ]
        );
    }
}
