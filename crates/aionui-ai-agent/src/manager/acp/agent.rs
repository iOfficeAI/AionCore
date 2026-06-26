use crate::agent_runtime::AgentRuntime;
use crate::capability::PromptCtx;
use crate::capability::cli_process::CliAgentProcess;
use crate::capability::prompt_pipeline::PromptPipeline;
use crate::capability::skill_manager::AcpSkillManager;
use crate::error::AgentError;
use crate::factory::acp_assembler::AcpSessionParams;
use crate::manager::acp::{AcpSession, AcpSessionEvent, PermissionRouter, SessionNewPreludeHook};
use crate::manager::process_registry::{register_session_process, unregister_agent_process};
use crate::protocol::acp::AcpProtocol;
use crate::protocol::error::{AcpError, CloseReason};
use crate::protocol::events::AgentStreamEvent;
use crate::protocol::send_error::AgentSendError;
use crate::registry::CatalogSender;
use crate::shared_kernel::{ConfigKey, ConfigValue, ModeId, ModelId, SessionId as DomainSessionId};
use crate::types::SendMessageData;
use agent_client_protocol::schema::{
    AvailableCommand, CancelNotification, SessionConfigOptionCategory, SessionId, SessionModelState,
    SessionNotification, SetSessionConfigOptionRequest, SetSessionModeRequest, SetSessionModelRequest, UsageUpdate,
};
use aionui_api_types::{
    AgentHandshake, ConfigOptionConfirmation, GetConfigOptionsResponse, SetConfigOptionResponse,
    SlashCommandCompletionBehavior, SlashCommandItem,
};
use aionui_common::{
    AgentKillReason, AgentType, ConversationStatus, ErrorChain, TimestampMs, normalize_keys_to_snake_case, now_ms,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tracing::{debug, error, info, warn};

use super::agent_session_flow::PromptOutcome;
use super::error_mapping::AcpSendFailure;

/// The user-visible body inside an [`AgentError`].
///
/// `AgentError`'s `Display` prefixes every variant with its status name
/// (`"Bad gateway: ..."`, `"Not found: ..."`, etc.). That's correct for HTTP
/// response bodies, but the WebSocket `error` event we broadcast goes straight
/// to the renderer and gets shown verbatim — the prefix only adds noise. Strip
/// it so the user sees the upstream message.
///
/// `pub(super)` so the close-path helpers in `agent_close.rs` can reuse the
/// same prefix-stripping logic when fabricating the `Failed { display }` arm.
pub(super) fn user_facing_message(err: &AgentError) -> String {
    let full = err.to_string();
    // Each variant's Display starts with `"<Tag>: "`. Find the first ": " and
    // return what follows. Variants without a colon (e.g. `RateLimited` →
    // "Rate limited") fall through to the full string.
    full.split_once(": ").map(|(_, rest)| rest.to_owned()).unwrap_or(full)
}

use super::codex_sandbox;
use super::config_options::{ConfigSetPath, ConfigSetPathError, ConfigSnapshot, resolve_set_path};
use super::mode_normalize::normalize_requested_mode;

/// Grace period before force-killing an ACP process (ms).
const ACP_KILL_GRACE_MS: u64 = 500;
const OBSERVED_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Decompose a child `ExitStatus` (or its absence) into the
/// `(exit_code, signal)` pair that `AcpError::StartupCrash` /
/// `AcpError::Disconnected` carry.
///
/// `None` ⇒ wait failed; we have no actionable info to pass on.
/// On Unix, terminating signals surface via `ExitStatusExt::signal()`; the
/// numeric value is rendered as `Some("signal:N")`. On Windows there are no
/// POSIX signals, so `signal` stays `None` and the upstream exit code is the
/// only diagnostic.
///
/// `pub(super)` so the close-path helpers in `agent_close.rs` can read the
/// child's status when a `send_message` fails after init.
pub(super) fn exit_status_parts(exit: Option<std::process::ExitStatus>) -> (Option<i32>, Option<String>) {
    let Some(status) = exit else {
        return (None, None);
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return (status.code(), Some(format!("signal:{sig}")));
        }
    }
    (status.code(), None)
}

fn initial_mode_from_params(params: &AcpSessionParams) -> Option<ModeId> {
    // Prefer the last-persisted mode; for brand-new conversations
    // fall back to `AcpBuildExtra::session_mode` so the first turn
    // still honours the caller's choice.
    params
        .session_snapshot
        .as_ref()
        .and_then(|s| s.current_mode_id.as_ref())
        .map(|m| normalize_requested_mode(&params.metadata, m.as_str()))
        .or_else(|| {
            params
                .config
                .session_mode
                .as_ref()
                .map(|m| normalize_requested_mode(&params.metadata, m))
        })
        .filter(|m| !m.is_empty())
        .map(ModeId::new)
}

fn has_persisted_config_for_category(
    initial_config: &HashMap<ConfigKey, ConfigValue>,
    category: &SessionConfigOptionCategory,
) -> bool {
    match category {
        SessionConfigOptionCategory::Mode => initial_config.keys().any(|key| key.as_str() == "mode"),
        SessionConfigOptionCategory::Model => initial_config.keys().any(|key| key.as_str() == "model"),
        SessionConfigOptionCategory::ThoughtLevel => initial_config.keys().any(|key| {
            matches!(
                key.as_str(),
                "thought_level" | "reasoning_effort" | "effort" | "thinking_budget" | "thinking"
            )
        }),
        _ => false,
    }
}

fn seed_startup_config_preferences(
    session: &mut AcpSession,
    params: &AcpSessionParams,
    initial_config: &HashMap<ConfigKey, ConfigValue>,
) {
    if let Some(mode) = params
        .config
        .session_mode
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|_| !has_persisted_config_for_category(initial_config, &SessionConfigOptionCategory::Mode))
    {
        session.seed_pending_startup_config(SessionConfigOptionCategory::Mode, ConfigValue::new(mode.to_owned()));
    }

    if let Some(model) = params
        .config
        .current_model_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|_| !has_persisted_config_for_category(initial_config, &SessionConfigOptionCategory::Model))
    {
        session.seed_pending_startup_config(SessionConfigOptionCategory::Model, ConfigValue::new(model.to_owned()));
    }

    if let Some(thought_level) = params
        .config
        .thought_level
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|_| !has_persisted_config_for_category(initial_config, &SessionConfigOptionCategory::ThoughtLevel))
    {
        session.seed_pending_startup_config(
            SessionConfigOptionCategory::ThoughtLevel,
            ConfigValue::new(thought_level.to_owned()),
        );
    }
}

fn confirm_option_id(data: &Value) -> Option<String> {
    match data {
        Value::String(v) => Some(v.clone()),
        Value::Object(map) => map
            .get("option_id")
            .or_else(|| map.get("optionId"))
            .or_else(|| map.get("value"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    }
}

/// Serialize an external value (typically an ACP SDK struct that emits
/// camelCase) and normalise every object key to snake_case before it
/// leaves the backend. All handshake columns, WebSocket payloads, and
/// HTTP responses share this rule — callers should go through this
/// helper instead of `serde_json::to_value` directly.
pub(super) fn sdk_to_snake_value<T: serde::Serialize>(value: &T) -> Option<Value> {
    let mut v = serde_json::to_value(value).ok()?;
    normalize_keys_to_snake_case(&mut v);
    Some(v)
}

fn parse_completion_behavior(meta: &serde_json::Map<String, Value>) -> Option<SlashCommandCompletionBehavior> {
    match meta.get("completion_behavior").and_then(Value::as_str) {
        Some("normal") => Some(SlashCommandCompletionBehavior::Normal),
        Some("neutral_tip_on_empty") => Some(SlashCommandCompletionBehavior::NeutralTipOnEmpty),
        _ => None,
    }
}

fn slash_command_item(command: &AvailableCommand) -> SlashCommandItem {
    let meta = command.meta.as_ref();
    let completion_behavior = meta.and_then(parse_completion_behavior);
    let empty_turn_tip_code = meta
        .and_then(|meta| meta.get("empty_turn_tip_code"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let empty_turn_tip_params = meta
        .and_then(|meta| meta.get("empty_turn_tip_params"))
        .filter(|value| value.is_object())
        .cloned();

    SlashCommandItem {
        command: command.name.clone(),
        description: command.description.clone(),
        completion_behavior,
        empty_turn_tip_code,
        empty_turn_tip_params,
    }
}

fn slash_command_items(commands: &[AvailableCommand]) -> Vec<SlashCommandItem> {
    commands.iter().map(slash_command_item).collect()
}

fn leading_slash_token(raw_user_input: &str) -> Option<&str> {
    raw_user_input
        .split_whitespace()
        .next()?
        .strip_prefix('/')
        .filter(|token| !token.is_empty())
}

fn matched_slash_command(raw_user_input: &str, commands: &[AvailableCommand]) -> Option<SlashCommandItem> {
    let token = leading_slash_token(raw_user_input)?;
    commands
        .iter()
        .find(|command| command.name == token)
        .map(slash_command_item)
}

/// Manages a single ACP Agent instance.
///
/// ACP is the most complex agent type, supporting 20+ CLI sub-backends
/// (Claude, Qwen, CodeBuddy, Codex, etc.). Communication now happens via
/// the `agent-client-protocol` SDK's JSON-RPC transport, replacing the
/// previous hand-crafted JSON-over-stdin/stdout approach.
fn mark_session_opened_after_protocol_ready(
    session: &mut AcpSession,
    sid: String,
    protocol_connected: bool,
    conversation_id: &str,
    backend: Option<&str>,
) -> Result<String, AgentError> {
    if !protocol_connected {
        warn!(
            conversation_id = %conversation_id,
            backend = backend.unwrap_or("-"),
            "ACP session open returned after protocol disconnected; rejecting opened transition"
        );
        return Err(AcpError::NotConnected.into());
    }
    session.mark_opened();
    Ok(sid)
}

pub struct AcpAgentManager {
    /// Pre-computed, immutable session parameters assembled by the factory.
    pub(super) params: Arc<AcpSessionParams>,

    /// Session aggregate root — owns desired/observed/advertised state.
    /// Single in-memory source of truth for session lifecycle, modes,
    /// models, config, and all runtime data previously split across
    /// `AcpRuntimeSnapshot` and `AcpState`.
    pub(super) session: RwLock<AcpSession>,

    /// Shared runtime holding status, last_activity, and the event
    /// broadcast channel. `pub(super)` so sibling modules (session_flow,
    /// event_tracker) can call `self.runtime.emit(...)` directly.
    ///
    /// Lifecycle: written by `IAgentTask::send_message` (Running →
    /// Finished/Error), `stop` (emit_finish), and `kill` (emit_error).
    /// `emit_finish` / `emit_error` are idempotent in the Finished
    /// absorbing state — multiple calls are safe.
    pub(super) runtime: AgentRuntime,

    /// ACP protocol handle (SDK connection).
    pub(super) protocol: AcpProtocol,

    /// Routes permission requests from the protocol layer to the user
    /// and back. Owns the receiver channel, pending map, and closing flag.
    pub(super) permission_router: Arc<PermissionRouter>,

    /// Shared skill manager — used to discover skills for first-message injection.
    pub(super) skill_manager: Arc<AcpSkillManager>,

    /// Domain event sender — session aggregate events are forwarded here
    /// for the persistence consumer (`AcpSessionSyncService`).
    pub(super) domain_event_tx: mpsc::Sender<AcpSessionEvent>,

    /// Outbound prompt transformation chain. Constructed once at build
    /// time with the two built-in hooks; not swapped at runtime.
    pub(super) pipeline: PromptPipeline,

    /// Underlying CLI process (for lifecycle management: kill, is_running).
    /// `pub(super)` so the close-path helpers in `agent_close.rs` can read
    /// `exit_status` and peek stderr without going through a wrapper method.
    pub(super) process: Arc<CliAgentProcess>,

    /// Mutex for serializing session operations (new/load/send).
    session_lock: Mutex<()>,
}

impl AcpAgentManager {
    /// Create a new ACP agent manager by spawning a CLI subprocess and
    /// establishing an ACP protocol connection.
    ///
    /// `params` is the pre-computed, immutable session bundle assembled by
    /// `assemble_acp_params` in the factory layer. `catalog_tx` is the
    /// MPSC sender used for the one-shot initialize handshake write;
    /// session-driven fields flow through the `CatalogForwarder` the
    /// factory spawns after construction.
    pub async fn build(
        params: Arc<AcpSessionParams>,
        skill_manager: Arc<AcpSkillManager>,
        catalog_tx: &CatalogSender,
    ) -> Result<
        (
            Self,
            mpsc::Receiver<AcpSessionEvent>,
            mpsc::Receiver<SessionNotification>,
        ),
        AgentError,
    > {
        let (this, domain_event_rx, notification_rx) = AcpAgentManager::new(params, skill_manager).await?;
        this.init(catalog_tx).await;
        Ok((this, domain_event_rx, notification_rx))
    }

    async fn new(
        params: Arc<AcpSessionParams>,
        skill_manager: Arc<AcpSkillManager>,
    ) -> Result<
        (
            Self,
            mpsc::Receiver<AcpSessionEvent>,
            mpsc::Receiver<SessionNotification>,
        ),
        AgentError,
    > {
        let initial_mode = initial_mode_from_params(&params);
        codex_sandbox::sync_for_agent(&params.metadata, initial_mode.as_ref().map(|m| m.as_str())).await;

        let process = Arc::new(CliAgentProcess::spawn_for_sdk(params.command_spec.clone(), &params.data_dir).await?);
        register_session_process(
            &params.data_dir,
            Arc::clone(&process),
            params.conversation_id.clone(),
            AgentType::Acp,
            params.metadata.backend.clone(),
            Some(format!(
                "{} {}",
                params.command_spec.command.display(),
                params.command_spec.args.join(" ")
            )),
        )?;
        let (stdin, stdout) = process.take_stdio().await.ok_or_else(|| {
            error!(conversation_id = %params.conversation_id, "Failed to take stdio from CLI process");
            let _ = unregister_agent_process(&params.data_dir, process.pid());
            AgentError::internal("Failed to take stdio from CLI process")
        })?;

        // Dedicated channel for raw SDK SessionNotifications → session tracker.
        // This channel is separate from event_tx so the tracker never re-applies
        // events that were broadcast for the UI (e.g. from emit_snapshot_events).
        let (notification_tx, notification_rx) = mpsc::channel::<SessionNotification>(256);
        let (domain_event_tx, domain_event_rx) = mpsc::channel(256);
        let (permission_tx, permission_rx) = mpsc::channel(32);
        let runtime = AgentRuntime::new(params.conversation_id.clone(), params.workspace.path.clone(), 256);

        // Race the handshake against process exit. The SDK's stdout EOF
        // detection can lag (observed: 30s on Windows when the agent dies
        // 70ms in — ELECTRON-1BT), so we explicitly watch the child. If
        // it dies before init completes, surface a `StartupCrash` carrying
        // the buffered stderr instead of waiting out the timeout.
        let connect_fut = AcpProtocol::connect(stdin, stdout, runtime.event_sender(), permission_tx, notification_tx);
        tokio::pin!(connect_fut);
        let protocol = tokio::select! {
            biased;
            exit = process.wait_for_exit() => {
                let stderr = process.peek_stderr_tail(64).await;
                let (exit_code, signal) = exit_status_parts(exit);
                error!(
                    conversation_id = %params.conversation_id,
                    exit_code = ?exit_code,
                    signal = ?signal,
                    stderr = %stderr,
                    "Agent process exited before ACP handshake completed"
                );
                let _ = unregister_agent_process(&params.data_dir, process.pid());
                return Err(AgentError::from(AcpError::StartupCrash { exit_code, signal, stderr }));
            }
            res = &mut connect_fut => res.map_err(|e| {
                error!(
                    conversation_id = %params.conversation_id,
                    error = %ErrorChain(&e),
                    "Failed to establish ACP protocol connection"
                );
                let _ = unregister_agent_process(&params.data_dir, process.pid());
                AgentError::from(e)
            })?,
        };
        let permission_router = Arc::new(PermissionRouter::new(permission_rx));

        let snapshot = params.session_snapshot.as_ref();

        let (initial_model, initial_config) = (
            snapshot.and_then(|s| s.current_model_id.clone()).or_else(|| {
                params
                    .config
                    .current_model_id
                    .as_ref()
                    .filter(|m| !m.is_empty())
                    .map(|m| ModelId::new(m.clone()))
            }),
            snapshot.map(|s| s.config_selections.clone()).unwrap_or_default(),
        );

        let startup_config_seed_base = initial_config.clone();
        let mut session = AcpSession::new(initial_mode, initial_model, initial_config);
        seed_startup_config_preferences(&mut session, &params, &startup_config_seed_base);

        let pipeline = PromptPipeline::new(vec![Arc::new(SessionNewPreludeHook)]);

        let manager = Self {
            params,
            session: RwLock::new(session),
            runtime,
            process,
            protocol,
            session_lock: Mutex::new(()),
            permission_router,
            skill_manager,
            domain_event_tx,
            pipeline,
        };
        Ok((manager, domain_event_rx, notification_rx))
    }

    async fn init(&self, catalog_tx: &CatalogSender) {
        let init_handshake = AgentHandshake {
            agent_capabilities: self.protocol.agent_capabilities().and_then(|c| sdk_to_snake_value(&c)),
            auth_methods: self.protocol.auth_methods().and_then(|m| sdk_to_snake_value(&m)),
            ..Default::default()
        };
        if init_handshake.agent_capabilities.is_some() || init_handshake.auth_methods.is_some() {
            catalog_tx.send_partial(self.params.metadata.id.clone(), init_handshake);
        }

        // Seed the observed/advertised layers (observed mode/model, cached
        // context_usage) from the persisted snapshot. Desired fields are
        // already populated via `AcpSession::new`.
        if let Some(snapshot) = self.params.session_snapshot.as_ref() {
            let mut session = self.session.write().await;
            session.preload_persisted(snapshot);
            // Preload did not come from the user this turn — drain so the
            // persistence consumer doesn't echo the DB back into itself.
            session.drain_events();
        }
        if let Some(agent_capabilities) = self.protocol.agent_capabilities() {
            let mut session = self.session.write().await;
            session.apply_advertised_capabilities(agent_capabilities);
        }
        if let Some(auth_methods) = self.protocol.auth_methods() {
            let mut session = self.session.write().await;
            session.apply_advertised_auth_methods(auth_methods);
        }
    }
}

impl AcpAgentManager {
    fn record_user_cancel_request(runtime: &AgentRuntime, session: &mut AcpSession) {
        session.record_close_reason(Some(CloseReason::UserCancel));
        runtime.bump_activity();
    }

    fn ensure_protocol_connected_for_operation(&self, operation: &'static str) -> Result<(), AgentError> {
        if self.protocol.is_connected() {
            return Ok(());
        }
        warn!(
            conversation_id = %self.params.conversation_id,
            agent_backend = ?self.params.metadata.backend,
            operation,
            "ACP operation rejected because protocol is disconnected"
        );
        Err(AcpError::NotConnected.into())
    }

    pub(crate) async fn mode(&self) -> Result<aionui_api_types::AgentModeResponse, AgentError> {
        let desired = self
            .session
            .read()
            .await
            .desired_mode()
            .map(|mode| normalize_requested_mode(&self.params.metadata, mode))
            .filter(|mode| !mode.is_empty());
        Ok(aionui_api_types::AgentModeResponse {
            mode: self
                .session
                .read()
                .await
                .modes()
                .map(|modes| modes.current_mode_id.to_string())
                .or(desired)
                .unwrap_or_else(|| normalize_requested_mode(&self.params.metadata, "default")),
            initialized: self.session_id().await.is_some(),
        })
    }

    pub(crate) fn is_claude_backend(&self) -> bool {
        self.params.metadata.backend.as_deref() == Some("claude")
    }

    /// Cached model info from the ACP backend, if any has been received.
    pub(crate) async fn model(&self) -> Option<SessionModelState> {
        self.session.read().await.model_info().cloned()
    }

    /// Cached context usage info from the ACP backend.
    pub(crate) async fn usage(&self) -> Option<UsageUpdate> {
        self.session.read().await.context_usage().cloned()
    }

    pub(crate) async fn config_options(&self) -> Result<GetConfigOptionsResponse, AgentError> {
        let session = self.session.read().await;
        Ok(GetConfigOptionsResponse {
            config_options: session.config_snapshot().options,
        })
    }

    pub(crate) async fn set_config_option_confirmed(
        &self,
        option_id: &str,
        value: &str,
    ) -> Result<SetConfigOptionResponse, AgentError> {
        let option_id = option_id.trim();
        let value = value.trim();
        if option_id.is_empty() {
            return Err(AgentError::bad_request("option_id must not be empty"));
        }
        if value.is_empty() {
            return Err(AgentError::bad_request("value must not be empty"));
        }

        let guard = {
            let mut session = self.session.write().await;
            session.try_begin_config_set()
        };
        let Some(guard) = guard else {
            tracing::info!(
                conversation_id = %self.params.conversation_id,
                agent_backend = ?self.params.metadata.backend,
                requested_option_id = %option_id,
                requested_value = %value,
                "acp_config_option_update_rejected_in_progress"
            );
            return Err(AgentError::conflict("ACP config update is already in progress"));
        };

        let result = self.set_config_option_confirmed_inner(option_id, value).await;

        {
            let mut session = self.session.write().await;
            session.end_config_set(guard);
        }

        result
    }

    async fn set_config_option_confirmed_inner(
        &self,
        option_id: &str,
        value: &str,
    ) -> Result<SetConfigOptionResponse, AgentError> {
        self.ensure_protocol_connected_for_operation("set_config_option")?;

        let (session_id, set_path, is_mode_option) = {
            let session = self.session.read().await;
            let snapshot = session.config_snapshot();
            let mut set_path = resolve_set_path(&snapshot, option_id, value).map_err(|err| match err {
                ConfigSetPathError::OptionNotFound => {
                    AgentError::bad_request(format!("Config option '{option_id}' is not available"))
                }
                ConfigSetPathError::ValueNotSelectable => AgentError::bad_request(format!(
                    "Value '{value}' is not selectable for config option '{option_id}'"
                )),
            })?;
            if session.config_options().is_none() {
                set_path = match option_id {
                    "mode" => ConfigSetPath::LegacyMode,
                    "model" => ConfigSetPath::LegacyModel,
                    _ => set_path,
                };
            }
            let session_id = session.session_id().map(ToOwned::to_owned).ok_or_else(|| {
                warn!(
                    conversation_id = %self.params.conversation_id,
                    agent_backend = ?self.params.metadata.backend,
                    config_id = %option_id,
                    "acp_config_option_set_missing_session"
                );
                AgentError::bad_request("No active session")
            })?;
            (session_id, set_path, snapshot.is_mode_option(option_id))
        };

        tracing::info!(
            conversation_id = %self.params.conversation_id,
            agent_backend = ?self.params.metadata.backend,
            config_id = %option_id,
            requested = %value,
            "acp_config_option_set_requested"
        );

        if self.params.metadata.backend.as_deref() == Some("codex") && is_mode_option {
            codex_sandbox::sync_for_agent(&self.params.metadata, Some(value)).await;
        }

        match set_path {
            ConfigSetPath::ConfigOption { option_id: config_id } => {
                let response = self
                    .protocol
                    .set_config_option(SetSessionConfigOptionRequest::new(
                        SessionId::new(session_id.clone()),
                        config_id.clone(),
                        value.to_owned(),
                    ))
                    .await
                    .map_err(|err| {
                        warn!(
                            conversation_id = %self.params.conversation_id,
                            agent_backend = ?self.params.metadata.backend,
                            config_id = %config_id,
                            requested = %value,
                            error = %err,
                            "acp_config_option_command_failed"
                        );
                        AgentError::from(err)
                    })?;

                tracing::info!(
                    conversation_id = %self.params.conversation_id,
                    agent_backend = ?self.params.metadata.backend,
                    config_id = %config_id,
                    requested = %value,
                    method = "session/set_config_option",
                    "acp_config_option_command_ack"
                );

                {
                    let mut session = self.session.write().await;
                    if session.session_id() != Some(session_id.as_str()) {
                        return Err(AgentError::conflict(
                            "Active ACP session changed while applying config option",
                        ));
                    }
                    session.apply_advertised_config_options(response.config_options);
                    self.commit_session_changes(&mut session).await;
                }
                self.wait_for_observed_config_option(&config_id, value, OBSERVED_CONFIRMATION_TIMEOUT)
                    .await
            }
            ConfigSetPath::LegacyMode => {
                self.protocol
                    .set_mode(SetSessionModeRequest::new(
                        SessionId::new(session_id.clone()),
                        value.to_owned(),
                    ))
                    .await
                    .map_err(|err| {
                        warn!(
                            conversation_id = %self.params.conversation_id,
                            agent_backend = ?self.params.metadata.backend,
                            config_id = %option_id,
                            requested = %value,
                            error = %err,
                            "acp_config_option_command_failed"
                        );
                        AgentError::from(err)
                    })?;
                tracing::info!(
                    conversation_id = %self.params.conversation_id,
                    agent_backend = ?self.params.metadata.backend,
                    config_id = %option_id,
                    requested = %value,
                    method = "session/set_mode",
                    "acp_config_option_command_ack"
                );
                self.ensure_session_unchanged(&session_id, "mode").await?;
                self.wait_for_observed_config_option("mode", value, OBSERVED_CONFIRMATION_TIMEOUT)
                    .await
            }
            ConfigSetPath::LegacyModel => {
                self.protocol
                    .set_model(SetSessionModelRequest::new(
                        SessionId::new(session_id.clone()),
                        value.to_owned(),
                    ))
                    .await
                    .map_err(|err| {
                        warn!(
                            conversation_id = %self.params.conversation_id,
                            agent_backend = ?self.params.metadata.backend,
                            config_id = %option_id,
                            requested = %value,
                            error = %err,
                            "acp_config_option_command_failed"
                        );
                        AgentError::from(err)
                    })?;
                tracing::info!(
                    conversation_id = %self.params.conversation_id,
                    agent_backend = ?self.params.metadata.backend,
                    config_id = %option_id,
                    requested = %value,
                    method = "session/set_model",
                    "acp_config_option_command_ack"
                );
                self.ensure_session_unchanged(&session_id, "model").await?;
                self.wait_for_observed_config_option("model", value, OBSERVED_CONFIRMATION_TIMEOUT)
                    .await
            }
        }
        .map(|snapshot| SetConfigOptionResponse {
            confirmation: ConfigOptionConfirmation::Observed,
            config_options: Some(snapshot.options),
        })
    }

    async fn ensure_session_unchanged(&self, session_id: &str, field: &str) -> Result<(), AgentError> {
        let session = self.session.read().await;
        if session.session_id() == Some(session_id) {
            return Ok(());
        }
        warn!(
            conversation_id = %self.params.conversation_id,
            agent_backend = ?self.params.metadata.backend,
            config_id = %field,
            confirmed_session_id = %session_id,
            active_session_id = ?session.session_id(),
            "acp_config_option_session_changed"
        );
        Err(AgentError::conflict(
            "Active ACP session changed while applying config option",
        ))
    }

    async fn wait_for_observed_config_option(
        &self,
        option_id: &str,
        requested: &str,
        timeout: Duration,
    ) -> Result<ConfigSnapshot, AgentError> {
        let started = Instant::now();
        loop {
            let snapshot = {
                let session = self.session.read().await;
                session.config_snapshot()
            };
            if snapshot.observed_matches(option_id, requested) {
                tracing::info!(
                    conversation_id = %self.params.conversation_id,
                    agent_backend = ?self.params.metadata.backend,
                    config_id = %option_id,
                    requested = %requested,
                    elapsed_ms = started.elapsed().as_millis(),
                    "acp_config_option_observed_confirmed"
                );
                return Ok(snapshot);
            }
            if started.elapsed() >= timeout {
                tracing::warn!(
                    conversation_id = %self.params.conversation_id,
                    agent_backend = ?self.params.metadata.backend,
                    config_id = %option_id,
                    requested = %requested,
                    timeout_ms = timeout.as_millis(),
                    last_observed = ?snapshot.option_current(option_id),
                    "acp_config_option_confirmation_timeout"
                );
                return Err(AgentError::timeout("ACP config option confirmation timed out"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Return available slash commands from the session aggregate.
    pub(crate) async fn load_slash_commands(&self) -> Result<Vec<SlashCommandItem>, AgentError> {
        let session = self.session.read().await;
        let items = session
            .available_commands()
            .map(slash_command_items)
            .unwrap_or_default();
        Ok(items)
    }
}

impl AcpAgentManager {
    /// Current ACP session ID, if a session has been established.
    pub async fn session_id(&self) -> Option<String> {
        self.session.read().await.session_id().map(ToOwned::to_owned)
    }

    /// Restore a previously persisted session_id (e.g. from DB on task rebuild).
    /// Enables resume path on next send_message instead of creating a fresh session.
    ///
    /// Deliberately leaves `opened = false`: the CLI child process is
    /// brand new and still needs `session/load` (or claude-meta-resume) to
    /// re-attach to the persisted session before the next prompt. Subsequent
    /// turns — once the resume handshake has run — take the short path.
    pub async fn set_session_id(&self, sid: String) {
        let mut session = self.session.write().await;
        session.set_session_id(DomainSessionId::new(sid));
        session.drain_events();
    }

    /// Vendor label this session was spawned as (e.g. "claude"), if any.
    pub fn backend(&self) -> Option<&str> {
        self.params.metadata.backend.as_deref()
    }

    /// Agent metadata id this session was spawned from.
    pub fn agent_id(&self) -> &str {
        &self.params.metadata.id
    }

    /// Whether the configured agent supports side questions.
    pub fn supports_side_question(&self) -> bool {
        self.params.metadata.behavior_policy.supports_side_question
    }
}

impl AcpAgentManager {
    /// Ensure the ACP session is opened with the CLI. Does not send a
    /// prompt. Returns the session id that subsequent prompts should use
    /// (may differ from the input when claude-meta-resume rewrites it).
    ///
    /// Three paths mirror `ensure_session_and_send`:
    /// 1. No sid at all → `open_session_new`
    /// 2. Sid present but CLI has not opened it (fresh task) → `open_session_resume`
    /// 3. Already opened → noop, return the existing sid
    #[tracing::instrument(skip_all, fields(conversation_id = %self.params.conversation_id))]
    async fn ensure_session_opened(&self) -> Result<String, AgentError> {
        debug!("Ensuring ACP session is opened");
        let _lock = self.session_lock.lock().await;
        self.ensure_protocol_connected_for_operation("ensure_session_opened")?;

        let (session_id, opened) = {
            let s = self.session.read().await;
            (s.session_id().map(ToOwned::to_owned), s.is_opened())
        };

        let sid = match (session_id, opened) {
            (None, _) => self.open_session_new().await?,
            (Some(sid), false) => self.open_session_resume(&sid).await?,
            (Some(sid), true) => sid,
        };

        {
            let mut s = self.session.write().await;
            let sid = mark_session_opened_after_protocol_ready(
                &mut s,
                sid,
                self.protocol.is_connected(),
                &self.params.conversation_id,
                self.backend(),
            )?;
            self.commit_session_changes(&mut s).await;
            Ok(sid)
        }
    }

    /// Initialize or resume a session, then send the user message.
    ///
    /// The prompt is passed through `self.pipeline.pre_send` before being
    /// forwarded to the CLI. Each hook in the pipeline reads one-shot flags
    /// on `AcpSession` (e.g. `pending_session_new_prelude`,
    /// flags) and prepends the appropriate block when set.
    async fn ensure_session_and_send(&self, data: &SendMessageData) -> Result<PromptOutcome, AcpSendFailure> {
        let sid = self.ensure_session_opened().await.map_err(AcpSendFailure::from)?;
        self.runtime.reset_for_new_turn(ConversationStatus::Running);
        let raw_user_input = data.content.clone();
        let matched_command = {
            let session = self.session.read().await;
            session
                .available_commands()
                .and_then(|commands| matched_slash_command(&raw_user_input, commands))
        };

        let (content, dump_first_prompt) = {
            let mut s = self.session.write().await;
            let dump_first_prompt = s.has_pending_session_new_prelude();
            let mut ctx = PromptCtx {
                session: &mut s,
                params: &self.params,
                skill_manager: &self.skill_manager,
                runtime: &self.runtime,
            };
            let transformed = self.pipeline.pre_send(&mut ctx, data.content.clone()).await;
            self.commit_session_changes(&mut s).await;
            (transformed, dump_first_prompt)
        };

        if dump_first_prompt
            && let Some(dump_dir) =
                crate::dev_prompt_dump::dump_dir_for_data_dir(&self.params.data_dir, self.params.dump_prompts)
        {
            match crate::dev_prompt_dump::dump_prompt(
                &dump_dir,
                crate::dev_prompt_dump::PromptDump {
                    kind: "acp-first-prompt",
                    backend: self.backend(),
                    conversation_id: &self.params.conversation_id,
                    session_id: Some(&sid),
                    msg_id: Some(&data.msg_id),
                    turn_id: data.turn_id.as_deref(),
                    prompt: &content,
                },
            ) {
                Ok(path) => {
                    debug!(
                        conversation_id = %self.params.conversation_id,
                        path = %path.display(),
                        "DEV prompt dump written"
                    );
                }
                Err(error) => {
                    warn!(
                        conversation_id = %self.params.conversation_id,
                        error = %error,
                        "DEV prompt dump failed"
                    );
                }
            }
        }

        let data = SendMessageData {
            content,
            ..data.clone()
        };
        self.prompt_existing_session(&data, Some(&sid), matched_command.as_ref())
            .await
    }

    /// Pre-open the ACP session without sending a prompt. Called by the
    /// factory after `AcpAgentManager::build` so `POST /warmup` returns
    /// only after the session is ready to accept `set_mode` / `set_model`
    /// / `prompt`. Idempotent — if already opened, returns immediately.
    #[tracing::instrument(skip_all, fields(conversation_id = %self.params.conversation_id))]
    pub async fn warmup_session(&self) -> Result<(), AgentError> {
        info!("Warming up ACP session");
        let result = self.ensure_session_opened().await.map(|_sid| ());
        match &result {
            Ok(()) => info!("ACP session warmed up"),
            Err(e) => warn!(error = %ErrorChain(e), "ACP session warmup failed"),
        }
        result
    }
}

fn log_idle_acp_shutdown_started(conversation_id: &str, backend: &str, session_id: Option<&str>, pid: u32) {
    info!(
        conversation_id = %conversation_id,
        backend = %backend,
        session_id = %session_id.unwrap_or("none"),
        pid,
        reason = %"IdleTimeout",
        "Idle kill: ACP shutdown started"
    );
}

fn log_idle_acp_cancel_sent(conversation_id: &str, backend: &str, session_id: &str, pid: u32) {
    info!(
        conversation_id = %conversation_id,
        backend = %backend,
        session_id = %session_id,
        pid,
        "Idle kill: ACP session cancel sent"
    );
}

fn log_idle_acp_cancel_skipped(conversation_id: &str, backend: &str, pid: u32) {
    info!(
        conversation_id = %conversation_id,
        backend = %backend,
        pid,
        reason = %"no_session_id",
        "Idle kill: ACP session cancel skipped"
    );
}

#[async_trait::async_trait]
impl crate::agent_task::IAgentTask for AcpAgentManager {
    fn agent_type(&self) -> AgentType {
        AgentType::Acp
    }

    fn conversation_id(&self) -> &str {
        &self.params.conversation_id
    }

    fn workspace(&self) -> &str {
        &self.params.workspace.path
    }

    fn status(&self) -> Option<ConversationStatus> {
        self.runtime.status()
    }

    fn last_activity_at(&self) -> TimestampMs {
        self.runtime.last_activity_at()
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.runtime.subscribe()
    }

    #[tracing::instrument(skip_all, fields(conversation_id = %self.params.conversation_id, msg_id = %data.msg_id))]
    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        self.runtime.bump_activity();
        info!(
            conversation_id = %self.params.conversation_id,
            msg_id = %data.msg_id,
            turn_id = data.turn_id.as_deref().unwrap_or("none"),
            "ACP send_message started"
        );

        match self.ensure_session_and_send(&data).await {
            Ok(PromptOutcome::Completed { session_id }) => {
                info!(
                    agent_type = "acp",
                    terminal_kind = "finish",
                    source = "prompt_outcome",
                    "ACP send_message completed"
                );
                self.runtime.emit_finish(Some(session_id));
                Ok(())
            }
            Ok(PromptOutcome::Cancelled { session_id }) => {
                info!(
                    agent_type = "acp",
                    terminal_kind = "finish",
                    source = "prompt_cancelled",
                    "ACP send_message cancelled"
                );
                self.runtime.emit_finish(Some(session_id));
                Ok(())
            }
            Ok(PromptOutcome::InfoTip { session_id, tips } | PromptOutcome::WarningTip { session_id, tips }) => {
                info!(
                    agent_type = "acp",
                    terminal_kind = "finish",
                    source = "empty_response",
                    session_id = %session_id,
                    "ACP send_message completed without visible output"
                );
                self.runtime.emit(AgentStreamEvent::Tips(tips));
                self.runtime.emit_finish(Some(session_id));
                Ok(())
            }
            Ok(PromptOutcome::TerminalError { session_id, error }) => {
                info!(
                    agent_type = "acp",
                    terminal_kind = "error",
                    source = "empty_response_stderr",
                    session_id = %session_id,
                    error_code = ?error.code,
                    "ACP send_message empty turn classified as terminal upstream error"
                );
                self.runtime.emit_error_data(error);
                Ok(())
            }
            Err(err) => {
                let backend = self.params.metadata.backend.as_deref();
                let send_error = err.to_agent_send_error_for_backend(backend);
                let agent_err = err.into_agent_error();
                // Build a CloseReason that captures whatever context we still
                // have. Two cases matter:
                //   1. The CLI process has already exited — we can read the
                //      exit code/signal directly and run the stderr tail
                //      through the redaction allowlist, even if the SDK
                //      surfaced the failure as a generic JSON-RPC error.
                //   2. The process is still alive — fall back to the existing
                //      stderr-augmentation heuristic for the SDK's "default
                //      Internal error" shape; otherwise the user-facing form
                //      of the AgentError is the best we can do.
                let close_reason = self.build_close_reason_from_error(&agent_err).await;

                // Operator log: full error chain + the (raw, pre-redaction)
                // stderr peek so on-call can correlate. The redacted summary
                // is what reaches the UI.
                let summary = close_reason.user_facing_message();
                error!(error = %ErrorChain(&agent_err), close_reason_summary = %summary, "ACP send_message failed");

                {
                    let mut session = self.session.write().await;
                    session.record_close_reason(Some(close_reason));
                }
                self.runtime.emit_error_data(send_error.stream_error().clone());
                Err(send_error)
            }
        }
    }

    #[tracing::instrument(skip_all, fields(conversation_id = %self.params.conversation_id))]
    async fn cancel(&self) -> Result<(), AgentError> {
        info!("Cancelling ACP session");
        let session_id = self.session.read().await.session_id().map(ToOwned::to_owned);
        if let Some(sid) = &session_id {
            self.protocol
                .cancel(CancelNotification::new(SessionId::new(sid.as_str())));
        }
        self.permission_router.cancel_all();

        {
            let mut session = self.session.write().await;
            Self::record_user_cancel_request(&self.runtime, &mut session);
        }

        info!(
            agent_type = "acp",
            source = "cancel_request",
            session_id = session_id.as_deref().unwrap_or("none"),
            "ACP cancel requested; waiting for prompt outcome before terminal finish"
        );

        Ok(())
    }

    fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        info!(
            conversation_id = %self.params.conversation_id,
            ?reason,
            "Killing ACP agent"
        );

        // Mark closing to prevent reconnect attempts
        self.permission_router.set_closing();

        let backend = self.params.metadata.backend.as_deref().unwrap_or("unknown");
        let pid = self.process.pid();
        let idle_timeout = matches!(reason, Some(AgentKillReason::IdleTimeout));
        let session_id = self
            .session
            .try_read()
            .ok()
            .and_then(|session| session.session_id().map(ToOwned::to_owned));

        if idle_timeout {
            log_idle_acp_shutdown_started(&self.params.conversation_id, backend, session_id.as_deref(), pid);
        }

        if let Some(sid) = session_id.as_deref() {
            self.protocol.cancel(CancelNotification::new(SessionId::new(sid)));
            if idle_timeout {
                log_idle_acp_cancel_sent(&self.params.conversation_id, backend, sid, pid);
            }
        } else if idle_timeout {
            log_idle_acp_cancel_skipped(&self.params.conversation_id, backend, pid);
        }

        let process = Arc::clone(&self.process);
        let grace = Duration::from_millis(ACP_KILL_GRACE_MS);
        let conversation_id = self.params.conversation_id.clone();
        let process_group_id = process.process_group_id();
        let backend = backend.to_owned();
        let idle_timeout = matches!(reason, Some(AgentKillReason::IdleTimeout));

        tokio::spawn(async move {
            let started_at = now_ms();
            match process.kill(grace).await {
                Ok(()) => {
                    if idle_timeout {
                        info!(
                            %conversation_id,
                            backend,
                            pid,
                            process_group_id,
                            elapsed_ms = now_ms().saturating_sub(started_at),
                            result = "ok",
                            "Idle kill: ACP process shutdown completed"
                        );
                    } else {
                        debug!(%conversation_id, pid, process_group_id, "ACP process kill completed");
                    }
                }
                Err(e) => {
                    error!(
                        %conversation_id,
                        backend,
                        pid,
                        process_group_id,
                        elapsed_ms = now_ms().saturating_sub(started_at),
                        result = "error",
                        error = %ErrorChain(&e),
                        "Idle kill: ACP process shutdown completed"
                    );
                }
            }
        });

        self.permission_router.cancel_all();

        if matches!(reason, Some(AgentKillReason::UserCancelTimeout)) {
            if let Ok(mut session) = self.session.try_write() {
                session.record_close_reason(Some(CloseReason::UserCancel));
            }
            self.runtime.emit_finish(None);
        } else {
            // m1 fix: emit error with the kill reason so the status goes to
            // Finished and subscribers see a terminal event. Idempotent.
            // Source of truth for the toast text is `CloseReason::Killed`.
            let close_reason = CloseReason::Killed { reason };
            let message = close_reason.user_facing_message();
            if let Ok(mut session) = self.session.try_write() {
                session.record_close_reason(Some(close_reason));
            }
            self.runtime.emit_error(message);
        }

        Ok(())
    }
}

impl AcpAgentManager {
    pub fn kill_and_wait(
        &self,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let _ = crate::agent_task::IAgentTask::kill(self, reason);
        let process = Arc::clone(&self.process);
        let grace = Duration::from_millis(ACP_KILL_GRACE_MS);
        Box::pin(async move {
            let _ = process.kill(grace).await;
        })
    }

    /// Pending ACP permission prompts recoverable through the conversation
    /// confirmation API.
    pub fn get_confirmations(&self) -> Vec<aionui_common::Confirmation> {
        self.permission_router.get_confirmations()
    }

    /// Submit a permission response for a pending tool call. ACP confirms
    /// always carry an `option_id`; `always_allow` is consumed by the CLI
    /// and is not reflected in the local approval memory (the ACP CLI
    /// tracks its own).
    pub fn confirm(
        &self,
        _msg_id: &str,
        call_id: &str,
        data: serde_json::Value,
        _always_allow: bool,
    ) -> Result<(), AgentError> {
        let option_id = confirm_option_id(&data)
            .ok_or_else(|| AgentError::bad_request("ACP confirmation requires an option_id string"))?;

        self.permission_router
            .confirm(call_id, option_id, &self.params.conversation_id)
    }
}

// `augment_with_stderr` and `build_close_reason_from_error` live in
// `agent_close.rs` to keep this file under the 1000-line budget.

#[cfg(test)]
mod tests {
    use super::{exit_status_parts, user_facing_message};
    use crate::agent_runtime::AgentRuntime;
    use crate::error::AgentError;
    use crate::manager::acp::{AcpAgentManager, AcpSession};
    use crate::protocol::error::{AcpError, CloseReason};
    use crate::shared_kernel::{ConfigKey, ConfigValue, SessionId as DomainSessionId};
    use agent_client_protocol::schema::{AvailableCommand, SessionConfigOptionCategory};
    use serde_json::json;
    use std::collections::HashMap;

    fn capture_logs(max_level: tracing::Level, f: impl FnOnce()) -> String {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt;

        #[derive(Clone)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);

        impl Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let make_writer = {
            let buffer = Arc::clone(&buffer);
            move || SharedBuf(Arc::clone(&buffer))
        };

        let subscriber = fmt::Subscriber::builder()
            .with_max_level(max_level)
            .with_writer(make_writer)
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, f);

        String::from_utf8(buffer.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn exit_status_parts_handles_missing_status() {
        assert_eq!(exit_status_parts(None), (None, None));
    }

    #[cfg(unix)]
    #[test]
    fn exit_status_parts_extracts_unix_exit_code() {
        // ExitStatus::from_raw is the only stable constructor. On Unix the
        // low 8 bits are the signal; bits 8..15 are the exit code when the
        // process exited normally.
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(1 << 8); // exit 1
        let (code, signal) = exit_status_parts(Some(status));
        assert_eq!(code, Some(1));
        assert_eq!(signal, None);
    }

    #[test]
    fn strips_bad_gateway_prefix() {
        let err = AgentError::bad_gateway("API Error: Internal server error");
        assert_eq!(user_facing_message(&err), "API Error: Internal server error");
    }

    #[test]
    fn strips_not_found_prefix() {
        let err = AgentError::not_found("user 42");
        assert_eq!(user_facing_message(&err), "user 42");
    }

    #[test]
    fn rate_limited_has_no_colon_returns_full_string() {
        let err = AgentError::RateLimited;
        assert_eq!(user_facing_message(&err), "Rate limited");
    }

    #[test]
    fn idle_acp_shutdown_started_log_contains_diagnostic_context() {
        let captured = capture_logs(tracing::Level::INFO, || {
            super::log_idle_acp_shutdown_started("conv_openclaw", "openclaw", Some("session_123"), 4242);
        });

        assert!(captured.contains("Idle kill: ACP shutdown started"));
        assert!(captured.contains("conversation_id=conv_openclaw"));
        assert!(captured.contains("backend=openclaw"));
        assert!(captured.contains("session_id=session_123"));
        assert!(captured.contains("pid=4242"));
        assert!(captured.contains("reason=IdleTimeout"));
    }

    #[test]
    fn idle_acp_cancel_skipped_log_contains_reason() {
        let captured = capture_logs(tracing::Level::INFO, || {
            super::log_idle_acp_cancel_skipped("conv_openclaw", "openclaw", 4242);
        });

        assert!(captured.contains("Idle kill: ACP session cancel skipped"));
        assert!(captured.contains("conversation_id=conv_openclaw"));
        assert!(captured.contains("backend=openclaw"));
        assert!(captured.contains("pid=4242"));
        assert!(captured.contains("reason=no_session_id"));
    }

    #[test]
    fn warmup_does_not_mark_opened_when_protocol_disconnected_after_open() {
        let mut session = AcpSession::new(None, None, Default::default());
        session.set_session_id(DomainSessionId::new("sess-disconnected"));

        let err = super::mark_session_opened_after_protocol_ready(
            &mut session,
            "sess-disconnected".to_owned(),
            false,
            "conv-test",
            Some("codex"),
        )
        .expect_err("disconnected protocol must reject the opened transition");

        assert!(
            matches!(err, AgentError::Acp(AcpError::NotConnected)),
            "expected AcpError::NotConnected, got {err:?}"
        );
        assert_eq!(session.session_id(), Some("sess-disconnected"));
        assert!(
            !session.is_opened(),
            "warmup must not mark the aggregate opened when the protocol is already disconnected"
        );
    }

    #[test]
    fn nested_colons_only_strip_first() {
        // "Bad gateway: Internal error: API Error: ..." → keep everything after the first ": "
        let err = AgentError::bad_gateway("Internal error: API Error: Internal server error");
        assert_eq!(
            user_facing_message(&err),
            "Internal error: API Error: Internal server error"
        );
    }

    #[tokio::test]
    async fn acp_cancel_request_records_user_cancel_without_terminal_finish() {
        let runtime = AgentRuntime::new("conv-1", "/tmp/workspace", 8);
        let mut rx = runtime.subscribe();
        let mut session = AcpSession::new(None, None, Default::default());

        AcpAgentManager::record_user_cancel_request(&runtime, &mut session);

        assert!(matches!(session.last_close_reason(), Some(CloseReason::UserCancel)));
        assert_eq!(runtime.status(), None);
        let res = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(res.is_err(), "cancel request must not emit a terminal event");
    }

    // ---- augment_with_stderr behavioral tests ------------------------------
    //
    // We can't easily construct a real AcpAgentManager in a unit test (it
    // needs the full ACP plumbing). Instead we test the *composition* of
    // Task 3's peek_stderr_tail + Task 4's extract_error_message + this
    // task's "SDK default Display" shape detection by spawning a real
    // CliAgentProcess that writes the chosen stderr, then running the same
    // detection+peek+extract pipeline against it.
    //
    // The helper below MIRRORS `AcpAgentManager::augment_with_stderr`. If
    // you change the production helper (e.g. the prefix string, peek line
    // count, or extractor module path) update this helper to match.

    use super::CliAgentProcess;
    use std::sync::Arc;
    use std::time::Duration;

    /// Spawn a sh subprocess that writes `stderr_payload` to stderr then
    /// exits with `exit_code`. Used to simulate ACP CLI crashes/exits in
    /// close-path tests. Lines containing `'` are escaped for the heredoc.
    async fn spawn_with_stderr_and_exit(stderr_payload: &str, exit_code: u8) -> Arc<CliAgentProcess> {
        use aionui_common::CommandSpec;
        let payload = stderr_payload.replace('\'', "'\\''");
        let script = format!("cat <<'EOF' >&2\n{payload}\nEOF\nexit {exit_code}");
        let config = CommandSpec {
            command: "sh".into(),
            args: vec!["-c".into(), script],
            env: vec![],
            cwd: None,
        };
        let data_dir = tempfile::tempdir().unwrap();
        let proc = CliAgentProcess::spawn_for_sdk(config, data_dir.path()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), proc.wait_for_exit())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        Arc::new(proc)
    }

    async fn spawn_with_stderr(stderr_payload: &str) -> Arc<CliAgentProcess> {
        spawn_with_stderr_and_exit(stderr_payload, 0).await
    }

    async fn augment_via_process(proc: &Arc<CliAgentProcess>, err: &AgentError) -> Option<String> {
        const SDK_DEFAULT_BAD_GATEWAY_PREFIX: &str = "Bad gateway: Agent internal error (code ";
        let display = err.to_string();
        let is_default_internal = display.starts_with(SDK_DEFAULT_BAD_GATEWAY_PREFIX) && display.ends_with(')');
        if !is_default_internal {
            return None;
        }
        // Mirror the production STDERR_PEEK_LINES (32). If you change one, change both.
        let tail = proc.peek_stderr_tail(32).await;
        super::super::stderr_error_extractor::extract_error_message(&tail)
    }

    #[tokio::test]
    async fn augments_when_codex_usage_limit_in_stderr() {
        let stderr = "\u{1b}[2m2026-05-13T20:01:21Z\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m codex_acp::thread: Unhandled error during turn: You've hit your usage limit. Try again later. Some(UsageLimitExceeded)";
        let proc = spawn_with_stderr(stderr).await;
        let err = AgentError::bad_gateway("Agent internal error (code -32603)");

        let augmented = augment_via_process(&proc, &err).await;
        let msg = augmented.expect("must augment when stderr matches allowlist");
        assert!(msg.to_lowercase().contains("usage limit"), "got {msg}");
    }

    #[tokio::test]
    async fn does_not_augment_when_message_is_specific() {
        // 1BF case: SDK already gave us a real message → don't second-guess.
        let proc = spawn_with_stderr("ERROR something: usage limit exceeded").await;
        let err = AgentError::bad_gateway("Internal error: API Error: Internal server error");

        assert!(augment_via_process(&proc, &err).await.is_none());
    }

    #[tokio::test]
    async fn returns_none_when_stderr_has_no_allowlisted_keywords() {
        let stderr = "ERROR widget_loader: failed to load module 'foo'";
        let proc = spawn_with_stderr(stderr).await;
        let err = AgentError::bad_gateway("Agent internal error (code -32603)");

        assert!(augment_via_process(&proc, &err).await.is_none());
    }

    #[test]
    fn session_command_loading_preserves_empty_turn_meta() {
        let mut session = AcpSession::new(None, None, Default::default());
        let mut command = AvailableCommand::new("review", "Review the current diff");
        command.meta = Some(
            serde_json::from_value(json!({
                "completion_behavior": "neutral_tip_on_empty",
                "empty_turn_tip_code": "acp.empty_turn.choose_command",
                "empty_turn_tip_params": {
                    "command_count": 1
                }
            }))
            .unwrap(),
        );
        session.apply_advertised_commands(vec![command]);

        let items = super::slash_command_items(session.available_commands().expect("commands advertised"));

        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.command, "review");
        assert_eq!(item.description, "Review the current diff");
        assert_eq!(
            item.completion_behavior,
            Some(aionui_api_types::SlashCommandCompletionBehavior::NeutralTipOnEmpty)
        );
        assert_eq!(
            item.empty_turn_tip_code.as_deref(),
            Some("acp.empty_turn.choose_command")
        );
        assert_eq!(item.empty_turn_tip_params, Some(json!({ "command_count": 1 })));
    }

    #[test]
    fn matches_leading_slash_token_against_advertised_commands() {
        let mut command = AvailableCommand::new("ctx-flush", "Flush context");
        command.meta = Some(
            serde_json::from_value(json!({
                "completion_behavior": "neutral_tip_on_empty",
            }))
            .unwrap(),
        );

        let matched = super::matched_slash_command("/ctx-flush now", &[command]).expect("command should match");

        assert_eq!(matched.command, "ctx-flush");
        assert_eq!(
            matched.completion_behavior,
            Some(aionui_api_types::SlashCommandCompletionBehavior::NeutralTipOnEmpty)
        );
        assert_eq!(matched.empty_turn_tip_code.as_deref(), None);
    }

    #[test]
    fn persisted_thought_config_does_not_block_model_startup_seed_category() {
        let persisted_config = HashMap::from([(ConfigKey::new("effort"), ConfigValue::new("medium"))]);

        assert!(!super::has_persisted_config_for_category(
            &persisted_config,
            &SessionConfigOptionCategory::Model
        ));
    }

    #[test]
    fn persisted_thought_config_is_detected_by_known_raw_keys() {
        let persisted_config = HashMap::from([(ConfigKey::new("reasoning_effort"), ConfigValue::new("low"))]);

        assert!(super::has_persisted_config_for_category(
            &persisted_config,
            &SessionConfigOptionCategory::ThoughtLevel
        ));
    }

    // Close-reason compositional tests live in `agent_close.rs` so that
    // (a) `agent.rs` stays under the 1000-line budget, and (b) the test
    // suite for the close-path helpers sits next to the production logic.
}
