//! agy wire events -> backend-neutral `SessionEvent`. Pure: no IO, no spawning.
//!
//! Demux key: agy's `step_index` is monotonic within a conversation and keeps
//! counting across a `--conversation` resume, so `step-<index>` is a stable
//! id for pairing a tool's ACTIVE and DONE frames.

use super::wire::{AgyEvent, AgyState, AgyStepType, AgyStepUpdate, AgyUsage};
use crate::event::{CancelReason, SessionEvent, ToolResultContent, TurnOutcome, UsageBreakdown};

/// Folds the agy stream into `SessionEvent`s, carrying the small amount of
/// cross-frame state the translation needs.
#[derive(Debug, Default)]
pub(crate) struct Translator {
    /// The agy conversation id to resume with. Only ever set from a NON-EMPTY
    /// id: a failed run (e.g. logged out) reports `conversation_id: ""`, and
    /// storing that would launch the next turn with an empty `--conversation`.
    backend_session_id: Option<String>,
    /// Model agy echoed in `init`.
    current_model: Option<String>,
    /// `init` arrived without a `model` key, which is how agy reports that it
    /// rejected `--model` and fell back to its default. There is no error on
    /// stderr, so this flag is the only signal.
    model_fallback: bool,
}

impl Translator {
    pub(crate) fn backend_session_id(&self) -> Option<&str> {
        self.backend_session_id.as_deref()
    }

    pub(crate) fn current_model(&self) -> Option<&str> {
        self.current_model.as_deref()
    }

    pub(crate) fn model_fallback_detected(&self) -> bool {
        self.model_fallback
    }

    pub(crate) fn translate(&mut self, ev: AgyEvent) -> Vec<SessionEvent> {
        match ev {
            AgyEvent::Init(init) => {
                if !init.conversation_id.is_empty() {
                    self.backend_session_id = Some(init.conversation_id.clone());
                }
                self.model_fallback = init.model.is_none();
                self.current_model = init.model;
                vec![SessionEvent::BackendBound {
                    backend_session_id: self.backend_session_id.clone(),
                }]
            }
            AgyEvent::StepUpdate(su) => self.translate_step(su),
            AgyEvent::Result(r) => {
                if !r.conversation_id.is_empty() {
                    self.backend_session_id = Some(r.conversation_id);
                }
                let is_error = !r.status.eq_ignore_ascii_case("SUCCESS");
                let outcome = match r.status.to_ascii_uppercase().as_str() {
                    "SUCCESS" => TurnOutcome::EndTurn,
                    "INTERRUPTED" => TurnOutcome::Cancelled {
                        reason: CancelReason::UserCancel,
                    },
                    _ => TurnOutcome::Failed,
                };
                // On failure agy puts the reason in `error`, not `response`.
                let result_text = if is_error {
                    r.error.unwrap_or(r.response)
                } else {
                    r.response
                };
                let mut out = Vec::with_capacity(2);
                if let Some(u) = r.usage {
                    out.push(usage_delta(&u));
                }
                out.push(SessionEvent::TurnResult {
                    is_error,
                    api_error_status: None,
                    result_text,
                    // The adapter layer is epoch-agnostic; the ai-agent reader
                    // stamps the live epoch when forwarding.
                    epoch: 0,
                    outcome,
                });
                out
            }
        }
    }

    /// agy routes EVERY MCP tool through the single `call_mcp_tool` tool and
    /// puts the real target in the parameters. Left as-is, a team conversation
    /// renders as a run of identical `call_mcp_tool` steps with no indication
    /// of what the agent actually did.
    fn display_tool_name(name: &str, params: Option<&serde_json::Value>) -> String {
        if name != "call_mcp_tool" {
            return name.to_owned();
        }
        let Some(p) = params else {
            return name.to_owned();
        };
        match (
            p.get("ServerName").and_then(|v| v.as_str()),
            p.get("ToolName").and_then(|v| v.as_str()),
        ) {
            (Some(server), Some(tool)) => format!("{server}/{tool}"),
            (None, Some(tool)) => tool.to_owned(),
            _ => name.to_owned(),
        }
    }

    fn translate_step(&mut self, su: AgyStepUpdate) -> Vec<SessionEvent> {
        let id = format!("step-{}", su.step_index);
        let mut out = Vec::new();

        match su.step_type {
            AgyStepType::AgentResponse => {
                if let Some(text) = su.text_delta.filter(|t| !t.is_empty()) {
                    out.push(SessionEvent::MessageDelta { item_id: id, text });
                }
                // NOTE: `usage.thinking_tokens` is a COUNT only — agy never
                // emits thinking text. Synthesizing a ThoughtDelta here would
                // render a permanently empty thought card.
            }
            AgyStepType::Tool => {
                let info = su.tool_info;
                let raw_name = info
                    .as_ref()
                    .map(|i| i.name.clone())
                    .filter(|n| !n.is_empty())
                    .or(su.tool_name)
                    .unwrap_or_default();
                let name = Self::display_tool_name(&raw_name, info.as_ref().and_then(|i| i.parameters.as_ref()));
                match su.state {
                    AgyState::Active => out.push(SessionEvent::ToolCall {
                        tool_use_id: id,
                        name,
                        subagent: Default::default(),
                        input: info.and_then(|i| i.parameters).unwrap_or(serde_json::Value::Null),
                        parent_tool_use_id: None,
                    }),
                    AgyState::Done => {
                        let text = info.and_then(|i| i.output).unwrap_or_default();
                        out.push(SessionEvent::ToolResult {
                            tool_use_id: id,
                            is_error: false,
                            content: if text.is_empty() {
                                Vec::new()
                            } else {
                                vec![ToolResultContent::Text(text)]
                            },
                            parent_tool_use_id: None,
                        });
                    }
                    AgyState::Error => {
                        let msg = info
                            .and_then(|i| i.error)
                            .map(|e| e.message)
                            .unwrap_or_else(|| "tool failed".to_owned());
                        out.push(SessionEvent::ToolResult {
                            tool_use_id: id,
                            is_error: true,
                            content: vec![ToolResultContent::Text(msg)],
                            parent_tool_use_id: None,
                        });
                    }
                    AgyState::Unknown => {}
                }
            }
            // Pure bookkeeping frames: agy echoes the user turn, checkpoints and
            // system notices as steps. They carry no user-visible content.
            AgyStepType::UserInput | AgyStepType::Checkpoint | AgyStepType::SystemMessage | AgyStepType::Unknown => {}
        }

        if let Some(u) = su.usage.as_ref() {
            out.push(usage_delta(u));
        }
        out
    }
}

fn usage_delta(u: &AgyUsage) -> SessionEvent {
    SessionEvent::UsageDelta {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        total_tokens: u.total_tokens,
        // agy reports no cost figures.
        cost_usd: None,
        breakdown: UsageBreakdown {
            cached_read_tokens: u.cache_read_tokens,
            cached_write_tokens: 0,
            thought_tokens: u.thinking_tokens,
        },
        context_window: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::antigravity::wire::parse_line;

    fn tr(lines: &[&str]) -> (Translator, Vec<SessionEvent>) {
        let mut t = Translator::default();
        let mut out = Vec::new();
        for l in lines {
            if let Some(ev) = parse_line(l) {
                out.extend(t.translate(ev));
            }
        }
        (t, out)
    }

    #[test]
    fn init_binds_backend_session_id() {
        let (t, evs) = tr(&[r#"{"event":"init","conversation_id":"conv-9","init":{"cwd":"/w"}}"#]);
        assert_eq!(t.backend_session_id(), Some("conv-9"));
        assert!(matches!(
            evs.first(),
            Some(SessionEvent::BackendBound {
                backend_session_id: Some(id)
            }) if id == "conv-9"
        ));
    }

    #[test]
    fn init_records_the_model_agy_reported() {
        let (t, _) = tr(&[r#"{"event":"init","conversation_id":"c1","init":{"model":"gemini-3.1-pro-high"}}"#]);
        assert_eq!(t.current_model(), Some("gemini-3.1-pro-high"));
    }

    #[test]
    fn missing_init_model_is_flagged_as_fallback() {
        // agy silently ignores an unusable `--model` and runs its default.
        let (t, _) = tr(&[r#"{"event":"init","conversation_id":"c1","init":{"cwd":"/w"}}"#]);
        assert!(t.model_fallback_detected());
    }

    #[test]
    fn agent_response_delta_becomes_message_delta() {
        let (_, evs) = tr(&[
            r#"{"event":"step_update","step_update":{"step_index":2,"state":"ACTIVE","step_type":"agent_response","text_delta":"Hel"}}"#,
        ]);
        match &evs[0] {
            SessionEvent::MessageDelta { item_id, text } => {
                assert_eq!(item_id, "step-2");
                assert_eq!(text, "Hel");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn tool_active_then_done_pairs_by_step_index() {
        let (_, evs) = tr(&[
            r#"{"event":"step_update","step_update":{"step_index":3,"state":"ACTIVE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"echo hi"}}}}"#,
            r#"{"event":"step_update","step_update":{"step_index":3,"state":"DONE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","output":"hi\n"}}}"#,
        ]);
        match &evs[0] {
            SessionEvent::ToolCall {
                tool_use_id,
                name,
                input,
                ..
            } => {
                assert_eq!(tool_use_id, "step-3");
                assert_eq!(name, "run_command");
                assert_eq!(input["CommandLine"], "echo hi");
            }
            other => panic!("unexpected {other:?}"),
        }
        match &evs[1] {
            SessionEvent::ToolResult {
                tool_use_id,
                is_error,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "step-3");
                assert!(!is_error);
                assert!(matches!(content.first(), Some(ToolResultContent::Text(t)) if t == "hi\n"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn mcp_calls_are_displayed_as_server_slash_tool() {
        // Otherwise every team step reads as a bare "call_mcp_tool".
        let (_, evs) = tr(&[
            r#"{"event":"step_update","step_update":{"step_index":6,"state":"ACTIVE","step_type":"tool","tool_name":"call_mcp_tool","tool_info":{"name":"call_mcp_tool","parameters":{"ServerName":"aionui-team","ToolName":"aionui_team_ping","Arguments":{}}}}}"#,
        ]);
        match &evs[0] {
            SessionEvent::ToolCall { name, .. } => assert_eq!(name, "aionui-team/aionui_team_ping"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_non_mcp_tool_name_is_left_alone() {
        let (_, evs) = tr(&[
            r#"{"event":"step_update","step_update":{"step_index":3,"state":"ACTIVE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"echo hi"}}}}"#,
        ]);
        match &evs[0] {
            SessionEvent::ToolCall { name, .. } => assert_eq!(name, "run_command"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_malformed_mcp_call_degrades_to_the_raw_name() {
        // Missing ServerName/ToolName must not panic or produce "/"-junk.
        let (_, evs) = tr(&[
            r#"{"event":"step_update","step_update":{"step_index":7,"state":"ACTIVE","step_type":"tool","tool_name":"call_mcp_tool","tool_info":{"name":"call_mcp_tool","parameters":{}}}}"#,
        ]);
        match &evs[0] {
            SessionEvent::ToolCall { name, .. } => assert_eq!(name, "call_mcp_tool"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn tool_error_marks_is_error_and_carries_the_message() {
        let (_, evs) = tr(&[
            r#"{"event":"step_update","step_update":{"step_index":4,"state":"ERROR","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","error":{"type":"TOOL_ERROR","message":"denied"}}}}"#,
        ]);
        match evs.last() {
            Some(SessionEvent::ToolResult { is_error, content, .. }) => {
                assert!(is_error);
                assert!(matches!(content.first(), Some(ToolResultContent::Text(t)) if t.contains("denied")));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn thinking_tokens_never_synthesize_a_thought() {
        // agy reports thinking_tokens but emits NO thinking text. Fabricating a
        // ThoughtDelta would render a permanently empty thought card.
        let (_, evs) = tr(&[
            r#"{"event":"step_update","step_update":{"step_index":5,"state":"DONE","step_type":"agent_response","usage":{"thinking_tokens":260,"total_tokens":300}}}"#,
        ]);
        assert!(!evs.iter().any(|e| matches!(e, SessionEvent::ThoughtDelta { .. })));
    }

    #[test]
    fn usage_is_forwarded_as_usage_delta() {
        let (_, evs) = tr(&[
            r#"{"event":"step_update","step_update":{"step_index":5,"state":"DONE","step_type":"agent_response","usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}}"#,
        ]);
        assert!(
            evs.iter()
                .any(|e| matches!(e, SessionEvent::UsageDelta { total_tokens: 12, .. }))
        );
    }

    #[test]
    fn result_produces_turn_result() {
        let (_, evs) = tr(&[
            r#"{"event":"result","result":{"conversation_id":"c1","status":"SUCCESS","response":"PONG\n","num_turns":1}}"#,
        ]);
        match evs.last() {
            Some(SessionEvent::TurnResult {
                is_error, result_text, ..
            }) => {
                assert!(!is_error);
                assert_eq!(result_text, "PONG\n");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn empty_conversation_id_is_never_bound_as_resume_anchor() {
        // A logged-out run reports conversation_id:"" — binding it would make
        // the next turn launch with an empty `--conversation`.
        let (t, evs) = tr(&[
            r#"{"event":"result","result":{"conversation_id":"","status":"ERROR","error":"authentication failed or timed out"}}"#,
        ]);
        assert_eq!(t.backend_session_id(), None);
        match evs.last() {
            Some(SessionEvent::TurnResult {
                is_error, result_text, ..
            }) => {
                assert!(is_error);
                assert!(result_text.contains("authentication"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn bookkeeping_steps_emit_nothing() {
        let (_, evs) = tr(&[
            r#"{"event":"step_update","step_update":{"step_index":0,"state":"DONE","step_type":"user_input"}}"#,
            r#"{"event":"step_update","step_update":{"step_index":1,"state":"DONE","step_type":"checkpoint"}}"#,
            r#"{"event":"step_update","step_update":{"step_index":2,"state":"DONE","step_type":"system_message"}}"#,
            r#"{"event":"step_update","step_update":{"step_index":9,"state":"DONE","step_type":"brand_new_thing"}}"#,
        ]);
        assert!(evs.is_empty(), "unexpected events: {evs:?}");
    }
}
