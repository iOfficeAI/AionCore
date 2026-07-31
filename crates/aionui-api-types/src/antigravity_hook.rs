//! Contract for the Antigravity permission bridge.
//!
//! agy cannot prompt for tool permission in headless mode, and a `PreToolUse`
//! hook can only TIGHTEN a decision, never loosen one (verified). So AionUi
//! opens agy's gate with `--dangerously-skip-permissions` and registers its own
//! backend binary as the hook — making that hook the sole gatekeeper.
//!
//! The registered `hooks.json` carries NO policy: it only points at the binary.
//! Every decision is made at runtime, by the user, in AionUi.
//!
//! Flow: agy → hook process (stdin JSON) → AionUi endpoint → user's permission
//! card → decision → hook stdout JSON → agy.

use serde::{Deserialize, Serialize};

/// How the hook process learns where to call back.
///
/// Mirrors [`crate::TeamMcpStdioConfig`]'s env-var approach: agy inherits these
/// from its own environment and passes them to the hook child process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AntigravityHookConfig {
    /// Scheme + host + port of the running AionUi backend.
    pub base_url: String,
    pub token: String,
    pub conversation_id: String,
}

impl AntigravityHookConfig {
    /// env key the hook reads to learn AionUi's base URL (scheme + host +
    /// port, e.g. `http://127.0.0.1:25808`).
    pub const ENV_BASE_URL: &'static str = "AIONUI_ANTIGRAVITY_HOOK_BASE_URL";
    /// env key the hook reads to learn its auth token.
    pub const ENV_TOKEN: &'static str = "AIONUI_ANTIGRAVITY_HOOK_TOKEN";
    /// env key the hook reads to learn which conversation it belongs to.
    pub const ENV_CONVERSATION_ID: &'static str = "AIONUI_ANTIGRAVITY_HOOK_CONVERSATION_ID";
    /// Header carrying [`Self::ENV_TOKEN`] on the callback request.
    pub const TOKEN_HEADER: &'static str = "x-aionui-hook-token";
    /// Name under which the bridge registers itself in `hooks.json`.
    pub const HOOK_NAME: &'static str = "aionui-permission-bridge";
}

/// What agy writes to the hook's stdin before a tool runs.
///
/// Shape verified against a captured `PreToolUse` invocation
/// (`samples/antigravity-cli/1.1.8/pretooluse_hook_stdin_sample.json`).
/// Unknown fields are ignored so a new agy release cannot break the bridge.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityHookInput {
    #[serde(default)]
    pub conversation_id: String,
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub step_idx: Option<u64>,
    #[serde(default)]
    pub tool_call: AntigravityHookToolCall,
    #[serde(default)]
    pub workspace_paths: Vec<String>,
}

/// The tool agy is about to run.
///
/// `args` is deliberately opaque: each tool has its OWN argument shape
/// (`run_command` uses `CommandLine`/`Cwd`, `view_file` uses `AbsolutePath`, …),
/// and MCP calls arrive as `call_mcp_tool` with `ServerName`/`ToolName` inside.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AntigravityHookToolCall {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

impl AntigravityHookToolCall {
    /// Name to show the user on the permission card.
    ///
    /// agy routes every MCP tool through the single `call_mcp_tool` tool and
    /// puts the real target in the arguments, so a team conversation would
    /// otherwise ask "allow call_mcp_tool?" for every distinct action.
    pub fn display_name(&self) -> String {
        if self.name != "call_mcp_tool" {
            return self.name.clone();
        }
        match (
            self.args.get("ServerName").and_then(|v| v.as_str()),
            self.args.get("ToolName").and_then(|v| v.as_str()),
        ) {
            (Some(server), Some(tool)) => format!("{server}/{tool}"),
            (None, Some(tool)) => tool.to_owned(),
            _ => self.name.clone(),
        }
    }
}

/// What the hook writes to stdout to gate the call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AntigravityHookOutput {
    pub decision: AntigravityHookDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl AntigravityHookOutput {
    pub fn allow(reason: impl Into<String>) -> Self {
        Self {
            decision: AntigravityHookDecision::Allow,
            reason: Some(reason.into()),
        }
    }

    /// Fail-closed answer. Used whenever the bridge cannot obtain a real
    /// decision — bad token, unreachable backend, a session that went away —
    /// because the alternative is letting an unapproved tool run.
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            decision: AntigravityHookDecision::Deny,
            reason: Some(reason.into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AntigravityHookDecision {
    Allow,
    Deny,
    Ask,
    ForceAsk,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_captured_pretooluse_payload() {
        // Verbatim from the captured sample.
        let raw = r#"{
            "artifactDirectoryPath": "/Users/x/.gemini/antigravity-cli/brain/8f69e77c",
            "conversationId": "8f69e77c-3b5d-4a39-8923-628290cddaaa",
            "modelName": "gemini-3.6-flash-high",
            "stepIdx": 6,
            "toolCall": {
                "args": {"CommandLine": "echo LETMEPASS", "Cwd": "/w", "WaitMsBeforeAsync": 1000},
                "name": "run_command"
            },
            "transcriptPath": "/Users/x/logs/transcript_full.jsonl",
            "workspacePaths": ["/w"]
        }"#;
        let input: AntigravityHookInput = serde_json::from_str(raw).expect("parse");
        assert_eq!(input.conversation_id, "8f69e77c-3b5d-4a39-8923-628290cddaaa");
        assert_eq!(input.tool_call.name, "run_command");
        assert_eq!(input.tool_call.args["CommandLine"], "echo LETMEPASS");
        assert_eq!(input.step_idx, Some(6));
        assert_eq!(input.workspace_paths, vec!["/w".to_string()]);
    }

    #[test]
    fn unknown_fields_do_not_break_the_bridge() {
        // agy self-updates; a new key must not turn every tool call into a
        // parse error (which would fail-closed on everything).
        let raw = r#"{"conversationId":"c1","toolCall":{"name":"view_file","args":{}},"brandNewKey":42}"#;
        let input: AntigravityHookInput = serde_json::from_str(raw).expect("parse");
        assert_eq!(input.tool_call.name, "view_file");
    }

    #[test]
    fn an_mcp_call_is_shown_as_server_slash_tool() {
        let raw = r#"{"conversationId":"c1","toolCall":{"name":"call_mcp_tool","args":{"ServerName":"aionui-team","ToolName":"aionui_team_ping"}}}"#;
        let input: AntigravityHookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(input.tool_call.display_name(), "aionui-team/aionui_team_ping");
    }

    #[test]
    fn a_plain_tool_keeps_its_own_name() {
        let raw = r#"{"conversationId":"c1","toolCall":{"name":"run_command","args":{"CommandLine":"ls"}}}"#;
        let input: AntigravityHookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(input.tool_call.display_name(), "run_command");
    }

    #[test]
    fn a_malformed_mcp_call_falls_back_to_the_raw_name() {
        let raw = r#"{"conversationId":"c1","toolCall":{"name":"call_mcp_tool","args":{}}}"#;
        let input: AntigravityHookInput = serde_json::from_str(raw).unwrap();
        assert_eq!(input.tool_call.display_name(), "call_mcp_tool");
    }

    #[test]
    fn decisions_serialize_to_agys_lowercase_vocabulary() {
        let out = AntigravityHookOutput::deny("user rejected");
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json["decision"], "deny");
        assert_eq!(json["reason"], "user rejected");

        let allow = serde_json::to_value(AntigravityHookOutput::allow("ok")).unwrap();
        assert_eq!(allow["decision"], "allow");
    }
}
