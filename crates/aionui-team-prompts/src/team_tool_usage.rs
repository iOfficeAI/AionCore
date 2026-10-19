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
        r##"You MUST use AionCore Team CLI for ALL team coordination:
"$AIONUI_HELPER_BIN" team ...

Run "$AIONUI_HELPER_BIN" team capabilities when you need command names,
stdin JSON schema, required fields, enum values, permissions, examples,
or error meanings.

Run "$AIONUI_HELPER_BIN" team help when you need a short readable guide.

Supply JSON with --json JSON (highest precedence), --input PATH,
--input -, or piped stdin, in that order. Terminal stdin is not read
automatically. If implicit stdin stays open without a first byte for
1000 ms, it becomes an empty JSON object; AIONUI_CLI_STDIN_TIMEOUT_MS
overrides that wait from 1 to 30000 ms. After a byte arrives, the total
read limit is 30 seconds. Input is limited to 4 MiB. An explicit stdin
timeout returns TEAM_CLI_STDIN_TIMEOUT with source, bytes_read, and a
16-byte prefix only (verified: crates/aionui-app/src/cli.rs, crates/aionui-app/src/commands/cmd_team.rs, and crates/aionui-app/src/commands/json_input.rs).
Run each Team CLI command in the foreground; do not background it
(verified: crates/aionui-app/src/commands/team_capabilities.rs).

No-input read, POSIX shell:
"$AIONUI_HELPER_BIN" team read-messages --json '{}'

No-input read, Windows PowerShell:
& $env:AIONUI_HELPER_BIN team read-messages --json '{}'

Send message, POSIX shell (the descriptor requires to and message, verified: crates/aionui-api-types/src/team_tools.rs):
"$AIONUI_HELPER_BIN" team send-message --json '{"to":"<slot_id>","message":"Review complete."}'

Send message, Windows PowerShell 7.3+ with Standard/Windows native argument mode
(PowerShell 7.3 changed native argument parsing; source:
https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_parsing?view=powershell-7.5#passing-arguments-that-contain-quote-characters):
& $env:AIONUI_HELPER_BIN team send-message --json '{"to":"<slot_id>","message":"Review complete."}'

Send message, Windows PowerShell 5.1 or Legacy native argument mode, using a temporary JSON file:
$teamInputPath = [IO.Path]::GetTempFileName()
[IO.File]::WriteAllText($teamInputPath, '{"to":"<slot_id>","message":"Review complete."}', [Text.Encoding]::UTF8)
& $env:AIONUI_HELPER_BIN team send-message --input $teamInputPath
Remove-Item -LiteralPath $teamInputPath

Do not guess team_id, slot_id, role, permissions, or internal tokens.
Do not claim you used MCP tools in CLI transport.
Use slot_id values from this prompt, team context, or team members results
for all agent targets.

If the CLI returns schema_validation_failed, unknown_command, or permission_denied,
consult team capabilities or team help, correct the call, and retry at most once.

For exact schema, run team capabilities.

| When | CLI command | Canonical tool | Input summary |
| --- | --- | --- | --- |
"##,
    );
    for tool in descriptors {
        text.push_str(&format!(
            "| {} | team {} | {} | {} |\n",
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
        assert!(usage.contains("--json JSON"));
        assert!(usage.contains("foreground; do not background it"));
        assert!(usage.contains("team read-messages --json '{}'"));
        assert!(usage.contains("& $env:AIONUI_HELPER_BIN team read-messages --json '{}'"));
        assert!(usage.contains(r#""message":"Review complete.""#));
        assert!(usage.contains("Windows PowerShell 7.3+ with Standard/Windows native argument mode"));
        assert!(usage.contains("Windows PowerShell 5.1 or Legacy native argument mode"));
        assert!(usage.contains("--input $teamInputPath"));
        assert!(
            usage.contains("learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_parsing")
        );
        assert!(usage.contains("TEAM_CLI_STDIN_TIMEOUT"));
        assert!(!usage.contains(r#""content":"Review complete.""#));
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
