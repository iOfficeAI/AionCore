//! Serde types for the `agy -p --output-format stream-json` NDJSON stream.
//!
//! Shapes verified against captured samples in
//! `~/aion/protocols/samples/antigravity-cli/1.1.8/`.
//!
//! Everything here is tolerant by design: every field is optional or guarded by
//! `#[serde(other)]`. agy ships new step types and fields between releases (it
//! self-updates), and an unrecognised value must degrade — never abort the
//! stream mid-turn.

// Fields with no consumer yet are kept deliberately: this module is the record
// of agy's wire contract, and dropping a field would lose that documentation
// the next time someone needs it.
#![allow(dead_code)]

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum AgyState {
    Active,
    Done,
    Error,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgyStepType {
    UserInput,
    AgentResponse,
    Tool,
    Checkpoint,
    SystemMessage,
    /// A step agy failed. Carries NO payload — no message, no tool name, no
    /// cause — only `duration_seconds`, so it can be counted but not explained.
    ///
    /// Measured on 1.1.9: a refused tool produces one of these per attempt and
    /// agy then reports the TURN as `status:"SUCCESS"` anyway (verified with a
    /// PreToolUse hook answering `deny`: four `error_message` steps, terminal
    /// `SUCCESS`, reply "DONE", and the file never written).
    ErrorMessage,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct AgyUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// agy reports a thinking-token COUNT but never emits thinking TEXT.
    /// Never synthesize a thought card from this.
    #[serde(default)]
    pub thinking_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AgyToolError {
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AgyToolInfo {
    #[serde(default)]
    pub name: String,
    /// Raw tool arguments. For `call_mcp_tool` this carries `ServerName` /
    /// `ToolName` / `Arguments` — the real MCP target.
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub error: Option<AgyToolError>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AgySubagentInfo {
    #[serde(default)]
    pub conversation_id: String,
    #[serde(default)]
    pub log_uri: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct AgyInit {
    #[serde(default)]
    pub conversation_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Authoritative echo of the model actually in use. ABSENT means agy
    /// rejected `--model` and silently fell back to its default — a bare or
    /// bogus id both yield no `model` key and no error on stderr.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AgyStepUpdate {
    #[serde(default)]
    pub conversation_id: String,
    /// Monotonic within one agy conversation, and it KEEPS counting across a
    /// `--conversation` resume — so it is a reliable demux key.
    #[serde(default)]
    pub step_index: u64,
    pub state: AgyState,
    pub step_type: AgyStepType,
    #[serde(default)]
    pub text_delta: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_info: Option<AgyToolInfo>,
    #[serde(default)]
    pub subagent_info: Option<AgySubagentInfo>,
    #[serde(default)]
    pub usage: Option<AgyUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AgyResult {
    /// EMPTY when the run failed before a conversation existed (verified: a
    /// logged-out run). Must never be persisted as a resume anchor.
    #[serde(default)]
    pub conversation_id: String,
    /// `"SUCCESS"` | `"ERROR"` | `"INTERRUPTED"`.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub response: String,
    /// Present on failures, e.g. `"authentication failed or timed out"`.
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub num_turns: u64,
    #[serde(default)]
    pub usage: Option<AgyUsage>,
}

#[derive(Debug, Clone)]
pub(crate) enum AgyEvent {
    Init(AgyInit),
    StepUpdate(AgyStepUpdate),
    Result(AgyResult),
}

#[derive(Deserialize)]
struct Envelope {
    event: String,
    #[serde(default)]
    init: Option<serde_json::Value>,
    #[serde(default)]
    step_update: Option<serde_json::Value>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    conversation_id: Option<String>,
}

/// Parse one NDJSON line.
///
/// Returns `None` for blank lines, the plain-text notices agy prints alongside
/// the stream, and envelopes we do not model — all non-fatal by contract.
pub(crate) fn parse_line(line: &str) -> Option<AgyEvent> {
    let line = line.trim();
    if line.is_empty() || !line.starts_with('{') {
        return None;
    }
    let env: Envelope = serde_json::from_str(line).ok()?;
    match env.event.as_str() {
        "init" => {
            let mut init: AgyInit = serde_json::from_value(env.init?).ok()?;
            // For `init`, agy puts `conversation_id` on the ENVELOPE, not in
            // the payload.
            if init.conversation_id.is_empty() {
                init.conversation_id = env.conversation_id.unwrap_or_default();
            }
            Some(AgyEvent::Init(init))
        }
        "step_update" => Some(AgyEvent::StepUpdate(serde_json::from_value(env.step_update?).ok()?)),
        "result" => Some(AgyEvent::Result(serde_json::from_value(env.result?).ok()?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_init_event() {
        let line = r#"{"event":"init","conversation_id":"c1","init":{"cwd":"/w","tools":["run_command"],"permission_mode":"request-review"}}"#;
        let AgyEvent::Init(init) = parse_line(line).expect("parsed") else {
            panic!("expected init");
        };
        assert_eq!(init.conversation_id, "c1");
        assert_eq!(init.cwd.as_deref(), Some("/w"));
    }

    #[test]
    fn init_carries_the_model_agy_actually_used() {
        let line = r#"{"event":"init","conversation_id":"c1","init":{"cwd":"/w","model":"gemini-3.6-flash-low"}}"#;
        let AgyEvent::Init(init) = parse_line(line).expect("parsed") else {
            panic!("expected init");
        };
        assert_eq!(init.model.as_deref(), Some("gemini-3.6-flash-low"));
    }

    #[test]
    fn absent_init_model_signals_a_silent_fallback() {
        // Verified: a bare or bogus `--model` value makes agy drop the flag and
        // fall back to its default, emitting NO `model` key and no error.
        let line = r#"{"event":"init","conversation_id":"c1","init":{"cwd":"/w"}}"#;
        let AgyEvent::Init(init) = parse_line(line).expect("parsed") else {
            panic!("expected init");
        };
        assert!(init.model.is_none());
    }

    #[test]
    fn parses_tool_step_with_output() {
        let line = r#"{"event":"step_update","step_update":{"conversation_id":"c1","step_index":3,"state":"DONE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"echo hi"},"output":"hi\n"}}}"#;
        let AgyEvent::StepUpdate(su) = parse_line(line).expect("parsed") else {
            panic!("expected step_update");
        };
        assert_eq!(su.step_index, 3);
        assert_eq!(su.state, AgyState::Done);
        assert_eq!(su.step_type, AgyStepType::Tool);
        let info = su.tool_info.expect("tool_info");
        assert_eq!(info.output.as_deref(), Some("hi\n"));
    }

    #[test]
    fn parses_tool_error() {
        let line = r#"{"event":"step_update","step_update":{"conversation_id":"c1","step_index":4,"state":"ERROR","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","error":{"type":"TOOL_ERROR","message":"denied"}}}}"#;
        let AgyEvent::StepUpdate(su) = parse_line(line).expect("parsed") else {
            panic!("expected step_update");
        };
        assert_eq!(su.state, AgyState::Error);
        assert_eq!(
            su.tool_info.and_then(|i| i.error).map(|e| e.message),
            Some("denied".to_string())
        );
    }

    #[test]
    fn parses_mcp_tool_call_parameters() {
        // agy routes every MCP tool through the single `call_mcp_tool` tool and
        // puts the real target in the parameters.
        let line = r#"{"event":"step_update","step_update":{"step_index":6,"state":"ACTIVE","step_type":"tool","tool_name":"call_mcp_tool","tool_info":{"name":"call_mcp_tool","parameters":{"ServerName":"aionui-team","ToolName":"aionui_team_ping","Arguments":{}}}}}"#;
        let AgyEvent::StepUpdate(su) = parse_line(line).expect("parsed") else {
            panic!("expected step_update");
        };
        let params = su.tool_info.expect("tool_info").parameters.expect("parameters");
        assert_eq!(params["ServerName"], "aionui-team");
        assert_eq!(params["ToolName"], "aionui_team_ping");
    }

    #[test]
    fn parses_result_with_usage() {
        let line = r#"{"event":"result","result":{"conversation_id":"c1","status":"SUCCESS","response":"PONG\n","num_turns":1,"usage":{"input_tokens":10,"output_tokens":2,"thinking_tokens":1,"cache_read_tokens":0,"total_tokens":12}}}"#;
        let AgyEvent::Result(r) = parse_line(line).expect("parsed") else {
            panic!("expected result");
        };
        assert_eq!(r.status, "SUCCESS");
        assert_eq!(r.usage.map(|u| u.total_tokens), Some(12));
    }

    #[test]
    fn parses_auth_failure_result() {
        // Verified shape of a logged-out headless run: agy fails fast with a
        // structured ERROR result and an EMPTY conversation_id.
        let line = r#"{"event":"result","result":{"conversation_id":"","status":"ERROR","response":"","error":"authentication failed or timed out","num_turns":0}}"#;
        let AgyEvent::Result(r) = parse_line(line).expect("parsed") else {
            panic!("expected result");
        };
        assert_eq!(r.status, "ERROR");
        assert_eq!(r.error.as_deref(), Some("authentication failed or timed out"));
        assert!(r.conversation_id.is_empty());
    }

    #[test]
    fn unknown_step_type_degrades_instead_of_failing() {
        // agy adds step types over time; an unknown one must not kill the stream.
        let line = r#"{"event":"step_update","step_update":{"conversation_id":"c1","step_index":9,"state":"DONE","step_type":"brand_new_thing"}}"#;
        let AgyEvent::StepUpdate(su) = parse_line(line).expect("parsed") else {
            panic!("expected step_update");
        };
        assert_eq!(su.step_type, AgyStepType::Unknown);
    }

    #[test]
    fn non_json_noise_is_ignored() {
        // agy prints plain-text notices alongside the stream (e.g. the
        // soft-deny hint) — these must not be treated as protocol errors.
        assert!(parse_line("jetski: no output produced").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
    }
}
