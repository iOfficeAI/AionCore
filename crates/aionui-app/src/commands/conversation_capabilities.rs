use aionui_api_types::{CONVERSATION_TOOLS_SCHEMA_VERSION, conversation_tool_descriptors};
use serde_json::{Value, json};

pub(crate) fn data() -> Value {
    let tools = conversation_tool_descriptors()
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "cli_command": tool.cli_command,
                "description": tool.description,
                "when": tool.when,
                "input_summary": tool.input_summary,
                "stdin_json_schema": tool.input_schema,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "schema_version": CONVERSATION_TOOLS_SCHEMA_VERSION,
        "contract": "agent-facing-conversation-cli",
        "commands": {
            "capabilities": { "runtime_env_required": [] },
            "create": { "runtime_env_required": ["AIONUI_BASE_URL", "AIONUI_USER_ID", "AIONUI_CONVERSATION_ID", "AIONUI_RUNTIME_TOKEN"] }
        },
        "output_envelope": {
            "success": "boolean",
            "data": "object when success=true: { id, name, workspace, assistant? { id, name, backend } }",
            "error": "object when success=false: { code, message, details? }",
            "meta": { "schema_version": CONVERSATION_TOOLS_SCHEMA_VERSION }
        },
        "semantics": {
            "synchronous": "when success=true the conversation exists and can immediately be addressed by id",
            "does_not": ["send a first message", "open the conversation", "switch the user's current conversation"],
            "inheritance": "workspace and assistant default to THIS conversation's; the new conversation is a clean instance of the same assistant, not a copy of this conversation's runtime state"
        },
        "tools": tools,
        "errors": [
            "caller_is_team",
            "workspace_not_absolute",
            "workspace_unavailable",
            "assistant_not_found",
            "assistant_disabled",
            "assistant_model_unresolved",
            "runtime_auth_failed",
            "schema_validation_failed",
            "transport_unavailable"
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_lists_every_registry_tool_and_its_cli_path() {
        let data = data();
        let tools = data["tools"].as_array().unwrap();
        assert_eq!(tools.len(), conversation_tool_descriptors().len());
        for descriptor in conversation_tool_descriptors() {
            let entry = tools
                .iter()
                .find(|tool| tool["name"] == json!(descriptor.name))
                .unwrap_or_else(|| panic!("{} missing from capabilities", descriptor.name));
            assert_eq!(
                entry["cli_command"],
                serde_json::to_value(&descriptor.cli_command).unwrap()
            );
            assert!(entry["stdin_json_schema"].is_object(), "{}", descriptor.name);
            let command = descriptor.cli_command.join(" ");
            assert!(
                data["commands"][&command].is_object(),
                "`{command}` has no runtime_env entry"
            );
        }
    }

    #[test]
    fn capabilities_declares_that_it_needs_no_runtime_env() {
        assert_eq!(data()["commands"]["capabilities"]["runtime_env_required"], json!([]));
    }

    #[test]
    fn every_error_code_the_service_can_return_is_documented() {
        use aionui_api_types::ConversationToolErrorCode as Code;
        let documented: Vec<String> = data()["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect();
        for code in [
            Code::CallerIsTeam,
            Code::WorkspaceNotAbsolute,
            Code::WorkspaceUnavailable,
            Code::AssistantNotFound,
            Code::AssistantDisabled,
            Code::AssistantModelUnresolved,
            Code::RuntimeAuthFailed,
            Code::SchemaValidationFailed,
            Code::TransportUnavailable,
        ] {
            assert!(
                documented.contains(&code.as_str().to_owned()),
                "{} is undocumented",
                code.as_str()
            );
        }
        assert_eq!(documented.len(), 9, "no stray codes: {documented:?}");
    }
}
