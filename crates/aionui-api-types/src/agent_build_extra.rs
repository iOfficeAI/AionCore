use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::TeamMcpStdioConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionMcpTransport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    Sse {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    StreamableHttp {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMcpServer {
    pub id: String,
    pub name: String,
    pub transport: SessionMcpTransport,
}

/// ACP-specific fields extracted from `extra` in build task options.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AcpBuildExtra {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub cli_path: Option<String>,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub custom_agent_id: Option<String>,
    #[serde(default)]
    pub preset_context: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub preset_assistant_id: Option<String>,
    #[serde(default)]
    pub session_mode: Option<String>,
    #[serde(default)]
    pub current_model_id: Option<String>,
    #[serde(default)]
    pub thought_level: Option<String>,
    #[serde(default)]
    pub cron_job_id: Option<String>,
    #[serde(default)]
    pub team_mcp_stdio_config: Option<TeamMcpStdioConfig>,
    #[serde(default)]
    pub mcp_server_ids: Option<Vec<String>>,
    #[serde(default)]
    pub session_mcp_servers: Vec<SessionMcpServer>,
    #[serde(default)]
    pub user_id: Option<String>,
}

/// Aionrs-specific fields extracted from `extra` in build task options.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AionrsBuildExtra {
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub preset_rules: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub max_turns: Option<usize>,
    #[serde(default)]
    pub max_tool_call_malformed_turns: Option<usize>,
    #[serde(default)]
    pub max_tool_call_failure_turns: Option<usize>,
    #[serde(default)]
    pub session_mode: Option<String>,
    #[serde(default)]
    pub team_mcp_stdio_config: Option<TeamMcpStdioConfig>,
    #[serde(default)]
    pub mcp_server_ids: Option<Vec<String>>,
    #[serde(default)]
    pub session_mcp_servers: Vec<SessionMcpServer>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
}

/// ACP model information returned by the ACP backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpModelInfo {
    pub model_id: String,
    pub model_name: Option<String>,
    pub provider: Option<String>,
}

/// Controls what happens when a slash command produces an empty turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlashCommandCompletionBehavior {
    Normal,
    NeutralTipOnEmpty,
}

/// A slash command item available in a conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandItem {
    pub command: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_behavior: Option<SlashCommandCompletionBehavior>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_turn_tip_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_turn_tip_params: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_build_extra_defaults_thought_level_to_none() {
        let parsed: AcpBuildExtra = serde_json::from_str(r#"{"backend":"codex"}"#).unwrap();
        assert!(parsed.thought_level.is_none());
    }

    #[test]
    fn acp_build_extra_parses_thought_level_seed() {
        let parsed: AcpBuildExtra = serde_json::from_str(r#"{"backend":"codex","thought_level":"high"}"#).unwrap();
        assert_eq!(parsed.thought_level.as_deref(), Some("high"));
    }

    #[test]
    fn acp_build_extra_ignores_legacy_guide_config_field() {
        let legacy_key = concat!("guide", "_mcp_config");
        let parsed: AcpBuildExtra = serde_json::from_value(serde_json::json!({
            "backend": "claude",
            legacy_key: {"port": 1234, "token": "legacy", "binary_path": "/bin/aioncore"}
        }))
        .unwrap();

        assert_eq!(parsed.backend.as_deref(), Some("claude"));
        let serialized = serde_json::to_value(&parsed).unwrap();
        assert!(
            serialized.get(legacy_key).is_none(),
            "legacy guide config must be ignored, not re-serialized"
        );
    }

    #[test]
    fn aionrs_build_extra_ignores_legacy_fields() {
        let legacy_key = concat!("guide", "_mcp_config");
        let parsed: AionrsBuildExtra = serde_json::from_value(serde_json::json!({
            "backend": "aionrs",
            "max_tokens": 8192,
            legacy_key: {"port": 1234, "token": "legacy", "binary_path": "/bin/aioncore"}
        }))
        .unwrap();

        assert_eq!(parsed.backend.as_deref(), Some("aionrs"));
        let serialized = serde_json::to_value(&parsed).unwrap();
        assert!(
            serialized.get(legacy_key).is_none(),
            "legacy guide config must be ignored, not re-serialized"
        );
        assert!(
            serialized.get("max_tokens").is_none(),
            "legacy max_tokens must be ignored, not re-serialized"
        );
    }
}
