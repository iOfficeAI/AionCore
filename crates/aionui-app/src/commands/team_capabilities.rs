use aionui_api_types::{TEAM_TOOLS_SCHEMA_VERSION, team_tool_descriptors};
use serde_json::{Value, json};

pub(crate) fn data() -> Value {
    let tools = team_tool_descriptors()
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "cli_command": tool.cli_command,
                "permission": tool.permission,
                "lead_only": tool.lead_only(),
                "description": tool.description,
                "when": tool.when,
                "input_summary": tool.input_summary,
                "stdin_json_schema": tool.input_schema,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "schema_version": TEAM_TOOLS_SCHEMA_VERSION,
        "contract": "agent-facing-team-cli",
        "commands": {
            "capabilities": { "runtime_env_required": [] },
            "help": { "runtime_env_required": [] },
            "context": { "runtime_env_required": ["AIONUI_BASE_URL", "AIONUI_USER_ID", "AIONUI_CONVERSATION_ID", "AIONUI_RUNTIME_TOKEN"] },
            "tool_call": { "runtime_env_required": ["AIONUI_BASE_URL", "AIONUI_USER_ID", "AIONUI_CONVERSATION_ID", "AIONUI_RUNTIME_TOKEN"] }
        },
        "output_envelope": {
            "success": "boolean",
            "data": "object when success=true",
            "error": "object when success=false",
            "meta": { "schema_version": TEAM_TOOLS_SCHEMA_VERSION }
        },
        "tools": tools,
        "errors": [
            "unknown_tool",
            "schema_validation_failed",
            "permission_denied",
            "team_not_found",
            "conversation_not_found",
            "agent_not_found",
            "not_in_team",
            "transport_unavailable",
            "runtime_context_missing",
            "runtime_auth_failed"
        ]
    })
}

pub(crate) fn help_markdown() -> String {
    let mut text = String::from("# AionCore Team CLI\n\nUse `aioncore team capabilities` for exact schemas.\n\n");
    for tool in team_tool_descriptors() {
        let command = tool.cli_command.join(" ");
        let permission = if tool.lead_only() {
            "lead only"
        } else {
            "any team agent"
        };
        text.push_str(&format!(
            "- `aioncore team {command}` -> `{}` ({permission}): {}\n",
            tool.name, tool.input_summary
        ));
    }
    text
}
