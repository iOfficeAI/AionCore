use serde::{Deserialize, Serialize};

use crate::id::fnv1a_hex8;

/// Type of AI agent backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentType {
    Acp,
    #[serde(rename = "openclaw-gateway")]
    OpenclawGateway,
    Nanobot,
    Remote,
    Aionrs,
    /// Legacy Gemini conversations. Kept solely so that historical rows
    /// with `type='gemini'` remain readable in the conversation list and
    /// message history. Any attempt to run the agent (send a message,
    /// resume a session) returns an error — this variant has no factory
    /// branch. New Gemini conversations use `AgentType::Acp` with
    /// `backend='gemini'`.
    Gemini,
    /// Legacy Codex conversations. Kept solely so that historical rows
    /// with `type='codex'` remain readable in the conversation list and
    /// message history. New Codex conversations use `AgentType::Acp` with
    /// `backend='codex'`.
    Codex,
}

impl AgentType {
    pub fn display_name(&self) -> &'static str {
        match self {
            AgentType::Acp => "ACP",
            AgentType::OpenclawGateway => "OpenClaw Gateway",
            AgentType::Nanobot => "Nanobot",
            AgentType::Remote => "Remote",
            AgentType::Aionrs => "Aion CLI",
            AgentType::Gemini => "Gemini (legacy)",
            AgentType::Codex => "Codex (legacy)",
        }
    }

    pub fn serde_name(&self) -> &'static str {
        match self {
            AgentType::Acp => "acp",
            AgentType::OpenclawGateway => "openclaw-gateway",
            AgentType::Nanobot => "nanobot",
            AgentType::Remote => "remote",
            AgentType::Aionrs => "aionrs",
            AgentType::Gemini => "gemini",
            AgentType::Codex => "codex",
        }
    }

    pub fn supports_new_conversation(&self) -> bool {
        matches!(self, AgentType::Acp | AgentType::Aionrs)
    }

    pub fn is_deprecated_runtime(&self) -> bool {
        !self.supports_new_conversation()
    }

    pub fn id(&self) -> String {
        let hash = fnv1a_hex8(self.serde_name().as_bytes());
        // SAFETY: fnv1a_hex8 only produces ASCII hex digits
        unsafe { std::str::from_utf8_unchecked(&hash) }.into()
    }

    /// Native skill-discovery directories for non-ACP agent types.
    ///
    /// ACP vendors own their skill dirs through the `agent_metadata`
    /// table; this method covers the few non-ACP agent types that still
    /// support native skill discovery. Returns `None` for agent types
    /// that require prompt-injection instead of workspace symlinks.
    ///
    /// `AgentType::Gemini` is intentionally absent: new Gemini
    /// conversations use `AgentType::Acp` with `backend = "gemini"`, so
    /// their skill dirs come from the Gemini row in the catalog.
    /// Historical `AgentType::Gemini` rows cannot start a new runtime
    /// (see the variant's doc comment) and therefore never reach this
    /// path during workspace provisioning.
    ///
    /// `AgentType::Codex` follows the same historical-only policy. New
    /// Codex conversations use ACP metadata with `backend = "codex"`.
    pub fn native_skills_dirs(&self) -> Option<&'static [&'static str]> {
        match self {
            AgentType::Aionrs => Some(&[".aionrs/skills"]),
            AgentType::Acp
            | AgentType::OpenclawGateway
            | AgentType::Nanobot
            | AgentType::Remote
            | AgentType::Gemini
            | AgentType::Codex => None,
        }
    }

    /// Canonical full-auto session mode id for this agent type.
    ///
    /// ACP agents need backend-specific mode ids, while other agent types
    /// currently converge on the permissive `yolo` mode. Hermes is the
    /// exception: it has no full-auto ACP mode, so callers must stay on its
    /// native `default`.
    ///
    /// `backend` is the vendor label (e.g. `"claude"`, `"codex"`) used
    /// only by ACP; pass `None` for non-ACP agents. This mapping is
    /// duplicated in the seed of `agent_metadata.yolo_id` — code paths
    /// with DB access should prefer reading that column. This function
    /// is a fallback for offline / pre-hydrate callers (cron, tests).
    pub fn full_auto_mode_id(&self, backend: Option<&str>) -> &'static str {
        match self {
            AgentType::Acp => match backend {
                Some("claude") | Some("codebuddy") => "bypassPermissions",
                Some("codex") => "agent-full-access",
                Some("hermes") => "default",
                Some("opencode") => "build",
                Some("cursor") => "agent",
                _ => "yolo",
            },
            AgentType::Aionrs
            | AgentType::Gemini
            | AgentType::Codex
            | AgentType::OpenclawGateway
            | AgentType::Nanobot
            | AgentType::Remote => "yolo",
        }
    }
}

/// Runtime status of a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConversationStatus {
    Pending,
    Running,
    Finished,
}

/// Origin of a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConversationSource {
    Aionui,
    Telegram,
    Lark,
    Dingtalk,
    Weixin,
}

/// Type discriminant for messages in a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    Text,
    Tips,
    ToolCall,
    ToolGroup,
    AgentStatus,
    Permission,
    AcpToolCall,
    Plan,
    Thinking,
    AvailableCommands,
    SkillSuggest,
    CronTrigger,
}

/// Display position of a message in the chat UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessagePosition {
    Right,
    Left,
    Center,
    Pop,
}

/// Processing status of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageStatus {
    Finish,
    Pending,
    Error,
    Work,
}

/// LLM API protocol type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProtocolType {
    #[serde(rename = "openai")]
    OpenAI,
    Anthropic,
    Gemini,
    Unknown,
}

/// Remote Agent protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteAgentProtocol {
    OpenClaw,
    ZeroClaw,
    Acp,
}

/// Remote Agent authentication method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteAgentAuthType {
    Bearer,
    Password,
    None,
}

/// Remote Agent connection status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteAgentStatus {
    Unknown,
    Connected,
    Pending,
    Error,
}

/// Reason for terminating an Agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKillReason {
    IdleTimeout,
    /// The ACP session ended a turn with a terminal error. The conversation is
    /// preserved; only the in-memory agent task is recycled before the next send
    /// so a potentially desynchronised upstream session is not reused.
    AgentErrorRecovery,
    /// Team session is rebuilding the agent process to inject a fresh
    /// `team_mcp_stdio_config`. The conversation is preserved; only the
    /// in-memory ACP CLI is recycled.
    TeamMcpRebuild,
    /// Team is being deleted; every agent process under it must be torn
    /// down before the team's conversations / rows are removed.
    TeamDeleted,
    /// The owning conversation was deleted via `DELETE /api/conversations/{id}`.
    /// The agent process must be torn down so it stops emitting stream events
    /// for a conversation row that no longer exists.
    ConversationDeleted,
    /// User cancel was accepted but the ACP prompt did not drain before the
    /// watchdog timeout. The stale ACP process is recycled while the user turn
    /// is treated as a clean cancellation.
    UserCancelTimeout,
    /// The requested runtime capabilities changed, so the in-memory task must
    /// be rebuilt before handling the next turn.
    RuntimeCapabilityChanged,
}

/// Preview content type for document preview history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewContentType {
    Markdown,
    Diff,
    Code,
    Html,
    Pdf,
    Ppt,
    Word,
    Excel,
    Image,
    Url,
}

/// File change operation type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileChangeOperation {
    Create,
    Modify,
    Delete,
}

/// AI Agent CLI source identifier for MCP configuration sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpSource {
    Claude,
    Gemini,
    Qwen,
    Codex,
    #[serde(rename = "codebuddy")]
    CodeBuddy,
    #[serde(rename = "opencode")]
    OpenCode,
    Aionrs,
    Nanobot,
    Aionui,
}

/// MCP server connection status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpServerStatus {
    Connected,
    Disconnected,
    Error,
    Testing,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_type_display_names() {
        assert_eq!(AgentType::OpenclawGateway.display_name(), "OpenClaw Gateway");
        assert_eq!(AgentType::Aionrs.display_name(), "Aion CLI");
        assert_eq!(AgentType::Nanobot.display_name(), "Nanobot");
        assert_eq!(AgentType::Remote.display_name(), "Remote");
        assert_eq!(AgentType::Acp.display_name(), "ACP");
        assert_eq!(AgentType::Codex.display_name(), "Codex (legacy)");
    }

    #[test]
    fn test_agent_type_id_stability() {
        let id = AgentType::Aionrs.id();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(AgentType::Aionrs.id(), AgentType::Aionrs.id());
    }

    #[test]
    fn test_agent_type_id_unique_per_variant() {
        let ids: Vec<String> = [
            AgentType::Acp,
            AgentType::OpenclawGateway,
            AgentType::Nanobot,
            AgentType::Remote,
            AgentType::Aionrs,
            AgentType::Codex,
        ]
        .iter()
        .map(|t| t.id())
        .collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn test_agent_type_serde_roundtrip() {
        let val = AgentType::OpenclawGateway;
        let json = serde_json::to_string(&val).unwrap();
        assert_eq!(json, r#""openclaw-gateway""#);
        let parsed: AgentType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, val);
    }

    #[test]
    fn test_agent_type_all_variants() {
        let cases = [
            (AgentType::Acp, "acp"),
            (AgentType::OpenclawGateway, "openclaw-gateway"),
            (AgentType::Nanobot, "nanobot"),
            (AgentType::Remote, "remote"),
            (AgentType::Aionrs, "aionrs"),
            (AgentType::Codex, "codex"),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, format!("\"{expected}\""), "serialize {variant:?}");
            let parsed: AgentType = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant, "deserialize {expected}");
        }
    }

    #[test]
    fn agent_type_new_conversation_support_policy_is_explicit() {
        assert!(AgentType::Acp.supports_new_conversation());
        assert!(AgentType::Aionrs.supports_new_conversation());

        assert!(!AgentType::Gemini.supports_new_conversation());
        assert!(!AgentType::Codex.supports_new_conversation());
        assert!(!AgentType::OpenclawGateway.supports_new_conversation());
        assert!(!AgentType::Nanobot.supports_new_conversation());
        assert!(!AgentType::Remote.supports_new_conversation());
    }

    #[test]
    fn agent_type_deprecated_runtime_policy_matches_new_conversation_support() {
        assert!(!AgentType::Acp.is_deprecated_runtime());
        assert!(!AgentType::Aionrs.is_deprecated_runtime());

        assert!(AgentType::Gemini.is_deprecated_runtime());
        assert!(AgentType::Codex.is_deprecated_runtime());
        assert!(AgentType::OpenclawGateway.is_deprecated_runtime());
        assert!(AgentType::Nanobot.is_deprecated_runtime());
        assert!(AgentType::Remote.is_deprecated_runtime());
    }

    #[test]
    fn test_protocol_type_openai() {
        let val = ProtocolType::OpenAI;
        let json = serde_json::to_string(&val).unwrap();
        assert_eq!(json, r#""openai""#);
        let parsed: ProtocolType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ProtocolType::OpenAI);
    }

    #[test]
    fn test_conversation_status_lowercase() {
        let val = ConversationStatus::Pending;
        let json = serde_json::to_string(&val).unwrap();
        assert_eq!(json, r#""pending""#);
    }

    #[test]
    fn test_message_type_snake_case() {
        let val = MessageType::ToolCall;
        let json = serde_json::to_string(&val).unwrap();
        assert_eq!(json, r#""tool_call""#);

        let val = MessageType::AcpToolCall;
        let json = serde_json::to_string(&val).unwrap();
        assert_eq!(json, r#""acp_tool_call""#);

        let val = MessageType::AgentStatus;
        let json = serde_json::to_string(&val).unwrap();
        assert_eq!(json, r#""agent_status""#);
    }

    #[test]
    fn test_file_change_operation_roundtrip() {
        for op in [
            FileChangeOperation::Create,
            FileChangeOperation::Modify,
            FileChangeOperation::Delete,
        ] {
            let json = serde_json::to_string(&op).unwrap();
            let parsed: FileChangeOperation = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, op);
        }
    }

    #[test]
    fn test_mcp_source_serde_roundtrip() {
        let cases = [
            (McpSource::Claude, r#""claude""#),
            (McpSource::Gemini, r#""gemini""#),
            (McpSource::Qwen, r#""qwen""#),
            (McpSource::Codex, r#""codex""#),
            (McpSource::CodeBuddy, r#""codebuddy""#),
            (McpSource::OpenCode, r#""opencode""#),
            (McpSource::Aionrs, r#""aionrs""#),
            (McpSource::Nanobot, r#""nanobot""#),
            (McpSource::Aionui, r#""aionui""#),
        ];
        for (variant, expected_json) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_json, "serialize {variant:?}");
            let parsed: McpSource = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant, "deserialize {expected_json}");
        }
    }

    #[test]
    fn test_mcp_server_status_serde_roundtrip() {
        let cases = [
            (McpServerStatus::Connected, r#""connected""#),
            (McpServerStatus::Disconnected, r#""disconnected""#),
            (McpServerStatus::Error, r#""error""#),
            (McpServerStatus::Testing, r#""testing""#),
        ];
        for (variant, expected_json) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_json, "serialize {variant:?}");
            let parsed: McpServerStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant, "deserialize {expected_json}");
        }
    }

    #[test]
    fn agent_type_full_auto_mode_id_supports_non_acp_agents() {
        assert_eq!(AgentType::Acp.full_auto_mode_id(Some("codex")), "agent-full-access");
        assert_eq!(AgentType::Acp.full_auto_mode_id(Some("claude")), "bypassPermissions");
        assert_eq!(AgentType::Acp.full_auto_mode_id(Some("gemini")), "yolo");
        assert_eq!(AgentType::Acp.full_auto_mode_id(Some("hermes")), "default");
        assert_eq!(AgentType::Acp.full_auto_mode_id(None), "yolo");
        assert_eq!(AgentType::Aionrs.full_auto_mode_id(None), "yolo");
        assert_eq!(AgentType::Remote.full_auto_mode_id(None), "yolo");
    }
}
