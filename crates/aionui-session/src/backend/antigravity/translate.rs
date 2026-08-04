//! agy wire events -> backend-neutral `SessionEvent`. Pure: no IO, no spawning.
//!
//! Demux key: agy's `step_index` is monotonic within a conversation and keeps
//! counting across a `--conversation` resume, so `step-<index>` is a stable
//! id for pairing a tool's ACTIVE and DONE frames.

use super::version::CODE_STEPS_FAILED;
use super::wire::{AgyEvent, AgyState, AgyStepType, AgyStepUpdate, AgySubagent, AgyUsage};
use crate::event::{
    CancelReason, LocalizedText, NoticeLevel, SessionEvent, ToolResultContent, TurnOutcome, UsageBreakdown,
};

/// Rewrite agy's terminal error into something the user can act on.
///
/// agy reports a signed-out run as `authentication failed or timed out`, which
/// tells the user nothing about what to do. Everything else is passed through
/// unchanged rather than guessed at.
pub(crate) fn explain_turn_error(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("authentication") || lower.contains("not logged in") || lower.contains("sign in") {
        return format!(
            "Antigravity is not signed in. Run `agy` in a terminal to log in with your Google account, then try again. (agy: {raw})"
        );
    }
    raw.to_owned()
}

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
    /// How many steps agy failed during THIS turn.
    ///
    /// agy reports a turn as `SUCCESS` even when every tool in it was refused,
    /// and its closing message claims the work is done. Counting these is the
    /// only way to notice, because the terminal frame carries no trace of them.
    failed_steps: u32,
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
                    explain_turn_error(&r.error.unwrap_or(r.response))
                } else {
                    r.response
                };
                let mut out = Vec::with_capacity(3);
                if let Some(u) = r.usage {
                    out.push(usage_delta(&u));
                }
                // BEFORE the terminal, and it MUST stay there. `StreamRelay`
                // breaks out of its receive loop the moment it sees the turn's
                // terminal frame (stream_relay.rs, `break RelayOutcome`), so
                // anything emitted after it is never persisted and never
                // rendered. Moving this below `TurnResult` to give the
                // correction the last word silently deleted it: verified live in
                // conversation 0803-4, where the log shows four denied
                // permissions, `event=Notice` at 05:58:29.558 — and no tips row
                // in the database at all.
                //
                // The cost is that it renders above the claim it corrects. That
                // is worse than ideal and better than absent.
                //
                // It states what the STREAM shows — N steps failed — and never
                // what the agent said. Whether the reply claims success is not
                // knowable from these frames: the same refusal that produced
                // "已成功创建 …" once produced an honest apology the next turn,
                // and a notice opening "The agent reported success, but …" then
                // accused it of a lie it had not told.
                //
                // agy's own text is still delivered unchanged — it can carry
                // real reasoning, and rewriting the agent's words would be its
                // own kind of lie. This only refuses to let "success" stand
                // unqualified.
                let failed = std::mem::take(&mut self.failed_steps);
                if !is_error && failed > 0 {
                    out.push(SessionEvent::Notice {
                        // Info, not Warning. The common trigger is a refusal the
                        // user made on purpose and already understands, so an
                        // alarm-styled card on every such turn is noise that
                        // trains them to dismiss it — and this notice is only
                        // worth anything if it is still read on the turn where
                        // agy fabricates a result.
                        level: NoticeLevel::Info,
                        message: format!(
                            "{failed} step{} failed and did NOT take effect. agy ends such turns as successful, \
                             so check the result rather than the reply.",
                            if failed == 1 { "" } else { "s" }
                        ),
                        localized: Some(LocalizedText::new(CODE_STEPS_FAILED).with("count", failed)),
                    });
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

    /// Stable id for the step's tool card.
    ///
    /// NAMESPACED by agy's conversation id, because `messages.id` is unique
    /// across the whole database while `step_index` only counts within one agy
    /// conversation. Bare `step-<n>` collided the moment a second conversation
    /// reached the same index — observed 52 times in one log
    /// ("Duplicate record: Message with id 'step-6' already exists outside the
    /// requested conversation"), which silently dropped the tool card for every
    /// conversation after the first to use that number.
    ///
    /// Falls back to the bare form only before `init` has bound a session id,
    /// where there is nothing to namespace with and no second conversation to
    /// collide against yet.
    fn step_id(&self, step_index: u64) -> String {
        match self.backend_session_id.as_deref() {
            Some(session) => format!("{session}-step-{step_index}"),
            None => format!("step-{step_index}"),
        }
    }

    fn translate_step(&mut self, su: AgyStepUpdate) -> Vec<SessionEvent> {
        let id = self.step_id(su.step_index);
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
            // A subagent dispatch is work the user should be able to watch. agy
            // pairs ACTIVE/DONE on one step index, so it maps onto the
            // tool-call shape the frontend already renders — no new surface.
            //
            // The DONE frame carries no result: agy folds the subagent's answer
            // into its own prose. The card therefore settles empty rather than
            // inventing an output it was never given.
            AgyStepType::Subagent => {
                let agents = su.subagent_info.map(|i| i.subagents).unwrap_or_default();
                match su.state {
                    AgyState::Active => out.push(SessionEvent::ToolCall {
                        tool_use_id: id,
                        name: subagent_display_name(&agents),
                        subagent: Default::default(),
                        input: subagent_input(&agents),
                        parent_tool_use_id: None,
                    }),
                    AgyState::Done | AgyState::Error => out.push(SessionEvent::ToolResult {
                        tool_use_id: id,
                        is_error: su.state == AgyState::Error,
                        content: Vec::new(),
                        parent_tool_use_id: None,
                    }),
                    // A state this build does not know is not evidence the
                    // dispatch started or finished; settling the card on it
                    // would report an outcome agy never sent.
                    AgyState::Unknown => {}
                }
            }
            // Counted, not rendered: the frame carries no message, tool name or
            // cause (only `duration_seconds`), so there is nothing truthful to
            // put on screen per-step. The count is what makes the turn's
            // closing "success" answerable.
            AgyStepType::ErrorMessage => {
                if su.state == AgyState::Done {
                    self.failed_steps = self.failed_steps.saturating_add(1);
                }
            }
            AgyStepType::UserInput | AgyStepType::Checkpoint | AgyStepType::SystemMessage | AgyStepType::Unknown => {}
        }

        if let Some(u) = su.usage.as_ref() {
            out.push(usage_delta(u));
        }
        out
    }
}

/// Card title for a subagent dispatch.
///
/// `role` is agy's own description of the job ("File Counter") and is what a
/// reader wants; `type_name` ("research") is the template behind it and only
/// stands in when there is no role. A batch is summarised by count instead of
/// concatenated, which would overflow the card.
fn subagent_display_name(agents: &[AgySubagent]) -> String {
    match agents {
        [] => "subagent".to_owned(),
        [one] => {
            let label = [one.role.trim(), one.type_name.trim()]
                .into_iter()
                .find(|v| !v.is_empty())
                .unwrap_or("subagent");
            format!("subagent: {label}")
        }
        many => format!("{} subagents", many.len()),
    }
}

/// What the card shows when expanded. agy's internal handles
/// (`conversation_id`, `log_uri`) are deliberately left out: they point into
/// its private brain directory and mean nothing to the user.
fn subagent_input(agents: &[AgySubagent]) -> serde_json::Value {
    serde_json::Value::Array(
        agents
            .iter()
            .map(|a| {
                serde_json::json!({
                    "type": a.type_name,
                    "role": a.role,
                    "prompt": a.initial_prompt,
                })
            })
            .collect(),
    )
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
    fn a_signed_out_turn_tells_the_user_how_to_fix_it() {
        // Probing cannot detect a signed-out install (agy --version exits 0
        // either way), so this message is the user's ONLY signal.
        let (_, evs) = tr(&[
            r#"{"event":"result","result":{"conversation_id":"","status":"ERROR","error":"authentication failed or timed out"}}"#,
        ]);
        match evs.last() {
            Some(SessionEvent::TurnResult {
                is_error, result_text, ..
            }) => {
                assert!(is_error);
                assert!(
                    result_text.contains("agy"),
                    "must name the command to run: {result_text}"
                );
                assert!(
                    result_text.to_lowercase().contains("sign"),
                    "must say what is wrong: {result_text}"
                );
                // The original text stays for diagnosis.
                assert!(result_text.contains("authentication failed or timed out"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn an_unrelated_error_is_passed_through_untouched() {
        // Only the auth case is recognised; inventing explanations for other
        // failures would mislead.
        assert_eq!(explain_turn_error("disk quota exceeded"), "disk quota exceeded");
    }

    /// Captured from agy 1.1.10 (trimmed to the fields we read).
    const SUBAGENT_ACTIVE: &str = r#"{"event":"step_update","step_update":{"step_index":3,"state":"ACTIVE","step_type":"subagent","subagent_info":{"subagents":[{"type_name":"research","role":"File Counter","initial_prompt":"Count the files.","conversation_id":"108d172f","log_uri":"file:///brain/log"}]}}}"#;
    const SUBAGENT_DONE: &str = r#"{"event":"step_update","step_update":{"step_index":3,"state":"DONE","step_type":"subagent","duration_seconds":0.06,"subagent_info":{"subagents":[{"type_name":"research","role":"File Counter","initial_prompt":"Count the files.","conversation_id":"108d172f","log_uri":"file:///brain/log"}]}}}"#;

    #[test]
    fn tool_ids_do_not_collide_across_conversations() {
        // `messages.id` is unique across the whole database, but `step_index`
        // only counts within one agy conversation. A bare `step-<n>` therefore
        // collided as soon as a second conversation reached the same index:
        // "Duplicate record: Message with id 'step-6' already exists outside
        // the requested conversation" — 52 of them in one log, each a tool card
        // silently dropped. Every existing test drove a single Translator, so
        // none of them could see it.
        let tool_step = r#"{"event":"step_update","step_update":{"step_index":3,"state":"ACTIVE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{}}}}"#;
        let id_in = |conv: &str| {
            let init = format!(r#"{{"event":"init","conversation_id":"{conv}","init":{{"cwd":"/w"}}}}"#);
            let (_, evs) = tr(&[&init, tool_step]);
            evs.iter()
                .find_map(|e| match e {
                    SessionEvent::ToolCall { tool_use_id, .. } => Some(tool_use_id.clone()),
                    _ => None,
                })
                .expect("tool call")
        };
        assert_ne!(
            id_in("conv-a"),
            id_in("conv-b"),
            "the same step index in two conversations must not produce the same message id"
        );
    }

    #[test]
    fn a_subagent_dispatch_is_visible_as_a_tool_call() {
        // Before this arm the step fell to `Unknown` and the dispatch was
        // dropped: the user saw only whatever prose agy wrote afterwards, with
        // no sign that work had been handed to a subagent at all.
        let (_, evs) = tr(&[SUBAGENT_ACTIVE, SUBAGENT_DONE]);
        let (name, input) = evs
            .iter()
            .find_map(|e| match e {
                SessionEvent::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
                _ => None,
            })
            .expect("a dispatch must open a card");
        // The role is the human-written job description; the template name is
        // not what a reader is looking for.
        assert!(name.contains("File Counter"), "{name}");
        assert!(
            input.to_string().contains("Count the files."),
            "the prompt the subagent was given must be inspectable: {input}"
        );
        // agy's private handles are not the user's business.
        let rendered = format!("{name}{input}");
        assert!(
            !rendered.contains("108d172f") && !rendered.contains("brain/log"),
            "{rendered}"
        );

        assert!(
            evs.iter()
                .any(|e| matches!(e, SessionEvent::ToolResult { is_error: false, .. })),
            "the card must settle, or it spins forever: {evs:?}"
        );
    }

    #[test]
    fn a_batch_dispatch_is_summarised_not_concatenated() {
        let two = r#"{"event":"step_update","step_update":{"step_index":3,"state":"ACTIVE","step_type":"subagent","subagent_info":{"subagents":[{"role":"A"},{"role":"B"}]}}}"#;
        let (_, evs) = tr(&[two]);
        let name = evs
            .iter()
            .find_map(|e| match e {
                SessionEvent::ToolCall { name, .. } => Some(name.clone()),
                _ => None,
            })
            .expect("card");
        assert_eq!(name, "2 subagents");
    }

    #[test]
    fn a_dispatch_without_details_still_opens_a_card() {
        // Never drop the signal because a field is missing: that is how the
        // whole step type went unnoticed in the first place.
        let bare = r#"{"event":"step_update","step_update":{"step_index":7,"state":"ACTIVE","step_type":"subagent"}}"#;
        let (_, evs) = tr(&[bare]);
        assert!(
            evs.iter()
                .any(|e| matches!(e, SessionEvent::ToolCall { name, .. } if name == "subagent")),
            "{evs:?}"
        );
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

    /// The exact frame sequence agy 1.1.9 produced when a PreToolUse hook
    /// answered `deny`: failed steps, then a SUCCESS terminal claiming "DONE",
    /// with the file never written.
    const REFUSED_TURN: &[&str] = &[
        r#"{"event":"step_update","step_update":{"step_index":3,"state":"DONE","step_type":"error_message","duration_seconds":0.62}}"#,
        r#"{"event":"step_update","step_update":{"step_index":6,"state":"DONE","step_type":"error_message","duration_seconds":0.02}}"#,
        r#"{"event":"step_update","step_update":{"step_index":11,"state":"DONE","step_type":"agent_response","text_delta":"DONE\n"}}"#,
        r#"{"event":"result","result":{"conversation_id":"c1","status":"SUCCESS","response":"DONE\n"}}"#,
    ];

    #[test]
    fn a_success_that_hides_refused_steps_is_flagged() {
        let (_, evs) = tr(REFUSED_TURN);
        let notice = evs
            .iter()
            .find_map(|e| match e {
                SessionEvent::Notice { level, message, .. } => Some((level, message)),
                _ => None,
            })
            .expect("a turn that refused work must not read as plain success");
        assert_eq!(*notice.0, NoticeLevel::Info);
        assert!(notice.1.contains('2'), "the count is the whole signal: {}", notice.1);
        // The stream shows which STEPS failed, never what the agent claimed.
        // Asserting the reply said "success" accused an agy that had just
        // apologised for the same refusal (observed in 0803-2).
        let lower = notice.1.to_ascii_lowercase();
        assert!(
            !lower.contains("reported success") && !lower.contains("the agent said"),
            "the notice must not characterise the agent's words: {}",
            notice.1
        );

        // agy's own text still reaches the user — it may carry real reasoning,
        // and silently rewriting the agent's words would be its own kind of lie.
        assert!(evs.iter().any(|e| matches!(
            e,
            SessionEvent::TurnResult { is_error: false, result_text, .. } if result_text.contains("DONE")
        )));
    }

    #[test]
    fn the_notice_precedes_the_terminal_or_it_is_never_delivered() {
        // NOT cosmetic ordering. `StreamRelay` breaks out of its receive loop on
        // the terminal frame, so an event emitted after it is dropped before
        // persistence. Moving the notice below `TurnResult` — to give the
        // correction the last word — deleted it outright: conversation 0803-4
        // logged four denied permissions and `event=Notice`, and no tips row was
        // ever written. Reading above the claim is the price of existing.
        let (_, evs) = tr(REFUSED_TURN);
        let result_at = evs
            .iter()
            .position(|e| matches!(e, SessionEvent::TurnResult { .. }))
            .expect("a terminal is always emitted");
        let notice_at = evs
            .iter()
            .position(|e| matches!(e, SessionEvent::Notice { .. }))
            .expect("a refused turn must be flagged");
        assert!(
            notice_at < result_at,
            "a notice after the terminal is discarded by the relay, got notice@{notice_at} result@{result_at}"
        );
    }

    #[test]
    fn a_clean_turn_stays_quiet() {
        // The warning must mean something; firing it on healthy turns would
        // train the user to ignore it.
        let (_, evs) = tr(&[
            r#"{"event":"step_update","step_update":{"step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"hi"}}"#,
            r#"{"event":"result","result":{"conversation_id":"c1","status":"SUCCESS","response":"hi"}}"#,
        ]);
        assert!(
            !evs.iter().any(|e| matches!(e, SessionEvent::Notice { .. })),
            "unexpected notice: {evs:?}"
        );
    }

    #[test]
    fn an_already_failed_turn_is_not_double_reported() {
        // The terminal already says it failed; a second warning adds nothing.
        let (_, evs) = tr(&[
            r#"{"event":"step_update","step_update":{"step_index":3,"state":"DONE","step_type":"error_message"}}"#,
            r#"{"event":"result","result":{"conversation_id":"c1","status":"ERROR","error":"boom"}}"#,
        ]);
        assert!(
            !evs.iter().any(|e| matches!(e, SessionEvent::Notice { .. })),
            "unexpected notice: {evs:?}"
        );
    }

    #[test]
    fn the_count_does_not_leak_into_the_next_turn() {
        // One Translator lives for the whole session (agy resumes by id), so a
        // refused turn must not make every later success look suspect.
        let mut t = Translator::default();
        for line in REFUSED_TURN {
            t.translate(parse_line(line).expect("fixture must parse"));
        }
        let clean: Vec<_> = t.translate(
            parse_line(r#"{"event":"result","result":{"conversation_id":"c1","status":"SUCCESS","response":"ok"}}"#)
                .expect("fixture must parse"),
        );
        assert!(
            !clean.iter().any(|e| matches!(e, SessionEvent::Notice { .. })),
            "stale count leaked: {clean:?}"
        );
    }

    #[test]
    fn an_in_flight_failed_step_is_counted_once() {
        // agy emits ACTIVE then DONE for a step; counting both would double it.
        let (_, evs) = tr(&[
            r#"{"event":"step_update","step_update":{"step_index":3,"state":"ACTIVE","step_type":"error_message"}}"#,
            r#"{"event":"step_update","step_update":{"step_index":3,"state":"DONE","step_type":"error_message"}}"#,
            r#"{"event":"result","result":{"conversation_id":"c1","status":"SUCCESS","response":"done"}}"#,
        ]);
        let msg = evs
            .iter()
            .find_map(|e| match e {
                SessionEvent::Notice { message, .. } => Some(message.clone()),
                _ => None,
            })
            .expect("notice expected");
        assert!(msg.contains('1') && !msg.contains('2'), "counted twice: {msg}");
    }
}
