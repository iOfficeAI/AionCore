//! opencode durable/SSE event → `SessionEvent` translation.
//!
//! Every mapping here is backed by a live capture against `opencode serve`
//! 1.18.30 (see the PR description). Key contracts:
//!
//! * Durable envelope: `{id, type, durable:{aggregateID, seq, version}, data}`
//!   delivered as one `data:` SSE line; transient events (e.g. `session.idle`)
//!   share the shape minus `durable`.
//! * Text/reasoning arrive as `*.started` / `*.delta` / `*.ended` with
//!   `textID`/`reasoningID`; `*.ended` carries the FULL text, so when no
//!   delta streamed (tiny/fast answers) we emit the complete text once at
//!   ended — never duplicates, never losses.
//! * Tools arrive pre-ordered `tool.input.started → tool.input.delta* →
//!   tool.input.ended → tool.called → tool.success|tool.failed`; input JSON is
//!   finalized by `tool.input.ended`, so one `ToolCall` goes out on `.called`.
//! * `session.next.step.ended{finish,cost,tokens}` is PER LLM step
//!   (`finish:"tool-calls"` continues the loop — NOT turn end); the
//!   authoritative turn-end signal is the session leaving
//!   `GET /api/session/active` (captured behavior), backed by the transient
//!   `session.idle`/`session.error` if the server sends one.
//! * Permission/question CARDS come from the pending-list poller (authoritative
//!   `GET /api/session/{id}/permission|question`), because a turn blocked on
//!   approval emits nothing durable (captured); `*.replied/rejected` resolve
//!   events do stream and are mapped here.

use std::collections::HashMap;

use aionui_session::{
    CancelReason, NoticeLevel, PermissionKind, SessionEvent, StopReason, SubagentKind, ToolResultContent, TurnOutcome,
    UsageBreakdown,
};
use serde_json::{Value, json};

use super::client::StreamEvent;

/// Per-conversation translation state. The orchestrator guarantees one
/// turn-at-a-time, so a single set of turn accumulators suffices.
#[derive(Default)]
pub struct TranslateState {
    /// textID → bytes already emitted from deltas.
    streamed_text: HashMap<String, usize>,
    /// reasoningID → bytes already emitted from deltas.
    streamed_reasoning: HashMap<String, usize>,
    /// callID → (tool name, finalized input JSON).
    tool_inputs: HashMap<String, (String, Option<Value>)>,
    /// Turn usage totals (sum over `step.ended` frames).
    turn_input_tokens: u64,
    turn_output_tokens: u64,
    turn_thought_tokens: u64,
    turn_cache_read: u64,
    turn_cache_write: u64,
    turn_cost_usd: f64,
}

impl TranslateState {
    /// Reset turn-scoped state at a NEW turn (dispatch Send).
    pub fn begin_turn(&mut self) {
        *self = TranslateState::default();
    }

    pub fn turn_usage_event(&self) -> SessionEvent {
        SessionEvent::UsageDelta {
            input_tokens: self.turn_input_tokens,
            output_tokens: self.turn_output_tokens,
            total_tokens: self.turn_input_tokens + self.turn_output_tokens,
            cost_usd: if self.turn_cost_usd > 0.0 {
                Some(self.turn_cost_usd)
            } else {
                None
            },
            breakdown: UsageBreakdown {
                cached_read_tokens: self.turn_cache_read,
                cached_write_tokens: self.turn_cache_write,
                thought_tokens: self.turn_thought_tokens,
            },
            context_window: None,
        }
    }
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or_default().to_string()
}

fn req_id(d: &Value) -> String {
    let id = s(d, "requestID");
    if id.is_empty() { s(d, "id") } else { id }
}

/// Translate one opencode stream event into zero or more SessionEvents.
pub fn translate(ev: &StreamEvent, st: &mut TranslateState) -> Vec<SessionEvent> {
    let d = &ev.data;
    match ev.event_type.as_str() {
        // ── assistant text ─────────────────────────────────────────────
        "session.next.text.delta" => {
            let delta = s(d, "delta");
            if delta.is_empty() {
                return Vec::new();
            }
            *st.streamed_text.entry(s(d, "textID")).or_default() += delta.len();
            vec![SessionEvent::MessageDelta {
                item_id: format!("msg-{}", s(d, "textID")),
                text: delta,
            }]
        }
        "session.next.text.ended" => {
            let text = s(d, "text");
            let id = s(d, "textID");
            let seen = st.streamed_text.remove(&id).unwrap_or(0);
            // Nothing streamed for this textID → the full text exists only
            // here (fast answers never stream deltas) → emit once.
            if !text.is_empty() && seen == 0 {
                vec![SessionEvent::MessageDelta {
                    item_id: format!("msg-{id}"),
                    text,
                }]
            } else {
                Vec::new()
            }
        }
        // ── reasoning ──────────────────────────────────────────────────
        "session.next.reasoning.delta" => {
            let delta = s(d, "delta");
            if delta.is_empty() {
                return Vec::new();
            }
            *st.streamed_reasoning.entry(s(d, "reasoningID")).or_default() += delta.len();
            vec![SessionEvent::ThoughtDelta {
                item_id: format!("thought-{}", s(d, "reasoningID")),
                text: delta,
            }]
        }
        "session.next.reasoning.ended" => {
            let text = s(d, "text");
            let id = s(d, "reasoningID");
            let seen = st.streamed_reasoning.remove(&id).unwrap_or(0);
            if !text.is_empty() && seen == 0 {
                vec![SessionEvent::ThoughtDelta {
                    item_id: format!("thought-{id}"),
                    text,
                }]
            } else {
                Vec::new()
            }
        }
        // ── steps ──────────────────────────────────────────────────────
        "session.next.step.ended" => {
            // Usage accounting only; TURN END is decided by the active-map
            // poller, never here (`finish:"tool-calls"` continues the agent
            // loop, and even "stop" needs the settle grace for trailing
            // frames — captured behavior of 1.18.30).
            let tokens = d.get("tokens").cloned().unwrap_or(json!({}));
            let g = |keys: &[&str]| -> u64 {
                keys.iter()
                    .find_map(|k| tokens.get(*k).and_then(|v| v.as_u64()))
                    .unwrap_or(0)
            };
            st.turn_input_tokens += g(&["input"]);
            st.turn_output_tokens += g(&["output"]);
            st.turn_thought_tokens += g(&["reasoning"]);
            if let Some(c) = tokens.get("cache") {
                st.turn_cache_read += c.get("read").and_then(|v| v.as_u64()).unwrap_or(0);
                st.turn_cache_write += c.get("write").and_then(|v| v.as_u64()).unwrap_or(0);
            }
            if let Some(c) = d.get("cost").and_then(|v| v.as_f64()) {
                st.turn_cost_usd += c;
            }
            Vec::new()
        }
        // ── tools ──────────────────────────────────────────────────────
        "session.next.tool.input.started" => {
            st.tool_inputs.insert(s(d, "callID"), (s(d, "name"), None));
            Vec::new()
        }
        "session.next.tool.input.ended" => {
            let call_id = s(d, "callID");
            let raw = s(d, "text");
            let parsed: Option<Value> = serde_json::from_str(&raw).ok();
            let entry = st.tool_inputs.entry(call_id).or_insert_with(|| (String::new(), None));
            if entry.0.is_empty() {
                entry.0 = s(d, "name");
            }
            entry.1 = Some(parsed.unwrap_or_else(|| json!({ "raw": raw })));
            Vec::new()
        }
        "session.next.tool.called" => {
            let call_id = s(d, "callID");
            let streamed = st.tool_inputs.get(&call_id).cloned();
            let name = match (&streamed, d.get("tool").and_then(|t| t.as_str())) {
                (Some((n, _)), None) | (Some((n, _)), Some("")) => n.clone(),
                (_, Some(t)) if !t.is_empty() => t.to_string(),
                (Some((n, _)), _) => n.clone(),
                (None, _) => String::new(),
            };
            let input = streamed
                .and_then(|(_, i)| i)
                .or_else(|| d.get("input").cloned())
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            vec![SessionEvent::ToolCall {
                tool_use_id: call_id,
                name,
                // Subagent framing is out of scope: opencode `task` spawns
                // NESTED sessions (own session ids), not claude-style inline
                // subagents — the tool result still surfaces either way.
                subagent: SubagentKind::default(),
                input,
                parent_tool_use_id: None,
            }]
        }
        "session.next.tool.success" => {
            let text = tool_output_text(d);
            vec![SessionEvent::ToolResult {
                tool_use_id: s(d, "callID"),
                is_error: false,
                content: vec![ToolResultContent::Text(text)],
                parent_tool_use_id: None,
            }]
        }
        "session.next.tool.failed" => {
            let msg = d
                .get("error")
                .map(|e| {
                    e.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("tool failed")
                        .to_string()
                })
                .unwrap_or_else(|| "tool failed".to_string());
            vec![SessionEvent::ToolResult {
                tool_use_id: s(d, "callID"),
                is_error: true,
                content: vec![ToolResultContent::Text(msg)],
                parent_tool_use_id: None,
            }]
        }
        "session.next.tool.progress" => vec![SessionEvent::Heartbeat],
        // ── permission / question resolves (asks come from the poller) ──
        "permission.v2.replied" | "permission.v2.rejected" => {
            let request_id = req_id(d);
            if request_id.is_empty() {
                return Vec::new();
            }
            vec![SessionEvent::PermissionResolved {
                request_id,
                kind: PermissionKind::Tool,
            }]
        }
        "question.v2.replied" | "question.v2.rejected" => {
            let request_id = req_id(d);
            if request_id.is_empty() {
                return Vec::new();
            }
            vec![SessionEvent::AskResolved { request_id }]
        }
        // ── errors / retry advisories ──────────────────────────────────
        "session.error" => {
            let msg = d
                .get("error")
                .map(|e| {
                    e.get("data")
                        .and_then(|x| x.get("message"))
                        .and_then(|m| m.as_str())
                        .or_else(|| e.as_str())
                        .unwrap_or("session error")
                        .to_string()
                })
                .unwrap_or_else(|| "session error".to_string());
            // The CALLER promotes this to a failed TurnResult when a turn is
            // in flight (it is also the turn-end signal on that path).
            vec![SessionEvent::Notice {
                level: NoticeLevel::Warning,
                message: msg,
                localized: None,
                supersedes_key: None,
            }]
        }
        "session.next.retried" => {
            let reason = d
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("retrying");
            vec![SessionEvent::Notice {
                level: NoticeLevel::Info,
                message: format!("provider retry: {reason}"),
                localized: None,
                // Successive retries replace one card (codex-retry pattern the
                // UI already understands).
                supersedes_key: Some("opencode-retry".into()),
            }]
        }
        // Everything else (prompt.admitted/prompted echoes — Send already owns
        // those; step.started bookkeeping; session.updated/moved; todo.*;
        // model/agent switch confirmations — ConfigChanged rides the
        // dispatch receipt instead; unknown future events) produces nothing.
        _ => Vec::new(),
    }
}

/// Flatten `content[]` text parts, falling back to compact structured output.
fn tool_output_text(d: &Value) -> String {
    let mut out = String::new();
    if let Some(parts) = d.get("content").and_then(|c| c.as_array()) {
        for p in parts {
            if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
    }
    if out.is_empty()
        && let Some(su) = d.get("structured")
        && !su.is_null()
    {
        out = serde_json::to_string(su).unwrap_or_default();
    }
    out
}

/// Build the terminal event once the active-map poller (or a transient
/// `session.idle`) confirms completion.
pub fn turn_result_event(usage_turn_failed: bool, cancelled: bool) -> SessionEvent {
    SessionEvent::TurnResult {
        is_error: usage_turn_failed,
        api_error_status: None,
        // opencode has no turn-level summary string; the streamed MessageDelta
        // text IS the answer and the finalizer folds it. "" = empty-turn.
        result_text: String::new(),
        // Stamped with the live epoch by the dispatch machinery that owns the
        // turn; cancelled turns ride it too so the reducer's epoch guard drops
        // the flushed late result instead of mislabelling the NEXT turn.
        epoch: 0,
        outcome: if cancelled {
            TurnOutcome::Cancelled {
                reason: CancelReason::UserCancel,
            }
        } else {
            TurnOutcome::Completed {
                stop_reason: StopReason::EndTurn,
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(type_: &str, data: Value) -> StreamEvent {
        StreamEvent {
            seq: Some(1),
            event_type: type_.into(),
            data,
        }
    }

    #[test]
    fn text_deltas_then_end_no_dupes() {
        let mut st = TranslateState::default();
        let a = translate(
            &ev("session.next.text.delta", json!({"textID":"t1","delta":"Hello "})),
            &mut st,
        );
        let b = translate(
            &ev("session.next.text.delta", json!({"textID":"t1","delta":"world"})),
            &mut st,
        );
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        // ended carries the FULL text; deltas already streamed → no re-emit.
        let c = translate(
            &ev("session.next.text.ended", json!({"textID":"t1","text":"Hello world"})),
            &mut st,
        );
        assert!(c.is_empty());
    }

    #[test]
    fn text_end_only_build_emits_full_text() {
        let mut st = TranslateState::default();
        let c = translate(
            &ev("session.next.text.ended", json!({"textID":"t9","text":"DONE"})),
            &mut st,
        );
        assert_eq!(c.len(), 1);
        match &c[0] {
            SessionEvent::MessageDelta { item_id, text } => {
                assert_eq!(item_id, "msg-t9");
                assert_eq!(text, "DONE");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tool_pipeline_emits_call_then_result() {
        let mut st = TranslateState::default();
        translate(
            &ev(
                "session.next.tool.input.started",
                json!({"callID":"call_1","name":"bash"}),
            ),
            &mut st,
        );
        translate(
            &ev(
                "session.next.tool.input.ended",
                json!({"callID":"call_1","text":"{\"command\":\"echo V2TOOL_OK\"}"}),
            ),
            &mut st,
        );
        let called = translate(
            &ev(
                "session.next.tool.called",
                json!({"callID":"call_1","tool":"bash","input":{"command":"echo V2TOOL_OK"}}),
            ),
            &mut st,
        );
        assert_eq!(called.len(), 1);
        match &called[0] {
            SessionEvent::ToolCall {
                tool_use_id,
                name,
                input,
                ..
            } => {
                assert_eq!(tool_use_id, "call_1");
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "echo V2TOOL_OK");
            }
            other => panic!("{other:?}"),
        }
        let done = translate(
            &ev(
                "session.next.tool.success",
                json!({"callID":"call_1","structured":{},"content":[{"type":"text","text":"V2TOOL_OK"}]}),
            ),
            &mut st,
        );
        match &done[0] {
            SessionEvent::ToolResult {
                tool_use_id,
                is_error,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "call_1");
                assert!(!is_error);
                match &content[0] {
                    ToolResultContent::Text(t) => assert_eq!(t, "V2TOOL_OK"),
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tool_input_arrives_finalized_by_ended_event() {
        // .called must carry the JSON parsed from input.ended's text even when
        // the .called frame itself has no input field (capture ordering).
        let mut st = TranslateState::default();
        translate(
            &ev("session.next.tool.input.started", json!({"callID":"c","name":"read"})),
            &mut st,
        );
        translate(
            &ev(
                "session.next.tool.input.ended",
                json!({"callID":"c","text":"{\"filePath\":\"a.txt\"}"}),
            ),
            &mut st,
        );
        let called = translate(
            &ev("session.next.tool.called", json!({"callID":"c","tool":"read"})),
            &mut st,
        );
        match &called[0] {
            SessionEvent::ToolCall { input, .. } => assert_eq!(input["filePath"], "a.txt"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn interrupted_tool_surfaces_error() {
        let mut st = TranslateState::default();
        let failed = translate(
            &ev(
                "session.next.tool.failed",
                json!({"callID":"call_9","error":{"type":"unknown","message":"Tool execution interrupted"}}),
            ),
            &mut st,
        );
        match &failed[0] {
            SessionEvent::ToolResult { is_error, content, .. } => {
                assert!(is_error);
                match &content[0] {
                    ToolResultContent::Text(t) => assert_eq!(t, "Tool execution interrupted"),
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn usage_accumulates_across_steps() {
        let mut st = TranslateState::default();
        translate(
            &ev(
                "session.next.step.ended",
                json!({"finish":"tool-calls","cost":0.01,"tokens":{"input":100,"output":20,"reasoning":5,"total":125,"cache":{"read":8,"write":0}}}),
            ),
            &mut st,
        );
        translate(
            &ev(
                "session.next.step.ended",
                json!({"finish":"stop","cost":0.02,"tokens":{"input":150,"output":30,"reasoning":8,"total":180,"cache":{"read":0,"write":0}}}),
            ),
            &mut st,
        );
        match st.turn_usage_event() {
            SessionEvent::UsageDelta {
                input_tokens,
                output_tokens,
                total_tokens,
                cost_usd,
                breakdown,
                ..
            } => {
                assert_eq!(input_tokens, 250);
                assert_eq!(output_tokens, 50);
                assert_eq!(total_tokens, 300);
                assert_eq!(breakdown.thought_tokens, 13);
                assert_eq!(breakdown.cached_read_tokens, 8);
                assert!((cost_usd.unwrap() - 0.03).abs() < 1e-9);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reasoning_end_without_deltas_emits_once() {
        let mut st = TranslateState::default();
        let out = translate(
            &ev(
                "session.next.reasoning.ended",
                json!({"reasoningID":"r1","text":"thinking…"}),
            ),
            &mut st,
        );
        assert_eq!(out.len(), 1);
        translate(
            &ev("session.next.reasoning.delta", json!({"reasoningID":"r2","delta":"a"})),
            &mut st,
        );
        let out2 = translate(
            &ev("session.next.reasoning.ended", json!({"reasoningID":"r2","text":"ab"})),
            &mut st,
        );
        assert!(out2.is_empty());
    }

    #[test]
    fn question_resolve_maps_request_id() {
        let mut st = TranslateState::default();
        let out = translate(
            &ev(
                "question.v2.replied",
                json!({"sessionID":"ses_1","requestID":"que_1","answers":[["red"]]}),
            ),
            &mut st,
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            SessionEvent::AskResolved { request_id } if request_id == "que_1"
        ));
    }

    #[test]
    fn permission_resolve_falls_back_to_id_field() {
        let mut st = TranslateState::default();
        let out = translate(
            &ev("permission.v2.replied", json!({"id":"per_42","sessionID":"ses_1"})),
            &mut st,
        );
        assert!(matches!(
            &out[0],
            SessionEvent::PermissionResolved { request_id, .. } if request_id == "per_42"
        ));
    }

    #[test]
    fn unknown_events_are_inert() {
        let mut st = TranslateState::default();
        assert!(translate(&ev("session.updated", json!({"info":{}})), &mut st).is_empty());
        assert!(translate(&ev("session.next.mcp.started", json!({"server":"x"})), &mut st).is_empty());
        assert!(translate(&ev("session.next.text.started", json!({"textID":"t"})), &mut st).is_empty());
        assert!(
            translate(
                &ev("session.next.prompt.admitted", json!({"prompt":{"text":"x"}})),
                &mut st
            )
            .is_empty()
        );
    }

    #[test]
    fn session_error_maps_to_notice_with_captured_shape() {
        let mut st = TranslateState::default();
        let out = translate(
            &ev(
                "session.error",
                json!({"error":{"type":"unknown","data":{"message":"boom"}}}),
            ),
            &mut st,
        );
        match &out[0] {
            SessionEvent::Notice { message, .. } => assert_eq!(message, "boom"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cancelled_turn_result_carries_user_cancel_outcome() {
        match turn_result_event(false, true) {
            SessionEvent::TurnResult { outcome, is_error, .. } => {
                assert!(!is_error);
                assert!(matches!(
                    outcome,
                    TurnOutcome::Cancelled {
                        reason: CancelReason::UserCancel
                    }
                ));
            }
            other => panic!("{other:?}"),
        }
    }
}
