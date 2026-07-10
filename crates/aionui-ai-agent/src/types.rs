use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::session_context::AgentSessionContext;

/// Data payload for sending a user message to an Agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageData {
    /// User message content.
    pub content: String,
    /// Client-generated message ID for correlation.
    pub msg_id: String,
    /// Runtime turn ID for backend logs and tests. Not part of the ACP wire protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// File paths attached to the message.
    #[serde(default)]
    pub files: Vec<String>,
    /// Skills to inject into this message turn.
    #[serde(default)]
    pub inject_skills: Vec<String>,
}

/// Options for building (creating or resuming) an Agent task.
#[derive(Debug, Clone)]
pub struct BuildTaskOptions {
    pub context: AgentSessionContext,
    pub runtime_capabilities: RuntimeCapabilities,
}

impl BuildTaskOptions {
    pub fn new(context: AgentSessionContext) -> Self {
        Self {
            context,
            runtime_capabilities: RuntimeCapabilities::default(),
        }
    }

    pub fn conversation_id(&self) -> &str {
        self.context.conversation_id()
    }

    pub fn apply_conversation_runtime_context(
        &mut self,
        user_id: &str,
        conversation_id: &str,
        helper_bin: Option<&str>,
        base_url: Option<&str>,
    ) {
        self.context.runtime_env.retain(|(key, _)| {
            !matches!(
                key.as_str(),
                AIONUI_USER_ID_ENV | AIONUI_CONVERSATION_ID_ENV | AIONUI_HELPER_BIN_ENV | AIONUI_BASE_URL_ENV
            )
        });
        self.context
            .runtime_env
            .push((AIONUI_USER_ID_ENV.to_owned(), user_id.to_owned()));
        self.context
            .runtime_env
            .push((AIONUI_CONVERSATION_ID_ENV.to_owned(), conversation_id.to_owned()));
        if let Some(helper_bin) = helper_bin {
            self.context
                .runtime_env
                .push((AIONUI_HELPER_BIN_ENV.to_owned(), helper_bin.to_owned()));
        }
        if let Some(base_url) = base_url {
            self.context
                .runtime_env
                .push((AIONUI_BASE_URL_ENV.to_owned(), base_url.to_owned()));
        }
        self.runtime_capabilities.conversation_runtime_context_version = Some(CONVERSATION_RUNTIME_CONTEXT_VERSION);
    }
}

pub const AIONUI_USER_ID_ENV: &str = "AIONUI_USER_ID";
pub const AIONUI_CONVERSATION_ID_ENV: &str = "AIONUI_CONVERSATION_ID";
pub const AIONUI_HELPER_BIN_ENV: &str = "AIONUI_HELPER_BIN";
pub const AIONUI_BASE_URL_ENV: &str = "AIONUI_BASE_URL";
pub const CONVERSATION_RUNTIME_CONTEXT_VERSION: u32 = 2;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeCapabilities {
    pub conversation_runtime_context_version: Option<u32>,
}

impl RuntimeCapabilities {
    pub fn satisfies(&self, requested: &Self) -> bool {
        match requested.conversation_runtime_context_version {
            Some(version) => self
                .conversation_runtime_context_version
                .is_some_and(|actual| actual >= version),
            None => true,
        }
    }
}

/// Provider-specific compat overrides resolved in the factory.
#[derive(Debug, Clone, Default)]
pub struct AionrsCompatOverrides {
    pub max_tokens_field: Option<String>,
    pub api_path: Option<String>,
}

/// Fully resolved Aionrs configuration passed to the agent manager.
#[derive(Debug, Clone)]
pub struct AionrsResolvedConfig {
    /// LLM provider name (anthropic, openai, bedrock, vertex).
    pub provider: String,
    /// Decrypted API key.
    pub api_key: String,
    /// Model identifier.
    pub model: String,
    /// Provider base URL.
    pub base_url: Option<String>,
    /// System prompt override.
    pub system_prompt: Option<String>,
    /// Optional max tokens per response.
    pub max_tokens: Option<u32>,
    /// Max agentic turns.
    pub max_turns: Option<usize>,
    /// Max repeated malformed tool-call turns before stopping.
    pub max_tool_call_malformed_turns: Option<usize>,
    /// Max repeated tool-call failure turns before stopping.
    pub max_tool_call_failure_turns: Option<usize>,
    /// Provider-specific compat overrides.
    pub compat_overrides: AionrsCompatOverrides,
    /// Directory for aionrs session persistence files.
    pub session_directory: PathBuf,
    /// Session mode (default, auto_edit, yolo).
    pub session_mode: Option<String>,
    /// Extra MCP servers to inject (team coordination or guide).
    pub extra_mcp_servers: HashMap<String, aion_config::config::McpServerConfig>,
    /// AWS Bedrock credentials (region + access key or profile).
    pub bedrock_config: Option<aion_config::config::BedrockConfig>,
    /// Per-turn environment values exposed to runtime tool execution.
    pub runtime_env: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::{AcpBuildExtra, AcpModelInfo, AionrsBuildExtra, SlashCommandItem};
    use serde_json::json;

    #[test]
    fn acp_build_extra_accepts_payload_without_skills() {
        let legacy = r#"{"backend":"claude"}"#;
        let parsed: AcpBuildExtra = serde_json::from_str(legacy).unwrap();
        assert!(parsed.skills.is_empty());
    }

    #[test]
    fn build_task_options_applies_conversation_runtime_context_once() {
        use crate::session_context::{
            AcpSessionBuildContext, AgentSessionContext, AgentSessionKind, ConversationContext, WorkspaceContext,
        };
        use aionui_common::{AgentType, ProviderWithModel};

        let context = AgentSessionContext {
            conversation: ConversationContext {
                conversation_id: "conv-old".into(),
                user_id: "user-old".into(),
                agent_type: AgentType::Acp,
                source: None,
            },
            workspace: WorkspaceContext {
                path: "/tmp/workspace".into(),
                stored_path: "/tmp/workspace".into(),
                is_custom: false,
            },
            model: ProviderWithModel {
                provider_id: "provider".into(),
                model: "model".into(),
                use_model: None,
            },
            skills: vec![],
            runtime_env: vec![
                (AIONUI_USER_ID_ENV.into(), "old-user".into()),
                (AIONUI_CONVERSATION_ID_ENV.into(), "old-conv".into()),
                ("EXISTING".into(), "1".into()),
            ],
            team: None,
            kind: AgentSessionKind::Acp(Box::new(AcpSessionBuildContext {
                config: Default::default(),
                team: None,
                belongs_to_team: false,
                session_id: None,
                session_snapshot: None,
            })),
        };
        let mut options = BuildTaskOptions::new(context);

        options.apply_conversation_runtime_context(
            "user-1",
            "conv-1",
            Some("/Applications/AionUi/aioncore"),
            Some("http://127.0.0.1:25808"),
        );

        assert_eq!(
            options
                .context
                .runtime_env
                .iter()
                .filter(|(key, _)| key == AIONUI_USER_ID_ENV)
                .count(),
            1
        );
        assert!(
            options
                .context
                .runtime_env
                .contains(&(AIONUI_USER_ID_ENV.to_owned(), "user-1".to_owned()))
        );
        assert!(
            options
                .context
                .runtime_env
                .contains(&(AIONUI_CONVERSATION_ID_ENV.to_owned(), "conv-1".to_owned()))
        );
        assert!(options.context.runtime_env.contains(&(
            AIONUI_HELPER_BIN_ENV.to_owned(),
            "/Applications/AionUi/aioncore".to_owned()
        )));
        assert!(
            options
                .context
                .runtime_env
                .contains(&(AIONUI_BASE_URL_ENV.to_owned(), "http://127.0.0.1:25808".to_owned()))
        );
        assert!(options.context.runtime_env.contains(&("EXISTING".into(), "1".into())));
        assert_eq!(
            options.runtime_capabilities.conversation_runtime_context_version,
            Some(CONVERSATION_RUNTIME_CONTEXT_VERSION)
        );
    }

    #[test]
    fn acp_build_extra_accepts_skills() {
        let with_field = r#"{"backend":"claude","skills":["cron","pdf"]}"#;
        let parsed: AcpBuildExtra = serde_json::from_str(with_field).unwrap();
        assert_eq!(parsed.skills, vec!["cron".to_owned(), "pdf".to_owned()]);
    }

    #[test]
    fn acp_build_extra_accepts_thought_level_seed() {
        let with_field = r#"{"backend":"codex","thought_level":"high"}"#;
        let parsed: AcpBuildExtra = serde_json::from_str(with_field).unwrap();
        assert_eq!(parsed.thought_level.as_deref(), Some("high"));
    }

    #[test]
    fn acp_build_extra_missing_team_mcp_stdio_config_is_none() {
        let legacy = r#"{"backend":"claude","skills":["cron"]}"#;
        let parsed: AcpBuildExtra = serde_json::from_str(legacy).unwrap();
        assert!(parsed.team_mcp_stdio_config.is_none());
    }

    #[test]
    fn acp_build_extra_parses_team_mcp_stdio_config() {
        let with_cfg = r#"{
            "backend":"claude",
            "team_mcp_stdio_config":{
                "team_id":"team-42",
                "port":54321,
                "token":"tok-abc",
                "slot_id":"slot-lead",
                "binary_path":"/bin/backend"
            }
        }"#;
        let parsed: AcpBuildExtra = serde_json::from_str(with_cfg).unwrap();
        let cfg = parsed.team_mcp_stdio_config.expect("config present");
        assert_eq!(cfg.team_id, "team-42");
        assert_eq!(cfg.port, 54321);
        assert_eq!(cfg.token, "tok-abc");
        assert_eq!(cfg.slot_id, "slot-lead");
    }

    #[test]
    fn send_message_data_serde_roundtrip() {
        let data = SendMessageData {
            content: "Hello".into(),
            msg_id: "msg-001".into(),
            turn_id: Some("turn-001".into()),
            files: vec!["/tmp/a.txt".into()],
            inject_skills: vec!["review".into()],
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["content"], "Hello");
        assert_eq!(json["msg_id"], "msg-001");
        assert_eq!(json["turn_id"], "turn-001");
        assert_eq!(json["files"], json!(["/tmp/a.txt"]));
        assert_eq!(json["inject_skills"], json!(["review"]));

        let parsed: SendMessageData = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.content, "Hello");
        assert_eq!(parsed.msg_id, "msg-001");
        assert_eq!(parsed.turn_id.as_deref(), Some("turn-001"));
    }

    #[test]
    fn send_message_data_defaults_optional_fields() {
        let json = json!({ "content": "Hi", "msg_id": "m1" });
        let data: SendMessageData = serde_json::from_value(json).unwrap();
        assert!(data.turn_id.is_none());
        assert!(data.files.is_empty());
        assert!(data.inject_skills.is_empty());
    }

    #[test]
    fn acp_model_info_serde() {
        let info = AcpModelInfo {
            model_id: "claude-sonnet-4".into(),
            model_name: Some("Claude Sonnet 4".into()),
            provider: Some("anthropic".into()),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["model_id"], "claude-sonnet-4");
        assert_eq!(json["model_name"], "Claude Sonnet 4");
    }

    #[test]
    fn slash_command_item_serde() {
        let cmd = SlashCommandItem {
            command: "/review".into(),
            description: "Code review".into(),
            completion_behavior: None,
            empty_turn_tip_code: None,
            empty_turn_tip_params: None,
        };
        let json = serde_json::to_value(&cmd).unwrap();
        assert_eq!(json["command"], "/review");
    }

    #[test]
    fn aionrs_build_extra_serde_defaults() {
        let json = json!({});
        let extra: AionrsBuildExtra = serde_json::from_value(json).unwrap();
        assert!(extra.system_prompt.is_none());
        assert!(extra.preset_rules.is_none());
        assert_eq!(extra.max_tokens, None);
        assert!(extra.max_turns.is_none());
        assert!(extra.max_tool_call_malformed_turns.is_none());
        assert!(extra.max_tool_call_failure_turns.is_none());
    }

    #[test]
    fn aionrs_build_extra_serde_with_overrides() {
        let json = json!({
            "system_prompt": "You are a helpful assistant.",
            "max_tokens": 4096,
            "max_turns": 10,
            "max_tool_call_malformed_turns": 2,
            "max_tool_call_failure_turns": 3
        });
        let extra: AionrsBuildExtra = serde_json::from_value(json).unwrap();
        assert_eq!(extra.system_prompt.unwrap(), "You are a helpful assistant.");
        assert_eq!(extra.max_tokens, Some(4096));
        assert_eq!(extra.max_turns.unwrap(), 10);
        assert_eq!(extra.max_tool_call_malformed_turns.unwrap(), 2);
        assert_eq!(extra.max_tool_call_failure_turns.unwrap(), 3);
    }

    #[test]
    fn aionrs_build_extra_serde_with_preset_rules() {
        let json = json!({
            "preset_rules": "You are a data analyst.",
            "max_tokens": 8192
        });
        let extra: AionrsBuildExtra = serde_json::from_value(json).unwrap();
        assert!(extra.system_prompt.is_none());
        assert_eq!(extra.preset_rules.unwrap(), "You are a data analyst.");
    }

    #[test]
    fn aionrs_build_extra_accepts_frozen_skills_snapshot() {
        let json = json!({
            "preset_rules": "Rules",
            "skills": ["pdf", "cron"]
        });
        let extra: AionrsBuildExtra = serde_json::from_value(json).unwrap();
        assert_eq!(extra.skills, vec!["pdf".to_owned(), "cron".to_owned()]);
    }
}
