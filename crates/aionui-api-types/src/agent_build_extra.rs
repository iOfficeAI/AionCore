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
///
/// `use_ollama` deserialization follows priority:
/// 1. If JSON includes an explicit `use_ollama` field → use that value.
/// 2. Otherwise, `use_ollama` defaults to `false`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct AcpBuildExtra {
    pub agent_id: Option<String>,
    pub backend: Option<String>,
    pub cli_path: Option<String>,
    pub agent_name: Option<String>,
    pub custom_agent_id: Option<String>,
    pub preset_context: Option<String>,
    pub skills: Vec<String>,
    pub preset_assistant_id: Option<String>,
    pub session_mode: Option<String>,
    pub current_model_id: Option<String>,
    pub thought_level: Option<String>,
    pub cron_job_id: Option<String>,
    pub team_mcp_stdio_config: Option<TeamMcpStdioConfig>,
    pub mcp_server_ids: Option<Vec<String>>,
    pub session_mcp_servers: Vec<SessionMcpServer>,
    pub user_id: Option<String>,

    /// When enabled, delegates agent spawning to `ollama launch <agent>`
    /// instead of the agent's native command.
    pub use_ollama: bool,

    /// Model to use with Ollama Launch (e.g. `"llama3.2"`, `"qwen3:14b"`).
    ///
    /// Required when `use_ollama` is `true` — Ollama Launch cannot run
    /// in headless mode (no TTY) without an explicit model selection.
    /// When set, AionCore passes `--model <ollama_model> -y` to
    /// `ollama launch`. When absent, the backend falls back to the
    /// agent's native command.
    pub ollama_model: Option<String>,
}

// Custom Deserialize to ensure `use_ollama` is only true when explicitly
// set by the caller. The presence of `ollama_model` alone must not toggle
// `use_ollama`. This avoids ambiguity in CI environments where serde
// derive behavior may differ across toolchain versions.
impl<'de> serde::Deserialize<'de> for AcpBuildExtra {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize, Default)]
        #[serde(default)]
        struct Raw {
            agent_id: Option<String>,
            backend: Option<String>,
            cli_path: Option<String>,
            agent_name: Option<String>,
            custom_agent_id: Option<String>,
            preset_context: Option<String>,
            skills: Vec<String>,
            preset_assistant_id: Option<String>,
            session_mode: Option<String>,
            current_model_id: Option<String>,
            thought_level: Option<String>,
            cron_job_id: Option<String>,
            team_mcp_stdio_config: Option<TeamMcpStdioConfig>,
            mcp_server_ids: Option<Vec<String>>,
            session_mcp_servers: Vec<SessionMcpServer>,
            user_id: Option<String>,
            use_ollama: Option<bool>,
            ollama_model: Option<String>,
        }

        let raw = Raw::deserialize(deserializer)?;

        Ok(AcpBuildExtra {
            agent_id: raw.agent_id,
            backend: raw.backend,
            cli_path: raw.cli_path,
            agent_name: raw.agent_name,
            custom_agent_id: raw.custom_agent_id,
            preset_context: raw.preset_context,
            skills: raw.skills,
            preset_assistant_id: raw.preset_assistant_id,
            session_mode: raw.session_mode,
            current_model_id: raw.current_model_id,
            thought_level: raw.thought_level,
            cron_job_id: raw.cron_job_id,
            team_mcp_stdio_config: raw.team_mcp_stdio_config,
            mcp_server_ids: raw.mcp_server_ids,
            session_mcp_servers: raw.session_mcp_servers,
            user_id: raw.user_id,
            use_ollama: raw.use_ollama.unwrap_or(false),
            ollama_model: raw.ollama_model,
        })
    }
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
    pub max_tokens: Option<u32>,
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
    fn aionrs_build_extra_ignores_legacy_guide_config_field() {
        let legacy_key = concat!("guide", "_mcp_config");
        let parsed: AionrsBuildExtra = serde_json::from_value(serde_json::json!({
            "backend": "aionrs",
            legacy_key: {"port": 1234, "token": "legacy", "binary_path": "/bin/aioncore"}
        }))
        .unwrap();

        assert_eq!(parsed.backend.as_deref(), Some("aionrs"));
        let serialized = serde_json::to_value(&parsed).unwrap();
        assert!(
            serialized.get(legacy_key).is_none(),
            "legacy guide config must be ignored, not re-serialized"
        );
    }
}
