use crate::error::AgentError;
use crate::manager::acp::AcpAgentManager;
use crate::manager::acp::mode_normalize::agent_metadata_uses_meta_resume;
use crate::protocol::error::AcpError;
use crate::protocol::events::{
    AgentStreamEvent, AvailableCommandsEventData, ErrorEventData, SessionAssignedEventData, StartEventData, TipType,
    TipsEventData,
};
use crate::protocol::send_error::AgentSendError;
use crate::shared_kernel::SessionId as DomainSessionId;
use crate::types::SendMessageData;
use agent_client_protocol::schema::v1::{
    AuthMethod, ContentBlock, LoadSessionRequest, PromptRequest, PromptResponse, SessionId, StopReason, Usage,
    UsageUpdate,
};
use aionui_api_types::SlashCommandItem;
use serde_json::Value;
use tokio::sync::broadcast::error::TryRecvError;

use super::agent::sdk_to_snake_value;
use super::agent_close::STDERR_PEEK_LINES;
use super::error_mapping::{AcpSendFailure, is_acp_session_not_found, is_missing_resumed_session};
use super::legacy_session_model::LegacySessionModelState;
use tracing::warn;

#[derive(Debug)]
pub(super) enum PromptOutcome {
    Completed { session_id: String },
    Cancelled { session_id: String },
    TerminalError { session_id: String, error: ErrorEventData },
    InfoTip { session_id: String, tips: TipsEventData },
    WarningTip { session_id: String, tips: TipsEventData },
}

impl AcpAgentManager {
    /// Establish a fresh ACP session (session/new) and apply desired
    /// mode/model/config via reconcile. Does NOT send a prompt and
    /// does NOT emit Start/Finish — callers wrap that around if needed.
    ///
    /// Returns the CLI-assigned session id.
    pub(super) async fn open_session_new(&self) -> Result<String, AgentError> {
        let req = self.params.new_session_request();
        let (session_response, legacy_models) = self.protocol.new_session(req).await?;

        let sid = session_response.session_id.to_string();

        {
            let mut session = self.session.write().await;
            if let Some(models) = legacy_models
                .as_ref()
                .and_then(LegacySessionModelState::from_state_value)
            {
                session.apply_advertised_models(models);
            }
            if let Some(modes) = session_response.modes {
                session.apply_advertised_modes(modes);
            }
            if let Some(config_options) = session_response.config_options {
                session.apply_advertised_config_options(config_options);
            }
            session.set_session_id(DomainSessionId::new(sid.clone()));
            // Mark that the next prompt should carry the first-prompt prelude
            // (preset_context + skill index). Consumed by SessionNewPreludeHook.
            session.mark_pending_session_new_prelude();
            self.commit_session_changes(&mut session).await;
        }
        self.emit_snapshot_events().await;

        // Notify session_sync consumer so the new id hits the DB and
        // future rebuilds can take the resume path.
        self.runtime
            .emit(AgentStreamEvent::SessionAssigned(SessionAssignedEventData {
                session_id: sid.clone(),
            }));

        // Best-effort reconcile on a freshly-opened session. SessionNotFound
        // here would be pathological (we just created the session) but is
        // still surfaced for consistency.
        self.reconcile_session(&sid).await?;
        Ok(sid)
    }

    /// Drop the in-aggregate session id and re-run `open_session_new`.
    /// Used as the rescue path when resume helpers see `SessionNotFound`.
    /// Emits a `warn!` so ops can still see the original failure that
    /// triggered the rebuild.
    async fn rebuild_after_session_not_found(&self, stale_sid: &str, err: &AcpError) -> Result<String, AgentError> {
        warn!(
            conversation_id = %self.params.conversation_id,
            stale_session_id = %stale_sid,
            recovery_action = "clear_persisted_session_id_and_session_new",
            error = %err,
            "open_session_resume: stale session id rejected by CLI; clearing persisted session id and rebuilding via session/new"
        );
        {
            let mut session = self.session.write().await;
            session.clear_session_id();
            self.commit_session_changes(&mut session).await;
        }
        self.open_session_new().await
    }

    async fn rebuild_after_acp_session_not_found(&self, stale_sid: &str, err: AcpError) -> Result<String, AgentError> {
        self.rebuild_after_session_not_found(stale_sid, &err).await
    }

    /// Resume an existing ACP session and apply desired mode/model/config.
    /// Does NOT send a prompt. Returns the (possibly rewritten) session id.
    ///
    /// - Claude-meta-resume backends: `session/new` with
    ///   `_meta.claudeCode.options.resume`. The CLI may assign a new session id,
    ///   which we persist via `SessionAssigned`.
    /// - `session/load`-capable backends (e.g. Codex, OpenCode): `session/load`,
    ///   keep id.
    /// - Backends that support neither: seed the aggregate and hope the CLI
    ///   still recognises the id (legacy behaviour — matches pre-refactor).
    ///
    /// In all three branches a `SessionNotFound` reply (the persisted sid
    /// became stale, e.g. after a CLI upgrade or restart) triggers
    /// `rebuild_after_session_not_found`, which clears the sid and
    /// re-runs `open_session_new`. ELECTRON-1HQ regressed because we
    /// silently swallowed this case during warmup, leaving every
    /// subsequent `session/prompt` to surface the same error to the user.
    pub(super) async fn open_session_resume(&self, session_id: &str) -> Result<String, AgentError> {
        if agent_metadata_uses_meta_resume(&self.params.metadata) {
            let mut meta = serde_json::Map::new();
            let mut claude_code = serde_json::Map::new();
            let mut options = serde_json::Map::new();
            options.insert("resume".into(), Value::String(session_id.to_owned()));
            claude_code.insert("options".into(), Value::Object(options));
            meta.insert("claudeCode".into(), Value::Object(claude_code));

            let req = self.params.new_session_request().meta(meta);
            let (new_response, legacy_models) = match self.protocol.new_session(req).await {
                Ok(r) => r,
                Err(e) if is_missing_resumed_session(&e, session_id) => {
                    return self.rebuild_after_acp_session_not_found(session_id, e).await;
                }
                Err(e) => return Err(e.into()),
            };
            let new_sid = new_response.session_id.to_string();

            {
                let mut session = self.session.write().await;
                if let Some(models) = legacy_models
                    .as_ref()
                    .and_then(LegacySessionModelState::from_state_value)
                {
                    session.apply_advertised_models(models);
                }
                if let Some(modes) = new_response.modes {
                    session.apply_advertised_modes(modes);
                }
                if let Some(config_options) = new_response.config_options {
                    session.apply_advertised_config_options(config_options);
                }
                session.set_session_id(DomainSessionId::new(new_sid.clone()));
                self.commit_session_changes(&mut session).await;
            }
            self.emit_snapshot_events().await;

            if new_sid != session_id {
                self.runtime
                    .emit(AgentStreamEvent::SessionAssigned(SessionAssignedEventData {
                        session_id: new_sid.clone(),
                    }));
            }

            return match self.reconcile_session(&new_sid).await {
                Ok(()) => Ok(new_sid),
                Err(e) if is_acp_session_not_found(&e) => self.rebuild_after_session_not_found(&new_sid, &e).await,
                Err(e) => Err(e.into()),
            };
        }

        let (supports_load, preloaded_mode) = {
            let session = self.session.read().await;
            (
                session.agent_capabilities().map(|c| c.load_session).unwrap_or(false),
                session.modes().map(|m| m.current_mode_id.to_string()),
            )
        };

        if supports_load {
            let mut load_req = LoadSessionRequest::new(SessionId::new(session_id), &self.params.workspace.path);
            if !self.params.mcp_servers.is_empty() {
                load_req = load_req.mcp_servers(self.params.mcp_servers.clone());
            }
            let (load_response, legacy_models) = match self.protocol.load_session(load_req).await {
                Ok(r) => r,
                Err(e) if is_acp_session_not_found(&e) => {
                    return self.rebuild_after_acp_session_not_found(session_id, e).await;
                }
                Err(e) => return Err(e.into()),
            };

            {
                let mut session = self.session.write().await;
                if let Some(models) = legacy_models
                    .as_ref()
                    .and_then(LegacySessionModelState::from_state_value)
                {
                    session.apply_advertised_models(models);
                }
                if let Some(mut modes) = load_response.modes {
                    if let Some(db_current) = preloaded_mode {
                        modes.current_mode_id = db_current.into();
                    }
                    session.apply_advertised_modes(modes);
                }
                if let Some(config_options) = load_response.config_options {
                    session.apply_advertised_config_options(config_options);
                }
                session.set_session_id(DomainSessionId::new(session_id.to_owned()));
                self.commit_session_changes(&mut session).await;
            }
            self.emit_snapshot_events().await;

            return match self.reconcile_session(session_id).await {
                Ok(()) => Ok(session_id.to_owned()),
                Err(e) if is_acp_session_not_found(&e) => self.rebuild_after_session_not_found(session_id, &e).await,
                Err(e) => Err(e.into()),
            };
        }

        // Legacy path: backend advertised neither claude-meta-resume nor
        // session/load. Seed the aggregate with the stored id and let the
        // caller prompt — matches pre-refactor behaviour.
        {
            let mut session = self.session.write().await;
            session.set_session_id(DomainSessionId::new(session_id.to_owned()));
            self.commit_session_changes(&mut session).await;
        }
        self.emit_snapshot_events().await;
        match self.reconcile_session(session_id).await {
            Ok(()) => Ok(session_id.to_owned()),
            Err(e) if is_acp_session_not_found(&e) => self.rebuild_after_session_not_found(session_id, &e).await,
            Err(e) => Err(e.into()),
        }
    }

    /// Send a prompt to an already-established session.
    pub(super) async fn prompt_existing_session(
        &self,
        data: &SendMessageData,
        session_id: Option<&str>,
        matched_command: Option<&SlashCommandItem>,
    ) -> Result<PromptOutcome, AcpSendFailure> {
        let sid = session_id
            .ok_or_else(|| AgentError::internal("Cannot prompt: no session ID available"))
            .map_err(AcpSendFailure::from)?;

        let content = data.content.clone();

        // Subscribe BEFORE emitting Start so we can observe every event
        // produced during this turn. Used after `prompt()` returns to detect
        // the "empty finish" scenario (model produced no text and no tool
        // calls); see `is_empty_turn` below.
        let mut probe_rx = self.runtime.subscribe();

        // Emit Start event
        self.runtime.emit(AgentStreamEvent::Start(StartEventData {
            session_id: Some(sid.to_owned()),
        }));

        // Scope stderr classification to this prompt so stale lines from an
        // earlier turn cannot override a later benign empty turn.
        self.process.clear_stderr().await;

        let prompt_response = self
            .protocol
            .prompt(PromptRequest::new(
                SessionId::new(sid),
                vec![ContentBlock::from(content)],
            ))
            .await
            .map_err(AcpSendFailure::from)?;

        // End-of-turn usage: agents that never emit UsageUpdate notifications
        // report token usage on the prompt response instead — either via the
        // unstable `usage` field or a `_meta` dialect. Re-emit it as the same
        // AcpContextUsage frame the notification path produces — it must
        // precede the Finish frame, because the stream relay stops forwarding
        // a turn once Finish is seen. The session event tracker only observes
        // CLI notifications, not runtime-emitted frames, so the snapshot
        // (which backs GET /usage) is updated here directly.
        if let Some(mut frame) = end_turn_usage_frame_from_response(&prompt_response) {
            if let Ok(mut update) = serde_json::from_value::<UsageUpdate>(frame.clone()) {
                let mut session = self.session.write().await;
                // Agents like OpenCode report through BOTH channels: a mid-turn
                // UsageUpdate notification carrying the real window size, and an
                // end-of-turn usage without one. Never let the sizeless end-of-turn
                // write clobber a window size the session already knows.
                preserve_known_window(&mut update, &mut frame, session.context_usage());
                session.apply_context_usage(update);
                self.commit_session_changes(&mut session).await;
            }
            self.runtime.emit(AgentStreamEvent::AcpContextUsage(frame));
        }

        // Drain the turn-scoped receiver once: detect both the empty-turn
        // condition and any CodeBuddy dialect signal (session_end / token
        // pressure) the tolerant transport absorbed during this turn. Because
        // `probe_rx` was subscribed just before this prompt and is drained here,
        // the signal is correlated strictly to the turn/near-window — signals
        // seen earlier in the session lifetime are never attributed here.
        let observations = drain_turn_observations(&mut probe_rx);
        let empty_turn = observations.empty;
        if empty_turn && let Some(error) = self.empty_turn_terminal_error().await {
            return Ok(PromptOutcome::TerminalError {
                session_id: sid.to_owned(),
                error,
            });
        }

        // On an empty end-of-turn, surface the agent's advertised login method
        // (if any). ACP defines no auth-failure stop reason, so an agent that
        // isn't signed in can swallow the failure and return `end_turn` with no
        // content (observed with kilo: the model call 401s, yet ACP reports a
        // clean empty turn). The stable `authMethods` from `initialize` lets us
        // turn that blank tip into an actionable "you may need to sign in" hint.
        let auth_hint = if empty_turn {
            auth_login_hint(self.session.read().await.auth_methods())
        } else {
            None
        };

        Ok(prompt_outcome_from_stop_reason(
            sid,
            prompt_response.stop_reason,
            empty_turn,
            matched_command,
            auth_hint,
            observations.dialect_signal,
        ))
    }

    /// Emit model/mode/config events from the session aggregate so the frontend
    /// receives the initial session state via WebSocket immediately after
    /// session creation or load.
    async fn emit_snapshot_events(&self) {
        use aionui_api_types::{ModelInfoEntry, ModelInfoPayload};

        let session = self.session.read().await;
        if let Some(models) = session.model_info() {
            let current_id = models.current_model_id.to_string();
            let available: Vec<ModelInfoEntry> = models
                .available_models
                .iter()
                .map(|am| ModelInfoEntry {
                    id: am.model_id.to_string(),
                    label: am.name.clone(),
                })
                .collect();
            let current_label = available
                .iter()
                .find(|e| e.id == current_id)
                .map(|e| e.label.clone())
                .unwrap_or_else(|| current_id.clone());
            let payload = ModelInfoPayload {
                current_model_id: Some(current_id),
                current_model_label: Some(current_label),
                available_models: available,
            };
            // ModelInfoPayload is our own struct but go through the
            // normaliser for consistency with sibling events.
            if let Some(v) = sdk_to_snake_value(&payload) {
                self.runtime.emit(AgentStreamEvent::AcpModelInfo(v));
            }
        }
        if let Some(modes) = session.modes()
            && let Some(v) = sdk_to_snake_value(&modes)
        {
            self.runtime.emit(AgentStreamEvent::AcpModeInfo(v));
        }
        if let Some(config_options) = session.config_options()
            && let Some(v) = sdk_to_snake_value(&serde_json::json!({
                "config_options": config_options,
            }))
        {
            // Wrap in `{config_options: [...]}` to match the SDK
            // `ConfigOptionUpdate` shape used by the streaming path —
            // handshake blobs and downstream consumers see a uniform
            // structure regardless of origin.
            self.runtime.emit(AgentStreamEvent::AcpConfigOption(v));
        }
        if let Some(cmds) = session.available_commands() {
            self.runtime
                .emit(AgentStreamEvent::AvailableCommands(AvailableCommandsEventData {
                    commands: cmds.to_vec(),
                }));
        }
    }

    async fn empty_turn_terminal_error(&self) -> Option<ErrorEventData> {
        let tail = self.process.peek_stderr_tail(STDERR_PEEK_LINES).await;
        let detail = super::stderr_error_extractor::extract_error_message(&tail)?;
        Some(classify_empty_turn_stderr_error(&detail))
    }
}

/// Drain the supplied turn-scoped receiver and return `true` when the turn
/// produced neither agent text nor any tool-call activity.
///
/// Used by `prompt_existing_session` to detect the "blank reply" scenario
/// (ELECTRON-1JG): the ACP backend returned `StopReason::EndTurn` (or
/// similar terminal reason) without ever emitting a `Text` /
/// `Thinking` / `ToolCall` / `AcpToolCall` chunk. We treat presence of
/// any of those as a non-empty turn.
///
/// `Lagged` is treated as non-empty: the broadcast buffer overflowed,
/// meaning many events flew by — definitely not an empty turn.
/// Thin wrapper retained for the existing empty-turn detection tests; the
/// production path now uses [`drain_turn_observations`] to observe the dialect
/// signal alongside emptiness in a single drain.
#[cfg(test)]
fn is_empty_turn(rx: &mut tokio::sync::broadcast::Receiver<AgentStreamEvent>) -> bool {
    drain_turn_observations(rx).empty
}

/// What a single drain of the turn-scoped receiver observed.
struct TurnObservations {
    /// The turn produced no user-visible output (see `is_empty_turn`).
    empty: bool,
    /// The tolerant transport absorbed at least one CodeBuddy dialect signal
    /// (`session_end` or token-pressure/compaction) during this turn/near-window.
    dialect_signal: bool,
}

/// Drain the turn-scoped receiver once, recording both the empty-turn condition
/// and whether any `AcpDialectSignal` arrived. Preserves `is_empty_turn`'s
/// original semantics for `empty` (visible output → not empty; `Lagged` → not
/// empty) while additionally surfacing the dialect signal so the empty-turn
/// judgment can prefer accurate token-limit attribution over the auth hint.
fn drain_turn_observations(rx: &mut tokio::sync::broadcast::Receiver<AgentStreamEvent>) -> TurnObservations {
    let mut empty = true;
    let mut dialect_signal = false;
    loop {
        match rx.try_recv() {
            Ok(AgentStreamEvent::AcpDialectSignal(_)) => dialect_signal = true,
            Ok(event) => {
                if event_is_user_visible_output(&event) {
                    empty = false;
                }
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
            // Buffer overflow: many events occurred — turn was clearly not empty.
            // Keep draining so a dialect signal later in the buffer is still seen.
            Err(TryRecvError::Lagged(_)) => empty = false,
        }
    }
    TurnObservations { empty, dialect_signal }
}

/// Whether a stream event represents user-visible output produced by the
/// model during a turn. Anything that would render in chat counts.
fn event_is_user_visible_output(event: &AgentStreamEvent) -> bool {
    matches!(
        event,
        AgentStreamEvent::Text(_)
            | AgentStreamEvent::Thinking(_)
            | AgentStreamEvent::ToolCall(_)
            | AgentStreamEvent::AcpToolCall(_)
            | AgentStreamEvent::ToolGroup(_)
            | AgentStreamEvent::Plan(_)
            | AgentStreamEvent::Permission(_)
            | AgentStreamEvent::AcpPermission(_)
    )
}

/// Build the `AcpContextUsage` frame value from an end-of-turn usage report.
///
/// The shape mirrors the `UsageUpdate` notification passthrough (`{used,
/// size}`) so the event tracker can persist it into the session snapshot.
/// The per-turn counters ride inside `_meta` — a `UsageUpdate` field — so
/// they survive the typed round-trip into the snapshot and come back out of
/// GET /usage; top-level extra keys would be dropped by deserialization.
/// `size: 0` means "context window unknown" — the frontend only applies
/// sizes > 0.
fn end_turn_usage_frame(usage: &Usage) -> Value {
    let mut breakdown = serde_json::json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
    });
    if let Some(thought) = usage.thought_tokens {
        breakdown["thought_tokens"] = thought.into();
    }
    if let Some(read) = usage.cached_read_tokens {
        breakdown["cached_read_tokens"] = read.into();
    }
    if let Some(write) = usage.cached_write_tokens {
        breakdown["cached_write_tokens"] = write.into();
    }
    serde_json::json!({
        "used": usage.total_tokens,
        "size": 0,
        "_meta": breakdown,
    })
}

/// Extract an end-of-turn usage frame from a prompt response.
///
/// Sources, in priority order:
/// 1. the SDK's unstable `usage` field (`unstable_end_turn_token_usage`);
/// 2. the gemini-cli dialect: `_meta.quota.token_count`
///    (`{input_tokens, output_tokens}` — input is the full context sent this
///    turn, so input+output approximates current context occupancy).
fn end_turn_usage_frame_from_response(response: &PromptResponse) -> Option<Value> {
    if let Some(usage) = response.usage.as_ref().filter(|u| u.total_tokens > 0) {
        return Some(end_turn_usage_frame(usage));
    }
    let counts = response.meta.as_ref()?.get("quota")?.get("token_count")?;
    let input = counts.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output = counts.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
    let used = input + output;
    if used == 0 {
        return None;
    }
    Some(serde_json::json!({
        "used": used,
        "size": 0,
        "_meta": { "input_tokens": input, "output_tokens": output },
    }))
}

/// Carry previously-reported fields the end-of-turn write doesn't know —
/// the context window size and the cumulative session cost — into a
/// partial end-of-turn usage update (both the snapshot value and the wire
/// frame), so the frontend denominator and cost survive hydration.
fn preserve_known_window(update: &mut UsageUpdate, frame: &mut Value, existing: Option<&UsageUpdate>) {
    let Some(known) = existing else {
        return;
    };
    if update.size == 0 && known.size > 0 {
        update.size = known.size;
        frame["size"] = known.size.into();
    }
    if update.cost.is_none()
        && let Some(cost) = known.cost.as_ref()
    {
        update.cost = Some(cost.clone());
        frame["cost"] = serde_json::to_value(cost).unwrap_or_default();
    }
}

fn prompt_outcome_from_stop_reason(
    session_id: &str,
    stop_reason: StopReason,
    empty_turn: bool,
    _matched_command: Option<&SlashCommandItem>,
    auth_hint: Option<Value>,
    token_limit_signal: bool,
) -> PromptOutcome {
    if matches!(stop_reason, StopReason::Cancelled) {
        return PromptOutcome::Cancelled {
            session_id: session_id.to_owned(),
        };
    }

    if empty_turn {
        if matches!(stop_reason, StopReason::EndTurn) {
            // Priority 2 (B): a CodeBuddy dialect signal observed in this turn /
            // near-window (session_end or emergency compaction / token pressure)
            // attributes the empty turn to a likely context/token limit — taken
            // *before* the auth hint so the accurate cause wins. Possibility
            // wording only: the exact upstream cause is cross-boundary and not
            // asserted here.
            if token_limit_signal {
                return PromptOutcome::InfoTip {
                    session_id: session_id.to_owned(),
                    tips: empty_turn_info_tip("ACP_EMPTY_TURN_TOKEN_LIMIT", None),
                };
            }
            // The agent ended the turn producing nothing. If it advertised a
            // login method at initialize, the most likely cause is that it
            // isn't signed in and silently returned an empty end_turn — point
            // the user at the login step instead of a blank tip. (Presence of
            // authMethods is not proof of being logged out, so the client copy
            // must stay a soft "may need sign-in" hint, not an assertion.)
            if let Some(params) = auth_hint {
                return PromptOutcome::InfoTip {
                    session_id: session_id.to_owned(),
                    tips: empty_turn_info_tip("ACP_EMPTY_TURN_NEEDS_AUTH", Some(params)),
                };
            }
            return PromptOutcome::InfoTip {
                session_id: session_id.to_owned(),
                tips: empty_turn_info_tip("ACP_EMPTY_TURN", None),
            };
        }

        return PromptOutcome::WarningTip {
            session_id: session_id.to_owned(),
            tips: empty_finish_diagnostic_tip(stop_reason),
        };
    }

    PromptOutcome::Completed {
        session_id: session_id.to_owned(),
    }
}

/// Build empty-turn tip params from the agent's advertised ACP auth methods,
/// or `None` when the agent advertised none. `hint` is the first
/// human-readable description/name (e.g. "Run `kilo auth login` in the
/// terminal"); `methods` carries the full advertised list so the client can
/// render every option. Uses the stable `authMethods` from the `initialize`
/// handshake — no agent-specific flags or stderr scraping.
fn auth_login_hint(methods: Option<&[AuthMethod]>) -> Option<Value> {
    let methods = methods?;
    let entries: Vec<Value> = methods.iter().filter_map(|m| serde_json::to_value(m).ok()).collect();
    if entries.is_empty() {
        return None;
    }
    let hint = entries.iter().find_map(|m| {
        m.get("description")
            .and_then(Value::as_str)
            .or_else(|| m.get("name").and_then(Value::as_str))
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    });
    Some(serde_json::json!({ "methods": entries, "hint": hint }))
}

fn empty_turn_info_tip(code: &str, params: Option<Value>) -> TipsEventData {
    TipsEventData {
        content: String::new(),
        tip_type: TipType::Info,
        code: Some(code.to_owned()),
        params,
    }
}

fn empty_finish_diagnostic_tip(stop_reason: StopReason) -> TipsEventData {
    TipsEventData {
        content: String::new(),
        tip_type: TipType::Warning,
        code: Some(empty_finish_tip_code(stop_reason).to_owned()),
        params: None,
    }
}

fn classify_empty_turn_stderr_error(detail: &str) -> ErrorEventData {
    AgentSendError::from_agent_error(AgentError::bad_gateway(detail.to_owned())).into_stream_error()
}

fn empty_finish_tip_code(stop_reason: StopReason) -> &'static str {
    match stop_reason {
        StopReason::MaxTokens => "ACP_EMPTY_TURN_MAX_TOKENS",
        StopReason::MaxTurnRequests => "ACP_EMPTY_TURN_MAX_TURN_REQUESTS",
        StopReason::Refusal => "ACP_EMPTY_TURN_REFUSAL",
        _ => "ACP_EMPTY_TURN",
    }
}

#[cfg(test)]
mod tests {
    //! Contract tests for the post-`warmup_session` session invariant.
    //!
    //! The integration-test harness in `tests/acp_agent_integration.rs`
    //! cannot drive `AcpAgentManager` through a JSON-RPC mock today (all
    //! existing ACP tests there are `#[ignore]` for the same reason), so we
    //! pin the observable contract at the aggregate-root layer instead:
    //! whatever `warmup_session` does internally, the session aggregate
    //! must end up with `is_opened() == true` and a populated
    //! `session_id()` — the same terminal state the real `open_session_new`
    //! / `open_session_resume` helpers leave behind.
    use crate::manager::acp::{AcpSession, AcpSessionEvent};
    use crate::protocol::error::AcpError;
    use crate::shared_kernel::SessionId as DomainSessionId;
    use agent_client_protocol::schema::v1::{AgentCapabilities, Cost, PromptResponse, Usage, UsageUpdate};

    use super::{end_turn_usage_frame, end_turn_usage_frame_from_response, preserve_known_window};

    /// The end-of-turn usage frame must stay deserializable as `UsageUpdate` —
    /// that is the contract with `agent_event_tracker`, which persists the
    /// frame into the session snapshot (and thus GET /usage) via
    /// `from_value::<UsageUpdate>`. If this breaks, the indicator still lights
    /// up live but silently stops surviving hydration.
    #[test]
    fn end_turn_usage_frame_is_snapshot_compatible() {
        let frame = end_turn_usage_frame(&Usage::new(1200, 1000, 200));

        let update: UsageUpdate = serde_json::from_value(frame.clone()).expect("frame must parse as UsageUpdate");
        assert_eq!(update.used, 1200);
        assert_eq!(
            update.size, 0,
            "unknown context window must serialize as 0, not be omitted"
        );
        assert_eq!(frame["_meta"]["input_tokens"], 1000);
        assert_eq!(frame["_meta"]["output_tokens"], 200);

        // The breakdown must survive the typed round-trip — `_meta` is a real
        // UsageUpdate field, so the snapshot (and GET /usage) keeps it.
        let round_tripped = serde_json::to_value(&update).expect("serialize");
        assert_eq!(round_tripped["_meta"]["input_tokens"], 1000);
        assert_eq!(round_tripped["_meta"]["output_tokens"], 200);
    }

    #[test]
    fn end_turn_usage_frame_includes_optional_counters_only_when_reported() {
        let bare = end_turn_usage_frame(&Usage::new(10, 6, 4));
        assert!(bare["_meta"].get("thought_tokens").is_none());
        assert!(bare["_meta"].get("cached_read_tokens").is_none());

        let full = end_turn_usage_frame(
            &Usage::new(10, 6, 4)
                .thought_tokens(3)
                .cached_read_tokens(2)
                .cached_write_tokens(1),
        );
        assert_eq!(full["_meta"]["thought_tokens"], 3);
        assert_eq!(full["_meta"]["cached_read_tokens"], 2);
        assert_eq!(full["_meta"]["cached_write_tokens"], 1);
    }

    /// gemini-cli reports token counts in `_meta.quota.token_count` instead of
    /// the unstable `usage` field (observed on gemini-cli via a raw ACP stdio
    /// probe, 2026-07-29). `input_tokens` there is the full context sent this
    /// turn, so input+output is the context-occupancy figure the indicator
    /// wants.
    #[test]
    fn end_turn_usage_falls_back_to_gemini_quota_meta() {
        let meta = serde_json::json!({
            "quota": {
                "token_count": { "input_tokens": 19769, "output_tokens": 4 },
                "model_usage": [],
            }
        });
        let response = PromptResponse::new(StopReason::EndTurn).meta(meta.as_object().cloned().expect("meta object"));

        let frame = end_turn_usage_frame_from_response(&response).expect("quota dialect must produce a frame");
        assert_eq!(frame["used"], 19773);
        assert_eq!(frame["_meta"]["input_tokens"], 19769);
        assert_eq!(frame["_meta"]["output_tokens"], 4);

        let update: UsageUpdate = serde_json::from_value(frame).expect("frame must parse as UsageUpdate");
        assert_eq!(update.used, 19773);
    }

    #[test]
    fn end_turn_usage_prefers_the_typed_usage_field_over_meta() {
        let meta = serde_json::json!({
            "quota": { "token_count": { "input_tokens": 1, "output_tokens": 1 } }
        });
        let response = PromptResponse::new(StopReason::EndTurn)
            .usage(Usage::new(500, 400, 100))
            .meta(meta.as_object().cloned().expect("meta object"));

        let frame = end_turn_usage_frame_from_response(&response).expect("typed usage must win");
        assert_eq!(frame["used"], 500);
    }

    #[test]
    fn end_turn_usage_is_absent_for_responses_without_any_usage_report() {
        let bare = PromptResponse::new(StopReason::EndTurn);
        assert!(end_turn_usage_frame_from_response(&bare).is_none());

        let empty_counts = serde_json::json!({
            "quota": { "token_count": { "input_tokens": 0, "output_tokens": 0 } }
        });
        let zero = PromptResponse::new(StopReason::EndTurn).meta(empty_counts.as_object().cloned().unwrap());
        assert!(end_turn_usage_frame_from_response(&zero).is_none());
    }

    /// OpenCode regression: a mid-turn UsageUpdate notification stores the
    /// real window size in the snapshot; the sizeless end-of-turn write must
    /// inherit it instead of resetting it to 0 (which hid the frontend ring
    /// after every reload).
    #[test]
    fn sizeless_end_turn_write_preserves_the_known_window_size() {
        let existing = UsageUpdate::new(12_600, 262_144);
        let mut frame = serde_json::json!({ "used": 13_000, "size": 0 });
        let mut update: UsageUpdate = serde_json::from_value(frame.clone()).unwrap();

        preserve_known_window(&mut update, &mut frame, Some(&existing));

        assert_eq!(update.size, 262_144);
        assert_eq!(frame["size"], 262_144);
        assert_eq!(update.used, 13_000, "the fresh counter must still win");
    }

    #[test]
    fn end_turn_write_keeps_its_own_size_when_nothing_better_is_known() {
        let mut frame = serde_json::json!({ "used": 10, "size": 0 });
        let mut update: UsageUpdate = serde_json::from_value(frame.clone()).unwrap();

        preserve_known_window(&mut update, &mut frame, None);
        assert_eq!(update.size, 0);

        // An agent-reported size on the fresh update must never be replaced.
        let mut sized_frame = serde_json::json!({ "used": 10, "size": 4096 });
        let mut sized: UsageUpdate = serde_json::from_value(sized_frame.clone()).unwrap();
        let stale = UsageUpdate::new(5, 262_144);
        preserve_known_window(&mut sized, &mut sized_frame, Some(&stale));
        assert_eq!(sized.size, 4096);
    }

    /// A cumulative session cost reported by a mid-turn UsageUpdate must not
    /// be dropped by the costless end-of-turn write — same clobber class as
    /// the window size.
    #[test]
    fn costless_end_turn_write_preserves_the_known_session_cost() {
        let existing = UsageUpdate::new(12_600, 262_144).cost(Cost::new(0.42, "USD"));
        let mut frame = serde_json::json!({ "used": 13_000, "size": 0 });
        let mut update: UsageUpdate = serde_json::from_value(frame.clone()).unwrap();

        preserve_known_window(&mut update, &mut frame, Some(&existing));

        assert_eq!(update.cost.as_ref().map(|c| c.amount), Some(0.42));
        assert_eq!(frame["cost"]["amount"], 0.42);
        assert_eq!(frame["cost"]["currency"], "USD");

        // A fresh agent-reported cost must never be replaced by a stale one.
        let mut priced_frame =
            serde_json::json!({ "used": 10, "size": 0, "cost": { "amount": 0.5, "currency": "USD" } });
        let mut priced: UsageUpdate = serde_json::from_value(priced_frame.clone()).unwrap();
        preserve_known_window(&mut priced, &mut priced_frame, Some(&existing));
        assert_eq!(priced.cost.as_ref().map(|c| c.amount), Some(0.5));
    }

    fn make_session() -> AcpSession {
        AcpSession::new(None, None, Default::default())
    }

    /// `open_session_resume` reads `session.agent_capabilities().load_session`
    /// to decide between `session/load` and the legacy seed-and-pray path.
    /// Reading from the SDK-typed advertised capabilities (instead of poking
    /// at the persisted handshake JSON) is the contract that ELECTRON-1HQ
    /// regressed against — OpenCode advertises `loadSession: true` on the
    /// wire, the SDK exposes it as `load_session: true`, but the old code
    /// looked up the snake-cased key in a JSON blob that hadn't always been
    /// written yet. Pin the contract: once the CLI has handshaken, the
    /// advertised slot must be populated and read back as the source of
    /// truth.
    #[test]
    fn advertised_capabilities_drives_supports_session_load() {
        let mut session = make_session();
        assert!(
            session.agent_capabilities().is_none(),
            "precondition: capabilities unset until init handshake completes"
        );

        // After `apply_advertised_capabilities` the resume path can answer
        // the question without consulting the persisted catalog row.
        let mut caps = AgentCapabilities::new();
        caps.load_session = true;
        session.apply_advertised_capabilities(caps);

        let supports_load = session.agent_capabilities().map(|c| c.load_session).unwrap_or(false);
        assert!(
            supports_load,
            "OpenCode-style `loadSession: true` handshake must enable session/load"
        );
    }

    #[test]
    fn missing_capability_means_no_session_load() {
        let session = make_session();
        let supports_load = session.agent_capabilities().map(|c| c.load_session).unwrap_or(false);
        assert!(
            !supports_load,
            "without an init handshake the resume path must not call session/load"
        );
    }

    #[test]
    fn capability_load_session_false_means_no_session_load() {
        let mut session = make_session();
        let caps = AgentCapabilities::new();
        // Default is load_session = false; assert reading it back agrees.
        session.apply_advertised_capabilities(caps);
        let supports_load = session.agent_capabilities().map(|c| c.load_session).unwrap_or(false);
        assert!(!supports_load);
    }

    /// Simulate the aggregate-state effect of a successful warmup that
    /// took the "open new session" path: `open_session_new` calls
    /// `set_session_id`, the outer `ensure_session_opened` then calls
    /// `mark_opened`. Post-state must satisfy both invariants so the
    /// follow-up `PUT /mode` / `PUT /model` can reconcile without
    /// re-opening.
    #[test]
    fn warmup_success_marks_session_opened_with_sid() {
        let mut session = make_session();
        assert!(!session.is_opened(), "precondition: session starts unopened");
        assert!(session.session_id().is_none(), "precondition: no sid yet");

        // open_session_new assigns the CLI-issued sid
        session.set_session_id(DomainSessionId::new("sess-warm-1"));
        // ensure_session_opened marks opened after the protocol call returns
        session.mark_opened();

        assert!(session.is_opened(), "warmup must leave session opened");
        assert_eq!(
            session.session_id(),
            Some("sess-warm-1"),
            "warmup must leave session id populated"
        );

        let events = session.drain_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AcpSessionEvent::SessionAssigned { .. })),
            "warmup must emit SessionAssigned for the persistence consumer"
        );
        assert!(
            events.iter().any(|e| matches!(e, AcpSessionEvent::SessionOpened)),
            "warmup must emit SessionOpened exactly once"
        );
    }

    /// When warmup encounters an already-opened session (e.g. called a
    /// second time on a warm agent), it must be a no-op — no duplicate
    /// `SessionOpened` event, sid preserved.
    #[test]
    fn warmup_on_opened_session_is_idempotent() {
        let mut session = make_session();
        session.set_session_id(DomainSessionId::new("sess-warm-2"));
        session.mark_opened();
        let _ = session.drain_events();

        // Second warmup call path: ensure_session_opened sees
        // (Some(sid), true) → no open_session_* call, but still flips
        // mark_opened (idempotent on the aggregate side).
        session.mark_opened();

        assert!(session.is_opened());
        assert_eq!(session.session_id(), Some("sess-warm-2"));
        assert!(
            session.drain_events().is_empty(),
            "second warmup must not emit duplicate domain events"
        );
    }

    /// `rebuild_after_session_not_found` relies on `clear_session_id`
    /// resetting both the sid and the `opened` flag, so the subsequent
    /// `ensure_session_opened` re-enters the `(None, _)` branch and
    /// calls `open_session_new`. Pin both invariants — without the
    /// `opened = false` reset, the rescue path would land in the
    /// `(Some, true)` no-op branch and the next prompt would still hit
    /// the dead session.
    #[test]
    fn clear_session_id_resets_sid_and_opened() {
        let mut session = make_session();
        session.set_session_id(DomainSessionId::new("ses-stale"));
        session.mark_opened();
        assert!(session.is_opened());
        assert_eq!(session.session_id(), Some("ses-stale"));

        session.clear_session_id();

        assert_eq!(session.session_id(), None, "stale sid must be dropped");
        assert!(
            !session.is_opened(),
            "rebuild requires re-running open_session_new — opened must reset"
        );
    }

    /// The `is_acp_session_not_found` discriminator powers
    /// `open_session_resume`'s rescue path. Match strictly on the
    /// structured `AcpError::SessionNotFound` variant; other ACP failures
    /// must surface to callers instead of triggering a phantom session
    /// rebuild.
    #[test]
    fn is_acp_session_not_found_matches_session_not_found_only() {
        let session_err = AcpError::SessionNotFound {
            session_id: "ses-1".into(),
        };
        assert!(super::is_acp_session_not_found(&session_err));

        let invalid_params = AcpError::InvalidParams {
            message: "Workspace not found".into(),
        };
        assert!(!super::is_acp_session_not_found(&invalid_params));

        let auth_required = AcpError::AuthRequired;
        assert!(!super::is_acp_session_not_found(&auth_required));
    }

    #[test]
    fn resumed_session_resource_not_found_matches_only_the_exact_session_id() {
        let exact = AcpError::ResourceNotFound {
            resource: Some("ses-resume".into()),
            message: "missing".into(),
        };
        assert!(super::is_missing_resumed_session(&exact, "ses-resume"));

        let unrelated = AcpError::ResourceNotFound {
            resource: Some("/workspace/file.txt".into()),
            message: "missing".into(),
        };
        assert!(!super::is_missing_resumed_session(&unrelated, "ses-resume"));

        let unspecified = AcpError::ResourceNotFound {
            resource: None,
            message: "missing".into(),
        };
        assert!(!super::is_missing_resumed_session(&unspecified, "ses-resume"));

        let structured = AcpError::SessionNotFound {
            session_id: "different-wire-id".into(),
        };
        assert!(super::is_missing_resumed_session(&structured, "ses-resume"));
    }

    // -- empty-finish diagnostic (ELECTRON-1JG) -------------------------------

    use crate::protocol::events::{
        AgentStreamEvent, FinishEventData, StartEventData, TextEventData, ThinkingEventData, TipType,
        ToolCallEventData, ToolCallStatus,
    };
    use agent_client_protocol::schema::v1::StopReason;
    use aionui_api_types::{AgentErrorCode, SlashCommandCompletionBehavior, SlashCommandItem};
    use tokio::sync::broadcast;

    /// Lifecycle-only events (`Start`/`Finish`) must NOT count as
    /// user-visible output. This is the core empty-finish detection
    /// contract: the helper has to look past Start before declaring
    /// the turn empty.
    #[tokio::test]
    async fn is_empty_turn_returns_true_when_only_lifecycle_events() {
        let (tx, _) = broadcast::channel::<AgentStreamEvent>(8);
        let mut rx = tx.subscribe();
        tx.send(AgentStreamEvent::Start(StartEventData {
            session_id: Some("s1".into()),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData {
            session_id: Some("s1".into()),
        }))
        .unwrap();

        assert!(super::is_empty_turn(&mut rx));
    }

    /// A single Text chunk is enough to mark the turn non-empty,
    /// even when sandwiched between lifecycle events.
    #[tokio::test]
    async fn is_empty_turn_returns_false_when_text_emitted() {
        let (tx, _) = broadcast::channel::<AgentStreamEvent>(8);
        let mut rx = tx.subscribe();
        tx.send(AgentStreamEvent::Start(StartEventData::default())).unwrap();
        tx.send(AgentStreamEvent::Text(TextEventData { content: "hi".into() }))
            .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        assert!(!super::is_empty_turn(&mut rx));
    }

    /// Tool calls also count as visible output — even if the model
    /// produced no Text, executing a tool means the turn was not blank.
    #[tokio::test]
    async fn is_empty_turn_returns_false_when_tool_call_emitted() {
        let (tx, _) = broadcast::channel::<AgentStreamEvent>(8);
        let mut rx = tx.subscribe();
        tx.send(AgentStreamEvent::Start(StartEventData::default())).unwrap();
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "c1".into(),
            name: "read_file".into(),
            args: serde_json::json!({}),
            status: ToolCallStatus::Running,
            input: None,
            output: None,
            description: None,
        }))
        .unwrap();

        assert!(!super::is_empty_turn(&mut rx));
    }

    /// Thinking-only output (no final reply) still counts: the user
    /// saw something happen, even though the model didn't commit
    /// to a response. We don't want to double-up the diagnostic.
    #[tokio::test]
    async fn is_empty_turn_returns_false_when_only_thinking_emitted() {
        let (tx, _) = broadcast::channel::<AgentStreamEvent>(8);
        let mut rx = tx.subscribe();
        tx.send(AgentStreamEvent::Thinking(ThinkingEventData {
            content: "hmm".into(),
            subject: None,
            duration: None,
            status: None,
        }))
        .unwrap();

        assert!(!super::is_empty_turn(&mut rx));
    }

    /// Each empty-finish stop reason maps to a stable tip code so the UI can
    /// own the final localized copy.
    #[test]
    fn empty_finish_tip_code_per_stop_reason() {
        assert_eq!(super::empty_finish_tip_code(StopReason::EndTurn), "ACP_EMPTY_TURN");
        assert_eq!(
            super::empty_finish_tip_code(StopReason::MaxTokens),
            "ACP_EMPTY_TURN_MAX_TOKENS"
        );
        assert_eq!(
            super::empty_finish_tip_code(StopReason::MaxTurnRequests),
            "ACP_EMPTY_TURN_MAX_TURN_REQUESTS"
        );
        assert_eq!(
            super::empty_finish_tip_code(StopReason::Refusal),
            "ACP_EMPTY_TURN_REFUSAL"
        );
    }

    #[test]
    fn empty_finish_diagnostic_tip_is_warning() {
        let tip = super::empty_finish_diagnostic_tip(StopReason::EndTurn);
        assert_eq!(tip.tip_type, TipType::Warning);
        assert_eq!(tip.content, "");
        assert_eq!(tip.code.as_deref(), Some("ACP_EMPTY_TURN"));
    }

    #[test]
    fn benign_empty_turn_returns_info_tip() {
        let outcome = super::prompt_outcome_from_stop_reason("sess-1", StopReason::EndTurn, true, None, None, false);

        match outcome {
            super::PromptOutcome::InfoTip { session_id, tips } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(tips.tip_type, TipType::Info);
                assert_eq!(tips.code.as_deref(), Some("ACP_EMPTY_TURN"));
                assert_eq!(tips.content, "");
            }
            other => panic!("expected InfoTip, got {other:?}"),
        }
    }

    #[test]
    fn empty_turn_with_auth_methods_emits_needs_auth_hint() {
        // Mirrors the kilo case: the agent advertised a login method at
        // `initialize` and then returned an empty end_turn (swallowed 401).
        let auth_hint = serde_json::json!({
            "methods": [{"id": "kilo-login", "name": "Login with Kilo",
                         "description": "Run `kilo auth login` in the terminal"}],
            "hint": "Run `kilo auth login` in the terminal",
        });
        let outcome =
            super::prompt_outcome_from_stop_reason("sess-1", StopReason::EndTurn, true, None, Some(auth_hint), false);

        match outcome {
            super::PromptOutcome::InfoTip { session_id, tips } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(tips.tip_type, TipType::Info);
                assert_eq!(tips.code.as_deref(), Some("ACP_EMPTY_TURN_NEEDS_AUTH"));
                let params = tips.params.expect("needs-auth tip carries params");
                assert_eq!(params["hint"], "Run `kilo auth login` in the terminal");
                assert!(params["methods"].is_array());
            }
            other => panic!("expected InfoTip, got {other:?}"),
        }
    }

    #[test]
    fn auth_login_hint_returns_none_without_methods() {
        assert!(super::auth_login_hint(None).is_none());
        assert!(super::auth_login_hint(Some(&[])).is_none());
    }

    // -- token-limit signal priority (issue 136586749) ------------------------

    /// B priority: a CodeBuddy dialect signal (session_end / compaction) observed
    /// in the turn attributes the empty turn to a likely context limit — taken
    /// *before* the auth hint even when authMethods were advertised.
    #[test]
    fn empty_end_turn_with_token_signal_prefers_token_limit_over_auth_hint() {
        let auth_hint = serde_json::json!({
            "methods": [{"id": "iOA", "name": "Login with iOA"}],
            "hint": "Login with iOA",
        });
        let outcome =
            super::prompt_outcome_from_stop_reason("sess-1", StopReason::EndTurn, true, None, Some(auth_hint), true);
        match outcome {
            super::PromptOutcome::InfoTip { tips, .. } => {
                assert_eq!(tips.code.as_deref(), Some("ACP_EMPTY_TURN_TOKEN_LIMIT"));
                assert_eq!(tips.tip_type, TipType::Info);
                assert_eq!(
                    tips.params, None,
                    "token-limit tip is a possibility hint, no hint params"
                );
            }
            other => panic!("expected InfoTip, got {other:?}"),
        }
    }

    /// The reported issue's clean empty end_turn: authMethods advertised but NO
    /// dialect signal in the turn → needs-auth fallback (its copy is softened in
    /// the UI). Guards against mis-attributing this case to a token limit.
    #[test]
    fn empty_end_turn_without_token_signal_but_auth_hint_falls_back_to_needs_auth() {
        let auth_hint = serde_json::json!({
            "methods": [{"id": "iOA", "name": "Login with iOA"}],
            "hint": "Login with iOA",
        });
        let outcome =
            super::prompt_outcome_from_stop_reason("sess-1", StopReason::EndTurn, true, None, Some(auth_hint), false);
        match outcome {
            super::PromptOutcome::InfoTip { tips, .. } => {
                assert_eq!(tips.code.as_deref(), Some("ACP_EMPTY_TURN_NEEDS_AUTH"));
            }
            other => panic!("expected InfoTip, got {other:?}"),
        }
    }

    #[test]
    fn empty_end_turn_with_token_signal_and_no_auth_hint_is_token_limit() {
        let outcome = super::prompt_outcome_from_stop_reason("sess-1", StopReason::EndTurn, true, None, None, true);
        match outcome {
            super::PromptOutcome::InfoTip { tips, .. } => {
                assert_eq!(tips.code.as_deref(), Some("ACP_EMPTY_TURN_TOKEN_LIMIT"));
            }
            other => panic!("expected InfoTip, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drain_turn_observations_flags_dialect_signal_on_empty_turn() {
        use crate::protocol::events::{AcpDialectSignalData, AcpDialectSignalKind};
        let (tx, _) = broadcast::channel::<AgentStreamEvent>(8);
        let mut rx = tx.subscribe();
        tx.send(AgentStreamEvent::Start(StartEventData::default())).unwrap();
        tx.send(AgentStreamEvent::AcpDialectSignal(AcpDialectSignalData {
            kind: AcpDialectSignalKind::SessionEnd,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let obs = super::drain_turn_observations(&mut rx);
        assert!(obs.empty, "a dialect signal is not user-visible output");
        assert!(obs.dialect_signal, "session_end signal must be flagged");
    }

    #[tokio::test]
    async fn drain_turn_observations_no_signal_when_only_lifecycle() {
        let (tx, _) = broadcast::channel::<AgentStreamEvent>(8);
        let mut rx = tx.subscribe();
        tx.send(AgentStreamEvent::Start(StartEventData::default())).unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let obs = super::drain_turn_observations(&mut rx);
        assert!(obs.empty);
        assert!(!obs.dialect_signal);
    }

    #[tokio::test]
    async fn drain_turn_observations_text_makes_turn_non_empty() {
        let (tx, _) = broadcast::channel::<AgentStreamEvent>(8);
        let mut rx = tx.subscribe();
        tx.send(AgentStreamEvent::Text(TextEventData { content: "hi".into() }))
            .unwrap();

        let obs = super::drain_turn_observations(&mut rx);
        assert!(!obs.empty);
        assert!(!obs.dialect_signal);
    }

    #[test]
    fn dialect_signal_is_not_user_visible_output() {
        use crate::protocol::events::{AcpDialectSignalData, AcpDialectSignalKind};
        assert!(!super::event_is_user_visible_output(
            &AgentStreamEvent::AcpDialectSignal(AcpDialectSignalData {
                kind: AcpDialectSignalKind::TokenPressure,
            })
        ));
    }

    #[test]
    fn metadata_driven_command_empty_turn_uses_generic_tip_code() {
        let command = SlashCommandItem {
            command: "ctx-flush".into(),
            description: "Flush context".into(),
            completion_behavior: Some(SlashCommandCompletionBehavior::NeutralTipOnEmpty),
            empty_turn_tip_code: Some("ACP_CTX_FLUSH_COMPLETED".into()),
            empty_turn_tip_params: Some(serde_json::json!({ "scope": "session" })),
        };

        let outcome =
            super::prompt_outcome_from_stop_reason("sess-1", StopReason::EndTurn, true, Some(&command), None, false);

        match outcome {
            super::PromptOutcome::InfoTip { session_id, tips } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(tips.tip_type, TipType::Info);
                assert_eq!(tips.code.as_deref(), Some("ACP_EMPTY_TURN"));
                assert_eq!(tips.content, "");
                assert_eq!(tips.params, None);
            }
            other => panic!("expected InfoTip, got {other:?}"),
        }
    }

    #[test]
    fn non_benign_empty_turn_can_stay_warning_tip() {
        let outcome = super::prompt_outcome_from_stop_reason("sess-1", StopReason::MaxTokens, true, None, None, false);

        match outcome {
            super::PromptOutcome::WarningTip { session_id, tips } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(tips.tip_type, TipType::Warning);
                assert_eq!(tips.content, "");
                assert_eq!(tips.code.as_deref(), Some("ACP_EMPTY_TURN_MAX_TOKENS"));
            }
            other => panic!("expected WarningTip, got {other:?}"),
        }
    }

    #[test]
    fn prompt_outcome_cancelled_takes_priority_over_empty_response() {
        let outcome = super::prompt_outcome_from_stop_reason("sess-1", StopReason::Cancelled, true, None, None, false);

        match outcome {
            super::PromptOutcome::Cancelled { session_id } => {
                assert_eq!(session_id, "sess-1");
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[test]
    fn prompt_outcome_completed_when_visible_output_exists() {
        let outcome = super::prompt_outcome_from_stop_reason("sess-1", StopReason::EndTurn, false, None, None, false);

        match outcome {
            super::PromptOutcome::Completed { session_id } => {
                assert_eq!(session_id, "sess-1");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn classify_empty_turn_stderr_error_preserves_provider_billing_failure() {
        let error = super::classify_empty_turn_stderr_error("HTTP 402: Insufficient account balance");

        assert_eq!(error.code, Some(AgentErrorCode::UserLlmProviderBillingRequired));
        assert_eq!(error.retryable, Some(false));
        assert_eq!(error.feedback_recommended, Some(false));
    }

    #[test]
    fn classify_real_402_empty_turn_sentry_tail_preserves_provider_billing_failure() {
        let stderr = "[warn] CLI process stderr stderr=\"2026-06-05 03:27:44 [INFO] agent.chat_completion_helpers: Streaming failed before delivery: Error code: 402 - {'error': {'code': '402', 'message': 'Insufficient account balance', 'type': 'insufficient_balance'}}\"\n\
                      [warn] CLI process stderr stderr=\"💡 xiaomi reported that billing, credits, or account entitlement is exhausted for mimo-v2.5-pro.\"\n\
                      [warn] CLI process stderr stderr=\"💡 Add credits or update billing with that provider, then retry.\"\n\
                      [warn] CLI process stderr stderr=\"2026-06-05 03:27:44 [ERROR] agent.conversation_loop: Non-retryable client error: Error code: 402 - {'error': {'code': '402', 'message': 'Insufficient account balance', 'type': 'insufficient_balance'}}\"";
        let detail = super::super::stderr_error_extractor::extract_error_message(stderr)
            .expect("sample tail must surface a billing hint");
        let error = super::classify_empty_turn_stderr_error(&detail);

        assert_eq!(error.code, Some(AgentErrorCode::UserLlmProviderBillingRequired));
        assert_eq!(error.retryable, Some(false));
        assert_eq!(error.feedback_recommended, Some(false));
    }
}
