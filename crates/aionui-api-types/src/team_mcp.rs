//! Team session MCP stdio connection types.
//!
//! These are promoted from `aionui-team::mcp::bridge` so that downstream
//! crates (`aionui-ai-agent` deserializing `AcpBuildExtra`, etc.) can reference
//! the same shape without depending on `aionui-team`.

use serde::{Deserialize, Serialize};

/// Fixed wire-level MCP server name for the team stdio bridge.
///
/// Anthropic's tool name regex caps total length at 64 chars and the wire-level
/// tool name is `mcp__<server_name>__<tool>`. A 36-char UUID v7 `team_id`
/// embedded in the server name pushed `team_describe_assistant` to 78 chars and
/// caused `invalid_request_error: 工具名称过长` (ELECTRON-1JY). Team routing
/// has always been done via per-team TCP port + auth token, so the team_id was
/// redundant in the server name.
pub const TEAM_MCP_SERVER_NAME: &str = "aionui-team";

/// Stdio connection config for the team session MCP server.
///
/// `team_id` is persisted for diagnostics; the wire-level MCP server name is
/// the fixed [`TEAM_MCP_SERVER_NAME`] (team routing happens via per-team TCP
/// port + auth token, not via the server name — see ELECTRON-1JY).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMcpStdioConfig {
    pub team_id: String,
    pub port: u16,
    pub token: String,
    pub slot_id: String,
    pub binary_path: String,
}

impl TeamMcpStdioConfig {
    /// env key the stdio bridge reads to learn the backend TCP port.
    pub const ENV_PORT: &'static str = "TEAM_MCP_PORT";
    /// env key the stdio bridge reads to learn the auth token.
    pub const ENV_TOKEN: &'static str = "TEAM_MCP_TOKEN";
    /// env key the stdio bridge reads to learn which agent slot it represents.
    pub const ENV_SLOT_ID: &'static str = "TEAM_AGENT_SLOT_ID";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_preserves_all_fields() {
        let cfg = TeamMcpStdioConfig {
            team_id: "team-42".into(),
            port: 54321,
            token: "tok-abc".into(),
            slot_id: "slot-1".into(),
            binary_path: "/usr/bin/aioncore".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: TeamMcpStdioConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, parsed);
    }

    /// ELECTRON-1JY regression: Anthropic caps tool names at 64 chars,
    /// where the wire-level name is `mcp__<server_name>__<tool>`. The
    /// previous design embedded a 36-char UUID v7 `team_id` into the
    /// server name, which pushed `team_describe_assistant` to 78 chars
    /// and triggered `invalid_request_error: 工具名称过长`.
    ///
    /// This test pins the longest known team tool name against the
    /// 64-char bound so any future tool / rename that would re-break the
    /// limit fails locally instead of in production.
    #[test]
    fn team_mcp_tool_names_stay_within_anthropic_64_char_limit() {
        // Longest tool name currently registered on the team MCP server.
        // Update if a longer-named tool is added.
        let longest_tool = "team_describe_assistant";
        let wire_name = format!("mcp__{TEAM_MCP_SERVER_NAME}__{longest_tool}");
        assert!(
            wire_name.len() <= 64,
            "Anthropic 64-char tool-name limit exceeded: {} ({} chars)",
            wire_name,
            wire_name.len()
        );
    }

    #[test]
    fn deserialization_tolerates_unknown_fields() {
        // Forward-compat: extra fields in persisted `conversation.extra.team_mcp_stdio_config`
        // JSON (e.g. added by a later backend version) must still round-trip through
        // older binaries without error.
        let json = r#"{"team_id":"t-1","port":1,"token":"t","slot_id":"s","binary_path":"/usr/bin/aioncore","future_field":42}"#;
        let parsed: TeamMcpStdioConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.team_id, "t-1");
        assert_eq!(parsed.port, 1);
        assert_eq!(parsed.token, "t");
        assert_eq!(parsed.slot_id, "s");
    }
}
