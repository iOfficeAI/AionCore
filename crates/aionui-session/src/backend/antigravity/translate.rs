//! agy wire events -> backend-neutral `SessionEvent`. Pure: no IO, no spawning.
//!
//! Demux key: agy's `step_index` is monotonic within a conversation and keeps
//! counting across a `--conversation` resume, so `step-<index>` is a stable
//! id for pairing a tool's ACTIVE and DONE frames.

use std::collections::HashSet;
use std::time::Duration;

use super::wire::{AgyEvent, AgyState, AgyStepType, AgyStepUpdate, AgySubagent, AgyUsage};
use super::{
    CODE_COMPACTED, CODE_COMPACTING, CODE_STEPS_FAILED, CODE_THINKING, CODE_THOUGHT_COMPLETE, STATUS_NOTICE_KEY,
};
use crate::event::{
    CancelReason, LocalizedText, NoticeLevel, SessionEvent, ToolResultContent, TurnOutcome, UsageBreakdown,
};

/// Empirical context token ceiling at which `agy` triggers automatic
/// conversation compaction (`checkpoint` step).
///
/// Verified against `agy` session transcripts (`~/.gemini/antigravity-cli/brain/*/.system_generated/logs/transcript.jsonl`):
/// - Gemini models (`gemini-*`, default): `CHECKPOINT` steps fire when single-call
///   context (`input_tokens + cache_read_tokens`) reaches ~256k tokens
///   (measured across Gemini Carbon/Argon compactions at 255,518–259,379 tokens).
/// - Claude models (`claude-*`): 200k context window compacts at ~90% occupancy
///   (~180,000 tokens).
pub(crate) fn compaction_threshold_tokens(model: Option<&str>) -> u64 {
    match model.map(|m| m.trim().to_ascii_lowercase()) {
        Some(m) if m.starts_with("claude") => 180_000,
        _ => 256_000,
    }
}

/// Whether `context_tokens` is within 90% of the model's empirical compaction
/// ceiling (`>= 162,000` on Claude, `>= 230,400` on Gemini/default).
///
/// `agy` emits NO `checkpoint` `ACTIVE` frame when auto-compaction begins —
/// stdout stays completely silent for 15–78s until `checkpoint` `DONE` lands.
/// Above this line, a silent wait may be an in-flight auto-compaction rather
/// than ordinary reasoning, so the status hint says so honestly.
pub(crate) fn is_near_compaction_threshold(context_tokens: u64, model: Option<&str>) -> bool {
    let threshold = compaction_threshold_tokens(model);
    context_tokens >= (threshold * 9) / 10
}

pub(crate) fn compacting_notice(elapsed_secs: Option<u64>) -> SessionEvent {
    let (message, localized) = match elapsed_secs {
        Some(secs) => (
            format!("Compacting conversation context… ({secs}s)"),
            LocalizedText::new(CODE_COMPACTING).with("elapsed_seconds", secs),
        ),
        None => (
            "Compacting conversation context…".to_owned(),
            LocalizedText::new(CODE_COMPACTING),
        ),
    };
    SessionEvent::Notice {
        level: NoticeLevel::Info,
        message,
        localized: Some(localized),
        supersedes_key: Some(STATUS_NOTICE_KEY.to_owned()),
    }
}

pub(crate) fn compacted_notice(duration_seconds: Option<f64>) -> SessionEvent {
    let valid_duration = duration_seconds.filter(|d| d.is_finite() && *d >= 0.0);
    let (message, localized) = match valid_duration {
        Some(secs) => {
            let rounded = (secs * 10.0).round() / 10.0;
            (
                format!("Conversation context compacted ({secs:.1}s)."),
                LocalizedText::new(CODE_COMPACTED).with("duration_seconds", rounded),
            )
        }
        None => (
            "Conversation context compacted.".to_owned(),
            LocalizedText::new(CODE_COMPACTED),
        ),
    };
    SessionEvent::Notice {
        level: NoticeLevel::Info,
        message,
        localized: Some(localized),
        supersedes_key: Some(STATUS_NOTICE_KEY.to_owned()),
    }
}

/// Build the live status notice for a silent model-wait tick, returning
/// `(notice, is_compaction_hint)`.
pub(crate) fn thinking_or_compacting_notice(
    elapsed_secs: u64,
    context_tokens: u64,
    model: Option<&str>,
) -> (SessionEvent, bool) {
    if is_near_compaction_threshold(context_tokens, model) {
        let approx_k = context_tokens.saturating_add(500) / 1_000;
        let threshold = compaction_threshold_tokens(model);
        let localized = LocalizedText::new(CODE_COMPACTING)
            .with("elapsed_seconds", elapsed_secs)
            .with("context_tokens", context_tokens)
            .with("threshold_tokens", threshold);
        return (
            SessionEvent::Notice {
                level: NoticeLevel::Info,
                message: format!(
                    "Antigravity is thinking or compacting long context (~{approx_k}k tokens, {elapsed_secs}s)…"
                ),
                localized: Some(localized),
                supersedes_key: Some(STATUS_NOTICE_KEY.to_owned()),
            },
            true,
        );
    }

    (
        SessionEvent::Notice {
            level: NoticeLevel::Info,
            message: format!("Antigravity is thinking… ({elapsed_secs}s)"),
            localized: Some(LocalizedText::new(CODE_THINKING).with("elapsed_seconds", elapsed_secs)),
            supersedes_key: Some(STATUS_NOTICE_KEY.to_owned()),
        },
        false,
    )
}

/// Settle an already-visible thinking banner into past tense so neither the
/// live view nor reloaded SQLite history retains a frozen `"is thinking…"` card.
pub(crate) fn settled_thinking_notice(
    fallback_elapsed: Duration,
    wire_duration_secs: Option<f64>,
    thinking_tokens: u64,
) -> SessionEvent {
    let secs = wire_duration_secs
        .filter(|d| d.is_finite() && *d >= 0.0)
        .unwrap_or(fallback_elapsed.as_secs_f64());
    let rounded = (secs * 10.0).round() / 10.0;
    let (message, localized) = if thinking_tokens > 0 {
        (
            format!("Thought for {secs:.1}s ({thinking_tokens} thinking tokens)."),
            LocalizedText::new(CODE_THOUGHT_COMPLETE)
                .with("duration_seconds", rounded)
                .with("thinking_tokens", thinking_tokens),
        )
    } else {
        (
            format!("Thought for {secs:.1}s."),
            LocalizedText::new(CODE_THOUGHT_COMPLETE).with("duration_seconds", rounded),
        )
    };
    SessionEvent::Notice {
        level: NoticeLevel::Info,
        message,
        localized: Some(localized),
        supersedes_key: Some(STATUS_NOTICE_KEY.to_owned()),
    }
}

/// Settle an open live status banner when a turn ends in cancel, error, or EOF.
pub(crate) fn stopped_status_notice(was_compacting: bool, elapsed: Duration) -> SessionEvent {
    let secs = elapsed.as_secs_f64();
    let rounded = (secs * 10.0).round() / 10.0;
    let message = if was_compacting {
        format!("Stopped while compacting context ({secs:.1}s).")
    } else {
        format!("Stopped while thinking ({secs:.1}s).")
    };
    SessionEvent::Notice {
        level: NoticeLevel::Info,
        message,
        localized: Some(LocalizedText::new(CODE_THOUGHT_COMPLETE).with("duration_seconds", rounded)),
        supersedes_key: Some(STATUS_NOTICE_KEY.to_owned()),
    }
}

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
    /// True while a `/compact` command (`init.expanded_commands`) or an active
    /// `checkpoint` step is in flight.
    compaction_in_progress: bool,
    /// True once a `checkpoint` `DONE` step completed in this turn.
    compacted_this_turn: bool,
    /// Step indices of currently running `tool` / `subagent` steps. While
    /// non-empty, `agy` is waiting on external work rather than silent in an
    /// LLM reasoning/compaction call.
    active_foreground_steps: HashSet<u64>,
    /// Thinking tokens reported on `agent_response` `DONE` frames in the
    /// current model phase.
    phase_thinking_tokens: u64,
    /// Wall-clock duration reported on `agent_response` `DONE` frames in the
    /// current model phase.
    phase_duration_secs: Option<f64>,
    /// Latest single-call context occupancy (`Some(0)` right after a compaction
    /// invalidates the pre-compaction context size).
    latest_context_tokens: Option<u64>,
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

    pub(crate) fn is_compacting(&self) -> bool {
        self.compaction_in_progress
    }

    pub(crate) fn has_active_foreground_step(&self) -> bool {
        !self.active_foreground_steps.is_empty()
    }

    pub(crate) fn latest_context_tokens(&self) -> Option<u64> {
        self.latest_context_tokens
    }

    pub(crate) fn take_phase_thinking_metrics(&mut self) -> (Option<f64>, u64) {
        (
            self.phase_duration_secs.take(),
            std::mem::take(&mut self.phase_thinking_tokens),
        )
    }

    pub(crate) fn translate(&mut self, ev: AgyEvent) -> Vec<SessionEvent> {
        match ev {
            AgyEvent::Init(init) => {
                if !init.conversation_id.is_empty() {
                    self.backend_session_id = Some(init.conversation_id.clone());
                }
                // Verified live on `agy 1.5.3 -p "/compact"`: system slash-command
                // expansions omit `init.model` even when no `--model` fallback happened.
                self.model_fallback = init.model.is_none() && init.expanded_commands.is_empty();
                self.current_model = init.model;
                self.compaction_in_progress = init
                    .expanded_commands
                    .iter()
                    .any(|cmd| cmd.name.eq_ignore_ascii_case("compact"));
                self.compacted_this_turn = false;
                self.active_foreground_steps.clear();
                self.phase_thinking_tokens = 0;
                self.phase_duration_secs = None;
                let mut out = Vec::with_capacity(2);
                out.push(SessionEvent::BackendBound {
                    backend_session_id: self.backend_session_id.clone(),
                });
                if self.compaction_in_progress {
                    out.push(compacting_notice(None));
                }
                out
            }
            AgyEvent::StepUpdate(su) => self.translate_step(su),
            AgyEvent::Result(r) => {
                if !r.conversation_id.is_empty() {
                    self.backend_session_id = Some(r.conversation_id);
                }
                self.compaction_in_progress = false;
                self.active_foreground_steps.clear();
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
                    if self.latest_context_tokens.is_none() && !self.compacted_this_turn {
                        self.latest_context_tokens = Some(context_occupancy_tokens(&u));
                    }
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
                        supersedes_key: None,
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
                if let Some(u) = su.usage.as_ref() {
                    self.latest_context_tokens = Some(context_occupancy_tokens(u));
                    if su.state == AgyState::Done {
                        self.phase_thinking_tokens = self.phase_thinking_tokens.saturating_add(u.thinking_tokens);
                    }
                }
                if su.state == AgyState::Done
                    && let Some(d) = su.duration_seconds.filter(|d| d.is_finite() && *d >= 0.0)
                {
                    self.phase_duration_secs = Some(self.phase_duration_secs.unwrap_or(0.0) + d);
                }
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
                    AgyState::Active => {
                        self.active_foreground_steps.insert(su.step_index);
                        out.push(SessionEvent::ToolCall {
                            tool_use_id: id,
                            name,
                            subagent: Default::default(),
                            input: info.and_then(|i| i.parameters).unwrap_or(serde_json::Value::Null),
                            parent_tool_use_id: None,
                        });
                    }
                    AgyState::Done => {
                        self.active_foreground_steps.remove(&su.step_index);
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
                        self.active_foreground_steps.remove(&su.step_index);
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
                    AgyState::Active => {
                        self.active_foreground_steps.insert(su.step_index);
                        out.push(SessionEvent::ToolCall {
                            tool_use_id: id,
                            name: subagent_display_name(&agents),
                            subagent: Default::default(),
                            input: subagent_input(&agents),
                            parent_tool_use_id: None,
                        });
                    }
                    AgyState::Done | AgyState::Error => {
                        self.active_foreground_steps.remove(&su.step_index);
                        out.push(SessionEvent::ToolResult {
                            tool_use_id: id,
                            is_error: su.state == AgyState::Error,
                            content: Vec::new(),
                            parent_tool_use_id: None,
                        });
                    }
                    // A state this build does not know is not evidence the
                    // dispatch started or finished; settling the card on it
                    // would report an outcome agy never sent.
                    AgyState::Unknown => {}
                }
            }
            // Context compaction (`checkpoint`). Verified live on `agy 1.5.3`
            // (`-p "/compact" --conversation <id> --output-format stream-json`)
            // and across `transcript.jsonl` `CHECKPOINT` records: `agy` emits
            // `checkpoint` solely for conversation compaction, carrying
            // `duration_seconds` and pre-compaction `usage` on `DONE`.
            AgyStepType::Checkpoint => match su.state {
                AgyState::Active => {
                    if !self.compaction_in_progress {
                        self.compaction_in_progress = true;
                        out.push(compacting_notice(None));
                    }
                }
                AgyState::Done => {
                    self.compaction_in_progress = false;
                    self.compacted_this_turn = true;
                    // `su.usage` on a `checkpoint` step counts the PRE-compaction
                    // prompt fed into the summarizer (`input_tokens: 9299, output_tokens: 335`
                    // measured on `agy 1.5.3`). Resetting occupancy to 0 prevents the
                    // next turn from mistaking the pre-compaction context size for the
                    // freshly shrunk one (same invariant as `adapter/claude.rs` `compact_boundary`).
                    self.latest_context_tokens = Some(0);
                    self.phase_thinking_tokens = 0;
                    self.phase_duration_secs = None;
                    out.push(compacted_notice(su.duration_seconds));
                }
                AgyState::Error => {
                    self.compaction_in_progress = false;
                }
                AgyState::Unknown => {}
            },
            // Counted, not rendered: the frame carries no message, tool name or
            // cause (only `duration_seconds`), so there is nothing truthful to
            // put on screen per-step. The count is what makes the turn's
            // closing "success" answerable.
            AgyStepType::ErrorMessage => {
                if su.state == AgyState::Done {
                    self.failed_steps = self.failed_steps.saturating_add(1);
                }
            }
            // Pure bookkeeping frames: agy echoes the user turn and system
            // notices as steps with no user-facing payload.
            AgyStepType::UserInput | AgyStepType::SystemMessage | AgyStepType::Unknown => {}
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

fn context_occupancy_tokens(u: &AgyUsage) -> u64 {
    let summed = u
        .input_tokens
        .saturating_add(u.cache_read_tokens)
        .saturating_add(u.output_tokens);
    u.total_tokens.max(summed)
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

    /// Exact NDJSON sequence captured from `agy 1.5.3 -p "/compact" --conversation <id> --output-format stream-json`
    /// (note: `agy` omits `init.model` on system slash-command expansions).
    const SLASH_COMPACT_TURN: &[&str] = &[
        r#"{"event":"init","conversation_id":"286c2016-0000-0000-0000-000000000001","init":{"cwd":"/w","tools":["run_command"],"permission_mode":"always-proceed","expanded_commands":[{"name":"compact","type":"system"}]}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"286c2016-0000-0000-0000-000000000001","step_index":2,"state":"DONE","step_type":"user_input"}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"286c2016-0000-0000-0000-000000000001","step_index":3,"state":"DONE","step_type":"system_message","duration_seconds":0.0005519}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"286c2016-0000-0000-0000-000000000001","step_index":4,"state":"DONE","step_type":"checkpoint","duration_seconds":8.9457684,"usage":{"input_tokens":9299,"output_tokens":335,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":9634}}}"#,
        r#"{"event":"result","result":{"conversation_id":"286c2016-0000-0000-0000-000000000001","status":"SUCCESS","response":"","duration_seconds":21.8283029,"num_turns":2,"usage":{"input_tokens":17474,"output_tokens":343,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":17817}}}"#,
    ];

    #[test]
    fn slash_compact_turn_emits_compacting_then_superseding_compacted_notice() {
        let (t, evs) = tr(SLASH_COMPACT_TURN);
        assert!(!t.is_compacting(), "compaction flag must clear on checkpoint DONE");
        assert!(
            !t.model_fallback_detected(),
            "expanded `/compact` omits init.model and must not warn of model fallback"
        );
        // Pre-compaction usage (`9634`) must NOT be retained as post-compaction occupancy.
        assert_eq!(t.latest_context_tokens(), Some(0));
        assert!(
            !evs.iter().any(|e| matches!(e, SessionEvent::ThoughtDelta { .. })),
            "must never synthesize ThoughtDelta: {evs:?}"
        );

        let notices: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Notice {
                    level,
                    message,
                    localized,
                    supersedes_key,
                } => Some((level, message.as_str(), localized.as_ref(), supersedes_key.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(notices.len(), 2, "expected start + completion notices: {notices:?}");
        assert_eq!(*notices[0].0, NoticeLevel::Info);
        assert_eq!(notices[0].2.map(|l| l.code.as_str()), Some(CODE_COMPACTING));
        assert_eq!(notices[0].3, Some(STATUS_NOTICE_KEY));

        assert_eq!(*notices[1].0, NoticeLevel::Info);
        assert!(notices[1].1.contains("8.9s"), "unexpected body: {}", notices[1].1);
        assert_eq!(notices[1].2.map(|l| l.code.as_str()), Some(CODE_COMPACTED));
        assert_eq!(notices[1].3, Some(STATUS_NOTICE_KEY));

        let compacted_at = evs
            .iter()
            .position(|e| {
                matches!(
                    e,
                    SessionEvent::Notice {
                        localized: Some(l),
                        ..
                    } if l.code == CODE_COMPACTED
                )
            })
            .expect("compacted notice");
        let terminal_at = evs
            .iter()
            .position(|e| matches!(e, SessionEvent::TurnResult { .. }))
            .expect("terminal");
        assert!(compacted_at < terminal_at, "notice must precede TurnResult");
    }

    #[test]
    fn checkpoint_without_duration_still_surfaces_compaction_completion() {
        let (_, evs) = tr(&[
            r#"{"event":"step_update","step_update":{"step_index":1,"state":"ACTIVE","step_type":"checkpoint"}}"#,
            r#"{"event":"step_update","step_update":{"step_index":1,"state":"DONE","step_type":"checkpoint"}}"#,
        ]);
        let codes: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Notice {
                    localized: Some(l),
                    message,
                    ..
                } => Some((l.code.as_str(), message.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            codes,
            vec![
                (CODE_COMPACTING, "Compacting conversation context…"),
                (CODE_COMPACTED, "Conversation context compacted."),
            ]
        );
    }

    #[test]
    fn compaction_thresholds_distinguish_claude_and_gemini_windows() {
        assert_eq!(compaction_threshold_tokens(Some("claude-sonnet-4-6")), 180_000);
        assert_eq!(compaction_threshold_tokens(Some("Claude-Opus-4-7")), 180_000);
        assert_eq!(compaction_threshold_tokens(Some("gemini-4-carbon-high")), 256_000);
        assert_eq!(compaction_threshold_tokens(None), 256_000);

        // Claude 90% line = 162,000; Gemini 90% line = 230,400.
        assert!(!is_near_compaction_threshold(161_999, Some("claude-sonnet-4-6")));
        assert!(is_near_compaction_threshold(162_000, Some("claude-sonnet-4-6")));
        assert!(!is_near_compaction_threshold(175_000, Some("gemini-4-carbon")));
        assert!(!is_near_compaction_threshold(230_399, Some("gemini-4-carbon")));
        assert!(is_near_compaction_threshold(230_400, Some("gemini-4-carbon")));
    }

    #[test]
    fn active_foreground_step_tracks_tools_and_subagents_only() {
        let mut t = Translator::default();
        assert!(!t.has_active_foreground_step());

        t.translate(
            parse_line(
                r#"{"event":"step_update","step_update":{"step_index":3,"state":"ACTIVE","step_type":"tool","tool_name":"run_command"}}"#,
            )
            .unwrap(),
        );
        assert!(t.has_active_foreground_step());

        t.translate(
            parse_line(
                r#"{"event":"step_update","step_update":{"step_index":3,"state":"DONE","step_type":"tool","tool_name":"run_command"}}"#,
            )
            .unwrap(),
        );
        assert!(!t.has_active_foreground_step());
    }
}
