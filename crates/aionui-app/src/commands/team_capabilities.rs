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
        "input": {
            "default_mode": "piped_stdin_json",
            "supported_modes": ["--json JSON", "--input PATH", "--input -", "piped stdin"],
            "precedence": ["--json", "--input", "stdin"],
            "terminal_stdin": "not read automatically",
            "empty_implicit_stdin_first_byte_timeout": "empty JSON object",
            "stdin_timeout_env": "AIONUI_CLI_STDIN_TIMEOUT_MS",
            "stdin_timeout_default_ms": 1000,
            "stdin_timeout_max_ms": 30000,
            "stdin_timeout_error": "TEAM_CLI_STDIN_TIMEOUT",
            "max_bytes": 4194304,
            "overall_read_timeout_ms": 30000
        },
        "execution": {
            "instruction": "Run Team CLI commands in the foreground; do not background them."
        },
        "examples": [
            {
                "purpose": "no_input_read_messages",
                "shell": "POSIX",
                "command": "\"$AIONUI_HELPER_BIN\" team read-messages --json '{}'"
            },
            {
                "purpose": "send_message",
                "shell": "POSIX",
                "command": "\"$AIONUI_HELPER_BIN\" team send-message --json '{\"to\":\"<slot_id>\",\"message\":\"Review complete.\"}'"
            },
            {
                "purpose": "no_input_read_messages",
                "shell": "Windows PowerShell",
                "command": "& $env:AIONUI_HELPER_BIN team read-messages --json '{}'"
            },
            {
                "purpose": "send_message",
                "shell": "Windows PowerShell 7.3+ (Standard/Windows native argument mode)",
                "command": "& $env:AIONUI_HELPER_BIN team send-message --json '{\"to\":\"<slot_id>\",\"message\":\"Review complete.\"}'",
                "source": "https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_parsing?view=powershell-7.5#passing-arguments-that-contain-quote-characters"
            },
            {
                "purpose": "send_message_legacy_powershell",
                "shell": "Windows PowerShell 5.1 or Legacy native argument mode",
                "command": "$teamInputPath = [IO.Path]::GetTempFileName()\n[IO.File]::WriteAllText($teamInputPath, '{\"to\":\"<slot_id>\",\"message\":\"Review complete.\"}', [Text.Encoding]::UTF8)\n& $env:AIONUI_HELPER_BIN team send-message --input $teamInputPath\nRemove-Item -LiteralPath $teamInputPath",
                "source": "https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_parsing?view=powershell-7.5#passing-arguments-that-contain-quote-characters"
            }
        ],
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
    let mut text = String::from(
        r##"# AionCore Team CLI

Use aioncore team capabilities for exact schemas.

JSON input: --json JSON takes precedence over --input PATH or --input -; otherwise, piped stdin supplies JSON (verified: crates/aionui-app/src/cli.rs and crates/aionui-app/src/commands/json_input.rs). Terminal stdin is not read automatically.
If implicit stdin stays open without a first byte for 1000 ms, it is treated as an empty JSON object. Override this wait with AIONUI_CLI_STDIN_TIMEOUT_MS (1-30000 ms). After a byte arrives, a 30 second total read limit applies. Input is limited to 4 MiB.
An explicit stdin read that times out returns TEAM_CLI_STDIN_TIMEOUT with source, bytes_read, and a 16-byte prefix only (verified: crates/aionui-app/src/commands/cmd_team.rs and crates/aionui-app/src/commands/json_input.rs).
Run Team CLI commands in the foreground; do not background them.

No-input read with POSIX shell:
    "$AIONUI_HELPER_BIN" team read-messages --json '{}'

Send a message with POSIX shell:
    "$AIONUI_HELPER_BIN" team send-message --json '{"to":"<slot_id>","message":"Review complete."}'

No-input read with Windows PowerShell:
    & $env:AIONUI_HELPER_BIN team read-messages --json '{}'

Send a message with Windows PowerShell 7.3+ using Standard/Windows native argument mode (PowerShell 7.3 changed native argument parsing; see https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_parsing?view=powershell-7.5#passing-arguments-that-contain-quote-characters):
    & $env:AIONUI_HELPER_BIN team send-message --json '{"to":"<slot_id>","message":"Review complete."}'

Send a message with Windows PowerShell 5.1 or Legacy native argument mode using a temporary JSON file:
    $teamInputPath = [IO.Path]::GetTempFileName()
    [IO.File]::WriteAllText($teamInputPath, '{"to":"<slot_id>","message":"Review complete."}', [Text.Encoding]::UTF8)
    & $env:AIONUI_HELPER_BIN team send-message --input $teamInputPath
    Remove-Item -LiteralPath $teamInputPath

The send-message payload requires to and message (verified: crates/aionui-api-types/src/team_tools.rs).

"##,
    );
    for tool in team_tool_descriptors() {
        let command = tool.cli_command.join(" ");
        let permission = if tool.lead_only() {
            "lead only"
        } else {
            "any team agent"
        };
        text.push_str(&format!(
            "- aioncore team {command} -> {} ({permission}): {}\n",
            tool.name, tool.input_summary
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_help_documents_precedence_foreground_and_descriptor_payload() {
        let help = help_markdown();
        assert!(help.contains("--json JSON takes precedence"));
        assert!(help.contains("Run Team CLI commands in the foreground"));
        assert!(help.contains("team read-messages --json '{}'"));
        assert!(help.contains("& $env:AIONUI_HELPER_BIN team read-messages --json '{}'"));
        assert!(help.contains(r#""message":"Review complete.""#));
        assert!(help.contains(r#""$AIONUI_HELPER_BIN" team send-message --json"#));
        assert!(help.contains("Windows PowerShell 7.3+ using Standard/Windows native argument mode"));
        assert!(help.contains("Windows PowerShell 5.1 or Legacy native argument mode"));
        assert!(help.contains("--input $teamInputPath"));
        assert!(
            help.contains("learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_parsing")
        );
        assert!(help.contains("TEAM_CLI_STDIN_TIMEOUT"));
        assert!(!help.contains(r#""content":"Review complete.""#));
    }

    #[test]
    fn capabilities_expose_the_same_input_and_foreground_contract() {
        let data = data();
        assert_eq!(
            data["input"]["precedence"],
            serde_json::json!(["--json", "--input", "stdin"])
        );
        assert_eq!(data["input"]["terminal_stdin"], "not read automatically");
        assert_eq!(data["input"]["stdin_timeout_default_ms"], 1000);
        assert_eq!(data["input"]["stdin_timeout_max_ms"], 30000);
        assert_eq!(data["input"]["stdin_timeout_error"], "TEAM_CLI_STDIN_TIMEOUT");
        assert_eq!(data["examples"].as_array().unwrap().len(), 5);
        assert_eq!(data["examples"][0]["purpose"], "no_input_read_messages");
        assert_eq!(data["examples"][2]["shell"], "Windows PowerShell");
        assert!(
            data["examples"][2]["command"]
                .as_str()
                .unwrap()
                .contains("read-messages --json '{}'")
        );
        assert!(
            data["execution"]["instruction"]
                .as_str()
                .unwrap()
                .contains("foreground")
        );
        assert_eq!(
            data["examples"][3]["shell"],
            "Windows PowerShell 7.3+ (Standard/Windows native argument mode)"
        );
        assert!(
            data["examples"][3]["source"]
                .as_str()
                .unwrap()
                .contains("learn.microsoft.com")
        );
        assert_eq!(data["examples"][4]["purpose"], "send_message_legacy_powershell");
        assert!(
            data["examples"][4]["command"]
                .as_str()
                .unwrap()
                .contains("--input $teamInputPath")
        );
    }
}
