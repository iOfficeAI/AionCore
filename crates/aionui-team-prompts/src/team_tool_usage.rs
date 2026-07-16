use aionui_api_types::{TeamToolDescriptor, TeamToolRole, TeamToolTransport};

use crate::role_prompt::TeamPromptRole;

pub fn build_team_tool_usage(role: TeamPromptRole, transport: TeamToolTransport) -> String {
    let tool_role = match role {
        TeamPromptRole::Lead => TeamToolRole::Lead,
        TeamPromptRole::Teammate => TeamToolRole::Teammate,
    };
    let descriptors = aionui_api_types::team_tool_descriptors_for_role(tool_role);
    match transport {
        TeamToolTransport::Mcp => render_mcp_usage(role, &descriptors),
        TeamToolTransport::CliAssumed => render_cli_usage(role, &descriptors),
    }
}

fn render_mcp_usage(role: TeamPromptRole, descriptors: &[TeamToolDescriptor]) -> String {
    let mut text = String::from(
        "You MUST use the `team_*` MCP tools for ALL team coordination.\n\
Your platform may provide similarly named built-in tools. Do NOT use those.\n\
Always use the `team_*` MCP tool versions.\n\
Use `slot_id` values for all agent targets.\n\n\
If a single `team_*` MCP tool call fails because of invalid arguments, schema mismatch, or role/permission constraints,\n\
use \"$AIONUI_HELPER_BIN\" team capabilities or \"$AIONUI_HELPER_BIN\" team help to inspect the Team contract,\n\
then retry the MCP tool call with corrected arguments.\n\
If the `team_*` MCP tools are unavailable, missing, disconnected, or continue to fail after correction,\n\
use the Team CLI fallback through \"$AIONUI_HELPER_BIN\" team ... commands to continue Team coordination.\n\n\
For exact schema, run team capabilities.\n\n\
| When | MCP tool | Input summary |\n\
| --- | --- | --- |\n",
    );
    for tool in descriptors {
        text.push_str(&format!(
            "| {} | `{}` | {} |\n",
            tool.when, tool.name, tool.input_summary
        ));
    }
    if role == TeamPromptRole::Teammate {
        text.push_str("\nLead-only tools are unavailable to teammates.\n");
    }
    text
}

fn render_cli_usage(role: TeamPromptRole, descriptors: &[TeamToolDescriptor]) -> String {
    let mut text = String::from(
        "You MUST use AionCore Team CLI for ALL team coordination:\n\
\"$AIONUI_HELPER_BIN\" team ...\n\n\
Run \"$AIONUI_HELPER_BIN\" team capabilities when you need command names,\n\
stdin JSON schema, required fields, enum values, permissions, examples,\n\
or error meanings.\n\n\
Run \"$AIONUI_HELPER_BIN\" team help when you need a short readable guide.\n\n\
Do not guess team_id, slot_id, role, permissions, or internal tokens.\n\
Do not claim you used MCP tools in CLI transport.\n\
Use slot_id values from this prompt, team context, or team members results\n\
for all agent targets.\n\n\
If the CLI returns schema_validation_failed, unknown_command, or permission_denied,\n\
consult team capabilities or team help, correct the call, and retry at most once.\n\n\
For exact schema, run team capabilities.\n\n\
| When | CLI command | Canonical tool | Input summary |\n\
| --- | --- | --- | --- |\n",
    );
    for tool in descriptors {
        text.push_str(&format!(
            "| {} | `team {}` | `{}` | {} |\n",
            tool.when,
            tool.cli_command.join(" "),
            tool.name,
            tool.input_summary
        ));
    }
    if role == TeamPromptRole::Teammate {
        text.push_str("\nLead-only tools are unavailable to teammates.\n");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_teammate_usage_excludes_lead_only_tools() {
        let usage = build_team_tool_usage(TeamPromptRole::Teammate, TeamToolTransport::CliAssumed);
        assert!(usage.contains("\"$AIONUI_HELPER_BIN\" team"));
        assert!(usage.contains("team send-message"));
        assert!(!usage.contains("team spawn-agent"));
        assert!(usage.contains("Lead-only tools are unavailable to teammates"));
    }

    #[test]
    fn mcp_lead_usage_includes_lead_tools() {
        let usage = build_team_tool_usage(TeamPromptRole::Lead, TeamToolTransport::Mcp);
        assert!(usage.contains("team_*` MCP tools"));
        assert!(usage.contains("team_spawn_agent"));
        assert!(usage.contains("\"$AIONUI_HELPER_BIN\" team capabilities"));
        assert!(usage.contains("retry the MCP tool call with corrected arguments"));
        assert!(usage.contains("use the Team CLI fallback"));
    }

    #[test]
    fn mcp_teammate_usage_includes_shared_fallback_guidance() {
        let usage = build_team_tool_usage(TeamPromptRole::Teammate, TeamToolTransport::Mcp);
        assert!(usage.contains("\"$AIONUI_HELPER_BIN\" team capabilities"));
        assert!(usage.contains("retry the MCP tool call with corrected arguments"));
        assert!(usage.contains("use the Team CLI fallback"));
        assert!(usage.contains("Lead-only tools are unavailable to teammates"));
    }
}
