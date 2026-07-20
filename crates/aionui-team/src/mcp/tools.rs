use serde::Deserialize;
use serde_json::Value;

use crate::scheduler::SchedulerAction;
use crate::types::TeammateRole;

pub use aionui_api_types::{
    TEAM_DESCRIBE_ASSISTANT_DESCRIPTION, TEAM_LIST_ASSISTANTS_DESCRIPTION, TEAM_SPAWN_AGENT_DESCRIPTION,
};
use aionui_api_types::{TeamToolPermission, TeamToolRole};

// ---------------------------------------------------------------------------
// Tool descriptors (returned by tools/list)
// ---------------------------------------------------------------------------

pub type ToolDescriptor = aionui_api_types::TeamToolDescriptor;

pub fn all_tool_descriptors_for_role(caller_role: TeammateRole) -> Vec<ToolDescriptor> {
    let role = if caller_role == TeammateRole::Lead {
        TeamToolRole::Lead
    } else {
        TeamToolRole::Teammate
    };
    aionui_api_types::team_tool_descriptors_for_role(role)
}

pub fn all_tool_descriptors() -> Vec<ToolDescriptor> {
    all_tool_descriptors_for_role(TeammateRole::Lead)
}

pub fn authorize_tool(caller_role: TeammateRole, tool_name: &str) -> Result<(), String> {
    let Some(spec) = aionui_api_types::team_tool_descriptor(tool_name) else {
        return Err(format!("Unknown tool: {tool_name}"));
    };
    if spec.permission == TeamToolPermission::LeadOnly && caller_role != TeammateRole::Lead {
        return Err(format!("Only Lead can use {tool_name}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tool call input types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SendMessageInput {
    pub to: String,
    pub message: String,
    #[serde(default)]
    pub files: Vec<String>,
}

/// Arguments for the `team_spawn_agent` MCP tool call.
///
/// Team spawning is assistant-first. The MCP tool accepts an assistant identity;
/// model selection comes from the assistant configuration or UI model selector.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnAgentInput {
    pub name: String,
    #[serde(default)]
    #[serde(alias = "assistantId")]
    pub assistant_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TaskCreateInput {
    pub subject: String,
    pub description: Option<String>,
    pub owner: Option<String>,
    pub blocked_by: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct TaskUpdateInput {
    pub task_id: String,
    pub status: Option<String>,
    pub description: Option<String>,
    pub owner: Option<String>,
    pub blocked_by: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum TaskListStatusInput {
    Single(String),
    Many(Vec<String>),
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskListInput {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub status: Option<TaskListStatusInput>,
    #[serde(default)]
    pub include_deleted: Option<bool>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct RenameAgentInput {
    pub slot_id: String,
    pub new_name: String,
}

#[derive(Debug, Deserialize)]
pub struct ShutdownAgentInput {
    pub slot_id: String,
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Built-in backend predicate for spawn_agent (synchronous fast-path).
// Dynamic capability check (MCP-based) happens in TeamSession::spawn_agent.
// ---------------------------------------------------------------------------

pub fn is_builtin_mcp_backend(backend: &str) -> bool {
    backend == aionui_common::constants::AIONRS_RUNTIME_BACKEND
}

// ---------------------------------------------------------------------------
// Parse tool call into SchedulerAction
// ---------------------------------------------------------------------------

pub fn parse_tool_call(
    tool_name: &str,
    arguments: &Value,
    _caller_role: TeammateRole,
) -> Result<SchedulerAction, String> {
    match tool_name {
        "team_send_message" => {
            let input: SendMessageInput = serde_json::from_value(arguments.clone())
                .map_err(|e| format!("Invalid arguments for team_send_message: {e}"))?;
            Ok(SchedulerAction::SendMessage {
                to: input.to,
                message: input.message,
                files: input.files,
            })
        }
        "team_spawn_agent" => Err("handled directly by server".into()),
        "team_task_create" => {
            let input: TaskCreateInput = serde_json::from_value(arguments.clone())
                .map_err(|e| format!("Invalid arguments for team_task_create: {e}"))?;
            Ok(SchedulerAction::TaskCreate {
                subject: input.subject,
                description: input.description,
                owner: input.owner,
                blocked_by: input.blocked_by.unwrap_or_default(),
            })
        }
        "team_task_update" => {
            let input: TaskUpdateInput = serde_json::from_value(arguments.clone())
                .map_err(|e| format!("Invalid arguments for team_task_update: {e}"))?;
            Ok(SchedulerAction::TaskUpdate {
                task_id: input.task_id,
                status: input.status,
                description: input.description,
                owner: input.owner,
                blocked_by: input.blocked_by,
            })
        }
        "team_task_list"
        | "team_members"
        | "team_rename_agent"
        | "team_shutdown_agent"
        | "team_list_assistants"
        | "team_describe_assistant" => Err("handled directly by server".into()),
        _ => Err(format!("Unknown tool: {tool_name}")),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn all_descriptors_count() {
        assert_eq!(all_tool_descriptors().len(), 10);
    }

    #[test]
    fn descriptor_names_are_unique() {
        let descs = all_tool_descriptors();
        let mut names: Vec<&str> = descs.iter().map(|d| d.name.as_str()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 10);
    }

    #[test]
    fn descriptors_have_required_fields() {
        for d in all_tool_descriptors() {
            assert!(!d.name.is_empty());
            assert!(!d.description.is_empty());
            assert_eq!(d.input_schema["type"], "object");
        }
    }

    #[test]
    fn team_spawn_agent_description_is_aionui_original() {
        let desc = all_tool_descriptors()
            .into_iter()
            .find(|d| d.name == "team_spawn_agent")
            .expect("team_spawn_agent descriptor must exist")
            .description;
        assert_eq!(desc, TEAM_SPAWN_AGENT_DESCRIPTION);
        assert!(
            desc.contains("Before calling this tool"),
            "description must be the full AionUi original, not the legacy one-liner"
        );
        assert!(
            desc.contains("explicitly approved"),
            "description must retain the explicit-approval precondition clause"
        );
    }

    #[test]
    fn team_spawn_agent_schema_exposes_assistant_id_without_model_override() {
        let desc = all_tool_descriptors()
            .into_iter()
            .find(|d| d.name == "team_spawn_agent")
            .unwrap();
        let props = desc.input_schema["properties"].as_object().unwrap();
        assert!(
            !props.contains_key("model"),
            "team_spawn_agent must not expose model override; model comes from assistant configuration/UI"
        );
        assert!(
            props.contains_key("assistant_id"),
            "schema must expose 'assistant_id' field"
        );
        assert!(
            !props.contains_key("agent_type"),
            "assistant-first schema must not expose 'agent_type'"
        );
        assert!(!props.contains_key("role"), "spawn schema must not expose role");
    }

    #[test]
    fn team_spawn_agent_rejects_role_argument() {
        let result = serde_json::from_value::<SpawnAgentInput>(json!({
            "name": "Helper",
            "assistant_id": "word-creator",
            "role": "teammate"
        }));

        assert!(result.is_err(), "role must be denied as an unknown field");
    }

    #[test]
    fn team_spawn_agent_rejects_model_argument() {
        let result = serde_json::from_value::<SpawnAgentInput>(json!({
            "name": "Helper",
            "assistant_id": "word-creator",
            "model": "claude-sonnet-4"
        }));

        assert!(result.is_err(), "model must be denied as an unknown field");
    }

    #[test]
    fn team_spawn_agent_schema_requires_name_and_assistant_id() {
        let desc = all_tool_descriptors()
            .into_iter()
            .find(|d| d.name == "team_spawn_agent")
            .unwrap();
        let required = desc.input_schema["required"].as_array().unwrap();
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"name"), "name must be required");
        assert!(names.contains(&"assistant_id"), "assistant_id must be required");
        assert!(
            !names.contains(&"backend"),
            "backend should not appear in the assistant-first schema"
        );
    }

    #[test]
    fn parse_send_message() {
        let args = json!({
            "to": "slot-1",
            "message": "hello",
            "files": ["/tmp/image.png"]
        });
        let action = parse_tool_call("team_send_message", &args, TeammateRole::Teammate).unwrap();
        assert!(matches!(
            action,
            SchedulerAction::SendMessage { to, message, files }
            if to == "slot-1"
                && message == "hello"
                && files == vec!["/tmp/image.png".to_owned()]
        ));
    }

    #[test]
    fn parse_spawn_agent_is_handled_directly_by_server() {
        let args = json!({"name": "Helper", "assistant_id": "word-creator"});
        let result = parse_tool_call("team_spawn_agent", &args, TeammateRole::Lead);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("handled directly by server"));
    }

    #[test]
    fn parse_spawn_agent_teammate_rejected() {
        let args = json!({"name": "X", "assistant_id": "word-creator"});
        let result = parse_tool_call("team_spawn_agent", &args, TeammateRole::Teammate);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("handled directly by server"));
    }

    #[test]
    fn parse_spawn_agent_with_legacy_agent_type_is_handled_directly_by_server() {
        let args = json!({"name": "X", "agent_type": "malicious"});
        let result = parse_tool_call("team_spawn_agent", &args, TeammateRole::Lead);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("handled directly by server"));
    }

    #[test]
    fn parse_task_create() {
        let args = json!({"subject": "Implement X", "owner": "slot-a"});
        let action = parse_tool_call("team_task_create", &args, TeammateRole::Teammate).unwrap();
        assert!(matches!(
            action,
            SchedulerAction::TaskCreate { subject, owner, .. }
            if subject == "Implement X" && owner == Some("slot-a".into())
        ));
    }

    #[test]
    fn parse_task_update() {
        let args = json!({"task_id": "tk-1", "status": "completed"});
        let action = parse_tool_call("team_task_update", &args, TeammateRole::Teammate).unwrap();
        assert!(matches!(
            action,
            SchedulerAction::TaskUpdate { task_id, status, .. }
            if task_id == "tk-1" && status == Some("completed".into())
        ));
    }

    #[test]
    fn unknown_tool_errors() {
        let result = parse_tool_call("unknown_tool", &json!({}), TeammateRole::Lead);
        assert!(result.is_err());
    }

    #[test]
    fn builtin_mcp_backend_check() {
        assert!(is_builtin_mcp_backend("aionrs"));
        assert!(!is_builtin_mcp_backend("claude"));
        assert!(!is_builtin_mcp_backend("codex"));
        assert!(!is_builtin_mcp_backend("gpt"));
        assert!(!is_builtin_mcp_backend(""));
    }

    #[test]
    fn parse_send_message_missing_field() {
        let args = json!({"to": "slot-1"});
        let result = parse_tool_call("team_send_message", &args, TeammateRole::Teammate);
        assert!(result.is_err());
    }

    #[test]
    fn parse_spawn_with_explicit_role_is_handled_directly_by_server() {
        let args = json!({"name": "W", "role": "worker", "assistant_id": "word-creator"});
        let result = parse_tool_call("team_spawn_agent", &args, TeammateRole::Lead);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("handled directly by server"));
    }

    #[test]
    fn task_create_with_blocked_by() {
        let args = json!({"subject": "Test", "blocked_by": ["tk-a", "tk-b"]});
        let action = parse_tool_call("team_task_create", &args, TeammateRole::Lead).unwrap();
        assert!(matches!(
            action,
            SchedulerAction::TaskCreate { blocked_by, .. }
            if blocked_by == vec!["tk-a", "tk-b"]
        ));
    }

    #[test]
    fn parse_task_list_handled_by_server() {
        let result = parse_tool_call("team_task_list", &json!({}), TeammateRole::Teammate);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("handled directly by server"));
    }

    #[test]
    fn parse_members_handled_by_server() {
        let result = parse_tool_call("team_members", &json!({}), TeammateRole::Lead);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("handled directly by server"));
    }

    #[test]
    fn parse_rename_agent_handled_by_server() {
        let args = json!({"slot_id": "s1", "new_name": "X"});
        let result = parse_tool_call("team_rename_agent", &args, TeammateRole::Lead);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("handled directly by server"));
    }

    #[test]
    fn parse_shutdown_agent_handled_by_server() {
        let args = json!({"slot_id": "s1"});
        let result = parse_tool_call("team_shutdown_agent", &args, TeammateRole::Lead);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("handled directly by server"));
    }

    // ---- D4 descriptor text remains aligned with assistant-first MCP contract ----

    #[test]
    fn team_describe_assistant_descriptor_text_matches() {
        let desc = all_tool_descriptors()
            .into_iter()
            .find(|d| d.name == "team_describe_assistant")
            .expect("team_describe_assistant descriptor missing");
        assert_eq!(desc.description, TEAM_DESCRIBE_ASSISTANT_DESCRIPTION);
        assert!(
            desc.description
                .starts_with("Get detailed information about an assistant")
        );
        assert!(
            desc.description
                .contains("After confirming a match, call team_spawn_agent with the same assistant_id.")
        );
    }

    #[test]
    fn team_describe_assistant_schema_prefers_assistant_id() {
        let desc = all_tool_descriptors()
            .into_iter()
            .find(|d| d.name == "team_describe_assistant")
            .unwrap();
        let props = desc.input_schema["properties"].as_object().unwrap();
        let required = desc.input_schema["required"].as_array().unwrap();
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(props.contains_key("assistant_id"));
        assert!(!props.contains_key("custom_agent_id"));
        assert!(names.contains(&"assistant_id"));
        assert!(!names.contains(&"custom_agent_id"));
    }

    #[test]
    fn team_list_assistants_descriptor_guides_real_assistant_ids() {
        let desc = all_tool_descriptors()
            .into_iter()
            .find(|d| d.name == "team_list_assistants")
            .expect("team_list_assistants descriptor missing");
        assert!(
            desc.description
                .starts_with("List the assistants available for team spawning."),
            "unexpected descriptor text: {}",
            desc.description
        );
        assert!(desc.description.contains("real assistant_id values"));
    }

    #[test]
    fn team_list_assistants_schema_is_empty_object() {
        let desc = all_tool_descriptors()
            .into_iter()
            .find(|d| d.name == "team_list_assistants")
            .expect("team_list_assistants descriptor missing");
        let props = desc.input_schema["properties"].as_object().unwrap();
        assert!(props.is_empty(), "team_list_assistants should not accept arguments");
        assert!(desc.input_schema["required"].is_null());
    }

    #[test]
    fn parse_spawn_agent_requires_explicit_assistant_id_field() {
        let input: SpawnAgentInput = serde_json::from_value(json!({
            "name": "Preset helper",
            "assistant_id": "word-creator",
        }))
        .unwrap();
        assert_eq!(input.assistant_id.as_deref(), Some("word-creator"));
    }

    #[test]
    fn team_spawn_agent_schema_requires_assistant_id_only() {
        let desc = all_tool_descriptors()
            .into_iter()
            .find(|d| d.name == "team_spawn_agent")
            .unwrap();
        let props = desc.input_schema["properties"].as_object().unwrap();
        let assistant_desc = props["assistant_id"]["description"].as_str().unwrap();
        assert!(assistant_desc.starts_with("Assistant ID to spawn"));
        assert!(!props.contains_key("model"));
        assert!(!props.contains_key("agent_type"));
        assert!(!props.contains_key("backend"));
    }

    #[test]
    fn team_spawn_agent_description_uses_assistant_first_staffing_language() {
        let desc = all_tool_descriptors()
            .into_iter()
            .find(|d| d.name == "team_spawn_agent")
            .unwrap();
        assert!(desc.description.contains("recommended assistant"));
        assert!(!desc.description.contains("recommended model"));
        assert!(!desc.description.contains("model parameter"));
        assert!(!desc.description.contains("recommended assistant or backend"));
    }

    #[test]
    fn team_describe_assistant_description_uses_assistant_only_wording() {
        let desc = all_tool_descriptors()
            .into_iter()
            .find(|d| d.name == "team_describe_assistant")
            .unwrap();
        let props = desc.input_schema["properties"].as_object().unwrap();
        let assistant_desc = props["assistant_id"]["description"].as_str().unwrap();
        assert!(desc.description.contains("Get detailed information about an assistant"));
        assert!(!desc.description.contains("preset assistant"));
        assert!(!desc.description.contains("Available Preset Assistants"));
        assert!(assistant_desc.starts_with("The assistant ID from the available assistants catalog"));
        assert!(!assistant_desc.contains("preset assistant ID"));
    }
}
