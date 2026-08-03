//! `SessionAgentTask` — adapts the clean-slate `aionui_session::SessionBackend`
//! (direct-CLI actor model for claude/codex) to origin's `IAgentTask` contract.
//!
//! Phase 1 of the session-model port (see
//! `protocols/design/session-model-port-to-origin-plan.md`). ONLY claude and codex
//! run through this; every other backend keeps the existing `AcpAgentManager` path.
//!
//! Shape: hold the `SessionBackend`, spawn one translator task that drains its
//! `events()` (`SessionEnvelope` → `SessionEvent`) and re-broadcasts as
//! `AgentStreamEvent` on the channel `subscribe()` hands out. Commands lower to
//! `SessionBackend::dispatch`.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use aionui_common::{AgentKillReason, ConversationStatus, TimestampMs, now_ms};
use aionui_session::{
    BackendError, Command, CommandMeta, ContentBlock, SessionBackend, SessionEnvelope, SessionEvent, ToolResultContent,
};
use futures_util::stream::BoxStream;
use tokio::sync::broadcast;

use crate::agent_task::IAgentTask;
use crate::error::AgentError;
use crate::protocol::events::session_updates::AvailableCommandsEventData;
use crate::protocol::events::session_updates::ThinkingEventData;
use crate::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};
use crate::protocol::events::{
    AgentStreamEvent, FinishEventData, StartEventData, TextEventData, TipType, TipsEventData,
};
use crate::protocol::send_error::AgentSendError;
use crate::shared_kernel::PersistedSessionState;
use crate::types::SendMessageData;
use aionui_api_types::AcpBuildExtra;
use aionui_common::AgentType;
use aionui_db::{IAcpSessionRepository, IMcpServerRepository, SaveRuntimeStateParams};
use aionui_realtime::EventBroadcaster;

const EVENT_CHANNEL_CAPACITY: usize = 512;

// Option ids for the generic tool-approval card. `confirm()` maps the incoming
// `data` string against these to pick the PermissionDecision; anything else is
// treated as an AskUserQuestion answer label (Approved + `selected`).
const PERM_ALLOW: &str = "allow";
const PERM_ALLOW_ALWAYS: &str = "allow_always";
const PERM_REJECT: &str = "reject";

/// The `config_selections` key under which a claude session's chosen reasoning-effort
/// level is persisted. claude emits NO `ConfigChanged` for effort (only mode/model), so
/// `set_config_option` persists it here directly and `build_session_instance` re-applies
/// it after open (there is no spawn-time effort flag; it rides a post-open
/// control_request). The three accepted incoming option ids (`effort`/`reasoning_effort`/
/// `thought_level`) all normalize to this one storage key.
const EFFORT_CONFIG_KEY: &str = "effort";

/// Resolve the reasoning-effort catalog to surface for the effort picker, mirroring the
/// backend's `effort_is_supported` current-model precedence: the efforts of the resolved
/// current model if it can be pinned, else the union across all advertised models (so we
/// don't hide a level some selectable model supports when the current model is ambiguous /
/// not-yet-known). Empty result = no effort axis → the caller omits the option entirely.
fn resolve_current_model_efforts(models: &[aionui_session::ModelInfo], current_model: Option<&str>) -> Vec<String> {
    if let Some(model) = current_model.and_then(|id| models.iter().find(|m| m.id == id)) {
        return model.reasoning_efforts.clone();
    }
    let mut union: Vec<String> = Vec::new();
    for m in models {
        for e in &m.reasoning_efforts {
            if !union.contains(e) {
                union.push(e.clone());
            }
        }
    }
    union
}

/// Shared, cheaply-cloneable runtime state for a session task: the broadcast sender
/// the translator writes and `subscribe()` reads, plus liveness bookkeeping.
struct SessionRuntime {
    tx: broadcast::Sender<AgentStreamEvent>,
    last_activity_ms: AtomicI64,
    /// Coarse status derived from the FSM edge the translator observes.
    status: std::sync::Mutex<Option<ConversationStatus>>,
    /// The CLI-assigned backend session id, learned from `BackendBound`. The ACP
    /// path stamps every Start/Finish with its session id; we mirror that so the
    /// frontend + resume-anchor consumer see the same id. `None` until the backend
    /// binds (first turn); a resume seeds it via the first BackendBound echo.
    session_id: std::sync::Mutex<Option<String>>,
    /// Optimistic mode/model selections set via `set_config_option`. The frontend's
    /// `hasObservedValue` contract requires set_config_option to return
    /// `confirmation: Observed` AND the option's `current_value == requested` — but
    /// claude's `capabilities()` does NOT reflect an in-band switch synchronously
    /// (set_model has NO confirmation wire at all; set_permission_mode confirms only
    /// asynchronously via a later `system/status`). So we cache the requested value
    /// here at dispatch time and have `get_config_options`/`mode`/`get_model` prefer
    /// it over the (stale) capabilities snapshot — the same optimistic-override the
    /// clean-slate runtime applies. Cleared/overwritten on the next switch.
    mode_override: std::sync::Mutex<Option<String>>,
    model_override: std::sync::Mutex<Option<String>>,
    /// Optimistic reasoning-effort ("thought level") selection, symmetric with
    /// mode/model. claude emits NO `ConfigChanged`/echo for effort (unlike model/mode),
    /// so the streaming catalog push — which runs in the backend-Arc-free event pump and
    /// cannot read `capabilities().current_effort` — reads the highlight from here. REST
    /// (`get_config_options`) prefers this over the (synchronously-seeded) caps value so
    /// the observed re-read confirms the switch. `None` until the user picks a level.
    effort_override: std::sync::Mutex<Option<String>>,
}

impl SessionRuntime {
    fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }
    fn set_status(&self, s: ConversationStatus) {
        if let Ok(mut g) = self.status.lock() {
            *g = Some(s);
        }
    }
    fn set_session_id(&self, id: String) {
        if let Ok(mut g) = self.session_id.lock() {
            *g = Some(id);
        }
    }
    fn session_id(&self) -> Option<String> {
        self.session_id.lock().ok().and_then(|g| g.clone())
    }
    fn set_mode_override(&self, mode: String) {
        if let Ok(mut g) = self.mode_override.lock() {
            *g = Some(mode);
        }
    }
    fn mode_override(&self) -> Option<String> {
        self.mode_override.lock().ok().and_then(|g| g.clone())
    }
    fn set_model_override(&self, model: String) {
        if let Ok(mut g) = self.model_override.lock() {
            *g = Some(model);
        }
    }
    fn model_override(&self) -> Option<String> {
        self.model_override.lock().ok().and_then(|g| g.clone())
    }
    fn set_effort_override(&self, effort: String) {
        if let Ok(mut g) = self.effort_override.lock() {
            *g = Some(effort);
        }
    }
    fn effort_override(&self) -> Option<String> {
        self.effort_override.lock().ok().and_then(|g| g.clone())
    }

    /// Atomic clean-converge frame: if not already `Finished`, set status ←
    /// `Finished` AND broadcast a clean `Finish` on `tx`. Idempotent in the
    /// `Finished` absorbing state (a repeat cancel / a late real Finish is a
    /// no-op — no second broadcast). This is the precise isomorph of the ACP
    /// path's `AgentRuntime::emit_finish`: it drives the SAME convergence chain
    /// (relay break → orchestrator releases the turn claim → `cancelling`
    /// cleared → `Idle`) so the gate recovers in seconds on the `UserCancel`
    /// force-kill path, WITHOUT waiting for the workflow to finish naturally.
    /// It is emitted for a `UserCancelTimeout` kill BEFORE the process is torn
    /// down, so the relay returns clean (a "cancelled", not a red crash card).
    fn emit_finish_once(&self) {
        let already = {
            let mut g = self.status.lock().unwrap_or_else(|e| e.into_inner());
            let was = matches!(*g, Some(ConversationStatus::Finished));
            if !was {
                *g = Some(ConversationStatus::Finished);
            }
            was
        };
        if already {
            return;
        }
        let _ = self.tx.send(AgentStreamEvent::Finish(FinishEventData {
            session_id: self.session_id(),
        }));
    }
}

/// Cold-start catalog snapshot extracted from a persisted `agent_metadata`
/// handshake, in the SAME `aionui_session` shape the getters read off live
/// `capabilities()` — so serving the preload is a drop-in fallback with no shape
/// translation at read time. Empty vectors + `None` currents = nothing persisted.
#[derive(Default, Clone)]
struct CatalogPreload {
    available_models: Vec<aionui_session::ModelInfo>,
    current_model: Option<String>,
    available_modes: Vec<aionui_session::ModeInfo>,
    current_mode: Option<String>,
}

impl CatalogPreload {
    /// Parse the persisted handshake's `available_models` / `available_modes`
    /// columns into the live-capabilities shape. Reuses the ACP path's
    /// `extract_models_from_value` / `extract_modes_from_value` (the same
    /// multi-shape parser that accepts both the `{available_models:[{id,label}]}`
    /// column shape `spawn_catalog_writeback` persists AND a live-claude handshake),
    /// so the two paths stay byte-compatible. `reasoning_efforts` is intentionally
    /// dropped: the handshake catalog does not carry per-model efforts, and the
    /// getters this feeds do not surface efforts.
    fn from_handshake(handshake: &aionui_api_types::AgentHandshake) -> Self {
        use crate::manager::acp::config_option_catalog::{extract_models_from_value, extract_modes_from_value};
        let (available_models, current_model) = handshake
            .available_models
            .as_ref()
            .and_then(extract_models_from_value)
            .map(|state| {
                let models = state
                    .available_models
                    .iter()
                    .map(|m| aionui_session::ModelInfo {
                        id: m.model_id.to_string(),
                        name: m.name.clone(),
                        description: m.description.clone(),
                        reasoning_efforts: Vec::new(),
                    })
                    .collect::<Vec<_>>();
                let current = state.current_model_id.to_string();
                (models, (!current.is_empty()).then_some(current))
            })
            .unwrap_or_default();
        let (available_modes, current_mode) = handshake
            .available_modes
            .as_ref()
            .and_then(extract_modes_from_value)
            .map(|state| {
                let modes = state
                    .available_modes
                    .iter()
                    .map(|m| aionui_session::ModeInfo {
                        id: m.id.to_string(),
                        name: m.name.clone(),
                        description: m.description.clone(),
                    })
                    .collect::<Vec<_>>();
                let current = state.current_mode_id.to_string();
                (modes, (!current.is_empty()).then_some(current))
            })
            .unwrap_or_default();
        Self {
            available_models,
            current_model,
            available_modes,
            current_mode,
        }
    }
}

/// Resolved prompt-dump target for the direct-CLI (claude/codex) path.
///
/// Lifecycle: written once at task `build` time from the already-resolved
/// `<data_dir>/prompt-dumps` dir + the vendor label; read only in
/// `send_message`; never invalidated. `None` = `--dump-prompts` off (the
/// production default), in which case dumping is skipped with zero effect.
/// Carries `backend` because a `SessionBackend` exposes no vendor accessor and
/// both claude/codex present as `AgentType::Acp`, so the send-point dump could
/// not otherwise label the vendor.
#[derive(Debug, Clone)]
pub struct SessionPromptDump {
    dir: std::path::PathBuf,
    backend: &'static str,
}

/// Map the canonical multimodal block slice to raw JSON for a prompt dump.
/// Dev-only artifact: contents are kept RAW (image/audio base64 in full, text
/// verbatim) — the dump only runs under an explicit `--dump-prompts`.
fn session_content_blocks_to_json(content: &[ContentBlock]) -> Vec<serde_json::Value> {
    use base64::Engine as _;
    content
        .iter()
        .map(|b| match b {
            ContentBlock::Text(t) => serde_json::json!({ "type": "text", "text": t }),
            ContentBlock::Image { data, media_type } => serde_json::json!({
                "type": "image",
                "media_type": media_type,
                "data": base64::engine::general_purpose::STANDARD.encode(data),
            }),
            ContentBlock::Audio { data, media_type } => serde_json::json!({
                "type": "audio",
                "media_type": media_type,
                "data": base64::engine::general_purpose::STANDARD.encode(data),
            }),
            ContentBlock::ResourceLink { uri, mime_type } => serde_json::json!({
                "type": "resource_link",
                "uri": uri,
                "mime_type": mime_type,
            }),
            ContentBlock::AtMention { user_id } => serde_json::json!({
                "type": "at_mention",
                "user_id": user_id,
            }),
        })
        .collect()
}

/// One claude/codex session, presented as an `IAgentTask`.
pub struct SessionAgentTask {
    agent_type: AgentType,
    conversation_id: String,
    /// Owner Core user — every acp_session persistence write and catalog
    /// writeback is scoped to this user (multi-account boundary).
    user_id: String,
    workspace: String,
    backend: Arc<dyn SessionBackend>,
    runtime: Arc<SessionRuntime>,
    /// The `acp_session` persistence sink, retained so `set_config_option` can persist
    /// the chosen EFFORT level into `config_selections` — claude does NOT emit a
    /// `ConfigChanged` for effort (only for mode/model), so the event-pump's
    /// `persist_side_effects` never sees it. Without this write, effort would be lost
    /// across a respawn/resume (unlike mode/model, which persist via ConfigChanged).
    /// `None` (tests) = no persistence. Shared with the pump (same Arc).
    session_repo: Option<Arc<dyn IAcpSessionRepository>>,
    /// Cold-start catalog preload parsed from the persisted `agent_metadata`
    /// handshake (what a PRIOR session discovered and `spawn_catalog_writeback`
    /// stored). The backend's live `capabilities()` is empty until the initialize
    /// round-trip lands (~seconds on resume); the mode/model getters serve this in
    /// the meantime so the `/api/agents` picker is populated immediately instead of
    /// blank, then the live catalog overwrites it the moment it arrives. Empty on
    /// paths with no persisted catalog (fresh agent, tests). Mirrors the ACP path's
    /// `preload_advertised_catalogs` "fill-when-empty, live-overwrites" semantics.
    catalog_preload: CatalogPreload,
    /// Command-id counter for `CommandMeta` (dispatch correlation).
    command_seq: AtomicI64,
    /// Resolved prompt-dump target (see [`SessionPromptDump`]). `None` when
    /// `--dump-prompts` is off. Read only by `send_message`.
    prompt_dump: Option<SessionPromptDump>,
}

impl SessionAgentTask {
    /// Build a task around an already-opened `SessionBackend` and start the
    /// event-translation pump. `agent_type` is `AgentType::Acp` for claude/codex
    /// (they present as the ACP family to the rest of the app).
    ///
    /// `session_repo`, when present, is the persistence sink the event pump writes
    /// on the SAME signals the legacy ACP path persisted via
    /// `AcpSessionSyncService` (which this direct-CLI path bypasses): `BackendBound`
    /// → `acp_session.session_id` (the resume anchor `build_session_instance` reads
    /// back), `ConfigChanged` → `current_mode_id`/`current_model_id` (the mode/model
    /// precedence source). `None` (tests) = no persistence.
    pub fn new(
        agent_type: AgentType,
        conversation_id: String,
        user_id: String,
        workspace: String,
        backend: Arc<dyn SessionBackend>,
        session_repo: Option<Arc<dyn IAcpSessionRepository>>,
    ) -> Arc<Self> {
        Self::build(
            agent_type,
            conversation_id,
            user_id,
            workspace,
            backend,
            session_repo,
            CatalogPreload::default(),
            None,
            // No broadcaster: this ctor is the test/simple path, which has no
            // conversation WebSocket to push a late usage frame to.
            None,
        )
    }

    /// Same as [`new`], plus a cold-start catalog preload parsed from the
    /// persisted `agent_metadata` handshake. Production resume path uses this so
    /// the model/mode picker is populated immediately from the last discovered
    /// catalog while the backend's live `capabilities()` is still empty (the
    /// initialize round-trip lands a beat later and overwrites it).
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_preload(
        agent_type: AgentType,
        conversation_id: String,
        user_id: String,
        workspace: String,
        backend: Arc<dyn SessionBackend>,
        session_repo: Option<Arc<dyn IAcpSessionRepository>>,
        handshake: &aionui_api_types::AgentHandshake,
        prompt_dump: Option<SessionPromptDump>,
        broadcaster: Option<Arc<dyn EventBroadcaster>>,
    ) -> Arc<Self> {
        Self::build(
            agent_type,
            conversation_id,
            user_id,
            workspace,
            backend,
            session_repo,
            CatalogPreload::from_handshake(handshake),
            prompt_dump,
            broadcaster,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        agent_type: AgentType,
        conversation_id: String,
        user_id: String,
        workspace: String,
        backend: Arc<dyn SessionBackend>,
        session_repo: Option<Arc<dyn IAcpSessionRepository>>,
        catalog_preload: CatalogPreload,
        prompt_dump: Option<SessionPromptDump>,
        broadcaster: Option<Arc<dyn EventBroadcaster>>,
    ) -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let runtime = Arc::new(SessionRuntime {
            tx,
            last_activity_ms: AtomicI64::new(now_ms()),
            status: std::sync::Mutex::new(None),
            session_id: std::sync::Mutex::new(None),
            mode_override: std::sync::Mutex::new(None),
            model_override: std::sync::Mutex::new(None),
            effort_override: std::sync::Mutex::new(None),
        });
        // Subscribe to the backend's event stream HERE (sync), then hand ONLY the
        // stream to the pump — never a backend Arc (see `spawn_event_pump` for why
        // capturing a backend Arc there would leak the child process).
        let events = backend.events();
        spawn_event_pump(
            events,
            runtime.clone(),
            conversation_id.clone(),
            user_id.clone(),
            session_repo.clone(),
            broadcaster,
        );
        Arc::new(Self {
            agent_type,
            conversation_id,
            user_id,
            workspace,
            backend,
            runtime,
            session_repo,
            catalog_preload,
            command_seq: AtomicI64::new(0),
            prompt_dump,
        })
    }

    fn next_command_id(&self) -> u64 {
        self.command_seq.fetch_add(1, Ordering::Relaxed) as u64
    }

    /// DEV (`--dump-prompts`): dump this turn's final input blocks as a
    /// `session-cli-final-input` JSON, symmetric with the ACP path's
    /// `acp-final-input`. Best-effort: a failure only warns and never affects
    /// the send. No-op when `--dump-prompts` is off (`prompt_dump == None`).
    fn dump_session_cli_final_input(&self, content: &[ContentBlock], client_msg_id: Option<&str>) {
        let Some(dump) = self.prompt_dump.as_ref() else {
            return;
        };
        let input = serde_json::json!({ "content": session_content_blocks_to_json(content) });
        let resolved_context = serde_json::json!({ "workspace": &self.workspace });
        match crate::dev_prompt_dump::dump_agent_final_input(
            &dump.dir,
            crate::dev_prompt_dump::AgentFinalInputDump {
                kind: "session-cli-final-input",
                backend: dump.backend,
                conversation_id: &self.conversation_id,
                session_id: self.runtime.session_id().as_deref(),
                msg_id: client_msg_id,
                turn_id: None,
                input,
                resolved_context,
            },
        ) {
            Ok(path) => tracing::debug!(
                conversation_id = %self.conversation_id,
                path = %path.display(),
                "DEV session-cli final input dump written"
            ),
            Err(error) => tracing::warn!(
                conversation_id = %self.conversation_id,
                error = %error,
                "DEV session-cli final input dump failed"
            ),
        }
    }

    // ── enum-level helpers forwarded from AgentInstance::Session ──────────
    // Backed by the backend's cheap sync `capabilities()` snapshot (reflects
    // late model/mode/config discovery) and `dispatch` for mutations.

    /// Pending confirmations, projected from the backend's live
    /// `pending_permission_requests()`. The REST `/confirmations` recovery path
    /// (frontend `usePendingConfirmationsRecovery`) calls this on mount/reconnect to
    /// rebuild permission cards that were raised while the page was away — WITHOUT
    /// this returning them, a mid-turn permission (or AskUserQuestion) raised before
    /// the client subscribed is lost and the turn hangs forever waiting for an answer
    /// that can never be given. The card id == call_id == request_id, matching the
    /// live `AcpPermission` frame so a duplicate live+recovered pair de-dups. Options
    /// mirror the live translation: AskUserQuestion → its question options, else the
    /// generic allow/deny.
    /// Raise a tool approval that came from OUTSIDE the backend's stream and
    /// block until the user answers.
    ///
    /// Only Antigravity uses this: agy cannot prompt in headless mode, so its
    /// `PreToolUse` hook calls AionUi over HTTP instead, and that request lands
    /// here. Backends that raise permissions on their own wire return `Denied`
    /// from the trait default and never reach this path.
    pub async fn request_external_permission(
        &self,
        tool_name: String,
        input: serde_json::Value,
    ) -> aionui_session::PermissionDecision {
        self.backend.request_external_permission(tool_name, input).await
    }

    pub fn get_confirmations(&self) -> Vec<aionui_common::Confirmation> {
        self.backend
            .pending_permission_requests()
            .into_iter()
            .map(|p| {
                let is_ask = p.tool_name == "AskUserQuestion";
                let options = if is_ask {
                    ask_user_question_options(p.questions.as_ref())
                } else {
                    Vec::new()
                };
                let options = if options.is_empty() {
                    default_permission_options()
                } else {
                    options
                };
                aionui_common::Confirmation {
                    id: p.request_id.clone(),
                    call_id: p.request_id,
                    title: (!p.tool_name.is_empty()).then(|| p.tool_name.clone()),
                    action: None,
                    description: String::new(),
                    command_type: None,
                    options: options
                        .into_iter()
                        .map(|o| aionui_common::ConfirmationOption {
                            label: o.name,
                            value: serde_json::Value::String(o.option_id),
                            params: None,
                        })
                        .collect(),
                }
            })
            .collect()
    }

    /// Answer a pending permission. `data` is the option the user picked (the card
    /// echoes the option's `option_id` — a string, or `{option_id|value}` object).
    /// The picked id maps to the answer:
    ///   - `reject`        → Denied
    ///   - `allow_always`  → AllowAlways
    ///   - `allow`         → Approved
    ///   - anything else   → an AskUserQuestion answer LABEL → Approved + `selected`
    ///     (claude keys the AskUserQuestion answer by the chosen label — see
    ///     claude_conn `build_control_response`; single-select single-question path).
    ///
    /// `always_allow` (legacy flag) forces AllowAlways regardless.
    pub fn confirm(
        &self,
        _msg_id: &str,
        call_id: &str,
        data: serde_json::Value,
        always_allow: bool,
    ) -> Result<(), AgentError> {
        use aionui_session::PermissionDecision;
        let picked = confirm_option_id(&data);
        let (decision, selected) = if always_allow {
            (PermissionDecision::AllowAlways, None)
        } else {
            match picked.as_deref() {
                Some(PERM_REJECT) => (PermissionDecision::Denied, None),
                Some(PERM_ALLOW_ALWAYS) => (PermissionDecision::AllowAlways, None),
                Some(PERM_ALLOW) | None => (PermissionDecision::Approved, None),
                // A question answer label (AskUserQuestion): approve and forward the
                // label so claude records it as the chosen answer.
                Some(label) => (PermissionDecision::Approved, Some(label.to_owned())),
            }
        };
        let backend = self.backend.clone();
        let request_id = call_id.to_string();
        let conv_id = self.conversation_id.clone();
        // dispatch is async; confirm() is sync in IAgentTask's sibling API, so
        // fire-and-forget on the runtime (the answer rides the stdin FIFO). The
        // REST confirm has already returned success by the time this runs, so a
        // dispatch failure here is INVISIBLE to the caller — it MUST be surfaced
        // in the log or a wedged permission (claude blocked forever on
        // can_use_tool) is undiagnosable in production.
        tokio::spawn(async move {
            match backend
                .dispatch(Command::AnswerPermission {
                    request_id: request_id.clone(),
                    decision,
                    selected,
                    answers: Vec::new(),
                })
                .await
            {
                Ok(_) => tracing::info!(
                    conv_id = %conv_id,
                    request_id = %request_id,
                    "permission answer delivered to backend"
                ),
                Err(e) => tracing::error!(
                    conv_id = %conv_id,
                    request_id = %request_id,
                    error = %e,
                    "permission answer FAILED after REST confirm already returned success — claude stays blocked on can_use_tool"
                ),
            }
        });
        Ok(())
    }

    /// Resolve the catalog to serve: the backend's LIVE `capabilities()` when it has
    /// discovered models/modes, else the cold-start `catalog_preload` (last session's
    /// persisted handshake). The LIST and the CURRENT of each axis fall back
    /// INDEPENDENTLY — this matters at cold-start: the backend seeds `caps.current_model`
    /// /`caps.current_mode` from the RESOLVED snapshot at spawn (claude_conn spawn:
    /// `caps.current_model = config.model`, where config.model came from the persisted
    /// `current_model_id` column), so it is already the user's last interactive switch
    /// even before the initialize round-trip lands and fills `available_models`. The
    /// preload's current, by contrast, is frozen at the PRIOR session's write-back
    /// (`spawn_catalog_writeback` runs once at open, not on a mid-turn switch), so a
    /// list-emptiness-gated fallback would show a stale model in the pre-init window
    /// whenever the user switched mid-turn last session (backend runs the right model,
    /// picker briefly showed the old one). So: the LIST prefers live (non-empty) then
    /// preload; the CURRENT prefers `caps` (already snapshot-seeded) then preload.
    /// `None`-current + no preload = None (getters then lean on the runtime override).
    fn effective_catalog(
        &self,
    ) -> (
        Vec<aionui_session::ModelInfo>,
        Option<String>,
        Vec<aionui_session::ModeInfo>,
        Option<String>,
    ) {
        let caps = self.backend.capabilities();
        let models = if caps.available_models.is_empty() {
            self.catalog_preload.available_models.clone()
        } else {
            caps.available_models
        };
        let current_model = caps
            .current_model
            .or_else(|| self.catalog_preload.current_model.clone());
        let modes = if caps.available_modes.is_empty() {
            self.catalog_preload.available_modes.clone()
        } else {
            caps.available_modes
        };
        let current_mode = caps.current_mode.or_else(|| self.catalog_preload.current_mode.clone());
        (models, current_model, modes, current_mode)
    }

    /// Current mode: the optimistic override (last `set_config_option("mode")`) wins
    /// over the capabilities snapshot, which lags an in-band switch.
    pub async fn mode(&self) -> Result<aionui_api_types::AgentModeResponse, AgentError> {
        let caps = self.backend.capabilities();
        // Preload fallback: on cold-start resume the live catalog is empty until the
        // initialize round-trip lands, so serve the last-discovered current_mode
        // until then (override still wins; live current_mode overwrites once present).
        let current_mode = caps.current_mode.or_else(|| self.catalog_preload.current_mode.clone());
        Ok(aionui_api_types::AgentModeResponse {
            mode: self.runtime.mode_override().or(current_mode).unwrap_or_default(),
            initialized: true,
        })
    }

    /// Current model + catalog. The optimistic override (last set_config_option
    /// "model") wins over the capabilities snapshot (claude gives set_model no
    /// confirmation wire, so caps.current_model never reflects the switch).
    pub async fn get_model(&self) -> Result<aionui_api_types::GetModelInfoResponse, AgentError> {
        // Live catalog wins; cold-start resume falls back to the persisted-handshake
        // preload so the picker is populated before the initialize round-trip lands.
        let (models, current_model, _modes, _mode) = self.effective_catalog();
        let override_model = self.runtime.model_override();
        if models.is_empty() && current_model.is_none() && override_model.is_none() {
            return Ok(aionui_api_types::GetModelInfoResponse { model_info: None });
        }
        let available_models: Vec<aionui_api_types::ModelInfoEntry> = models
            .iter()
            .map(|m| aionui_api_types::ModelInfoEntry {
                id: m.id.clone(),
                label: m.name.clone(),
            })
            .collect();
        let current_id = override_model.or(current_model);
        let current_label = current_id
            .as_ref()
            .and_then(|id| available_models.iter().find(|e| &e.id == id).map(|e| e.label.clone()));
        Ok(aionui_api_types::GetModelInfoResponse {
            model_info: Some(aionui_api_types::ModelInfoPayload {
                current_model_id: current_id.clone(),
                current_model_label: current_label.or(current_id),
                available_models,
            }),
        })
    }

    /// Config-options (mode + model selects). For each select the optimistic override
    /// (last set_config_option) wins over the capabilities snapshot's current_value —
    /// this is what makes set_config_option's observed re-read succeed (the snapshot
    /// lags an in-band claude switch).
    pub async fn get_config_options(&self) -> Result<aionui_api_types::GetConfigOptionsResponse, AgentError> {
        // Live catalog wins; cold-start resume falls back to the persisted-handshake
        // preload (per-axis) so the picker renders before the initialize round-trip lands.
        let (models, current_model, modes, current_mode) = self.effective_catalog();
        // The effort catalog depends on the EFFECTIVE current model (override wins over the
        // snapshot's current_model), resolved before the model option consumes it below.
        let effective_model = self.runtime.model_override().or_else(|| current_model.clone());
        let mut config_options = Vec::new();
        if !modes.is_empty() {
            config_options.push(aionui_api_types::AcpConfigOptionDto {
                id: "mode".into(),
                name: Some("Mode".into()),
                label: None,
                description: None,
                category: Some("mode".into()),
                option_type: "select".into(),
                current_value: self.runtime.mode_override().or(current_mode),
                options: modes
                    .iter()
                    .map(|m| aionui_api_types::AcpConfigSelectOptionDto {
                        value: m.id.clone(),
                        name: Some(m.name.clone()),
                        label: None,
                        description: m.description.clone(),
                    })
                    .collect(),
            });
        }
        if !models.is_empty() {
            config_options.push(aionui_api_types::AcpConfigOptionDto {
                id: "model".into(),
                name: Some("Model".into()),
                label: None,
                description: None,
                category: Some("model".into()),
                option_type: "select".into(),
                current_value: self.runtime.model_override().or(current_model),
                options: models
                    .iter()
                    .map(|m| aionui_api_types::AcpConfigSelectOptionDto {
                        value: m.id.clone(),
                        name: Some(m.name.clone()),
                        label: None,
                        description: m.description.clone(),
                    })
                    .collect(),
            });
        }
        // Reasoning-effort ("thought level") axis — the direct-CLI analogue of the ACP
        // path's `thought_level` config option (category-keyed so AionUi's
        // `deriveSelectOption(..., 'thought_level', ['reasoning_effort'])` lights the
        // picker's effort group). Only claude advertises per-model `supportedEffortLevels`;
        // the option is emitted only when the resolved current model actually offers
        // efforts. `current_value` prefers the optimistic override (claude emits no echo
        // for effort) then the backend's synchronously-seeded `current_effort`.
        let caps = self.backend.capabilities();
        let efforts = resolve_current_model_efforts(&models, effective_model.as_deref());
        if !efforts.is_empty() {
            config_options.push(aionui_api_types::AcpConfigOptionDto {
                id: "reasoning_effort".into(),
                name: Some("Thinking".into()),
                label: None,
                description: None,
                category: Some("thought_level".into()),
                option_type: "select".into(),
                current_value: self.runtime.effort_override().or(caps.current_effort),
                options: efforts
                    .iter()
                    .map(|e| aionui_api_types::AcpConfigSelectOptionDto {
                        value: e.clone(),
                        name: Some(e.clone()),
                        label: None,
                        description: None,
                    })
                    .collect(),
            });
        }
        Ok(aionui_api_types::GetConfigOptionsResponse { config_options })
    }

    /// Apply a config option (mode/model/other) via dispatch.
    pub async fn set_config_option(
        &self,
        option_id: &str,
        value: &str,
    ) -> Result<aionui_api_types::SetConfigOptionResponse, AgentError> {
        // Validate a runtime mode/model switch against the advertised catalog BEFORE
        // dispatch — the ACP `clear_invalid_desired_*` semantic, but as REJECT+report
        // (not silent-drop) since this is an explicit user action at the single runtime
        // chokepoint. An EMPTY / not-yet-discovered catalog is permissive (matches ACP
        // `is_mode_valid`/`is_model_valid`: an absent catalog cannot invalidate — the
        // capabilities snapshot may simply not have the list yet). Only a NON-empty
        // catalog that omits `value` rejects. Other option ids (effort/thought_level)
        // are validated by the backend itself (claude effort catalog check).
        let caps = self.backend.capabilities();
        // A NON-empty catalog that omits `value` is the only rejection case (empty
        // catalog = permissive, per the comment above). `known` = catalog carries value.
        let invalid = |catalog_has_value: bool, catalog_empty: bool| !catalog_empty && !catalog_has_value;
        match option_id {
            "mode"
                if invalid(
                    caps.available_modes.iter().any(|m| m.id == value),
                    caps.available_modes.is_empty(),
                ) =>
            {
                return Err(AgentError::bad_request(format!(
                    "mode '{value}' is not one of the available modes"
                )));
            }
            "model"
                if invalid(
                    caps.available_models.iter().any(|m| m.id == value),
                    caps.available_models.is_empty(),
                ) =>
            {
                return Err(AgentError::bad_request(format!(
                    "model '{value}' is not one of the available models"
                )));
            }
            _ => {}
        }
        let cmd = match option_id {
            "mode" => Command::SetMode {
                mode: value.to_string(),
            },
            "model" => Command::SetModel {
                model: value.to_string(),
            },
            other => Command::SetConfigOption {
                option_id: other.to_string(),
                value: value.to_string(),
            },
        };
        self.backend
            .dispatch(cmd)
            .await
            .map_err(|e| AgentError::bad_request(e.to_string()))?;
        // Cache the requested value as an optimistic override for mode/model, then
        // re-read the config-options snapshot so the response satisfies the frontend's
        // `hasObservedValue` contract (confirmation == Observed AND the option's
        // current_value == requested). This is required because claude's own
        // `capabilities()` does NOT reflect an in-band switch synchronously (set_model
        // has no confirmation wire; set_permission_mode confirms only via a later
        // async system/status), so without the override the option would never read
        // back as observed and the frontend would reject the switch as `command_ack`.
        // Mirrors the clean-slate runtime's optimistic override + observed re-read.
        // effort/thought_level is now a surfaced picker option too (id `reasoning_effort`,
        // category `thought_level`), so it also caches an override + falls through to the
        // observed re-read — the frontend's `hasObservedValue` requires Observed AND the
        // option's current_value == requested, same as mode/model.
        match option_id {
            "mode" => self.runtime.set_mode_override(value.to_string()),
            "model" => self.runtime.set_model_override(value.to_string()),
            "effort" | "reasoning_effort" | "thought_level" => {
                // Optimistic highlight: claude emits no effort echo, so the streaming
                // catalog push reads the current level from this override.
                self.runtime.set_effort_override(value.to_string());
                // Persist the chosen effort into `config_selections` so it survives a
                // respawn/resume. Unlike mode/model (persisted by the pump on
                // ConfigChanged), claude emits no ConfigChanged for effort, so this is
                // the ONLY place the choice is durably recorded. Backend already accepted
                // + validated it (dispatch above); best-effort persist (a DB failure must
                // not fail the switch the CLI already applied).
                self.persist_effort(value).await;
            }
            _ => {
                return Ok(aionui_api_types::SetConfigOptionResponse {
                    confirmation: aionui_api_types::ConfigOptionConfirmation::CommandAck,
                    config_options: None,
                });
            }
        }
        let snapshot = self.get_config_options().await?;
        // Effort is emitted under the canonical id `reasoning_effort` (category
        // `thought_level`); a caller may address it via any of its aliases, so match by
        // category for the effort axis and by id otherwise.
        let is_effort_alias = matches!(option_id, "effort" | "reasoning_effort" | "thought_level");
        let observed = snapshot
            .config_options
            .iter()
            .find(|o| {
                if is_effort_alias {
                    o.category.as_deref() == Some("thought_level")
                } else {
                    o.id == option_id
                }
            })
            .and_then(|o| o.current_value.as_deref())
            == Some(value);
        Ok(aionui_api_types::SetConfigOptionResponse {
            confirmation: if observed {
                aionui_api_types::ConfigOptionConfirmation::Observed
            } else {
                aionui_api_types::ConfigOptionConfirmation::CommandAck
            },
            config_options: Some(snapshot.config_options),
        })
    }

    /// Persist the chosen effort level into `acp_session.config_selections` (under
    /// [`EFFORT_CONFIG_KEY`]) so it survives a respawn/resume. Reads the existing
    /// selections first and MERGES (rather than overwriting the whole map) so any other
    /// future config key is preserved. Best-effort: a repo miss/failure is logged, not
    /// propagated — the backend already applied the effort, and losing only the
    /// persistence (not the live switch) is the safe degradation. No-op without a repo.
    async fn persist_effort(&self, value: &str) {
        let Some(repo) = self.session_repo.as_ref() else {
            return;
        };
        // Merge into the existing selection map (preserve unrelated keys).
        let mut selections: std::collections::HashMap<String, String> = match repo
            .load_runtime_state_for_user(&self.user_id, &self.conversation_id)
            .await
        {
            Ok(Some(state)) => state
                .config_selections_json
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or_default(),
            Ok(None) => std::collections::HashMap::new(),
            Err(err) => {
                tracing::warn!(conversation_id = %self.conversation_id, error = %err, "persist_effort: load_runtime_state failed; skipping effort persist");
                return;
            }
        };
        selections.insert(EFFORT_CONFIG_KEY.to_owned(), value.to_owned());
        let json = match serde_json::to_string(&selections) {
            Ok(j) => j,
            Err(err) => {
                tracing::warn!(conversation_id = %self.conversation_id, error = %err, "persist_effort: encode config_selections failed");
                return;
            }
        };
        let params = SaveRuntimeStateParams {
            config_selections_json: Some(Some(&json)),
            ..Default::default()
        };
        if let Err(err) = repo
            .save_runtime_state_for_user(&self.user_id, &self.conversation_id, &params)
            .await
        {
            tracing::warn!(conversation_id = %self.conversation_id, error = %err, "persist_effort: save_runtime_state failed");
        }
    }

    /// Session usage snapshot, served from the persisted `context_usage` runtime
    /// state that the event pump writes on every `UsageDelta`.
    ///
    /// Reading through the repo (rather than an in-memory field) is what makes the
    /// figure survive a conversation switch: the task is dropped when the session
    /// is reaped, so anything held only in memory would be gone and the indicator
    /// would sit blank until the next turn produced fresh usage. Without a repo
    /// (tests) or with no row yet, `None` — the indicator simply stays empty.
    pub async fn get_usage(&self) -> Result<Option<serde_json::Value>, AgentError> {
        let Some(repo) = self.session_repo.as_ref() else {
            return Ok(None);
        };
        let state = repo
            .load_runtime_state_for_user(&self.user_id, &self.conversation_id)
            .await
            .map_err(|e| AgentError::internal(format!("Failed to load usage state: {e}")))?;
        Ok(state
            .and_then(|s| s.context_usage_json)
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok()))
    }

    /// Slash commands from the live capabilities snapshot.
    pub async fn get_slash_commands(&self) -> Result<Vec<aionui_api_types::SlashCommandItem>, AgentError> {
        let caps = self.backend.capabilities();
        Ok(caps
            .slash_commands
            .iter()
            .map(|c| aionui_api_types::SlashCommandItem {
                command: c.name.clone(),
                description: c.description.clone().unwrap_or_default(),
                completion_behavior: None,
                empty_turn_tip_code: None,
                empty_turn_tip_params: None,
            })
            .collect())
    }
}

#[async_trait::async_trait]
impl IAgentTask for SessionAgentTask {
    fn agent_type(&self) -> AgentType {
        self.agent_type
    }

    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    fn workspace(&self) -> &str {
        &self.workspace
    }

    fn status(&self) -> Option<ConversationStatus> {
        self.runtime.status.lock().ok().and_then(|g| *g)
    }

    fn last_activity_at(&self) -> TimestampMs {
        self.runtime.last_activity_ms.load(Ordering::Relaxed)
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.runtime.tx.subscribe()
    }

    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        self.runtime.touch();
        let mut content: Vec<ContentBlock> = Vec::new();
        if !data.content.is_empty() {
            content.push(ContentBlock::Text(data.content));
        }
        for path in data.files {
            // File paths ride as resource links; the claude/codex adapters resolve
            // them (Read tool / base64) at dispatch time.
            content.push(ContentBlock::ResourceLink {
                uri: path,
                mime_type: None,
            });
        }
        // DEV (`--dump-prompts`): borrow the final blocks BEFORE they move into
        // Command::Send. No-op / best-effort — never affects the dispatch.
        self.dump_session_cli_final_input(&content, Some(data.msg_id.as_str()));

        let cmd = Command::Send {
            content,
            metadata: CommandMeta {
                command_id: self.next_command_id(),
                cwd: None,
                extra_args: Vec::new(),
                client_msg_id: Some(data.msg_id),
            },
        };
        // Emit the turn-start lifecycle frame BEFORE dispatch, exactly like the ACP
        // path (agent_session_flow.rs emits Start{session_id} right before prompt()).
        // The backend's own turn-start signal (claude/codex PromptAccepted) arrives
        // AFTER the first text delta, so it cannot drive an at-the-front Start — the
        // send call is the correct, ordering-stable anchor. session_id is None on the
        // very first turn (backend not yet bound) and filled on every subsequent turn.
        let _ = self.runtime.tx.send(AgentStreamEvent::Start(StartEventData {
            session_id: self.runtime.session_id(),
        }));
        self.runtime.set_status(ConversationStatus::Running);
        match self.backend.dispatch(cmd).await {
            Ok(_receipt) => Ok(()),
            // Dead resume anchor surfaced at DISPATCH time (codex rejected the
            // thread/resume → bound-thread poison, ELECTRON-3Q0): the stored anchor
            // can never resume again — clear it NOW so the auto-replay (and any
            // later send) rebuilds Fresh. The stream-side self-heal
            // (`is_dead_resume_anchor`) cannot cover this path: a dispatch error
            // never becomes a `TurnResult` on the event pump.
            Err(BackendError::SessionNotFound(detail)) => {
                if let Some(repo) = self.session_repo.as_ref() {
                    match repo
                        .clear_session_id_for_user(&self.user_id, &self.conversation_id)
                        .await
                    {
                        Ok(_) => tracing::info!(
                            conversation_id = %self.conversation_id,
                            "send: cleared dead resume anchor (backend session not found) — next attempt opens Fresh"
                        ),
                        Err(err) => tracing::warn!(
                            conversation_id = %self.conversation_id,
                            error = %err,
                            "send: clear_session_id failed"
                        ),
                    }
                }
                // The "Session not found" prefix classifies as
                // `UserAgentSessionNotFound` (retryable) so `TurnRecoveryPolicy`
                // auto-replays once — with the anchor cleared above, the replay
                // opens Fresh and recovers transparently.
                Err(AgentSendError::from_agent_error(AgentError::not_found(format!(
                    "Session not found: {detail}"
                ))))
            }
            Err(e) => Err(AgentSendError::from_agent_error(AgentError::bad_gateway(e.to_string()))),
        }
    }

    async fn cancel(&self) -> Result<(), AgentError> {
        self.runtime.touch();
        self.backend
            .dispatch(Command::Cancel {
                target: aionui_session::CancelTarget::Turn,
            })
            .await
            .map(|_| ())
            .map_err(|e| AgentError::internal(e.to_string()))
    }

    fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        // For the `UserCancelTimeout` force-kill path, Drop-driven teardown is NOT
        // enough: during an in-flight turn the orchestrator legitimately holds an
        // `Arc<SessionAgentTask>` clone, so `tasks.remove()` does not drop the last
        // reference, `Drop` never fires, and the CLI + its workflow keep running
        // until the workflow ends naturally (~minutes) — the user is gated the whole
        // time (ELECTRON-3RW). So we (1) emit a clean `Finish` FIRST to converge the
        // turn (gate recovers in seconds, no crash card), then (2) delegate real
        // process teardown to the backend, which kills the process tree WITHOUT
        // waiting for the last Arc to drop.
        if matches!(reason, Some(AgentKillReason::UserCancelTimeout)) {
            tracing::info!(
                conversation_id = %self.conversation_id,
                "session kill(UserCancelTimeout): emitted clean Finish + delegating backend terminate (was Drop-only no-op)"
            );
            // 1) clean converge FIRST: relay breaks → orchestrator releases the turn
            //    claim → `cancelling` cleared → gate recovers (no red crash card).
            self.runtime.emit_finish_once();
            // 2) then real process teardown, fire-and-forget on this sync entry
            //    (the awaitable variant is `kill_and_wait`).
            let backend = self.backend.clone();
            tokio::spawn(async move {
                backend.terminate().await;
            });
            return Ok(());
        }
        // Non-`UserCancelTimeout` (idle cleanup): unchanged Drop-driven teardown. The
        // `SessionBackend` trait exposes no close/shutdown command; the sole reaper is
        // `Drop for ClaudeSessionBackend` / `CodexSessionBackend`, which aborts the
        // reader and `kill_on_drop`s the child once the last `Arc` is released. An idle
        // kill has no in-flight orchestrator Arc, so that removal drops the last Arc and
        // fires the backend's `Drop`. Nothing synchronous to do here.
        Ok(())
    }
}

impl SessionAgentTask {
    /// Awaitable force-kill (aligns with `AcpAgentManager::kill_and_wait`): the
    /// awaitable teardown entry the `UserCancel` watchdog uses. For a
    /// `UserCancelTimeout` kill it emits the clean `Finish` synchronously FIRST
    /// (so the gate recovers before the returned future is even polled), then
    /// returns a future that awaits the backend's real process teardown. Any
    /// other reason keeps the pre-existing Drop-driven semantics (nothing to
    /// await). The strict order — clean converge, THEN terminate — is what keeps
    /// the kill from surfacing as a crash (see `SuspendController::terminate`).
    pub fn kill_and_wait(
        &self,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        if matches!(reason, Some(AgentKillReason::UserCancelTimeout)) {
            tracing::info!(
                conversation_id = %self.conversation_id,
                "session kill_and_wait(UserCancelTimeout): emitted clean Finish + awaiting backend terminate"
            );
            self.runtime.emit_finish_once(); // clean converge FIRST (sync)
            let backend = self.backend.clone();
            Box::pin(async move { backend.terminate().await }) // then terminate
        } else {
            Box::pin(std::future::ready(())) // unchanged (Drop-driven)
        }
    }
}

/// Open a claude/codex `SessionBackend` via the clean-slate connection and wrap it
/// as an `AgentInstance::Session`. Called from the ACP factory when the resolved
/// backend is claude/codex and a spawner is available. `backend_label` is the
/// authoritative vendor ("claude"/"codex"); other labels return `None` so the caller
/// falls back to the ACP manager path.
/// Everything the caller (`factory::acp::build`) already resolved and that the
/// session assembly needs. Bundled so `build_session_instance` is the SINGLE
/// place that maps an ACP build request → the clean-slate `SessionSpec`/
/// `SessionConfig`, mirroring clean-slate's `build_runtime` (spec_and_config +
/// resolve_session_init + the per-backend spawn_env/sandbox/approval seams). Every
/// field here has a 1:1 counterpart in that path.
pub struct SessionBuildInputs<'a> {
    /// The conversation this session belongs to (the clean-slate `session_id`).
    pub conversation_id: String,
    /// Owner Core user. Scopes every acp_session persistence write, the MCP
    /// server resolution, and the catalog writeback (multi-account boundary).
    pub user_id: String,
    /// The resolved workspace path (`SessionConfig.cwd`).
    pub workspace: String,
    /// The conversation's persisted build `extra` (mode/model/mcp/preset/skills).
    pub config: &'a AcpBuildExtra,
    /// The resolved catalog row. Used to normalize the persisted/requested mode
    /// alias (`yolo`/`yoloNoSandbox` → the row's `yolo_id`; codex `default`/`autoEdit`
    /// → `auto`) into the backend-native mode id, exactly as the ACP path does via
    /// `initial_mode_from_params`. Without this a conversation persisted with a
    /// generic alias resumes by handing the raw alias to the backend (claude rejects
    /// an unknown permission-mode id; codex gets a non-native mode → wrong policy).
    pub metadata: &'a aionui_api_types::AgentMetadata,
    /// The persisted runtime snapshot, when present. Its `current_mode_id` /
    /// `current_model_id` are the interactive-switch-persisted selections and take
    /// precedence over the create-time `config` values — the same precedence
    /// clean-slate's `spec_and_config` applies (`current_mode_id` ⟶ `session_mode`).
    pub session_snapshot: Option<&'a PersistedSessionState>,
    /// The CLI-assigned backend session id anchor. `Some` ⇒ `SessionSpec::Resume`
    /// (the same signal clean-slate's `spec_and_config` uses); `None` ⇒ `Fresh`.
    pub backend_session_id: Option<String>,
    /// User-configured MCP server repository (feature ELECTRON-1JG). `None` on
    /// paths that never inject MCP (tests) ⇒ no injection.
    pub mcp_server_repo: Option<&'a Arc<dyn IMcpServerRepository>>,
    /// The conversation runtime context env (`AIONUI_USER_ID` /
    /// `AIONUI_CONVERSATION_ID` / `AIONUI_HELPER_BIN` / `AIONUI_BASE_URL` /
    /// `AIONUI_RUNTIME_TOKEN`, filled by `apply_conversation_runtime_context`).
    /// The legacy ACP path injects these into every agent spawn via
    /// `apply_acp_launch_policy`; the direct-CLI path forwards them through
    /// `SessionConfig.spawn_env` so team/helper tooling inside the agent process
    /// keeps working. Empty ⇒ nothing injected.
    pub runtime_env: &'a [(String, String)],
    /// Broadcaster forwarded to the MCP resolver for runtime-resolution reporting
    /// parity with the legacy ACP path.
    pub broadcaster: Arc<dyn EventBroadcaster>,
    /// The resolved catalog row id + the registry's catalog sender, used to write
    /// the backend's discovered modes/models/commands back into `agent_metadata`
    /// (GAP #7 / G5) so the `/api/agents` picker stays fresh. `None` on paths that
    /// have no catalog row to refresh.
    pub catalog_writeback: Option<(String, crate::registry::CatalogSender)>,
    /// The `acp_session` persistence sink. The event pump writes the resume anchor
    /// (`BackendBound` → `session_id`) + observed mode/model (`ConfigChanged`) here —
    /// the writes the legacy ACP path performed via `AcpSessionSyncService`, which
    /// this direct-CLI path bypasses. `None` (tests) = no persistence.
    pub acp_session_repo: Option<Arc<dyn IAcpSessionRepository>>,
    /// DEV (`--dump-prompts`): the already-resolved `<data_dir>/prompt-dumps`
    /// dir, or `None` when off. `build_session_instance` uses it for the
    /// spawn-time `session-cli-config` dump AND threads it (with the vendor
    /// label) into the `SessionAgentTask` for the send-time dump.
    pub prompt_dump_dir: Option<std::path::PathBuf>,
    /// Antigravity only: the prepared `.agents/hooks.json` body, so a session
    /// switched out of full auto can restore its approval gate.
    pub permission_hook_body: Option<String>,
}

/// The mode a session actually starts on.
///
/// The persisted snapshot wins over the create-time seed: it is where a runtime
/// switch lands (`save_acp_runtime_mode`) and where a scheduled run records the
/// full-auto mode it resolved. Reading only `config.session_mode` would see the
/// value the conversation was created with and miss both.
///
/// Aliases are normalized to the backend-native id here, so callers compare
/// against catalog values rather than whatever spelling reached the API.
pub(crate) fn resolved_session_mode(
    config: &AcpBuildExtra,
    session_snapshot: Option<&PersistedSessionState>,
    metadata: &aionui_api_types::AgentMetadata,
) -> Option<String> {
    session_snapshot
        .and_then(|s| s.current_mode_id.as_ref().map(|m| m.as_str().to_owned()))
        .or_else(|| config.session_mode.clone())
        .map(|m| crate::manager::acp::mode_normalize::normalize_requested_mode(metadata, &m))
        .filter(|s| !s.is_empty())
}

/// The pure spec + mode/model mapping — the sibling of clean-slate's
/// `spec_and_config`. Extracted from `build_session_instance` so it is unit-testable
/// without spawning a backend.
///
/// - Resume when the row carries a `backend_session_id` anchor, else Fresh (both key
///   on the conversation id).
/// - `mode`: the interactive-switch-persisted `snapshot.current_mode_id` wins over the
///   create-time `config.session_mode`; empty-filtered; NO default minted (each backend
///   safe-defaults).
/// - `model`: symmetric — `snapshot.current_model_id` wins over `config.current_model_id`.
///   A BARE runtime model id (never the JSON `ProviderWithModel` blob — clean-slate #7).
fn spec_mode_model(
    conversation_id: &str,
    backend_session_id: Option<String>,
    config: &AcpBuildExtra,
    session_snapshot: Option<&PersistedSessionState>,
    metadata: &aionui_api_types::AgentMetadata,
) -> (aionui_session::SessionSpec, Option<String>, Option<String>) {
    use aionui_session::SessionSpec;
    let spec = match &backend_session_id {
        Some(_) => SessionSpec::Resume {
            session_id: conversation_id.to_owned(),
            backend_session_id,
        },
        None => SessionSpec::Fresh {
            session_id: conversation_id.to_owned(),
        },
    };
    // Normalize the resolved mode alias into the backend-native id — the SAME
    // transform the ACP path applies in `initial_mode_from_params`. AionUi persists
    // generic aliases (`yolo`/`yoloNoSandbox`; codex `default`/`autoEdit`); handing
    // those raw to the backend on resume rejects (claude unknown permission-mode) or
    // mis-policies (codex non-native mode). `normalize_requested_mode` maps them via
    // the catalog row's `yolo_id` / backend label; a mode without an alias passes
    // through unchanged. Runs BEFORE the codex sandbox/approval derivation downstream
    // (which matches both the alias and the native id, so ordering is safe).
    let mode = resolved_session_mode(config, session_snapshot, metadata);
    let model = session_snapshot
        .and_then(|s| s.current_model_id.as_ref().map(|m| m.as_str().to_owned()))
        .or_else(|| config.current_model_id.clone())
        .filter(|s| !s.is_empty());
    (spec, mode, model)
}

/// Build a claude/codex `SessionAgentTask` (the session-model port's `IAgentTask`)
/// from a resolved ACP build request, or `Ok(None)` for a non-session backend.
///
/// This is the faithful port of clean-slate `build_runtime`'s per-conversation
/// assembly (`crates/aionui-app/src/session_runtime/mod.rs`): it resolves the
/// resume spec, the mode/model precedence, the MCP + preset + skills init surface,
/// the claude cc-switch provider env, and the codex sandbox/approval policy — so a
/// Build an Antigravity (`agy` CLI) `SessionAgentTask`.
///
/// Deliberately SEPARATE from [`build_session_instance`]: that function carries
/// claude/codex-private assembly — cc-switch provider env, the codex
/// sandbox/approval derivation, persisted-effort replay, and a binary
/// `if claude { "claude" } else { "codex" }` label that would silently
/// mislabel any third backend. Antigravity needs none of it, so it shares this
/// module's helpers (`spec_mode_model`, the MCP fold, `assemble_spawn_env`,
/// `spawn_catalog_writeback`) without entering that path.
pub async fn build_antigravity_instance(
    inputs: SessionBuildInputs<'_>,
    spawner: Arc<dyn aionui_process::Spawner>,
) -> Result<crate::agent_task::AgentInstance, AgentError> {
    use aionui_session::{AntigravityConnection, BackendConnection, McpServerSpec, SessionConfig, SessionInit};

    let SessionBuildInputs {
        conversation_id,
        user_id,
        workspace,
        config,
        metadata,
        session_snapshot,
        backend_session_id,
        mcp_server_repo,
        runtime_env,
        broadcaster,
        catalog_writeback,
        acp_session_repo,
        // agy has no prompt-dump lane yet (the dev dump is keyed by a
        // claude/codex label); skip it rather than mislabel the dump.
        prompt_dump_dir: _,
        permission_hook_body,
    } = inputs;

    let (spec, mode, model) = spec_mode_model(&conversation_id, backend_session_id, config, session_snapshot, metadata);

    // Same MCP init surface and ordering as the claude/codex path: user servers
    // resolved from the repo, plus the inline snapshot, with the team
    // coordination server PREPENDED.
    let mut neutral = match mcp_server_repo {
        Some(repo) => {
            crate::mcp_resolve::resolve_session_mcp_servers(
                repo.as_ref(),
                &user_id,
                config.mcp_server_ids.as_deref(),
                &conversation_id,
                broadcaster.clone(),
            )
            .await
        }
        None => Vec::new(),
    };
    neutral.extend(config.session_mcp_servers.iter().cloned());
    let mut mcp_servers: Vec<McpServerSpec> = neutral.iter().map(session_server_to_spec).collect();
    if let Some(cfg) = config.team_mcp_stdio_config.as_ref() {
        let mut coordination = vec![team_mcp_server_spec(cfg)];
        coordination.append(&mut mcp_servers);
        mcp_servers = coordination;
    }

    let init = SessionInit {
        mcp_servers,
        skills: config.skills.clone(),
        preset_context: config.preset_context.clone(),
        session_snapshot: None,
        resume: matches!(spec, aionui_session::SessionSpec::Resume { .. }),
    };

    let mut session_config = SessionConfig {
        cwd: Some(workspace.clone()),
        model,
        mode,
        init,
        permission_hook_body,
        // agy is NOT shipped with the app: it is a large native binary the user
        // installs themselves, so there is no bundled path to resolve and the
        // backend always spawns the `agy` on PATH.
        cli_program: None,
        ..Default::default()
    };
    session_config.spawn_env = assemble_spawn_env(&metadata.env, runtime_env);

    let backend = AntigravityConnection::new(spawner)
        .open_session(spec, session_config)
        .await
        .map_err(|e| match e {
            aionui_session::BackendError::WorkspaceUnavailable(path) => {
                AgentError::workspace_path_runtime_unavailable(path)
            }
            e => AgentError::bad_gateway(format!("open antigravity session: {e}")),
        })?;

    if let Some((agent_id, catalog_tx)) = catalog_writeback {
        spawn_catalog_writeback(agent_id, user_id.clone(), backend.clone(), catalog_tx);
    }

    let task = SessionAgentTask::new_with_preload(
        AgentType::Antigravity,
        conversation_id,
        user_id,
        workspace,
        backend,
        acp_session_repo,
        &metadata.handshake,
        None,
        Some(broadcaster),
    );
    Ok(crate::agent_task::AgentInstance::Session(task))
}

/// claude/codex session started through the ACP factory is byte-equivalent to one
/// started through the clean-slate registry.
pub async fn build_session_instance(
    backend_label: &str,
    inputs: SessionBuildInputs<'_>,
    spawner: Arc<dyn aionui_process::Spawner>,
) -> Result<Option<crate::agent_task::AgentInstance>, AgentError> {
    use aionui_session::{
        BackendConnection, ClaudeConnection, CodexConnection, McpServerSpec, SessionConfig, SessionInit, SessionSpec,
    };

    let connection: Box<dyn BackendConnection> = match backend_label {
        "claude" => Box::new(ClaudeConnection::new(spawner)),
        "codex" => Box::new(CodexConnection::new(spawner)),
        _ => return Ok(None),
    };

    let SessionBuildInputs {
        conversation_id,
        user_id,
        workspace,
        config,
        metadata,
        session_snapshot,
        backend_session_id,
        mcp_server_repo,
        runtime_env,
        broadcaster,
        catalog_writeback,
        acp_session_repo,
        prompt_dump_dir,
        // claude/codex gate through CLI flags, not an installed hook file.
        permission_hook_body: _,
    } = inputs;

    // GAP #1/#2 — the pure spec + mode/model mapping (resume anchor → Resume/Fresh,
    // snapshot-wins precedence). Extracted so it is unit-testable in isolation, the
    // exact sibling of clean-slate's `spec_and_config`.
    let (spec, mode, model) = spec_mode_model(&conversation_id, backend_session_id, config, session_snapshot, metadata);

    // GAP #3 — MCP init surface: resolve user-configured servers to the neutral
    // spec (clean-slate resolve_session_init), fold in the inline snapshot, then
    // prepend the team coordination MCP. Same order as the app boundary.
    let mut neutral = match mcp_server_repo {
        Some(repo) => {
            crate::mcp_resolve::resolve_session_mcp_servers(
                repo.as_ref(),
                &user_id,
                config.mcp_server_ids.as_deref(),
                &conversation_id,
                broadcaster.clone(),
            )
            .await
        }
        None => Vec::new(),
    };
    neutral.extend(config.session_mcp_servers.iter().cloned());
    let mut mcp_servers: Vec<McpServerSpec> = neutral.iter().map(session_server_to_spec).collect();
    if let Some(cfg) = config.team_mcp_stdio_config.as_ref() {
        // Team-MCP is PREPENDED before the user's servers (clean-slate + legacy
        // acp_assembler ordering).
        let mut coordination = vec![team_mcp_server_spec(cfg)];
        coordination.append(&mut mcp_servers);
        mcp_servers = coordination;
    }

    // GAP #4 — preset_context + skills carried into the init surface.
    let init = SessionInit {
        mcp_servers,
        skills: config.skills.clone(),
        preset_context: config.preset_context.clone(),
        // acp/codex resume via SessionSpec::Resume; no in-band snapshot needed.
        session_snapshot: None,
        resume: matches!(spec, SessionSpec::Resume { .. }),
    };

    let mut session_config = SessionConfig {
        cwd: Some(workspace.clone()),
        model,
        mode,
        init,
        // Packaged app: resolve the bundled claude/codex binary and forward its
        // absolute path so the backend spawns OUR CLI, not the user's PATH one.
        // Bundled-missing / dev falls back to a PATH lookup via
        // `resolve_command_path` (NOT the bare name): on Windows, npm installs
        // ship `claude.cmd`/`codex.cmd` shims which `CreateProcess` does not
        // find from a bare name (#299 parity; Rust std runs `.cmd` via
        // `cmd.exe` since the BatBadBut fix). `None` (nothing on PATH either)
        // keeps the bare name so the spawn error stays diagnosable. Detection
        // (cli_probe) stays PATH-only and is unaffected.
        cli_program: aionui_runtime::resolve_bundled_cli(backend_label)
            .or_else(|| aionui_runtime::resolve_command_path(backend_label)),
        ..Default::default()
    };

    // Ask claude for PLAINTEXT thinking. Current models resolve the thinking
    // display to `omitted`, which streams signature-only thinking blocks whose
    // text is empty — the reasoning then never reaches the UI at all. `summarized`
    // is the only other value the CLI accepts (verified:
    // @anthropic-ai/claude-agent-sdk 0.3.220 `sdk.d.ts` ThinkingEnabled /
    // ThinkingAdaptive `display?: 'summarized' | 'omitted'`, serialized as
    // `--thinking-display <value>`), and it yields a SUMMARY of the reasoning
    // rather than raw chain-of-thought.
    //
    // Version-gated on the binary we actually resolved above: the flag is hidden
    // (absent from `--help`) and older builds reject it at argument-parse time, so
    // an ungated flag would make every session unspawnable. See `claude_flags`.
    if backend_label == "claude"
        && let Some(program) = session_config.cli_program.clone()
        && crate::claude_flags::supports_thinking_display(&program).await
    {
        session_config
            .extra_args
            .extend(["--thinking-display".to_string(), "summarized".to_string()]);
        tracing::info!(conv_id = %conversation_id, "claude: requesting summarized thinking display");
    }

    // Spawn env (legacy spawn-surface parity, claude AND codex).
    session_config.spawn_env = assemble_spawn_env(&metadata.env, runtime_env);
    if !session_config.spawn_env.is_empty() {
        let keys: Vec<&str> = session_config.spawn_env.iter().map(|e| e.name.as_str()).collect();
        tracing::info!(conv_id = %conversation_id, ?keys, "session spawn env: agent overrides + runtime context");
    }

    // GAP #5 — claude cc-switch provider env: inject ANTHROPIC_BASE_URL /
    // ANTHROPIC_AUTH_TOKEN (third-party relay creds) into the spawn, mirroring the
    // legacy ACP-claude path. Empty (no cc-switch config) = byte-identical spawn.
    if backend_label == "claude" {
        let provider_env = crate::cc_switch::read_claude_provider_env();
        if !provider_env.is_empty() {
            let keys: Vec<String> = provider_env.keys().cloned().collect();
            session_config.spawn_env.extend(
                provider_env
                    .into_iter()
                    .map(|(name, value)| aionui_common::EnvVar { name, value }),
            );
            tracing::info!(conv_id = %conversation_id, ?keys, "cc-switch: provider env injected into claude spawn");
        }
    }

    // GAP #6 — codex sandbox + approval policy resolved from the requested mode
    // (clean-slate codex_sandbox_for_mode / codex_approval_for_mode). A full-access
    // / yolo mode escalates the sandbox and drops approval prompts; everything else
    // (incl. None) leaves these None so the backend safe-defaults
    // (workspace-write / on-request).
    if backend_label == "codex" {
        if let Some(sandbox) = codex_sandbox_for_mode(session_config.mode.as_deref()) {
            tracing::info!(conv_id = %conversation_id, sandbox, "codex: sandbox policy resolved from requested mode");
            session_config.sandbox_mode = Some(sandbox.to_string());
        }
        if let Some(approval) = codex_approval_for_mode(session_config.mode.as_deref()) {
            tracing::info!(conv_id = %conversation_id, approval, "codex: approval policy resolved from requested mode");
            session_config.approval_policy = Some(approval.to_string());
        }
    }

    // #4 — the persisted reasoning-effort level (claude only). There is no spawn-time
    // effort flag (effort rides a post-open control_request, NOT `--`args like
    // model/mode), so it cannot go into `SessionConfig`; instead we re-apply it AFTER
    // open. codex effort is not a standalone selection (it rides collaborationMode via
    // SetMode), so this is claude-scoped. Read from the snapshot's config_selections
    // (the map `set_config_option` persisted under EFFORT_CONFIG_KEY).
    let persisted_effort = (backend_label == "claude")
        .then(|| {
            session_snapshot.and_then(|s| {
                s.config_selections
                    .iter()
                    .find(|(k, _)| k.as_str() == EFFORT_CONFIG_KEY)
                    .map(|(_, v)| v.as_str().to_owned())
            })
        })
        .flatten()
        .filter(|s| !s.is_empty());

    // DEV (`--dump-prompts`): dump the resolved SessionConfig BEFORE it moves
    // into open_session. Best-effort — a failure only warns, never fails open.
    // Borrows `prompt_dump_dir`; the send-side `SessionPromptDump` consumes it
    // (via `.map`) later, after open_session (which only moves `session_config`).
    if let Some(dir) = prompt_dump_dir.as_ref() {
        let backend_static: &'static str = if backend_label == "claude" { "claude" } else { "codex" };
        let value = build_session_cli_config_dump_value(backend_static, &session_config);
        let input = value.get("input").cloned().unwrap_or(serde_json::Value::Null);
        let resolved_context = value
            .get("resolved_context")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        match crate::dev_prompt_dump::dump_agent_final_input(
            dir,
            crate::dev_prompt_dump::AgentFinalInputDump {
                kind: "session-cli-config",
                backend: backend_static,
                conversation_id: &conversation_id,
                session_id: None,
                msg_id: None,
                turn_id: None,
                input,
                resolved_context,
            },
        ) {
            Ok(path) => tracing::debug!(
                conversation_id = %conversation_id,
                path = %path.display(),
                "DEV session-cli config dump written"
            ),
            Err(error) => tracing::warn!(
                conversation_id = %conversation_id,
                error = %error,
                "DEV session-cli config dump failed"
            ),
        }
    }

    let backend = connection
        .open_session(spec, session_config)
        .await
        .map_err(|e| match e {
            // #410 parity: a missing/non-directory workspace keeps its dedicated
            // error class end-to-end (ProcessError::WorkspaceUnavailable →
            // BackendError::WorkspaceUnavailable → here), so the frontend gets
            // WORKSPACE_PATH_RUNTIME_UNAVAILABLE exactly like the legacy spawn
            // path — not an opaque 502.
            aionui_session::BackendError::WorkspaceUnavailable(path) => {
                AgentError::workspace_path_runtime_unavailable(path)
            }
            e => AgentError::bad_gateway(format!("open {backend_label} session: {e}")),
        })?;

    // Re-apply the persisted effort now that the session is open. The backend validates
    // it against the current model's advertised catalog (permissive until the catalog
    // is discovered) and drops it if unsupported — the same clear_invalid_desired_*
    // semantics as the codex model/mode reconcile. Best-effort: a dispatch failure must
    // not fail the open (the session is usable; only the persisted effort is lost).
    if let Some(effort) = persisted_effort {
        if let Err(e) = backend
            .dispatch(Command::SetConfigOption {
                option_id: EFFORT_CONFIG_KEY.to_owned(),
                value: effort.clone(),
            })
            .await
        {
            tracing::warn!(conv_id = %conversation_id, effort = %effort, error = %e, "session-port: re-applying persisted effort failed (session usable, effort not restored)");
        } else {
            tracing::info!(conv_id = %conversation_id, effort = %effort, "session-port: re-applied persisted reasoning effort after open");
        }
    }

    // GAP #7 (G5): project the backend's discovered catalog back into agent_metadata
    // so the cold-start picker stays fresh. Best-effort, detached, off the open path.
    if let Some((agent_id, catalog_tx)) = catalog_writeback {
        spawn_catalog_writeback(agent_id, user_id.clone(), backend.clone(), catalog_tx);
    }

    let prompt_dump = prompt_dump_dir.map(|dir| SessionPromptDump {
        dir,
        // Only "claude"/"codex" reach here (the caller guards the match; other
        // labels returned None above), so this binary choice is total.
        backend: if backend_label == "claude" { "claude" } else { "codex" },
    });

    let task = SessionAgentTask::new_with_preload(
        AgentType::Acp,
        conversation_id,
        user_id,
        workspace,
        backend,
        acp_session_repo,
        &metadata.handshake,
        prompt_dump,
        // Lets the pump push a usage frame that arrives after the turn's relay has
        // already stopped listening — the claude case (usage rides `result`).
        Some(broadcaster),
    );
    Ok(Some(crate::agent_task::AgentInstance::Session(task)))
}

/// Assemble the direct-CLI spawn env (legacy spawn-surface parity; order
/// matters — later entries win in `ManagedProcess::spawn`):
///  1. per-agent env overrides (`AgentMetadata.env`, repair panel) — the legacy
///     path injected these via `resolve_agent_command_spec`; `AIONUI_*`/`PATH`/…
///     keys are already filtered at the registry (`is_blocked_override_env_key`),
///     so they cannot shadow the runtime context below.
///  2. the `AIONUI_*` conversation runtime context (`AIONUI_USER_ID` /
///     `AIONUI_CONVERSATION_ID` / `AIONUI_HELPER_BIN` / `AIONUI_BASE_URL` /
///     `AIONUI_RUNTIME_TOKEN`) — the legacy path appended these via
///     `apply_acp_launch_policy` for every agent spawn.
fn assemble_spawn_env(
    agent_env: &[aionui_api_types::AgentEnvEntry],
    runtime_env: &[(String, String)],
) -> Vec<aionui_common::EnvVar> {
    let mut env: Vec<aionui_common::EnvVar> = agent_env
        .iter()
        .map(|entry| aionui_common::EnvVar {
            name: entry.name.clone(),
            value: entry.value.clone(),
        })
        .collect();
    env.extend(runtime_env.iter().map(|(name, value)| aionui_common::EnvVar {
        name: name.clone(),
        value: value.clone(),
    }));
    env
}

/// Build the `session-cli-config` dump payload from the resolved `SessionConfig`
/// captured just before `open_session`. Symmetric with the ACP path's
/// `build_acp_final_input_dump_value`: returns `{ "input", "resolved_context" }`.
/// `SessionConfig` has no `Serialize`, so fields are mapped by hand. Contents
/// are RAW (dev-only, `--dump-prompts`): secrets in `spawn_env` / MCP env are
/// not redacted, matching the existing acp dump.
fn build_session_cli_config_dump_value(backend: &str, cfg: &aionui_session::SessionConfig) -> serde_json::Value {
    use aionui_session::McpTransport;
    let mcp_servers: Vec<serde_json::Value> = cfg
        .init
        .mcp_servers
        .iter()
        .map(|s| {
            let transport = match &s.transport {
                McpTransport::Stdio { command, args, env } => serde_json::json!({
                    "type": "stdio",
                    "command": command,
                    "args": args,
                    "env": env.iter().map(|(k, v)| serde_json::json!({ "name": k, "value": v })).collect::<Vec<_>>(),
                }),
                McpTransport::Http { url, headers } => serde_json::json!({
                    "type": "http",
                    "url": url,
                    "headers": headers.iter().map(|(k, v)| serde_json::json!({ "name": k, "value": v })).collect::<Vec<_>>(),
                }),
                McpTransport::Sse { url, headers } => serde_json::json!({
                    "type": "sse",
                    "url": url,
                    "headers": headers.iter().map(|(k, v)| serde_json::json!({ "name": k, "value": v })).collect::<Vec<_>>(),
                }),
            };
            serde_json::json!({ "name": s.name, "transport": transport })
        })
        .collect();

    let spawn_env: Vec<serde_json::Value> = cfg
        .spawn_env
        .iter()
        .map(|e| serde_json::json!({ "name": e.name, "value": e.value }))
        .collect();

    serde_json::json!({
        "input": {
            "backend": backend,
            "mode": cfg.mode,
            "model": cfg.model,
            "cli_program": cfg.cli_program.as_ref().map(|p| p.to_string_lossy()),
            "sandbox_mode": cfg.sandbox_mode,
            "approval_policy": cfg.approval_policy,
            "resume": cfg.init.resume,
        },
        "resolved_context": {
            "preset_context": cfg.init.preset_context,
            "skills": cfg.init.skills,
            "mcp_servers": mcp_servers,
            "spawn_env": spawn_env,
            "extra_args": cfg.extra_args,
        }
    })
}

/// Convert a neutral `SessionMcpServer` (already stdio-launch-resolved by
/// `mcp_resolve`) into the crate-local `McpServerSpec`. Verbatim port of
/// clean-slate `session_runtime::session_server_to_spec`.
fn session_server_to_spec(server: &aionui_api_types::SessionMcpServer) -> aionui_session::McpServerSpec {
    use aionui_api_types::SessionMcpTransport as T;
    use aionui_session::{McpServerSpec, McpTransport};
    let sorted = |m: &std::collections::HashMap<String, String>| -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = m.iter().map(|(k, val)| (k.clone(), val.clone())).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    };
    let transport = match &server.transport {
        T::Stdio { command, args, env } => McpTransport::Stdio {
            command: command.clone(),
            args: args.clone(),
            env: sorted(env),
        },
        T::Http { url, headers } | T::StreamableHttp { url, headers } => McpTransport::Http {
            url: url.clone(),
            headers: sorted(headers),
        },
        T::Sse { url, headers } => McpTransport::Sse {
            url: url.clone(),
            headers: sorted(headers),
        },
    };
    McpServerSpec {
        name: server.name.clone(),
        transport,
    }
}

/// The team coordination MCP server as a neutral stdio spec. Verbatim port of
/// clean-slate `session_runtime::team_mcp_server_spec` (name = TEAM_MCP_SERVER_NAME,
/// arg `mcp-team-stdio`, env PORT/TOKEN/SLOT_ID) so a session-model teammate joins
/// the SAME per-team TCP bridge the ACP path used.
fn team_mcp_server_spec(cfg: &aionui_api_types::TeamMcpStdioConfig) -> aionui_session::McpServerSpec {
    use aionui_api_types::TeamMcpStdioConfig as C;
    aionui_session::McpServerSpec {
        name: aionui_api_types::TEAM_MCP_SERVER_NAME.to_owned(),
        transport: aionui_session::McpTransport::Stdio {
            command: cfg.binary_path.clone(),
            args: vec!["mcp-team-stdio".to_owned()],
            env: vec![
                (C::ENV_PORT.to_owned(), cfg.port.to_string()),
                (C::ENV_TOKEN.to_owned(), cfg.token.clone()),
                (C::ENV_SLOT_ID.to_owned(), cfg.slot_id.clone()),
            ],
        },
    }
}

/// GAP #7 (G5): spawn the one-shot catalog write-back for a session-model
/// (claude/codex) backend. The ACP catalog (modes/models/commands) lands a beat
/// AFTER `open_session` returns (the session/new|load response is parsed
/// asynchronously by the reader), so this waits for a discovery (bounded to ~5s),
/// then forwards the projected partial via the registry's `CatalogSender`
/// (best-effort — re-discovery on the next open is the idempotent fallback). Off
/// the open hot path. Without this the `/api/agents` model/mode picker never
/// refreshes for claude/codex sessions (the exact "codex 无法选择模型" regression).
///
/// Verbatim port of clean-slate `session_runtime::spawn_catalog_writeback`: wait
/// for MODELS specifically before committing (codex answers modes before models),
/// forwarding the best model-less partial only if the window elapses.
/// How often the write-back re-reads `capabilities()`.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
/// When to publish modes/commands even though no models have shown up (5s).
/// A backend that never reports models must not be held back by the longer
/// window below.
const INTERIM_PUBLISH_TICKS: usize = 100;
/// How long to keep watching for a LATE model list (35s). agy discovers models
/// by running `agy models` off the open path — ~3s cold and slower on a bad
/// network, which the 5s deadline alone could not cover.
const MODEL_WINDOW_TICKS: usize = 700;

pub fn spawn_catalog_writeback(
    agent_id: String,
    user_id: String,
    backend: Arc<dyn aionui_session::SessionBackend>,
    catalog_tx: crate::registry::CatalogSender,
) {
    tokio::spawn(async move {
        let mut best_partial = None;
        let mut interim_sent = false;
        for tick in 0..MODEL_WINDOW_TICKS {
            let caps = backend.capabilities();
            if let Some(partial) = catalog_partial_from_caps(&caps) {
                if !caps.available_models.is_empty() {
                    // Complete enough — models present → commit the full catalog.
                    catalog_tx.send_partial(user_id.clone(), agent_id, partial);
                    return;
                }
                // Modes/commands only so far — remember it, keep waiting for models.
                best_partial = Some(partial);
            }
            // Publish what we have at the original deadline so a backend that
            // legitimately has no model list (claude/codex fill capabilities
            // from the handshake) is not held back by the longer model window.
            if tick + 1 == INTERIM_PUBLISH_TICKS
                && let Some(partial) = best_partial.clone()
            {
                catalog_tx.send_partial(user_id.clone(), agent_id.clone(), partial);
                interim_sent = true;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        if !interim_sent && let Some(partial) = best_partial {
            catalog_tx.send_partial(user_id, agent_id, partial);
        }
    });
}

/// Project a backend's discovered `Capabilities` (modes / models / slash commands)
/// into an `AgentHandshake` partial for the `agent_metadata` catalog. Verbatim port
/// of clean-slate `session_runtime::catalog_partial_from_caps`: emits both the ACP
/// `config_options[]` wire shape AND the top-level `available_modes`/`available_models`
/// columns directly (the shape-stable path that keeps the codex model picker from
/// going empty).
fn catalog_partial_from_caps(caps: &aionui_session::Capabilities) -> Option<aionui_api_types::AgentHandshake> {
    let mut config_options = Vec::new();
    if !caps.available_modes.is_empty() {
        config_options.push(serde_json::json!({
            "id": "mode",
            "category": "mode",
            "type": "select",
            "currentValue": caps.current_mode,
            "options": caps.available_modes.iter().map(|m| serde_json::json!({
                "value": m.id, "name": m.name, "description": m.description,
            })).collect::<Vec<_>>(),
        }));
    }
    if !caps.available_models.is_empty() {
        config_options.push(serde_json::json!({
            "id": "model",
            "category": "model",
            "type": "select",
            "currentValue": caps.current_model,
            "options": caps.available_models.iter().map(|m| serde_json::json!({
                "value": m.id, "name": m.name, "description": m.description,
            })).collect::<Vec<_>>(),
        }));
    }
    let available_commands = if caps.slash_commands.is_empty() {
        None
    } else {
        Some(serde_json::json!(
            caps.slash_commands
                .iter()
                .map(|c| serde_json::json!({
                    "name": c.name, "description": c.description,
                }))
                .collect::<Vec<_>>()
        ))
    };
    if config_options.is_empty() && available_commands.is_none() {
        return None;
    }
    let config_options = if config_options.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(config_options))
    };
    // Also project the top-level `available_modes`/`available_models` fields directly
    // (shape: `{available_models:[{id,label}]}`), which `apply_handshake` persists to
    // the catalog columns VERBATIM — the authoritative, shape-stable path (matches what
    // a live claude handshake stores), so the codex model picker never goes empty.
    let available_modes = (!caps.available_modes.is_empty()).then(|| {
        serde_json::json!({
            "available_modes": caps.available_modes.iter().map(|m| serde_json::json!({
                "id": m.id, "name": m.name, "description": m.description,
            })).collect::<Vec<_>>(),
            "current_mode_id": caps.current_mode,
        })
    });
    let available_models = (!caps.available_models.is_empty()).then(|| {
        serde_json::json!({
            "available_models": caps.available_models.iter().map(|m| serde_json::json!({
                "id": m.id, "label": m.name,
            })).collect::<Vec<_>>(),
            "current_model_id": caps.current_model,
        })
    });
    Some(aionui_api_types::AgentHandshake {
        config_options,
        available_modes,
        available_models,
        available_commands,
        ..Default::default()
    })
}

/// Map a conversation's requested mode → the codex `thread/start.sandbox` string
/// (`SandboxMode`: `read-only` / `workspace-write` / `danger-full-access`, verified
/// `codex-cli/0.137.0/schema-full/ClientRequest.json` §SandboxMode), or `None` to keep
/// the backend's safe default (`unwrap_or("workspace-write")`).
///
/// This runs at OPEN time and pre-seeds `thread/start.sandbox` — the sandbox axis the
/// tier reaches the FIRST turn through, since `thread/start` carries no `permissions`
/// field and `permissions` is mutually exclusive with `sandbox` (U1). The post-open
/// `reconcile_codex_mode` only applies the matching permission profile via SetMode, and
/// that `thread/settings/update{permissions}` write "applies to the NEXT turn"
/// (codex_conn `Command::SetMode`) — so WITHOUT seeding the restrictive sandbox here, a
/// read-only conversation's first turn would run under the permissive `workspace-write`
/// default and a write would succeed before the profile lands. We therefore seed BOTH
/// escalation (`full-access` → `danger-full-access`) AND restriction (`read-only` →
/// `read-only`) at the sandbox axis; the middle `workspace`/`auto` tier keeps the
/// `workspace-write` default (returned as `None`).
///
/// The mode value reaching this boot helper is the persisted/config selection, which
/// under feature 012 "Plan B" is the LEGACY bare token (`full-access` / `read-only`);
/// the colon profile id (`:danger-full-access` / `:read-only`, e.g. from a readback that
/// skipped bare-mapping) and the legacy `yoloNoSandbox` alias stay recognized for
/// robustness. Kept in lockstep with `codex_conn::codex_perm::{normalize_to_profile_id,
/// profile_id_to_legacy_value}`.
fn codex_sandbox_for_mode(mode: Option<&str>) -> Option<&'static str> {
    match mode.map(str::trim) {
        // `agent-full-access` is the canonical codex full-access mode id since #608
        // (migration 021 rewrote builtin `yolo_id` `full-access`→`agent-full-access`, and
        // `normalize_requested_mode` now resolves yolo aliases to it). The legacy
        // `full-access` / `:danger-full-access` / `yoloNoSandbox` stay recognized for
        // pre-021 persisted data — all four are the same danger-full-access tier.
        Some(":danger-full-access" | "agent-full-access" | "full-access" | "yoloNoSandbox") => {
            Some("danger-full-access")
        }
        Some(":read-only" | "read-only") => Some("read-only"),
        _ => None,
    }
}

/// Map a conversation's requested mode → the codex `approvalPolicy` string, or
/// `None` to keep the default (`on-request`). Sibling of `codex_sandbox_for_mode`:
/// a full-access / yolo agent runs unattended → `"never"`. Recognizes the legacy bare
/// token `full-access` (the Plan B canonical value), the colon id `:danger-full-access`,
/// and the legacy `yoloNoSandbox` alias. Verbatim port of clean-slate
/// `session_runtime::codex_approval_for_mode`.
fn codex_approval_for_mode(mode: Option<&str>) -> Option<&'static str> {
    match mode.map(str::trim) {
        // Recognizes the #608 canonical `agent-full-access` alongside the legacy
        // `full-access` / `:danger-full-access` / `yoloNoSandbox` (see codex_sandbox_for_mode).
        Some(":danger-full-access" | "agent-full-access" | "full-access" | "yoloNoSandbox") => Some("never"),
        _ => None,
    }
}

/// Discriminant name of a `SessionEvent`, for the pump's diagnostic debug log
/// (no payload — safe at debug; used to confirm which backend events actually
/// arrive when comparing the session path against the legacy ACP path).
fn session_event_name(e: &SessionEvent) -> &'static str {
    match e {
        SessionEvent::TurnStarted { .. } => "TurnStarted",
        SessionEvent::MessageDelta { .. } => "MessageDelta",
        SessionEvent::ThoughtDelta { .. } => "ThoughtDelta",
        SessionEvent::ToolCall { .. } => "ToolCall",
        SessionEvent::ToolResult { .. } => "ToolResult",
        SessionEvent::TurnResult { .. } => "TurnResult",
        SessionEvent::Detached { .. } => "Detached",
        SessionEvent::Permission { .. } => "Permission",
        SessionEvent::PermissionResolved { .. } => "PermissionResolved",
        SessionEvent::UsageDelta { .. } => "UsageDelta",
        SessionEvent::ConfigChanged { .. } => "ConfigChanged",
        SessionEvent::BackendBound { .. } => "BackendBound",
        SessionEvent::PromptAccepted { .. } => "PromptAccepted",
        SessionEvent::Snapshot { .. } => "Snapshot",
        other => {
            // Fallback for the many additive variants the pump drops; a leaked
            // debug string is fine (no payload).
            let s: &'static str = match other {
                SessionEvent::Plan { .. } => "Plan",
                SessionEvent::Rewound { .. } => "Rewound",
                SessionEvent::SubagentUpdate { .. } => "SubagentUpdate",
                SessionEvent::SubagentDetail { .. } => "SubagentDetail",
                SessionEvent::Notice { .. } => "Notice",
                SessionEvent::ToolOutputDelta { .. } => "ToolOutputDelta",
                SessionEvent::TurnDiffUpdated { .. } => "TurnDiffUpdated",
                SessionEvent::Provisioning { .. } => "Provisioning",
                _ => "Other",
            };
            s
        }
    }
}

/// Drain the backend's `events()` and re-broadcast each as an `AgentStreamEvent`.
fn spawn_event_pump(
    mut events: BoxStream<'static, SessionEnvelope>,
    runtime: Arc<SessionRuntime>,
    conversation_id: String,
    user_id: String,
    session_repo: Option<Arc<dyn IAcpSessionRepository>>,
    broadcaster: Option<Arc<dyn EventBroadcaster>>,
) {
    use futures_util::StreamExt as _;
    // The pump owns ONLY the event stream (a broadcast `Receiver` handle — see
    // `ClaudeSessionBackend::events`), NEVER an `Arc<dyn SessionBackend>`. Holding a
    // backend Arc here would be self-referential: the backend struct owns the
    // `event_tx` this stream subscribes to, so a backend Arc in this task would keep
    // `event_tx` alive, the stream would never see `Closed`, this loop would never
    // exit, and the backend's `Drop` (the sole process reaper) would never run —
    // leaking the child CLI. By capturing only the stream, the sole long-lived
    // backend Arc is `SessionAgentTask.backend`; dropping the task (e.g. idle-kill
    // removing it from the manager map) drops that Arc → backend `Drop` → reader
    // abort + `kill_on_drop` → `event_tx` drops → this stream Closes → the loop ends.
    tokio::spawn(async move {
        // Per-tool accumulated live output for codex `ToolOutputDelta` (streamed
        // command stdout). The frontend merges `tool_call` frames by call_id with a
        // shallow REPLACE of `output` (hooks.ts: `{...existing, ...new}`), so we must
        // send the CUMULATIVE text each time, not the delta — otherwise each chunk
        // overwrites the last and only the final chunk shows. Keyed by item_id (==
        // the ToolCall tool_use_id). The authoritative full output still arrives on
        // the completed ToolResult, which harmlessly replaces this live view.
        let mut tool_output: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        // In-flight workflow/subagent refs, mirroring `state::background_active`
        // (any non-terminal roster entry ⇒ in-flight). claude's non-blocking
        // Workflow turn emits MULTIPLE `result` frames: the LAUNCH result arrives
        // while subagents are still running, and the TERMINAL result arrives only
        // AFTER every `task_notification{completed}` (fixture 2.1.176 invariant:
        // all completed precede all result). Forwarding the launch result's Finish
        // would terminate the relay and drop the workflow's completion message, so
        // we suppress the intermediate Finish until this set drains.
        let mut workflow_inflight: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Is a suppressed launch Finish still OWED for the current turn? While true,
        // the relay is held open on credit: the turn's real Finish was swallowed
        // (branch below), betting on the 2.1.176 invariant that a terminal `result`
        // follows natural workflow completion. The bet only pays on NATURAL
        // completion — an interrupt kills the workflow with task frames and NO
        // result frame (verified: samples/claude-cli/2.1.220/
        // _all_workflow_interrupt.jsonl, scenario A), so the `Interrupted` drain
        // below must settle the debt itself or the turn never drains and the 15s
        // UserCancelTimeout watchdog force-kills a healthy session (ELECTRON-3RP).
        // Settled by the drain, by a real (unsuppressed) terminal, or when the
        // envelope `turn_gen` advances (see `last_seen_turn_gen`).
        let mut finish_suppressed_pending = false;
        // Did the pump already close the current turn with a synthetic Finish (the
        // cancel-drain settlement below)? A real terminal `result` can still trail
        // it in an interrupt-vs-natural-completion race; its Finish must then be
        // swallowed — the relay/persistence contract is ONE Finish per turn.
        // Reset when `turn_gen` advances: the trailing-result window is bounded by
        // the turn itself, and the next turn's real Finish must NOT be eaten.
        let mut synthetic_finish_emitted = false;
        // Remembered `tool_use_id` → tool name, learned from each `ToolCall` frame.
        // A tool's lifecycle emits SEVERAL frames sharing one call_id — the initial
        // ToolCall (name known), any codex `ToolOutputDelta` (name absent on the wire),
        // and the terminal `ToolResult` (the wire `tool_result` block carries only
        // tool_use_id, NOT the name). The frontend persists tool_call rows keyed by
        // call_id (stream_persistence::persist_tool_call, upsert), so a later frame with
        // an empty name would OVERWRITE the row's name to "" and the tool would render
        // nameless. Stamp the remembered name onto every follow-up frame so the name
        // survives — mirroring the reference `BackendOutputSink::emit_tool_result`,
        // which re-sends the name on completion. Cleared per turn with `tool_output`.
        let mut tool_name: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        // Tool calls of the CURRENT turn still awaiting their terminal `ToolResult`
        // (call_id → name). If the turn ends without one — user cancel, process
        // crash, or the CLI dropping the result — the persisted tool_call row would
        // stay status "work" FOREVER and the frontend View-Steps spinner
        // (`hasRunningToolMessages`) would never stop, surviving even reloads. The
        // terminal arm below closes every remaining entry with a `Canceled` frame
        // BEFORE the Finish (the relay stops forwarding a turn at Finish).
        let mut open_tools: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        // Did the CURRENT turn emit any user-visible output (text / thinking / tool /
        // plan / permission)? Mirrors the ACP path's `is_empty_turn` (agent_session_flow.rs):
        // a clean terminal with this still `false` is a "blank reply" (ELECTRON-1JG) and
        // gets a diagnostic Tip so the user isn't left staring at an empty bubble. Set as
        // events are observed, reset at the per-turn terminal (with `tool_output`/`tool_name`).
        let mut saw_visible_output = false;
        // Did the CURRENT turn already reach a terminal `TurnResult`? Mirrors the
        // reducer's `Running{terminal_result_seen}` flag. The pump consumes RAW
        // pre-reducer events, so to classify a `Detached` the way the reducer's
        // `crash_outcome` does — a crash mid-turn (no result yet) → Crashed, a
        // Detached AFTER the turn's result → absorbed (I10) — it must track this
        // itself. Without it, a Detached that trails a completed turn (idle-kill,
        // clean shutdown) would be misread as a mid-turn crash. Set on the terminal
        // TurnResult, reset when `turn_gen` advances (new turn).
        let mut terminal_result_seen = false;
        // The `turn_gen` of the last envelope processed. This — not `TurnStarted` —
        // is the per-turn reset boundary that actually fires on EVERY backend:
        // claude_conn never emits `TurnStarted` (only the codex/acp adapters
        // synthesize it), but its reader stamps every envelope with the current
        // turn_gen, bumping it per accepted Send and deliberately NOT bumping it
        // for a mid-turn injection. A reset keyed only on the TurnStarted arm is
        // therefore dead code on the claude stream: the swallow guard armed by a
        // cancel-drain settlement in turn N leaked into turn N+1 and ate its real
        // Finish, leaving the turn open forever (live: dev 2026-07-31, conv
        // ee61bd05, turn_7c5c89fd stuck "processing" after workflow-cancel +
        // follow-up message). Gen-advance also bounds the trailing-result swallow
        // window precisely: a trailing result rides the OLD gen (FIFO stdout), the
        // next turn's frames ride the new gen.
        let mut last_seen_turn_gen: u64 = 0;
        while let Some(env) = events.next().await {
            runtime.touch();
            tracing::debug!(conv_id = %conversation_id, event = session_event_name(&env.event), "session-pump: backend event");

            if env.turn_gen > last_seen_turn_gen {
                if last_seen_turn_gen > 0 {
                    tracing::debug!(
                        conv_id = %conversation_id,
                        from = last_seen_turn_gen,
                        to = env.turn_gen,
                        "session-pump: turn gen advanced; per-turn suppression state reset"
                    );
                }
                last_seen_turn_gen = env.turn_gen;
                runtime.set_status(ConversationStatus::Running);
                terminal_result_seen = false;
                // Per-turn workflow-suppression state must not leak across turns —
                // same rationale as the TurnStarted arm below, which claude never
                // reaches.
                workflow_inflight.clear();
                finish_suppressed_pending = false;
                synthetic_finish_emitted = false;
            }

            // Empty-turn diagnostic Tip to emit for THIS terminal, if the turn was a
            // clean blank reply. Computed in the terminal match arm below (while
            // `saw_visible_output` still reflects this turn) and drained just before the
            // Finish in the translate loop — a Tips after Finish would be dropped, since
            // the relay breaks the turn on Finish. Per-iteration, so it never leaks
            // across turns.
            let mut pending_empty_turn_tip: Option<TipsEventData> = None;

            // ToolOutputDelta needs pump-local accumulation (see above), so it is
            // handled here rather than in the stateless translate_event.
            if let SessionEvent::ToolOutputDelta { item_id, text } = &env.event {
                // Streamed tool stdout is user-visible output — this turn is not blank.
                saw_visible_output = true;
                let acc = tool_output.entry(item_id.clone()).or_default();
                acc.push_str(text);
                let _ = runtime.tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
                    call_id: item_id.clone(),
                    // The wire delta carries no name; use the remembered one so this
                    // live-output frame doesn't overwrite the persisted row's name to "".
                    name: tool_name.get(item_id).cloned().unwrap_or_default(),
                    args: serde_json::Value::Null,
                    status: ToolCallStatus::Running,
                    input: None,
                    output: Some(acc.clone()),
                    description: None,
                }));
                continue;
            }

            // Async catalog discovery (claude `initialize` / codex `model/list` +
            // `collaborationMode/list` RESPONSE). Project it into an `AcpConfigOption`
            // frame — the direct-CLI analogue of the ACP path's `emit_snapshot_events`
            // catalog push. The frontend's `useAcpConfigOptions` handler REPLACES its
            // whole snapshot on this frame and re-derives the picker's `canSwitch`, so a
            // catalog that arrived ~6s after `open_session` (long after the frontend read
            // an empty `config_options`) finally lights the model/mode selector. Built
            // here, not in the stateless `translate_event`, because the current-value
            // highlight needs the runtime's optimistic overrides. Emitted whole (model +
            // mode categories together) so it never wipes a sibling category.
            if let SessionEvent::CatalogUpdated {
                models,
                modes,
                slash_commands,
            } = &env.event
            {
                let mut config_options: Vec<aionui_api_types::AcpConfigOptionDto> = Vec::new();
                if !modes.is_empty() {
                    config_options.push(aionui_api_types::AcpConfigOptionDto {
                        id: "mode".into(),
                        name: Some("Mode".into()),
                        label: None,
                        description: None,
                        category: Some("mode".into()),
                        option_type: "select".into(),
                        current_value: runtime.mode_override(),
                        options: modes
                            .iter()
                            .map(|m| aionui_api_types::AcpConfigSelectOptionDto {
                                value: m.id.clone(),
                                name: Some(m.name.clone()),
                                label: None,
                                description: m.description.clone(),
                            })
                            .collect(),
                    });
                }
                if !models.is_empty() {
                    config_options.push(aionui_api_types::AcpConfigOptionDto {
                        id: "model".into(),
                        name: Some("Model".into()),
                        label: None,
                        description: None,
                        category: Some("model".into()),
                        option_type: "select".into(),
                        current_value: runtime.model_override(),
                        options: models
                            .iter()
                            .map(|m| aionui_api_types::AcpConfigSelectOptionDto {
                                value: m.id.clone(),
                                name: Some(m.name.clone()),
                                label: None,
                                description: m.description.clone(),
                            })
                            .collect(),
                    });
                }
                // Reasoning-effort axis (claude per-model `supportedEffortLevels`). The
                // frontend REPLACES its whole config-options snapshot on this frame, so we
                // MUST re-emit effort here too — otherwise a late catalog push would wipe
                // the effort option that `get_config_options` (REST) surfaced. The pump has
                // no backend Arc, so the current model is resolved from the pushed catalog
                // and the highlight comes from the runtime's optimistic effort override
                // (claude emits no effort echo). Emitted only when the current model
                // advertises efforts (union fallback when the current model is unknown).
                let efforts = resolve_current_model_efforts(models, runtime.model_override().as_deref());
                if !efforts.is_empty() {
                    config_options.push(aionui_api_types::AcpConfigOptionDto {
                        id: "reasoning_effort".into(),
                        name: Some("Thinking".into()),
                        label: None,
                        description: None,
                        category: Some("thought_level".into()),
                        option_type: "select".into(),
                        current_value: runtime.effort_override(),
                        options: efforts
                            .iter()
                            .map(|e| aionui_api_types::AcpConfigSelectOptionDto {
                                value: e.clone(),
                                name: Some(e.clone()),
                                label: None,
                                description: None,
                            })
                            .collect(),
                    });
                }
                // No categories (both lists empty) → nothing to re-project; a spurious
                // empty-snapshot frame would only clobber the frontend's picker.
                if !config_options.is_empty()
                    && let Ok(v) = serde_json::to_value(serde_json::json!({ "config_options": config_options }))
                {
                    let _ = runtime.tx.send(AgentStreamEvent::AcpConfigOption(v));
                }
                // Slash-command catalog. claude advertises its command list in the
                // async `initialize` response — the same late-catalog timing that
                // strands the model/mode picker — and the frontend's mount-time REST
                // read (`fetchAcpSlashCommands`) returns empty before it lands. The
                // legacy ACP path recovers via a live `AvailableCommands` push
                // (translate.rs `AvailableCommandsUpdate` arm); this is its direct-CLI
                // analogue, so the `/` menu fills once discovery completes instead of
                // staying empty until a manual refetch.
                if !slash_commands.is_empty() {
                    let commands = slash_commands
                        .iter()
                        .map(|c| {
                            agent_client_protocol::schema::v1::AvailableCommand::new(
                                c.name.clone(),
                                c.description.clone().unwrap_or_default(),
                            )
                        })
                        .collect();
                    let _ = runtime
                        .tx
                        .send(AgentStreamEvent::AvailableCommands(AvailableCommandsEventData {
                            commands,
                        }));
                }
                continue;
            }

            // Track in-flight workflow/subagent refs so a non-blocking Workflow's
            // intermediate `result` frame does not prematurely terminate the turn.
            // Mirrors `state::background_active`: a ref is in-flight while its status
            // is non-terminal ({PendingInit, Running}); a terminal status
            // ({Interrupted, Completed, Errored, Shutdown}) removes it.
            if let SessionEvent::SubagentUpdate {
                r#ref, status, kind, ..
            } = &env.event
            {
                use aionui_session::{SubagentStatus, SubagentTaskKind};
                match status {
                    SubagentStatus::PendingInit | SubagentStatus::Running => {
                        // Suppression-roster admission is WORKFLOW-ONLY. The multi-result
                        // invariant this roster bets on (launch result suppressed, terminal
                        // result follows completion) is proven ONLY for `local_workflow`
                        // tasks. A background bash (`local_bash`) outlives its turn with NO
                        // later terminal result, so admitting it wedges the turn until the
                        // 15s watchdog (live 2026-07-30: user's `sleep 60 &` cancel hang).
                        // `kind` rides only `task_started`; a kind-less `task_updated`
                        // re-assert therefore can't insert — which also retires the
                        // "task_updated re-inserts a dead ref" oscillation by construction.
                        if matches!(kind, Some(SubagentTaskKind::WorkflowContainer)) {
                            workflow_inflight.insert(r#ref.clone());
                            tracing::debug!(
                                conv_id = %conversation_id,
                                subagent_ref = %r#ref,
                                ?status,
                                inflight = workflow_inflight.len(),
                                "session-pump: workflow container in-flight"
                            );
                        }
                    }
                    terminal @ (SubagentStatus::Interrupted
                    | SubagentStatus::Completed
                    | SubagentStatus::Errored
                    | SubagentStatus::Shutdown) => {
                        workflow_inflight.remove(r#ref);
                        tracing::debug!(
                            conv_id = %conversation_id,
                            subagent_ref = %r#ref,
                            status = ?terminal,
                            inflight = workflow_inflight.len(),
                            "session-pump: subagent terminal"
                        );
                        // The roster just drained while a launch Finish is owed: decide
                        // how the turn ends by HOW the LAST ref drained.
                        //  - `Completed`: keep waiting — after natural completion the
                        //    CLI still sends the terminal `result` (fixture invariant:
                        //    samples/claude-cli/2.1.176 + 2.1.220), and settling here
                        //    would break the relay before the completion message lands.
                        //  - `Interrupted`: the workflow was KILLED (user interrupt).
                        //    No result frame ever follows (verified: samples/claude-cli/
                        //    2.1.220/_all_workflow_interrupt.jsonl — a kill emits only
                        //    task_updated{killed}+task_notification{stopped} per task),
                        //    so the owed Finish must be paid NOW or the relay never
                        //    breaks and `cancelling` never clears.
                        //  - `Errored`/`Shutdown`: whether a result trails is UNSAMPLED;
                        //    settle like `Interrupted` (a hung turn costs more than a
                        //    raced double Finish, which the swallow guard below absorbs)
                        //    and warn so a live capture can pin the real contract.
                        // Keying on the LAST removal (not "any Interrupted seen") is
                        // deliberate: a workflow that survives one killed child still
                        // completes naturally, and its roster drains via `Completed`.
                        if workflow_inflight.is_empty()
                            && finish_suppressed_pending
                            && !matches!(terminal, SubagentStatus::Completed)
                        {
                            if matches!(terminal, SubagentStatus::Interrupted) {
                                tracing::info!(
                                    conv_id = %conversation_id,
                                    "session-pump: workflow interrupted while its launch Finish was suppressed; emitting the owed Finish (cancel drain)"
                                );
                            } else {
                                tracing::warn!(
                                    conv_id = %conversation_id,
                                    status = ?terminal,
                                    "session-pump: workflow drained via unsampled terminal while its launch Finish was suppressed; settling to avoid a hung turn"
                                );
                            }
                            finish_suppressed_pending = false;
                            synthetic_finish_emitted = true;
                            // The kill decided this turn: a later Detached is an
                            // absorbed teardown, not a mid-turn crash (see
                            // `crash_outcome`).
                            terminal_result_seen = true;
                            // Same per-turn closure the real terminal arm performs:
                            // close every tool call left open as Canceled BEFORE the
                            // Finish (the relay stops forwarding the turn at Finish).
                            for (call_id, name) in open_tools.drain() {
                                let _ = runtime.tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
                                    call_id,
                                    name,
                                    args: serde_json::Value::Null,
                                    status: ToolCallStatus::Canceled,
                                    input: None,
                                    output: None,
                                    description: None,
                                }));
                            }
                            tool_output.clear();
                            tool_name.clear();
                            saw_visible_output = false;
                            // Idempotent clean-converge shared with the watchdog kill
                            // path: sets status Finished + broadcasts the Finish that
                            // drives relay break → turn release → `cancelling` cleared.
                            runtime.emit_finish_once();
                        }
                    }
                }
            }

            // Is THIS TurnResult an intermediate (workflow-launch) result whose Finish
            // must be suppressed? True only for a clean (non-error, non-cancel) result
            // that arrives while a workflow is still in flight. An error/cancel result
            // is always honoured as the terminal (the user must see it, and the
            // fixture invariant only covers clean completion ordering).
            let suppress_intermediate_finish = matches!(&env.event, SessionEvent::TurnResult { is_error, outcome, .. }
                if !workflow_inflight.is_empty()
                    && !*is_error
                    && !matches!(outcome, aionui_session::TurnOutcome::Cancelled { .. }));
            if suppress_intermediate_finish {
                tracing::info!(
                    conv_id = %conversation_id,
                    inflight = workflow_inflight.len(),
                    "session-pump: suppressing intermediate workflow-launch Finish (turn stays open until workflow completes)"
                );
            }

            // Drive the coarse status off the turn-boundary events so `status()`
            // reflects running/finished (the app gates the sidebar spinner on it).
            match &env.event {
                SessionEvent::TurnStarted { .. } => {
                    runtime.set_status(ConversationStatus::Running);
                    // New turn: the prior turn's terminal no longer applies.
                    terminal_result_seen = false;
                    // Per-turn workflow-suppression state must not leak across turns:
                    // a stale in-flight ref (e.g. a `task_updated` re-insert racing a
                    // cancelled workflow's teardown) would re-arm suppression and
                    // swallow THIS turn's Finish. A new turn starts unsuppressed.
                    workflow_inflight.clear();
                    finish_suppressed_pending = false;
                    synthetic_finish_emitted = false;
                }
                // Track the call's open/closed lifecycle. Also remember the name for
                // `stamp_tool_name` — the map was previously never populated, so
                // ToolOutputDelta/ToolResult follow-up frames went out nameless.
                SessionEvent::ToolCall { tool_use_id, name, .. } => {
                    tool_name.insert(tool_use_id.clone(), name.clone());
                    open_tools.insert(tool_use_id.clone(), name.clone());
                }
                SessionEvent::ToolResult { tool_use_id, .. } => {
                    open_tools.remove(tool_use_id);
                }
                SessionEvent::TurnResult { .. } | SessionEvent::Detached { .. } if !suppress_intermediate_finish => {
                    // A real (unsuppressed) terminal settles any owed launch Finish —
                    // its own Finish/Error closes the turn.
                    finish_suppressed_pending = false;
                    runtime.set_status(ConversationStatus::Finished);
                    // Close every tool call the turn left open (cancel/crash/dropped
                    // result): emit a terminal `Canceled` frame per call so the
                    // persisted row leaves "work" and the frontend spinner stops.
                    // Must precede the Finish emitted by the translate loop below.
                    for (call_id, name) in open_tools.drain() {
                        tracing::info!(
                            conv_id = %conversation_id,
                            %call_id,
                            tool = %name,
                            "session-pump: closing tool call left open at turn end as canceled"
                        );
                        let _ = runtime.tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
                            call_id,
                            name,
                            args: serde_json::Value::Null,
                            status: ToolCallStatus::Canceled,
                            input: None,
                            output: None,
                            description: None,
                        }));
                    }
                    // A terminal TurnResult decided this turn; a later Detached is then
                    // an absorbed teardown, not a mid-turn crash (see `crash_outcome`).
                    if matches!(env.event, SessionEvent::TurnResult { .. }) {
                        terminal_result_seen = true;
                    }
                    // Empty-turn (blank-reply) diagnostic, mirroring the ACP path
                    // (agent_session_flow.rs `prompt_outcome_from_stop_reason`): a turn
                    // that reached a CLEAN terminal (`TurnResult{is_error:false}`, not
                    // cancelled) without emitting any user-visible output gets an
                    // informational/warning Tip so the user isn't left with an empty
                    // bubble. `Detached` (process crash) is excluded — that surfaces as a
                    // crash error elsewhere, not a "the model had nothing to say" tip, and
                    // ACP likewise only tips on a completed prompt. An error result is
                    // excluded because it already terminates as `AgentStreamEvent::Error`.
                    if let SessionEvent::TurnResult {
                        is_error: false,
                        outcome,
                        ..
                    } = &env.event
                        && !saw_visible_output
                    {
                        pending_empty_turn_tip = empty_turn_tip(outcome);
                    }
                    // Live tool-output accumulators are per-turn; the authoritative
                    // full output already rode each ToolResult. Drop them so a long
                    // session doesn't retain every turn's stdout.
                    tool_output.clear();
                    tool_name.clear();
                    // Reset the per-turn visibility flag for the next turn.
                    saw_visible_output = false;
                }
                // Learn the CLI-assigned session id so send_message (Start) and the
                // Finish stamping below carry it, matching the ACP path.
                SessionEvent::BackendBound {
                    backend_session_id: Some(bid),
                } => runtime.set_session_id(bid.clone()),
                _ => {}
            }
            // Persist the Tier-2 side-effects the legacy ACP path wrote via
            // AcpSessionSyncService (which this direct-CLI path bypasses). Best-effort:
            // a repo error is warn-logged, never fatal to the stream.
            if let Some(repo) = session_repo.as_ref() {
                persist_side_effects(repo.as_ref(), &user_id, &conversation_id, &env.event).await;
            }
            // Usage must ALSO reach the live view directly. claude reports it on the
            // `result` frame, which lands after the relay has already broken on
            // Finish, so the translated frame below would be shouted into an empty
            // room. Broadcasting straight from the pump (session-scoped, still alive)
            // is what turns claude's indicator on without waiting for a reload.
            if let (Some(bus), Some(_)) = (broadcaster.as_ref(), informative_usage(&env.event)) {
                broadcast_usage_frame(bus.as_ref(), &conversation_id, &user_id, &env.event);
            }
            for mut ev in translate_event(env.event, &conversation_id, terminal_result_seen) {
                // Keep the tool name alive across a call's multi-frame lifecycle (see
                // `stamp_tool_name`): the terminal ToolResult frame leaves the name
                // empty, and the upsert-by-call_id persistence would otherwise clobber
                // the row's name to "". Runs before any routing decision below;
                // no-op on non-ToolCall frames (e.g. the suppressed Finish).
                stamp_tool_name(&mut tool_name, &mut ev);
                // Record whether this turn produced user-visible output, so a clean
                // terminal with none is detected as a blank reply (see the terminal
                // match arm above). Checked against the translated frame so the
                // definition matches the relay's own notion of visible output.
                if event_is_user_visible_output(&ev) {
                    saw_visible_output = true;
                }
                // The pump already closed this turn with a synthetic Finish (cancel
                // drain above); a real terminal trailing it in the interrupt-vs-
                // completion race must not emit a SECOND Finish — and must not fire
                // the empty-turn Tip below either (the relay already broke; per-turn
                // output accumulators were reset at settlement, so the tip would be
                // spurious).
                if synthetic_finish_emitted && matches!(ev, AgentStreamEvent::Finish(_)) {
                    tracing::info!(
                        conv_id = %conversation_id,
                        "session-pump: swallowing trailing real Finish after synthetic cancel-drain Finish"
                    );
                    continue;
                }
                // Emit the empty-turn diagnostic Tip immediately BEFORE the Finish it
                // was computed for. It MUST precede Finish: the relay breaks the turn on
                // Finish (stream_relay.rs), so a Tips sent afterwards would never be
                // forwarded. `pending_empty_turn_tip` is only ever set on a clean
                // TurnResult, whose translation is exactly one Finish, so this fires once.
                if matches!(ev, AgentStreamEvent::Finish(_))
                    && let Some(tip) = pending_empty_turn_tip.take()
                {
                    let _ = runtime.tx.send(AgentStreamEvent::Tips(tip));
                }
                // Suppress the intermediate workflow-launch Finish: the assistant's
                // reply text already reached the frontend via MessageDelta→Text, so
                // dropping this Finish loses no output — it only keeps the relay open
                // so the workflow's later completion result can still be delivered.
                //
                // Emit a SegmentBreak in its place: the launch reply and the later
                // completion reply are two independent claude outputs, so the relay
                // must close the current text segment here. Otherwise both batches
                // accumulate under one msg_id and the frontend renders them as a
                // single bubble with no separator. SegmentBreak is consumed inside
                // the relay (never forwarded to the WS), so it changes only bubble
                // boundaries, not the wire contract.
                if suppress_intermediate_finish && matches!(ev, AgentStreamEvent::Finish(_)) {
                    // The turn now owes this Finish; the workflow drain (or a later
                    // real terminal) must settle it — see `finish_suppressed_pending`.
                    finish_suppressed_pending = true;
                    let _ = runtime.tx.send(AgentStreamEvent::SegmentBreak);
                    continue;
                }
                // Stamp the CLI session id onto the Finish frame, matching the ACP path
                // which sends Finish{session_id}. The resume anchor rides it to the
                // frontend. (Start is emitted by send_message, already stamped.)
                //
                // NOTE: claude emits its `UsageDelta` a few ms AFTER `TurnResult`
                // (it rides the `result` frame), and the relay stops forwarding a
                // turn the moment it sees this Finish — so the translated
                // AcpContextUsage below is shouted into an empty room and used to
                // leave the context indicator blank. That gap is now closed WITHOUT
                // an end-of-turn barrier: the pump persists every UsageDelta to
                // `context_usage` and broadcasts it directly (see the UsageDelta
                // handling above), both of which outlive the turn.
                if let AgentStreamEvent::Finish(data) = &mut ev
                    && data.session_id.is_none()
                {
                    data.session_id = runtime.session_id();
                }
                // A send error only means no live subscribers — harmless.
                let _ = runtime.tx.send(ev);
            }
        }
    });
}

/// Pure decision (FCIS core): does this terminal `TurnResult` prove the stored
/// resume anchor is dead, so the next turn must open Fresh?
///
/// A resume against a backend session the CLI no longer knows fails with a
/// structural error ("No conversation found" / `error_during_execution`), NOT an
/// ordinary tool/turn error (those terminate `is_error:false` or with other text).
/// Classified through the SAME single-source predicate the clean-slate
/// `Orchestrator` uses (`aionui_session::is_unrecoverable_resume_error`), so a
/// backend wording change is fixed in one place. A user-cancelled turn is excluded:
/// claude reports an interrupt as `is_error` with cancel-noise text, but the anchor
/// is still good.
fn is_dead_resume_anchor(event: &SessionEvent) -> bool {
    use aionui_session::{TurnOutcome, is_unrecoverable_resume_error};
    let SessionEvent::TurnResult {
        is_error,
        result_text,
        outcome,
        ..
    } = event
    else {
        return false;
    };
    if !is_error || matches!(outcome, TurnOutcome::Cancelled { .. }) {
        return false;
    }
    let reason = aionui_session::ErrorReason::Backend {
        api_error_status: None,
        message: result_text.clone(),
    };
    is_unrecoverable_resume_error(&reason)
}

/// Persist the backend-observed session identity + config to `acp_session`, the
/// SAME writes the legacy `AcpSessionSyncService` domain-event consumer performed
/// for the ACP-manager path. Without this the resume anchor
/// (`build_session_instance` GAP #1) and the mode/model precedence source (GAP #2)
/// are never written, so a restart always loses continuity.
async fn persist_side_effects(
    repo: &dyn IAcpSessionRepository,
    user_id: &str,
    conversation_id: &str,
    event: &SessionEvent,
) {
    // Self-heal a dead resume anchor: a turn that failed *because* the stored
    // backend session id no longer resolves must null that id, or every subsequent
    // send re-resumes the same dead session and the conversation wedges forever.
    // Nulling (not deleting) keeps config/runtime state; the next open reads a
    // `None` anchor → Fresh → rebinds a live id. This restores the self-heal the
    // direct-CLI path dropped: clean-slate `Orchestrator` emits `BackendBound{None}`
    // and legacy ACP does `rebuild_after_session_not_found` → `clear_session_id`.
    if is_dead_resume_anchor(event) {
        match repo.clear_session_id_for_user(user_id, conversation_id).await {
            Ok(_) => tracing::info!(
                conversation_id,
                "session-sync: cleared dead resume anchor (unrecoverable resume error) — next turn opens Fresh"
            ),
            Err(err) => {
                tracing::warn!(conversation_id, error = %err, "session-sync: clear_session_id failed")
            }
        }
    }
    match event {
        // The CLI-echoed backend session id — written immediately (no debounce) so
        // the next turn takes the resume path even if the process crashes. `None`
        // (lost-backend self-heal) leaves the stored anchor as-is; a fresh rebind
        // happens on the next open.
        SessionEvent::BackendBound {
            backend_session_id: Some(bid),
        } => {
            if let Err(err) = repo.update_session_id_for_user(user_id, conversation_id, bid).await {
                tracing::warn!(conversation_id, error = %err, "session-sync: update_session_id failed");
            }
        }
        // A confirmed mode/model switch → persist so the next respawn/resume seeds
        // the user's selection (mirrors ObservedModeSynced / ObservedModelSynced).
        SessionEvent::ConfigChanged { mode, model } if mode.is_some() || model.is_some() => {
            let params = SaveRuntimeStateParams {
                current_mode_id: mode.as_ref().map(|m| Some(m.as_str())),
                current_model_id: model.as_ref().map(|m| Some(m.as_str())),
                config_selections_json: None,
                context_usage_json: None,
            };
            if let Err(err) = repo
                .save_runtime_state_for_user(user_id, conversation_id, &params)
                .await
            {
                tracing::warn!(conversation_id, error = %err, "session-sync: save_runtime_state failed");
            }
        }
        // Token usage → the `context_usage` runtime snapshot the usage indicator
        // reads back. This is the ONLY durable sink for direct-CLI usage: the pump
        // is session-scoped and outlives the per-turn `StreamRelay`, so it still
        // sees claude's `UsageDelta` — which rides the `result` frame that lands
        // AFTER the relay has already broken on Finish.
        _ => {}
    }
    if let Some(usage) = informative_usage(event) {
        persist_context_usage(repo, user_id, conversation_id, usage).await;
    }
}

/// The single gate on whether a usage report says anything about context
/// occupancy. Both consumers — the durable snapshot and the live broadcast — ask
/// this, so a report can never be broadcast without being stored, or vice versa.
///
/// A zero-token report is DISCARDED. claude ends a no-op turn with an all-zero
/// `usage` object — live-captured (2.1.220) on all three variants:
///
/// | turn                | `usage`  | `total_cost_usd`      | `modelUsage` |
/// |---------------------|----------|-----------------------|--------------|
/// | `/compact` success  | all zero | 0.0131 (its own cost) | real numbers |
/// | `/compact` rejected | all zero | 0                     | `{}`         |
/// | `/clear`            | all zero | 0                     | `{}`         |
///
/// Note the successful compaction contradicts itself: `usage` reads zero while
/// `modelUsage` reports what the compaction actually spent. Recording the zero
/// overwrites a real occupancy figure — not merely stale but wrong, since a
/// compaction leaves the context SMALLER, never empty. Dropping it keeps the last
/// true reading until the next real turn reports the post-compaction size.
///
/// DO NOT be tempted by `system{subtype:"compact_boundary"}`, which carries
/// `compact_metadata{pre_tokens, post_tokens, cumulative_dropped_tokens}`. Its
/// `pre_tokens` matches our pre-compaction figure exactly (28_049 measured), which
/// makes `post_tokens` look like the answer — but the next real turn reported
/// 27_238, not `post_tokens`' 2_065. `post_tokens` counts only the compacted
/// transcript while `usage` also carries the system prompt and tool definitions
/// (~25k here). Different baselines; mixing them would understate occupancy ~13x.
fn informative_usage(event: &SessionEvent) -> Option<&SessionEvent> {
    match event {
        SessionEvent::UsageDelta { total_tokens, .. } if *total_tokens > 0 => Some(event),
        // Discarding is silent otherwise, which makes "the indicator is stuck on an
        // old number" undiagnosable from logs. One line per no-op turn is cheap.
        SessionEvent::UsageDelta { .. } => {
            tracing::debug!("usage report carries zero tokens; keeping the previous reading");
            None
        }
        _ => None,
    }
}

/// Broadcast a `UsageDelta` to the conversation's live stream.
///
/// Mirrors `StreamRelay::forward_to_websocket_with_msg_id` exactly — same
/// `message.stream` envelope, same snake_case normalisation — so the frame is
/// indistinguishable from the one the relay emits mid-turn.
///
/// `msg_id`/`turn_id` are empty: a usage report is CONVERSATION-scoped state, and
/// by the time claude's arrives its turn is already closed, so there is no message
/// to attach it to. The indicator reads `data`, keyed by conversation.
///
/// Fires for every backend, which means codex/ACP — whose reports land mid-turn,
/// while the relay is still forwarding — deliver TWO frames: the relay's catch-all
/// (with a real `msg_id`) and this one. Accepted deliberately: the renderer's
/// `setTokenUsage` replaces wholesale, so the duplicate is idempotent, and gating
/// on backend here would trade a harmless repeat for a rule that silently rots the
/// moment another backend starts reporting after its turn ends.
fn broadcast_usage_frame(bus: &dyn EventBroadcaster, conversation_id: &str, user_id: &str, event: &SessionEvent) {
    let Some(frame) = translate_event(event.clone(), conversation_id, false)
        .into_iter()
        .next()
    else {
        return;
    };
    let mut event_data = match serde_json::to_value(&frame) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(conversation_id, error = %err, "usage broadcast: serialize failed");
            return;
        }
    };
    aionui_common::normalize_keys_to_snake_case(&mut event_data);
    let payload = serde_json::json!({
        "conversation_id": conversation_id,
        "user_id": user_id,
        "msg_id": "",
        "turn_id": "",
        "type": event_data.get("type").cloned().unwrap_or(serde_json::json!("usage")),
        "data": event_data.get("data").cloned().unwrap_or(serde_json::json!({})),
        "hidden": false,
    });
    bus.broadcast(aionui_api_types::WebSocketMessage::new("message.stream", payload));
}

/// Merge one usage report into `acp_session.session_config.runtime.context_usage`.
///
/// Shape is the ACP `UsageUpdate` the frontend already consumes:
/// `{used, size, cost:{amount, currency}}`.
///
/// MERGE, not replace (mirrors the ACP path): `used` always takes the newer value,
/// while `size`/`cost` are only overwritten when the incoming report carries them.
/// codex sends no cost at all and may send `modelContextWindow: null`, so a blind
/// replace would blank a window the previous turn had already established.
async fn persist_context_usage(
    repo: &dyn IAcpSessionRepository,
    user_id: &str,
    conversation_id: &str,
    event: &SessionEvent,
) {
    let SessionEvent::UsageDelta {
        input_tokens,
        output_tokens,
        total_tokens: used,
        cost_usd,
        context_window,
        breakdown,
    } = event
    else {
        return;
    };
    let (used, cost_usd, context_window) = (*used, *cost_usd, *context_window);
    let mut usage = match repo.load_runtime_state_for_user(user_id, conversation_id).await {
        Ok(Some(state)) => state
            .context_usage_json
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default(),
        Ok(None) => serde_json::Map::new(),
        Err(err) => {
            tracing::warn!(conversation_id, error = %err, "session-sync: load_runtime_state failed; skipping usage persist");
            return;
        }
    };
    usage.insert("used".into(), serde_json::json!(used));
    if let Some(size) = context_window {
        usage.insert("size".into(), serde_json::json!(size));
    }
    if let Some(cost) = cost_usd {
        usage.insert("cost".into(), serde_json::json!({ "amount": cost, "currency": "USD" }));
    }
    // The detail line must survive a reload too — the renderer reads `_meta` off
    // the GET /usage snapshot exactly as it does off a live frame. Merged like
    // `size`/`cost`: only replaced when the incoming turn actually reported one.
    if !breakdown.is_empty() {
        usage.insert(
            "_meta".into(),
            serde_json::json!({
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cached_read_tokens": breakdown.cached_read_tokens,
                "cached_write_tokens": breakdown.cached_write_tokens,
                "thought_tokens": breakdown.thought_tokens,
            }),
        );
    }
    let json = match serde_json::to_string(&usage) {
        Ok(j) => j,
        Err(err) => {
            tracing::warn!(conversation_id, error = %err, "session-sync: encode context_usage failed");
            return;
        }
    };
    let params = SaveRuntimeStateParams {
        context_usage_json: Some(Some(&json)),
        ..Default::default()
    };
    if let Err(err) = repo
        .save_runtime_state_for_user(user_id, conversation_id, &params)
        .await
    {
        tracing::warn!(conversation_id, error = %err, "session-sync: save context_usage failed");
    }
}

/// Extract the picked option id from the confirm `data` payload. The frontend sends
/// either a bare string (the option_id) or an object `{option_id|optionId|value}`.
/// Mirrors the ACP path's `confirm_option_id`.
fn confirm_option_id(data: &serde_json::Value) -> Option<String> {
    match data {
        serde_json::Value::String(v) => Some(v.clone()),
        serde_json::Value::Object(map) => map
            .get("option_id")
            .or_else(|| map.get("optionId"))
            .or_else(|| map.get("value"))
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned),
        _ => None,
    }
}

/// Generic allow / allow-always / reject options for an ordinary tool-approval
/// permission card. `confirm()` maps these option ids back to a `PermissionDecision`.
fn default_permission_options() -> Vec<crate::protocol::events::AcpPermissionOptionData> {
    use crate::protocol::events::{AcpPermissionOptionData, AcpPermissionOptionKind};
    vec![
        AcpPermissionOptionData {
            option_id: PERM_ALLOW.to_owned(),
            name: "Allow".to_owned(),
            kind: AcpPermissionOptionKind::AllowOnce,
            meta: None,
        },
        AcpPermissionOptionData {
            option_id: PERM_ALLOW_ALWAYS.to_owned(),
            name: "Allow Always".to_owned(),
            kind: AcpPermissionOptionKind::AllowAlways,
            meta: None,
        },
        AcpPermissionOptionData {
            option_id: PERM_REJECT.to_owned(),
            name: "Reject".to_owned(),
            kind: AcpPermissionOptionKind::RejectOnce,
            meta: None,
        },
    ]
}

/// Project an AskUserQuestion tool `input` into permission-card options the user can
/// pick. `input` shape (claude, live-captured): `{questions:[{question, header,
/// options:[{label, description}], multiSelect}]}`. The frontend card is single-select
/// (one radio group, one confirm), so we surface the FIRST question's option labels as
/// the choices — `option_id == label` so `confirm()` can pass the picked label straight
/// into `AnswerPermission.selected` (claude keys the answer by label). A multi-question
/// AskUserQuestion degrades to answering the first question (a known single-select
/// frontend limitation — the remaining questions claude silently drops, same as the
/// legacy single-question path). Returns empty when the shape is absent/unparseable, so
/// the caller falls back to allow/deny.
fn ask_user_question_options(
    input: Option<&serde_json::Value>,
) -> Vec<crate::protocol::events::AcpPermissionOptionData> {
    use crate::protocol::events::{AcpPermissionOptionData, AcpPermissionOptionKind};
    let Some(first_q) = input
        .and_then(|i| i.get("questions"))
        .and_then(|q| q.as_array())
        .and_then(|arr| arr.first())
    else {
        return Vec::new();
    };
    let Some(opts) = first_q.get("options").and_then(|o| o.as_array()) else {
        return Vec::new();
    };
    opts.iter()
        .filter_map(|o| o.get("label").and_then(|l| l.as_str()))
        .map(|label| AcpPermissionOptionData {
            // option_id == label: confirm() forwards it as the chosen answer label.
            option_id: label.to_owned(),
            name: label.to_owned(),
            kind: AcpPermissionOptionKind::AllowOnce,
            meta: None,
        })
        .collect()
}

/// Keep a tool's name alive across the multiple `AgentStreamEvent::ToolCall` frames
/// that share one `call_id` over its lifecycle.
///
/// A single tool call surfaces as several frames keyed by the same `call_id`: the
/// initial `ToolCall` (status Running, name known); any codex `ToolOutputDelta`
/// (streamed stdout, name absent on the wire); and the terminal `ToolResult` (the
/// wire `tool_result` block carries only `tool_use_id`, never the name — so
/// `translate_event` leaves it empty). The frontend persists tool_call rows by
/// upsert on `call_id` (`stream_persistence::persist_tool_call`), so a later
/// empty-name frame would OVERWRITE the row's name to `""` and the tool would render
/// nameless.
///
/// This learns the name from the first frame that carries one and stamps it back
/// onto any later empty-name frame for the same `call_id`, mirroring the reference
/// `BackendOutputSink::emit_tool_result`, which re-sends the name on completion.
/// `names` is the pump-local map (cleared per turn); non-`ToolCall` events are inert.
fn stamp_tool_name(names: &mut std::collections::HashMap<String, String>, ev: &mut AgentStreamEvent) {
    let AgentStreamEvent::ToolCall(data) = ev else {
        return;
    };
    if data.name.is_empty() {
        if let Some(known) = names.get(&data.call_id) {
            data.name = known.clone();
        }
    } else {
        names.insert(data.call_id.clone(), data.name.clone());
    }
}

/// Translate one clean-slate `SessionEvent` into zero or more origin
/// `AgentStreamEvent`s. The fold SHAPE mirrors the clean-slate TurnFinalizer, but
/// the output targets origin's `AgentStreamEvent` enum instead of `ConvDomainEvent`.
/// Whether a translated stream event represents user-visible turn output —
/// anything that renders in chat. Mirrors the ACP path's
/// `event_is_user_visible_output` (agent_session_flow.rs) so the direct-CLI
/// empty-turn detection uses the same definition of "the turn said something".
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

/// Build the empty-turn diagnostic Tip for a clean terminal that produced no
/// user-visible output, mirroring the ACP path (agent_session_flow.rs:388-448):
/// a normal `EndTurn` is an informational "no reply" note; any other stop reason
/// (truncation / refusal / failure) is a warning naming the cause. Codes match
/// the `conversation.agentTip.codes.*` i18n keys the frontend `MessageTips`
/// renderer localizes. Cancelled is `None` (never a blank-reply; the caller also
/// guards it) so a user interrupt never surfaces a spurious tip.
fn empty_turn_tip(outcome: &aionui_session::TurnOutcome) -> Option<TipsEventData> {
    use aionui_session::{StopReason, TruncationKind, TurnOutcome};
    let (tip_type, code) = match outcome {
        TurnOutcome::EndTurn
        | TurnOutcome::Completed {
            stop_reason: StopReason::EndTurn,
        } => (TipType::Info, "ACP_EMPTY_TURN"),
        TurnOutcome::Completed {
            stop_reason: StopReason::Truncated(TruncationKind::MaxTokens),
        } => (TipType::Warning, "ACP_EMPTY_TURN_MAX_TOKENS"),
        TurnOutcome::Completed {
            stop_reason: StopReason::Truncated(TruncationKind::MaxTurns),
        } => (TipType::Warning, "ACP_EMPTY_TURN_MAX_TURN_REQUESTS"),
        TurnOutcome::Completed {
            stop_reason: StopReason::Refused { .. },
        } => (TipType::Warning, "ACP_EMPTY_TURN_REFUSAL"),
        // Other truncation kinds (context window / budget / bare wire-end) and a
        // clean `Failed` have no dedicated ACP code — surface the generic warning
        // so the user still sees "the turn ended without a reply" with a hint.
        TurnOutcome::Completed { .. } | TurnOutcome::Failed => (TipType::Warning, "ACP_EMPTY_TURN"),
        TurnOutcome::Cancelled { .. } => return None,
    };
    Some(TipsEventData {
        content: String::new(),
        tip_type,
        code: Some(code.to_owned()),
        params: None,
    })
}

/// `terminal_result_seen`: did the current turn already reach a terminal
/// `TurnResult`? Only consulted for `Detached` — it lets this stateless fn
/// replicate the reducer's `crash_outcome` guard (a Detached AFTER the turn's
/// result is an absorbed teardown, not a crash). Immaterial for every other arm.
fn translate_event(event: SessionEvent, conversation_id: &str, terminal_result_seen: bool) -> Vec<AgentStreamEvent> {
    match event {
        // NOTE: the Start lifecycle frame is emitted by `send_message` (before
        // dispatch), mirroring the ACP path which emits Start right before prompt().
        // The backend's own turn-start signals — claude/codex `PromptAccepted`
        // (arrives AFTER the first text delta) and the orchestrator-lowered
        // `TurnStarted` (never reaches this stream) — are therefore NOT re-projected
        // to Start here, or the frontend would see a late/duplicate turn boundary.
        SessionEvent::PromptAccepted { .. } | SessionEvent::TurnStarted { .. } => Vec::new(),
        SessionEvent::MessageDelta { text, .. } => {
            vec![AgentStreamEvent::Text(TextEventData { content: text })]
        }
        SessionEvent::ThoughtDelta { text, .. } => {
            vec![AgentStreamEvent::Thinking(ThinkingEventData {
                content: text,
                subject: None,
                duration: None,
                status: Some("thinking".into()),
            })]
        }
        SessionEvent::ToolCall {
            tool_use_id,
            name,
            input,
            ..
        } => {
            vec![AgentStreamEvent::ToolCall(ToolCallEventData {
                call_id: tool_use_id,
                name,
                args: input.clone(),
                status: ToolCallStatus::Running,
                input: Some(input),
                output: None,
                description: None,
            })]
        }
        SessionEvent::ToolResult {
            tool_use_id,
            is_error,
            content,
            ..
        } => {
            let output = tool_result_text(&content);
            vec![AgentStreamEvent::ToolCall(ToolCallEventData {
                call_id: tool_use_id,
                name: String::new(),
                args: serde_json::Value::Null,
                status: if is_error {
                    ToolCallStatus::Error
                } else {
                    ToolCallStatus::Completed
                },
                input: None,
                output,
                description: None,
            })]
        }
        SessionEvent::TurnResult {
            is_error,
            result_text,
            outcome,
            ..
        } => {
            // A user-cancelled turn is NOT an error: claude reports its interrupt as an
            // is_error result (e.g. `error_during_execution` / an aborted-tool
            // diagnostic), but the user asked for it — so a cancel ends with a plain
            // Finish, no error (the origin frontend lacks the clean-slate cancel-noise
            // suppression, so we suppress at the source).
            let is_cancel = matches!(outcome, aionui_session::TurnOutcome::Cancelled { .. });
            if is_error && !is_cancel && !result_text.trim().is_empty() {
                // A genuine turn error terminates as AgentStreamEvent::Error carrying the
                // FULL origin error model (code / ownership / retryable /
                // feedback_recommended), NOT a plain Tips. The relay reads
                // Error{code,retryable} to drive auto-replay + error classification
                // (stream_relay::terminal_from_event) and the frontend renders ownership/
                // feedback from it; a Tips carries none of that and is not even seen as a
                // terminal. Classify the result text through the SAME path the ACP empty-
                // turn error uses (AgentError::bad_gateway → classify_upstream_detail), so
                // provider/billing/rate-limit/lifecycle errors are categorized identically.
                // Error IS the terminal (relay breaks on it), so we do NOT also emit Finish.
                let stream_error =
                    AgentSendError::from_agent_error(AgentError::bad_gateway(result_text)).into_stream_error();
                return vec![AgentStreamEvent::Error(stream_error)];
            }
            vec![AgentStreamEvent::Finish(FinishEventData::default())]
        }
        SessionEvent::Detached { exit, redacted_summary } => {
            // A process exit is only an ERROR when it is a genuine mid-turn crash.
            // Classify it EXACTLY as the clean-slate reducer's `crash_outcome` does
            // (the pump consumes raw pre-reducer events, so it must replicate that
            // pure fn rather than inherit its verdict):
            //   - terminal result already seen this turn → absorbed teardown (I10);
            //   - clean exit-0, no result → EmptyTurn-class (a blank turn, not a
            //     crash) — the empty-turn Tip already rode the status match above;
            //   - signal / non-zero / unknown(None) exit, no result → CRASH.
            // Only the crash case surfaces as an error; the rest end with a plain
            // Finish (behaviour-preserving). This restores the legacy ACP path's
            // `AcpError::Disconnected → UserAgentDisconnected` terminal that the
            // direct-CLI bridge previously dropped: a CLI that dies mid-reply used
            // to render as a normal (empty) completion instead of a "disconnected,
            // reconnect" error card.
            match aionui_session::crash_outcome(terminal_result_seen, exit) {
                aionui_session::Outcome::Crashed => {
                    // Route through the SAME classifier legacy used, so the frontend
                    // gets the identical code/ownership/retryable/resolution. The
                    // allowlisted `redacted_summary` (already stripped of secrets at
                    // the backend boundary) rides the error message as the user-facing
                    // reason — mirroring `CloseReason::ProcessExited: {summary}`;
                    // without it the card shows only a bare exit code.
                    let acp_err = crate::protocol::error::AcpError::Disconnected {
                        exit_code: exit.and_then(|e| e.code),
                        signal: exit.and_then(|e| e.signal).map(|s| s.to_string()),
                        stderr: redacted_summary.clone().unwrap_or_default(),
                    };
                    let mut stream_error =
                        AgentSendError::from_agent_error(AgentError::Acp(acp_err)).into_stream_error();
                    if let Some(summary) = redacted_summary.filter(|s| !s.trim().is_empty()) {
                        stream_error.message = format!("{}: {summary}", stream_error.message);
                    }
                    vec![AgentStreamEvent::Error(stream_error)]
                }
                aionui_session::Outcome::CleanNoResult | aionui_session::Outcome::FollowResult => {
                    vec![AgentStreamEvent::Finish(FinishEventData::default())]
                }
            }
        }
        // Interactive tool approval: surface as an AcpPermission Request so the
        // frontend renders the allow/deny card. The `tool_call_id` MUST equal the
        // `request_id` — `SessionAgentTask::confirm` dispatches `AnswerPermission`
        // keyed on the same id (the frontend echoes the `call_id` it received here).
        // `input` (the raised tool's raw input — a Bash `command`, AskUserQuestion
        // `{questions:[…]}`) rides as `raw_input` so the card can show the approver
        // what they are approving (AionUi issue #3779); the generic
        // `Approved`/`Denied` options let the reducer + card render.
        SessionEvent::Permission {
            request_id,
            tool_name,
            input,
            ..
        } => {
            // The frontend permission card renders whatever `options[]` we send as the
            // selectable choices (MessageAcpPermission maps each to a radio). So the
            // options MUST reflect what the user is actually choosing between:
            //   - AskUserQuestion → the question's own options (labels), so the user
            //     answers the question. `confirm()` maps the picked label to the
            //     AnswerPermission `selected` (claude keys the answer by it).
            //   - any other tool approval → generic Allow / Allow Always / Reject.
            // (Before, EVERY permission — including AskUserQuestion — was hard-coded to
            // allow/deny, so a question rendered as an allow/deny card. TIO: the question
            // content in `input` is user-facing, not a sensitive tool body.)
            let is_ask = tool_name.as_deref() == Some("AskUserQuestion");
            let options = if is_ask {
                ask_user_question_options(input.as_ref())
            } else {
                Vec::new()
            };
            let options = if options.is_empty() {
                default_permission_options()
            } else {
                options
            };
            vec![AgentStreamEvent::AcpPermission(
                crate::protocol::events::AcpPermissionEventData::Request(
                    crate::protocol::events::AcpPermissionRequestData {
                        session_id: conversation_id.to_owned(),
                        tool_call: crate::protocol::events::AcpPermissionToolCall {
                            tool_call_id: request_id,
                            status: None,
                            title: tool_name,
                            kind: None,
                            raw_input: input,
                            raw_output: None,
                            content: None,
                            locations: None,
                            meta: None,
                        },
                        options,
                        meta: None,
                    },
                ),
            )]
        }
        // Per-turn usage/cost → the AcpContextUsage passthrough frame the frontend
        // usage indicator reads (shape: cumulative token counters).
        SessionEvent::UsageDelta {
            input_tokens,
            output_tokens,
            total_tokens,
            cost_usd,
            context_window,
            breakdown,
        } => {
            // The frontend ContextUsageIndicator reads `used` (tokens consumed) and,
            // optionally, `size` (context window) + `cost` — the exact shape the ACP
            // path forwards (the claude-agent-acp SDK's UsageUpdate: {used, size,
            // cost:{amount,currency}}). Emitting the raw {input_tokens,…} shape left
            // the indicator blank (no `used` key).
            //
            // `used` = total_tokens, which BOTH direct backends compute as current
            // context occupancy, not a per-turn delta: codex reports `last.totalTokens`
            // (its `inputTokens` already includes the cached part) and claude's
            // adapter sums input + output + both cache buckets. The cumulative
            // counters (codex `total.*`) are deliberately NOT used — they outgrow the
            // window on a long session.
            //
            // `size` is emitted only when the backend reported a window; the frontend
            // guards `if size>0`, so `None` degrades to a counter with no percentage.
            let mut usage = serde_json::json!({ "used": total_tokens });
            if let Some(size) = context_window {
                usage["size"] = serde_json::json!(size);
            }
            // Per-turn detail line ("input · output · cache read · thinking").
            // The renderer reads these five keys out of `_meta` (AionUi
            // useAcpMessage.ts `BREAKDOWN_KEYS`) and drops the line when none are
            // present — so a backend that reports nothing simply has no line,
            // rather than a row of zeros.
            if !breakdown.is_empty() {
                usage["_meta"] = serde_json::json!({
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "cached_read_tokens": breakdown.cached_read_tokens,
                    "cached_write_tokens": breakdown.cached_write_tokens,
                    "thought_tokens": breakdown.thought_tokens,
                });
            }
            if let Some(cost) = cost_usd {
                usage["cost"] = serde_json::json!({ "amount": cost, "currency": "USD" });
            }
            // Keep the raw counters too (harmless extra keys) for any richer consumer.
            usage["input_tokens"] = serde_json::json!(input_tokens);
            usage["output_tokens"] = serde_json::json!(output_tokens);
            vec![AgentStreamEvent::AcpContextUsage(usage)]
        }
        // A confirmed mode/model switch is NOT forwarded as a stream frame. The origin
        // frontend's mode/model pickers (AgentModeSelector / AcpModelSelector) track the
        // selection in local state updated optimistically on the PUT /config-options
        // call + its REST response — they do NOT consume a config stream frame. And the
        // origin `useAcpMessage` has no `acp_config_option` case, so any such frame falls
        // into its `default:` arm and lights the turn timer bar (`setRunning(true)`) —
        // the "switching mode shows a spurious timer" regression. So emit nothing here;
        // the selection persist is handled separately by `persist_side_effects`.
        SessionEvent::ConfigChanged { .. } => Vec::new(),
        // Handled earlier in the pump (needs runtime overrides for the current-value
        // highlight; projected to an AcpConfigOption frame there). Never reaches this
        // stateless translator, but the match is total so give it an explicit no-op arm.
        SessionEvent::CatalogUpdated { .. } => Vec::new(),
        // Live plan / to-do snapshot (codex `turn/plan/updated`; claude never emits it).
        // origin has `AgentStreamEvent::Plan` + a `MessagePlan` renderer that reads
        // `entries[].content` + `entries[].status` where status is snake_case
        // (`pending`/`in_progress`/`completed`). Our `PlanStatus` serializes PascalCase,
        // so map it to the frontend contract explicitly rather than serde-dumping the
        // struct (a raw dump would send `Completed` and the card would never tick).
        SessionEvent::Plan { entries, .. } => {
            let entries: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    let status = match e.status {
                        aionui_session::PlanStatus::Pending => "pending",
                        aionui_session::PlanStatus::InProgress => "in_progress",
                        aionui_session::PlanStatus::Completed => "completed",
                    };
                    serde_json::json!({ "content": e.content, "status": status })
                })
                .collect();
            vec![AgentStreamEvent::Plan(
                crate::protocol::events::session_updates::PlanEventData {
                    session_id: None,
                    entries,
                },
            )]
        }
        // Out-of-turn advisory (codex `warning`/`guardianWarning`/`configWarning`/
        // `deprecationNotice`; claude a rejected mode/model/effort set surfaced by
        // `sniff_set_config_reject`). Both backends emit `Notice` *specifically so a
        // failed/advisory event is VISIBLE instead of silently dropped* — re-dropping it
        // here would re-introduce exactly the silent-degradation the backends were coded
        // to avoid (e.g. a rejected effort switch would look like it succeeded). Surface
        // it as a `Tips` frame — the one advisory frame the origin frontend already
        // renders (`MessageTips`, warning/info styling). NOTE: origin's `useAcpMessage`
        // has no explicit `tips` case, so a `tips` frame lands in its `default:` arm,
        // which is benign for display (it renders via `mergeLiveMessage`) but also calls
        // `setRunning(true)`. That is acceptable here: a Notice only arrives mid/around a
        // turn that is already running (or immediately re-settled by the turn's terminal
        // Finish), so it does not manufacture a spurious idle timer the way a config frame
        // would. `NoticeLevel` has only Info/Warning (no Error tier), matching TipType.
        SessionEvent::Notice {
            level,
            message,
            localized,
        } => {
            let tip_type = match level {
                aionui_session::NoticeLevel::Info => TipType::Info,
                aionui_session::NoticeLevel::Warning => TipType::Warning,
            };
            // `content` stays the English text even when a code travels with it:
            // the frontend passes it as i18next's `defaultValue`, so a locale
            // that has not translated the key yet shows real prose instead of a
            // raw key.
            let (code, params) = match localized {
                Some(l) => (Some(l.code), Some(serde_json::Value::Object(l.params))),
                None => (None, None),
            };
            vec![AgentStreamEvent::Tips(TipsEventData {
                content: message,
                tip_type,
                code,
                params,
            })]
        }
        // Events with no origin-side counterpart (or purely internal) are dropped.
        // Cancel folds into the Finish emitted by the resulting terminal; Heartbeat,
        // PromptAccepted, Snapshot, Lagged, item lifecycle, subagent/rewound/etc. are
        // not part of origin's AgentStreamEvent vocabulary. codex ToolOutputDelta /
        // TurnDiffUpdated / SubagentUpdate are also dropped for now — separate
        // follow-ups (each needs its own origin frame + renderer verification).
        _ => Vec::new(),
    }
}

/// Flatten a tool result's content parts into a single text string for the
/// `ToolCallEventData.output` field (origin renders that).
fn tool_result_text(content: &[ToolResultContent]) -> Option<String> {
    let mut buf = String::new();
    for part in content {
        if let ToolResultContent::Text(t) = part {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(t);
        }
    }
    if buf.is_empty() { None } else { Some(buf) }
}

#[cfg(test)]
mod build_mapping_tests {
    //! Ported from clean-slate `session_runtime::tests` (the `spec_and_config` +
    //! `catalog_partial_from_caps` + `codex_sandbox`/`approval` + `session_server_to_spec`
    //! suite), adapted to the port's decomposed `spec_mode_model` inputs (AcpBuildExtra +
    //! PersistedSessionState) instead of a `ConversationRow`. Same assertions.
    use super::*;
    use crate::shared_kernel::{ModeId, ModelId};
    use aionui_session::SessionSpec;

    fn snapshot(mode: Option<&str>, model: Option<&str>) -> PersistedSessionState {
        PersistedSessionState {
            current_mode_id: mode.map(ModeId::new),
            current_model_id: model.map(ModelId::new),
            ..Default::default()
        }
    }

    #[test]
    fn assemble_spawn_env_orders_agent_overrides_before_runtime_context() {
        let agent_env = vec![
            aionui_api_types::AgentEnvEntry {
                name: "AWS_PROFILE".into(),
                value: "pionex".into(),
                description: None,
            },
            aionui_api_types::AgentEnvEntry {
                name: "HTTPS_PROXY".into(),
                value: "http://proxy:8080".into(),
                description: None,
            },
        ];
        let runtime_env = vec![
            ("AIONUI_USER_ID".to_owned(), "user-1".to_owned()),
            ("AIONUI_RUNTIME_TOKEN".to_owned(), "tok".to_owned()),
        ];
        let env = assemble_spawn_env(&agent_env, &runtime_env);
        let names: Vec<&str> = env.iter().map(|e| e.name.as_str()).collect();
        // Agent overrides first, runtime context after (later wins in
        // ManagedProcess::spawn, so the runtime context can never be shadowed).
        assert_eq!(
            names,
            ["AWS_PROFILE", "HTTPS_PROXY", "AIONUI_USER_ID", "AIONUI_RUNTIME_TOKEN"]
        );
        assert_eq!(env[0].value, "pionex");
        assert_eq!(env[3].value, "tok");
        assert!(
            assemble_spawn_env(&[], &[]).is_empty(),
            "empty in = empty out (inherit-only spawn)"
        );
    }

    #[test]
    fn session_cli_config_dump_value_captures_resolved_surface() {
        use aionui_session::{McpServerSpec, McpTransport, SessionConfig, SessionInit};
        let cfg = SessionConfig {
            model: Some("opus".into()),
            mode: Some("plan".into()),
            sandbox_mode: Some("danger-full-access".into()),
            approval_policy: Some("never".into()),
            cli_program: Some(std::path::PathBuf::from("/opt/claude")),
            spawn_env: vec![aionui_common::EnvVar {
                name: "ANTHROPIC_AUTH_TOKEN".into(),
                value: "raw-token".into(),
            }],
            init: SessionInit {
                preset_context: Some("SYSTEM PROMPT BODY".into()),
                skills: vec!["writer".into()],
                mcp_servers: vec![McpServerSpec {
                    name: "team".into(),
                    transport: McpTransport::Stdio {
                        command: "node".into(),
                        args: vec!["srv.js".into()],
                        env: vec![("K".into(), "V".into())],
                    },
                }],
                resume: true,
                ..Default::default()
            },
            ..Default::default()
        };

        let v = build_session_cli_config_dump_value("claude", &cfg);
        assert_eq!(v["input"]["backend"], "claude");
        assert_eq!(v["input"]["model"], "opus");
        assert_eq!(v["input"]["mode"], "plan");
        assert_eq!(v["input"]["sandbox_mode"], "danger-full-access");
        assert_eq!(v["input"]["approval_policy"], "never");
        assert_eq!(v["input"]["cli_program"], "/opt/claude");
        assert_eq!(v["input"]["resume"], true);
        // RAW, no redaction (dev-only), matching the acp dump behavior.
        assert_eq!(v["resolved_context"]["preset_context"], "SYSTEM PROMPT BODY");
        assert_eq!(v["resolved_context"]["skills"][0], "writer");
        assert_eq!(v["resolved_context"]["spawn_env"][0]["name"], "ANTHROPIC_AUTH_TOKEN");
        assert_eq!(v["resolved_context"]["spawn_env"][0]["value"], "raw-token");
        assert_eq!(v["resolved_context"]["mcp_servers"][0]["name"], "team");
        assert_eq!(v["resolved_context"]["mcp_servers"][0]["transport"]["type"], "stdio");
    }

    // Minimal catalog row for spec_mode_model's mode-normalize step. `backend` +
    // `yolo_id` drive the alias mapping; everything else is inert here.
    fn test_metadata(backend: Option<&str>, yolo_id: Option<&str>) -> aionui_api_types::AgentMetadata {
        use aionui_api_types::{AgentHandshake, AgentMetadata, AgentSource, AgentSourceInfo, BehaviorPolicy};
        use aionui_common::AgentType;
        AgentMetadata {
            id: "test".into(),
            icon: None,
            name: "Test".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: backend.map(ToOwned::to_owned),
            agent_type: AgentType::Acp,
            agent_source: AgentSource::Builtin,
            agent_source_info: AgentSourceInfo::default(),
            enabled: true,
            available: true,
            command: None,
            resolved_command: None,
            args: vec![],
            env: vec![],
            native_skills_dirs: None,
            behavior_policy: BehaviorPolicy::default(),
            yolo_id: yolo_id.map(ToOwned::to_owned),
            sort_order: 0,
            team_capable: false,
            last_check_status: None,
            last_check_kind: None,
            last_check_error_code: None,
            last_check_error_message: None,
            last_check_error_details: None,
            last_check_guidance: None,
            last_check_latency_ms: None,
            last_check_at: None,
            last_success_at: None,
            last_failure_at: None,
            handshake: AgentHandshake::default(),
            has_command_override: false,
            env_override_key_count: 0,
        }
    }

    #[test]
    fn spec_fresh_when_no_anchor() {
        let cfg = AcpBuildExtra::default();
        let (spec, mode, model) = spec_mode_model("conv_1", None, &cfg, None, &test_metadata(Some("claude"), None));
        assert!(matches!(spec, SessionSpec::Fresh { session_id } if session_id == "conv_1"));
        assert_eq!(mode, None);
        assert_eq!(model, None);
    }

    #[test]
    fn spec_resume_when_anchor_present() {
        let cfg = AcpBuildExtra {
            session_mode: Some("plan".into()),
            current_model_id: Some("claude-x".into()),
            ..Default::default()
        };
        let (spec, mode, model) = spec_mode_model(
            "conv_1",
            Some("bsid-xyz".into()),
            &cfg,
            None,
            &test_metadata(Some("claude"), None),
        );
        assert!(matches!(
            spec,
            SessionSpec::Resume { backend_session_id: Some(b), .. } if b == "bsid-xyz"
        ));
        assert_eq!(mode.as_deref(), Some("plan"));
        assert_eq!(model.as_deref(), Some("claude-x"));
    }

    // The interactive-switch-persisted snapshot selection MUST win over the
    // create-time config values on resume — else the user's choice is dropped on
    // respawn. (Clean-slate: spec_and_config_runtime_model_overrides_stale_model_column.)
    #[test]
    fn snapshot_mode_model_override_create_time_config() {
        let cfg = AcpBuildExtra {
            session_mode: Some("default".into()),
            current_model_id: Some("claude-sonnet-4-6".into()),
            ..Default::default()
        };
        let snap = snapshot(Some("plan"), Some("claude-opus-4-8"));
        let (_spec, mode, model) = spec_mode_model(
            "conv_1",
            Some("bsid".into()),
            &cfg,
            Some(&snap),
            &test_metadata(Some("claude"), None),
        );
        assert_eq!(mode.as_deref(), Some("plan"), "snapshot mode wins");
        assert_eq!(model.as_deref(), Some("claude-opus-4-8"), "snapshot model wins");
    }

    // Empty strings are filtered → None so the backend safe-defaults (never an empty
    // model/mode token on the wire).
    #[test]
    fn empty_selections_filter_to_none() {
        let cfg = AcpBuildExtra {
            session_mode: Some(String::new()),
            current_model_id: Some(String::new()),
            ..Default::default()
        };
        let (_spec, mode, model) = spec_mode_model("conv_1", None, &cfg, None, &test_metadata(Some("claude"), None));
        assert_eq!(mode, None);
        assert_eq!(model, None);
    }

    // HIGH-1 regression guard (equivalence audit): a persisted generic mode alias must
    // be normalized to the backend-native id via the catalog row — the SAME transform
    // the ACP path applies. Without it the raw alias reaches the backend on resume
    // (claude rejects an unknown permission-mode; codex mis-policies).
    #[test]
    fn mode_alias_is_normalized_via_catalog() {
        // codex: yoloNoSandbox → the row's yolo_id (full-access); default → auto.
        let codex = test_metadata(Some("codex"), Some("full-access"));
        let yolo_cfg = AcpBuildExtra {
            session_mode: Some("yoloNoSandbox".into()),
            ..Default::default()
        };
        let (_s, mode, _m) = spec_mode_model("c", None, &yolo_cfg, None, &codex);
        assert_eq!(
            mode.as_deref(),
            Some("full-access"),
            "yoloNoSandbox → codex native yolo_id"
        );

        let def_cfg = AcpBuildExtra {
            session_mode: Some("default".into()),
            ..Default::default()
        };
        let (_s, mode, _m) = spec_mode_model("c", None, &def_cfg, None, &codex);
        assert_eq!(mode.as_deref(), Some("auto"), "codex default → auto");

        // A native / non-alias mode passes through unchanged.
        let plan_cfg = AcpBuildExtra {
            session_mode: Some("plan".into()),
            ..Default::default()
        };
        let (_s, mode, _m) = spec_mode_model("c", None, &plan_cfg, None, &test_metadata(Some("claude"), None));
        assert_eq!(mode.as_deref(), Some("plan"), "non-alias mode unchanged");
    }

    /// G5: a discovered catalog projects mode/model as ACP `configOptions[]` + slash
    /// commands as `available_commands`; an empty catalog projects `None` (never
    /// clobbers the stored catalog). Ported verbatim from clean-slate.
    #[test]
    fn catalog_partial_projects_discovered_modes_models_commands() {
        use aionui_session::{ModeInfo, ModelInfo, SlashCommandInfo};
        let caps = aionui_session::Capabilities {
            available_modes: vec![ModeInfo {
                id: "plan".into(),
                name: "Plan".into(),
                description: Some("Planning".into()),
            }],
            current_mode: Some("plan".into()),
            available_models: vec![ModelInfo {
                id: "opus".into(),
                name: "Opus".into(),
                description: None,
                reasoning_efforts: Vec::new(),
            }],
            current_model: Some("opus".into()),
            slash_commands: vec![SlashCommandInfo {
                name: "review".into(),
                description: Some("Review a PR".into()),
            }],
            ..Default::default()
        };

        let partial = catalog_partial_from_caps(&caps).expect("a discovered catalog projects a partial");
        let cfg = partial.config_options.expect("config_options present");
        let opts = cfg.as_array().unwrap();
        assert_eq!(opts[0]["id"], "mode");
        assert_eq!(opts[0]["currentValue"], "plan");
        assert_eq!(opts[0]["options"][0]["value"], "plan");
        assert_eq!(opts[1]["id"], "model");
        assert_eq!(opts[1]["currentValue"], "opus");
        let cmds = partial.available_commands.expect("commands present");
        assert_eq!(cmds.as_array().unwrap()[0]["name"], "review");

        let empty = aionui_session::Capabilities::default();
        assert!(
            catalog_partial_from_caps(&empty).is_none(),
            "empty catalog projects nothing"
        );
    }

    /// Full-access / yolo escalates the codex sandbox to `danger-full-access`;
    /// read-only RESTRICTS it to `read-only` (so the FIRST turn is already locked
    /// down — the SetMode permission profile only lands on the NEXT turn); the
    /// workspace/auto middle tier keeps None ⇒ workspace-write.
    #[test]
    fn codex_sandbox_maps_full_access_and_read_only_modes() {
        // #608 canonical full-access id (migration 021 + normalize_requested_mode).
        assert_eq!(
            codex_sandbox_for_mode(Some("agent-full-access")),
            Some("danger-full-access")
        );
        // Plan B legacy bare token (pre-021 persisted data).
        assert_eq!(codex_sandbox_for_mode(Some("full-access")), Some("danger-full-access"));
        // The colon profile id (e.g. a readback that skipped bare-mapping) stays recognized.
        assert_eq!(
            codex_sandbox_for_mode(Some(":danger-full-access")),
            Some("danger-full-access")
        );
        assert_eq!(
            codex_sandbox_for_mode(Some("yoloNoSandbox")),
            Some("danger-full-access")
        );
        assert_eq!(
            codex_sandbox_for_mode(Some("  :danger-full-access  ")),
            Some("danger-full-access")
        );
        // read-only RESTRICTS the sandbox at OPEN time (regression fix: seeding this
        // at thread/start is what makes the first-turn write actually blocked; the
        // SetMode permission profile alone applies too late). Both the bare token and
        // the colon id (and surrounding whitespace) are recognized.
        assert_eq!(codex_sandbox_for_mode(Some("read-only")), Some("read-only"));
        assert_eq!(codex_sandbox_for_mode(Some(":read-only")), Some("read-only"));
        assert_eq!(codex_sandbox_for_mode(Some("  read-only  ")), Some("read-only"));
        // The workspace/auto middle tier keeps the safe workspace-write default.
        assert_eq!(codex_sandbox_for_mode(Some(":workspace")), None);
        assert_eq!(codex_sandbox_for_mode(Some("plan")), None);
        assert_eq!(codex_sandbox_for_mode(Some("default")), None);
        assert_eq!(codex_sandbox_for_mode(None), None);
    }

    /// Sibling of the sandbox map: a full-access / yolo mode drops approvals
    /// (→ "never"); everything else stays at on-request (None). Ported verbatim.
    #[test]
    fn codex_approval_maps_only_full_access_modes() {
        assert_eq!(codex_approval_for_mode(Some(":danger-full-access")), Some("never"));
        assert_eq!(codex_approval_for_mode(Some("agent-full-access")), Some("never"));
        assert_eq!(codex_approval_for_mode(Some("full-access")), Some("never"));
        assert_eq!(codex_approval_for_mode(Some("yoloNoSandbox")), Some("never"));
        assert_eq!(codex_approval_for_mode(Some("  :danger-full-access  ")), Some("never"));
        assert_eq!(codex_approval_for_mode(Some(":read-only")), None);
        assert_eq!(codex_approval_for_mode(Some(":workspace")), None);
        assert_eq!(codex_approval_for_mode(Some("plan")), None);
        assert_eq!(codex_approval_for_mode(Some("default")), None);
        assert_eq!(codex_approval_for_mode(None), None);
    }

    #[test]
    fn session_server_to_spec_collapses_4_transports_to_3_and_sorts_kv() {
        use aionui_api_types::{SessionMcpServer, SessionMcpTransport};
        use aionui_session::McpTransport;
        use std::collections::HashMap;

        let stdio = session_server_to_spec(&SessionMcpServer {
            id: "1".into(),
            name: "fs".into(),
            transport: SessionMcpTransport::Stdio {
                command: "/node".into(),
                args: vec!["s.js".into()],
                env: HashMap::from([("B".into(), "2".into()), ("A".into(), "1".into())]),
            },
        });
        assert_eq!(stdio.name, "fs");
        match stdio.transport {
            McpTransport::Stdio { command, env, .. } => {
                assert_eq!(command, "/node");
                assert_eq!(
                    env,
                    vec![("A".into(), "1".into()), ("B".into(), "2".into())],
                    "env sorted by key"
                );
            }
            other => panic!("expected Stdio, got {other:?}"),
        }

        for t in [
            SessionMcpTransport::StreamableHttp {
                url: "https://x".into(),
                headers: HashMap::new(),
            },
            SessionMcpTransport::Http {
                url: "https://x".into(),
                headers: HashMap::new(),
            },
        ] {
            let spec = session_server_to_spec(&SessionMcpServer {
                id: "2".into(),
                name: "h".into(),
                transport: t,
            });
            assert!(
                matches!(spec.transport, McpTransport::Http { .. }),
                "Http+StreamableHttp → Http"
            );
        }
    }
}

#[cfg(test)]
mod translate_tests {
    use super::*;
    use crate::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};
    use aionui_session::PermissionKind;

    fn usage_frame(total: u64, cost: Option<f64>, window: Option<u64>) -> serde_json::Value {
        let events = translate_event(
            SessionEvent::UsageDelta {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: total,
                cost_usd: cost,
                context_window: window,
                breakdown: Default::default(),
            },
            "conv-1",
            false,
        );
        match events.into_iter().next() {
            Some(AgentStreamEvent::AcpContextUsage(v)) => v,
            other => panic!("expected AcpContextUsage, got {other:?}"),
        }
    }

    /// The indicator needs a denominator: when the backend reports a context
    /// window it MUST ride the frame as `size`, alongside `used`. Values are the
    /// live-captured claude figures (occupancy 26_420 of a 1M window).
    #[test]
    fn context_window_rides_the_usage_frame_as_size() {
        let v = usage_frame(26_420, Some(0.117), Some(1_000_000));
        assert_eq!(v["used"], 26_420);
        assert_eq!(v["size"], 1_000_000);
        assert_eq!(v["cost"]["amount"], 0.117);
        assert_eq!(v["cost"]["currency"], "USD");
    }

    /// The detail line rides `_meta` under the five key names the renderer reads
    /// (AionUi `BREAKDOWN_KEYS`). A backend that reports nothing must emit NO
    /// `_meta` at all, so the renderer omits the line instead of drawing zeros.
    #[test]
    fn breakdown_rides_meta_under_the_renderer_key_names() {
        let with = translate_event(
            SessionEvent::UsageDelta {
                input_tokens: 1_100,
                output_tokens: 194,
                total_tokens: 18_400,
                cost_usd: None,
                context_window: Some(256_000),
                breakdown: aionui_session::UsageBreakdown {
                    cached_read_tokens: 16_900,
                    cached_write_tokens: 79,
                    thought_tokens: 242,
                },
            },
            "conv-1",
            false,
        );
        let v = match with.into_iter().next() {
            Some(AgentStreamEvent::AcpContextUsage(v)) => v,
            other => panic!("expected AcpContextUsage, got {other:?}"),
        };
        assert_eq!(v["_meta"]["input_tokens"], 1_100);
        assert_eq!(v["_meta"]["output_tokens"], 194);
        assert_eq!(v["_meta"]["cached_read_tokens"], 16_900);
        assert_eq!(v["_meta"]["cached_write_tokens"], 79);
        assert_eq!(v["_meta"]["thought_tokens"], 242);

        let without = usage_frame(18_400, None, Some(256_000));
        assert!(
            without.get("_meta").is_none(),
            "an empty breakdown must emit no `_meta`, got {without}"
        );
    }

    /// No window reported (codex `modelContextWindow: null`) → NO `size` key at
    /// all. Emitting `size: 0` would render a zero-width bar; the frontend guards
    /// on the key's absence instead.
    #[test]
    fn absent_context_window_emits_no_size_key() {
        let v = usage_frame(11_030, None, None);
        assert_eq!(v["used"], 11_030);
        assert!(v.get("size").is_none(), "no window → no `size` key, got {v}");
        assert!(v.get("cost").is_none(), "no cost → no `cost` key, got {v}");
    }

    fn tool_call(call_id: &str, name: &str, status: ToolCallStatus) -> AgentStreamEvent {
        AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: call_id.into(),
            name: name.into(),
            args: serde_json::Value::Null,
            status,
            input: None,
            output: None,
            description: None,
        })
    }

    // The bug: a tool's terminal ToolResult frame (and any codex ToolOutputDelta)
    // carries no name, so — persisted by upsert on call_id — it clobbered the tool
    // name to "" and the frontend rendered a nameless tool line. `stamp_tool_name`
    // remembers the name from the Running frame and refills the empty follow-ups.
    #[test]
    fn tool_name_survives_the_empty_name_result_frame() {
        let mut names = std::collections::HashMap::new();

        // Running frame carries the name; the map learns it.
        let mut running = tool_call("call-1", "Read", ToolCallStatus::Running);
        stamp_tool_name(&mut names, &mut running);
        assert_eq!(names.get("call-1").map(String::as_str), Some("Read"));

        // Codex live-output frame arrives with an empty name → refilled.
        let mut delta = tool_call("call-1", "", ToolCallStatus::Running);
        stamp_tool_name(&mut names, &mut delta);
        let AgentStreamEvent::ToolCall(d) = &delta else {
            unreachable!()
        };
        assert_eq!(d.name, "Read", "live-output frame must keep the name");

        // Terminal result frame arrives with an empty name → refilled, NOT clobbered.
        let mut result = tool_call("call-1", "", ToolCallStatus::Completed);
        stamp_tool_name(&mut names, &mut result);
        let AgentStreamEvent::ToolCall(r) = &result else {
            unreachable!()
        };
        assert_eq!(r.name, "Read", "result frame must keep the name, not go blank");
        assert_eq!(r.status, ToolCallStatus::Completed);
    }

    // A result frame for a call_id we never saw a name for stays empty (no panic,
    // no cross-call bleed) — and a different call_id is never cross-filled.
    #[test]
    fn stamp_tool_name_does_not_bleed_across_call_ids() {
        let mut names = std::collections::HashMap::new();
        let mut a = tool_call("call-a", "Bash", ToolCallStatus::Running);
        stamp_tool_name(&mut names, &mut a);

        let mut orphan = tool_call("call-b", "", ToolCallStatus::Completed);
        stamp_tool_name(&mut names, &mut orphan);
        let AgentStreamEvent::ToolCall(o) = &orphan else {
            unreachable!()
        };
        assert_eq!(o.name, "", "unknown call_id must not inherit another tool's name");
    }

    #[test]
    fn permission_surfaces_as_acp_permission_keyed_on_request_id() {
        let events = translate_event(
            SessionEvent::Permission {
                request_id: "req-42".into(),
                kind: PermissionKind::Tool,
                metadata: None,
                tool_name: Some("Bash".into()),
                input: Some(serde_json::json!({"command": "ls"})),
            },
            "conv-1",
            false,
        );
        assert_eq!(events.len(), 1, "permission must project to exactly one card");
        let crate::protocol::events::AgentStreamEvent::AcpPermission(
            crate::protocol::events::AcpPermissionEventData::Request(req),
        ) = &events[0]
        else {
            panic!("expected AcpPermission Request, got {:?}", events[0]);
        };
        // The confirm() path answers AnswerPermission keyed on this id — it MUST
        // equal the originating request_id or the approval never resolves.
        assert_eq!(req.tool_call.tool_call_id, "req-42");
        assert_eq!(req.tool_call.title.as_deref(), Some("Bash"));
        assert!(req.tool_call.raw_input.is_some(), "tool input rides as raw_input");
        // A NON-AskUserQuestion tool approval offers the generic Allow/AllowAlways/Reject.
        assert_eq!(req.options.len(), 3);
        let ids: Vec<&str> = req.options.iter().map(|o| o.option_id.as_str()).collect();
        assert_eq!(ids, vec!["allow", "allow_always", "reject"]);
    }

    // An AskUserQuestion permission must surface the QUESTION's own option labels as
    // the card choices (so the user answers the question), NOT a generic allow/deny.
    // This is the fix for "AskUserQuestion rendered as an allow box": the frontend
    // card renders whatever options[] the backend sends.
    #[test]
    fn ask_user_question_surfaces_question_options_not_allow_deny() {
        let events = translate_event(
            SessionEvent::Permission {
                request_id: "req-ask".into(),
                kind: PermissionKind::Tool,
                metadata: None,
                tool_name: Some("AskUserQuestion".into()),
                input: Some(serde_json::json!({
                    "questions": [{
                        "question": "Which database?",
                        "header": "DB",
                        "multiSelect": false,
                        "options": [
                            {"label": "Postgres", "description": "relational"},
                            {"label": "SQLite", "description": "embedded"}
                        ]
                    }]
                })),
            },
            "conv-1",
            false,
        );
        let crate::protocol::events::AgentStreamEvent::AcpPermission(
            crate::protocol::events::AcpPermissionEventData::Request(req),
        ) = &events[0]
        else {
            panic!("expected AcpPermission Request, got {:?}", events[0]);
        };
        let ids: Vec<&str> = req.options.iter().map(|o| o.option_id.as_str()).collect();
        let names: Vec<&str> = req.options.iter().map(|o| o.name.as_str()).collect();
        // The card offers the question's labels — option_id == label so confirm() can
        // forward the pick as the AnswerPermission answer label.
        assert_eq!(
            ids,
            vec!["Postgres", "SQLite"],
            "must render question options, not allow/deny"
        );
        assert_eq!(names, vec!["Postgres", "SQLite"]);
        assert!(!ids.contains(&"allow"), "must NOT be a generic allow/deny card");
    }

    // A malformed / optionless AskUserQuestion falls back to the generic card rather
    // than rendering an unanswerable empty option list.
    #[test]
    fn ask_user_question_without_options_falls_back_to_generic() {
        let events = translate_event(
            SessionEvent::Permission {
                request_id: "req-ask2".into(),
                kind: PermissionKind::Tool,
                metadata: None,
                tool_name: Some("AskUserQuestion".into()),
                input: Some(serde_json::json!({"questions": []})),
            },
            "conv-1",
            false,
        );
        let crate::protocol::events::AgentStreamEvent::AcpPermission(
            crate::protocol::events::AcpPermissionEventData::Request(req),
        ) = &events[0]
        else {
            panic!("expected AcpPermission Request");
        };
        let ids: Vec<&str> = req.options.iter().map(|o| o.option_id.as_str()).collect();
        assert_eq!(ids, vec!["allow", "allow_always", "reject"], "fallback to generic");
    }

    #[test]
    fn usage_delta_surfaces_as_context_usage() {
        let events = translate_event(
            SessionEvent::UsageDelta {
                input_tokens: 10,
                output_tokens: 20,
                total_tokens: 30,
                cost_usd: Some(0.5),
                context_window: None,
                breakdown: Default::default(),
            },
            "conv-1",
            false,
        );
        assert_eq!(events.len(), 1);
        let crate::protocol::events::AgentStreamEvent::AcpContextUsage(v) = &events[0] else {
            panic!("expected AcpContextUsage, got {:?}", events[0]);
        };
        // Frontend ContextUsageIndicator reads `used` (not `total_tokens`) — the
        // shape the ACP path forwards. `cost` rides as {amount,currency}.
        assert_eq!(v.get("used").and_then(|x| x.as_u64()), Some(30), "used = total_tokens");
        assert_eq!(
            v.get("cost").and_then(|c| c.get("amount")).and_then(|x| x.as_f64()),
            Some(0.5)
        );
        assert_eq!(
            v.get("cost").and_then(|c| c.get("currency")).and_then(|x| x.as_str()),
            Some("USD")
        );
    }

    // A backend Notice (a rejected mode/model/effort set, or a codex out-of-turn
    // warning/deprecation) must NOT be silently dropped at the seam — the backends emit
    // it precisely so the failure is visible. It surfaces as a `Tips` frame the frontend
    // already renders, carrying the notice level → TipType and the message verbatim.
    #[test]
    fn notice_surfaces_as_tips() {
        for (level, expected) in [
            (aionui_session::NoticeLevel::Warning, TipType::Warning),
            (aionui_session::NoticeLevel::Info, TipType::Info),
        ] {
            let events = translate_event(
                SessionEvent::Notice {
                    level,
                    message: "set effort: rejected by agent".into(),
                    localized: None,
                },
                "conv-1",
                false,
            );
            assert_eq!(events.len(), 1, "a Notice must produce exactly one Tips frame");
            let crate::protocol::events::AgentStreamEvent::Tips(tip) = &events[0] else {
                panic!("expected Tips, got {:?}", events[0]);
            };
            assert_eq!(tip.content, "set effort: rejected by agent");
            assert_eq!(tip.tip_type, expected, "notice level maps to tip severity");
            assert!(tip.code.is_none(), "ad-hoc notice carries no i18n code");
        }
    }

    // A ConfigChanged must NOT produce any stream frame: the origin frontend's mode/
    // model pickers track selection in local state (optimistic on the PUT + its REST
    // response), and an `acp_config_option` frame would fall into origin useAcpMessage's
    // `default:` arm and light a spurious turn timer bar. The selection is still
    // persisted separately (see persist_tests::config_changed_persists_mode_and_model).
    #[test]
    fn config_changed_emits_no_frame() {
        let events = translate_event(
            SessionEvent::ConfigChanged {
                mode: Some("plan".into()),
                model: Some("claude-opus-4-8".into()),
            },
            "conv-1",
            false,
        );
        assert!(
            events.is_empty(),
            "ConfigChanged must emit no stream frame, got {events:?}"
        );
    }

    // A codex Plan surfaces as AgentStreamEvent::Plan with the frontend's expected
    // entry shape: content + snake_case status (the MessagePlan renderer ticks on
    // status === 'completed'). A raw serde dump of PlanStatus would send PascalCase
    // and the card would never tick — this guards the explicit mapping.
    #[test]
    fn plan_surfaces_with_snake_case_status() {
        use aionui_session::{PlanEntry, PlanStatus};
        let events = translate_event(
            SessionEvent::Plan {
                entries: vec![
                    PlanEntry {
                        content: "step one".into(),
                        status: PlanStatus::Completed,
                        priority: None,
                    },
                    PlanEntry {
                        content: "step two".into(),
                        status: PlanStatus::InProgress,
                        priority: None,
                    },
                ],
                explanation: None,
            },
            "conv-1",
            false,
        );
        assert_eq!(events.len(), 1);
        let crate::protocol::events::AgentStreamEvent::Plan(data) = &events[0] else {
            panic!("expected Plan, got {:?}", events[0]);
        };
        assert_eq!(data.entries.len(), 2);
        assert_eq!(data.entries[0]["content"], "step one");
        assert_eq!(
            data.entries[0]["status"], "completed",
            "status must be snake_case for the frontend"
        );
        assert_eq!(data.entries[1]["status"], "in_progress");
    }

    // A user-cancelled turn must NOT surface an error Tips (claude reports the
    // interrupt as an is_error result, but the user asked for it — surfacing it pops
    // a spurious red bubble on every cancel). Only a plain Finish is emitted (no Error).
    #[test]
    fn cancelled_turn_emits_finish_without_error() {
        use aionui_session::{CancelReason, TurnOutcome};
        let events = translate_event(
            SessionEvent::TurnResult {
                is_error: true,
                api_error_status: None,
                result_text: "error_during_execution".into(),
                epoch: 0,
                outcome: TurnOutcome::Cancelled {
                    reason: CancelReason::UserCancel,
                },
            },
            "conv-1",
            false,
        );
        assert!(
            !events.iter().any(|e| matches!(e, AgentStreamEvent::Error(_))),
            "a cancelled turn must not emit an Error, got {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, AgentStreamEvent::Finish(_))),
            "a cancelled turn still finishes"
        );
    }

    // A genuine (non-cancel) error terminates as AgentStreamEvent::Error carrying the
    // full origin error model (code/ownership/retryable), NOT a plain Tips and NOT a
    // Finish (Error is itself the relay terminal). This is what lets the relay
    // classify + auto-replay and the frontend render ownership/feedback.
    #[test]
    fn errored_turn_emits_rich_error_terminal() {
        use aionui_session::{StopReason, TurnOutcome};
        let events = translate_event(
            SessionEvent::TurnResult {
                is_error: true,
                api_error_status: Some(500),
                result_text: "upstream exploded".into(),
                epoch: 0,
                outcome: TurnOutcome::Completed {
                    stop_reason: StopReason::EndTurn,
                },
            },
            "conv-1",
            false,
        );
        assert_eq!(
            events.len(),
            1,
            "a real error is a single Error terminal, got {events:?}"
        );
        let AgentStreamEvent::Error(data) = &events[0] else {
            panic!("expected Error terminal, got {:?}", events[0]);
        };
        // Classified through the origin error path → carries a code + ownership +
        // retryable (not a bare message). The exact code depends on the classifier;
        // the contract is that these fields are POPULATED, not None.
        assert!(!data.message.is_empty());
        assert!(data.code.is_some(), "error must carry a classified code");
        assert!(data.retryable.is_some(), "error must carry a retryable flag");
        // Must NOT also emit a Finish (Error is the terminal).
        assert!(!events.iter().any(|e| matches!(e, AgentStreamEvent::Finish(_))));
    }

    // A mid-turn process crash (Detached with a signal / non-zero / unknown exit and
    // NO prior terminal result) surfaces as a rich AgentStreamEvent::Error carrying the
    // legacy `UserAgentDisconnected` classification (code + retryable + ownership), with
    // the allowlisted redacted_summary appended to the message. This restores the ACP
    // path's `AcpError::Disconnected` terminal the direct-CLI bridge had collapsed to a
    // bare Finish (a dead CLI used to render as a normal empty completion).
    #[test]
    fn detached_crash_surfaces_as_rich_disconnect_error() {
        use aionui_session::ExitStatusLite;
        let events = translate_event(
            SessionEvent::Detached {
                exit: Some(ExitStatusLite {
                    code: None,
                    signal: Some(9),
                }),
                redacted_summary: Some("process killed (SIGKILL)".into()),
            },
            // No terminal TurnResult was seen this turn → a genuine mid-turn crash.
            "conv-1",
            false,
        );
        assert_eq!(events.len(), 1, "a crash is a single Error terminal, got {events:?}");
        let AgentStreamEvent::Error(data) = &events[0] else {
            panic!("expected Error terminal, got {:?}", events[0]);
        };
        // Classified through the SAME path legacy used → carries code + retryable, and
        // the allowlisted summary rides the user-facing message.
        assert!(data.code.is_some(), "crash must carry a classified code");
        assert!(data.retryable.is_some(), "crash must carry a retryable flag");
        assert!(
            data.message.contains("process killed (SIGKILL)"),
            "redacted_summary must ride the message, got {:?}",
            data.message
        );
        assert!(
            !events.iter().any(|e| matches!(e, AgentStreamEvent::Finish(_))),
            "Error is the terminal — must NOT also emit Finish"
        );
    }

    // A Detached that ARRIVES AFTER a terminal TurnResult is an absorbed teardown (the
    // reducer's I10 / `crash_outcome → FollowResult`), NOT a crash — it must end with a
    // plain Finish, never an error card (otherwise every normal turn's process exit
    // would pop a spurious "disconnected" error).
    #[test]
    fn detached_after_terminal_result_is_plain_finish() {
        use aionui_session::ExitStatusLite;
        let events = translate_event(
            SessionEvent::Detached {
                exit: Some(ExitStatusLite {
                    code: Some(1),
                    signal: None,
                }),
                redacted_summary: Some("exited".into()),
            },
            // The turn already reached its terminal TurnResult.
            "conv-1",
            true,
        );
        assert!(
            !events.iter().any(|e| matches!(e, AgentStreamEvent::Error(_))),
            "a post-terminal Detached must not emit an Error, got {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, AgentStreamEvent::Finish(_))),
            "an absorbed teardown still finishes, got {events:?}"
        );
    }

    // A clean exit-0 with no result (a genuinely blank turn) is NOT a crash — it ends
    // with a plain Finish (the empty-turn Tip rides the pump's status match, not here).
    #[test]
    fn detached_clean_exit_zero_is_plain_finish() {
        use aionui_session::ExitStatusLite;
        let events = translate_event(
            SessionEvent::Detached {
                exit: Some(ExitStatusLite {
                    code: Some(0),
                    signal: None,
                }),
                redacted_summary: None,
            },
            "conv-1",
            false,
        );
        assert!(
            !events.iter().any(|e| matches!(e, AgentStreamEvent::Error(_))),
            "a clean exit-0 must not emit an Error, got {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, AgentStreamEvent::Finish(_))),
            "a clean exit-0 finishes, got {events:?}"
        );
    }

    // --- empty-turn (blank-reply) diagnostic Tip, mirroring the ACP path ---

    fn tip_code(outcome: aionui_session::TurnOutcome) -> Option<(TipType, String)> {
        empty_turn_tip(&outcome).map(|t| (t.tip_type, t.code.unwrap()))
    }

    #[test]
    fn empty_turn_endturn_is_info_generic_code() {
        use aionui_session::{StopReason, TurnOutcome};
        // Both the legacy default `EndTurn` and the modern `Completed{EndTurn}` map
        // to the informational "no reply" note.
        for outcome in [
            TurnOutcome::EndTurn,
            TurnOutcome::Completed {
                stop_reason: StopReason::EndTurn,
            },
        ] {
            assert_eq!(tip_code(outcome), Some((TipType::Info, "ACP_EMPTY_TURN".to_owned())));
        }
    }

    #[test]
    fn empty_turn_truncation_and_refusal_map_to_acp_warning_codes() {
        use aionui_session::{StopReason, TruncationKind, TurnOutcome};
        // Exactly the codes the ACP path emits (agent_session_flow.rs empty_finish_tip_code).
        assert_eq!(
            tip_code(TurnOutcome::Completed {
                stop_reason: StopReason::Truncated(TruncationKind::MaxTokens),
            }),
            Some((TipType::Warning, "ACP_EMPTY_TURN_MAX_TOKENS".to_owned()))
        );
        assert_eq!(
            tip_code(TurnOutcome::Completed {
                stop_reason: StopReason::Truncated(TruncationKind::MaxTurns),
            }),
            Some((TipType::Warning, "ACP_EMPTY_TURN_MAX_TURN_REQUESTS".to_owned()))
        );
        assert_eq!(
            tip_code(TurnOutcome::Completed {
                stop_reason: StopReason::Refused { category: None },
            }),
            Some((TipType::Warning, "ACP_EMPTY_TURN_REFUSAL".to_owned()))
        );
    }

    #[test]
    fn empty_turn_other_truncation_and_failed_fall_back_to_generic_warning() {
        use aionui_session::{StopReason, TruncationKind, TurnOutcome};
        // Truncation kinds with no dedicated ACP code, plus a clean Failed, still
        // warn the user rather than silently rendering an empty bubble.
        for outcome in [
            TurnOutcome::Completed {
                stop_reason: StopReason::Truncated(TruncationKind::ContextWindow),
            },
            TurnOutcome::Completed {
                stop_reason: StopReason::Truncated(TruncationKind::Budget),
            },
            TurnOutcome::Failed,
        ] {
            assert_eq!(tip_code(outcome), Some((TipType::Warning, "ACP_EMPTY_TURN".to_owned())));
        }
    }

    #[test]
    fn empty_turn_cancelled_never_tips() {
        use aionui_session::{CancelReason, TurnOutcome};
        // A user interrupt is not a blank reply — no spurious tip.
        assert!(
            empty_turn_tip(&TurnOutcome::Cancelled {
                reason: CancelReason::UserCancel,
            })
            .is_none()
        );
    }

    #[test]
    fn user_visible_output_predicate_matches_renderable_frames() {
        // Frames that render in chat count as visible; lifecycle/metadata frames do not.
        assert!(event_is_user_visible_output(&AgentStreamEvent::Text(TextEventData {
            content: "hi".into(),
        })));
        assert!(event_is_user_visible_output(&tool_call(
            "c",
            "Read",
            ToolCallStatus::Running
        )));
        assert!(!event_is_user_visible_output(&AgentStreamEvent::Finish(
            FinishEventData::default()
        )));
        assert!(!event_is_user_visible_output(&AgentStreamEvent::SegmentBreak));
    }
}

#[cfg(test)]
mod persist_tests {
    //! The pump's persistence hookup — the writes the legacy ACP path performed via
    //! `AcpSessionSyncService` but which this direct-CLI path must do itself. Without
    //! these the resume anchor + mode/model precedence source are never written.
    use super::*;
    use aionui_db::{CreateAcpSessionParams, IAcpSessionRepository, SqliteAcpSessionRepository, init_database_memory};

    // Returns both the repo and the owning Database — the caller binds the Database
    // for the test's lifetime (the cloned SqlitePool keeps the in-memory DB alive).
    async fn seeded_repo() -> (Arc<dyn IAcpSessionRepository>, aionui_db::Database) {
        let db = init_database_memory().await.unwrap();
        // The scoped repo authorizes through the conversations parent chain, so
        // seed the owning user + conversation the acp_session row hangs off.
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
             VALUES ('user-1', 'user-1', 'hash', 0, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, status, created_at, updated_at, extra) \
             VALUES ('conv-1', 'user-1', 'conv-1', 'acp', 'pending', 0, 0, '{}')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let repo: Arc<dyn IAcpSessionRepository> = Arc::new(SqliteAcpSessionRepository::new(db.pool().clone()));
        repo.create(&CreateAcpSessionParams {
            user_id: "user-1",
            conversation_id: "conv-1",
            agent_source: "builtin",
            agent_id: "claude",
        })
        .await
        .unwrap();
        (repo, db)
    }

    fn usage(total: u64, cost: Option<f64>, window: Option<u64>) -> SessionEvent {
        SessionEvent::UsageDelta {
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: total,
            cost_usd: cost,
            context_window: window,
            breakdown: Default::default(),
        }
    }

    async fn stored_usage(repo: &dyn IAcpSessionRepository) -> serde_json::Value {
        let state = repo
            .load_runtime_state_for_user("user-1", "conv-1")
            .await
            .unwrap()
            .expect("runtime state exists");
        serde_json::from_str(state.context_usage_json.as_deref().expect("context_usage written")).unwrap()
    }

    /// Defect 1: claude reports usage on the `result` frame, which lands AFTER the
    /// turn's relay has broken on Finish. The pump is session-scoped and still
    /// running, so the write must land regardless of turn lifecycle. The event
    /// ORDER matters — a test that persisted a lone UsageDelta would not exercise
    /// the bug at all.
    #[tokio::test]
    async fn usage_arriving_after_the_turn_ended_is_still_persisted() {
        let (repo, _db) = seeded_repo().await;
        // The turn terminates first...
        persist_side_effects(
            repo.as_ref(),
            "user-1",
            "conv-1",
            &SessionEvent::TurnResult {
                is_error: false,
                api_error_status: None,
                result_text: String::new(),
                epoch: 0,
                outcome: aionui_session::TurnOutcome::EndTurn,
            },
        )
        .await;
        // ...and only then does claude's usage arrive.
        persist_side_effects(
            repo.as_ref(),
            "user-1",
            "conv-1",
            &usage(26_420, Some(0.117), Some(1_000_000)),
        )
        .await;

        let stored = stored_usage(repo.as_ref()).await;
        assert_eq!(stored["used"], 26_420, "late usage must still reach the snapshot");
        assert_eq!(stored["size"], 1_000_000, "the context window rides along as `size`");
        assert_eq!(stored["cost"]["amount"], 0.117);
        assert_eq!(stored["cost"]["currency"], "USD");
    }

    /// Writes MERGE. codex reports no cost at all and can report
    /// `modelContextWindow: null`, so a later sizeless/costless report must not
    /// blank a window the previous one established — that would make the indicator
    /// lose its denominator mid-conversation.
    #[tokio::test]
    async fn sizeless_costless_update_keeps_the_known_window_and_cost() {
        let (repo, _db) = seeded_repo().await;
        persist_side_effects(
            repo.as_ref(),
            "user-1",
            "conv-1",
            &usage(11_013, Some(0.5), Some(258_400)),
        )
        .await;
        persist_side_effects(repo.as_ref(), "user-1", "conv-1", &usage(11_030, None, None)).await;

        let stored = stored_usage(repo.as_ref()).await;
        assert_eq!(stored["used"], 11_030, "`used` always takes the newer value");
        assert_eq!(stored["size"], 258_400, "a sizeless update must not blank the window");
        assert_eq!(
            stored["cost"]["amount"], 0.5,
            "a costless update must not blank the cost"
        );
    }

    /// A `/compact` turn ends with an all-zero `usage` object (live-captured on
    /// claude 2.1.220: `num_turns: 0`, every token bucket 0, `total_cost_usd`
    /// unchanged). Recording it wiped the real figure to `used: 0` — observed in
    /// the wild as "the indicator showed 0, then vanished". The zero report must be
    /// dropped so the last true reading survives until the next real turn.
    #[tokio::test]
    async fn zero_usage_after_compaction_does_not_wipe_the_real_figure() {
        let (repo, _db) = seeded_repo().await;
        persist_side_effects(
            repo.as_ref(),
            "user-1",
            "conv-1",
            &usage(26_420, Some(0.0133), Some(1_000_000)),
        )
        .await;
        // The compaction turn: every token bucket 0, while cost reports what the
        // compaction itself spent (live-captured 0.0131 on a SUCCESSFUL compact; a
        // rejected one and `/clear` both report 0 — see `informative_usage`).
        persist_side_effects(
            repo.as_ref(),
            "user-1",
            "conv-1",
            &usage(0, Some(0.0133), Some(1_000_000)),
        )
        .await;

        let stored = stored_usage(repo.as_ref()).await;
        assert_eq!(
            stored["used"], 26_420,
            "a zero-token turn must not overwrite real occupancy"
        );
        assert_eq!(stored["size"], 1_000_000);
    }

    /// The live frame must match what the renderer actually switches on.
    /// `useAcpMessage.ts` handles `case 'acp_context_usage'`, reads `data.used`
    /// into the indicator and `data.size` into `context_limit` (only when > 0), so
    /// the tag and both key names are a hard contract — a rename silently blanks
    /// the indicator rather than failing anything.
    #[test]
    fn broadcast_frame_matches_the_renderer_contract() {
        #[derive(Default)]
        struct Recorder(std::sync::Mutex<Vec<aionui_api_types::WebSocketMessage<serde_json::Value>>>);
        impl EventBroadcaster for Recorder {
            fn broadcast(&self, event: aionui_api_types::WebSocketMessage<serde_json::Value>) {
                self.0.lock().unwrap().push(event);
            }
        }

        let bus = Recorder::default();
        broadcast_usage_frame(&bus, "conv-1", "user-1", &usage(26_420, Some(0.117), Some(1_000_000)));
        let sent = bus.0.lock().unwrap();
        let msg = sent.first().expect("one frame broadcast");
        assert_eq!(msg.name, "message.stream");
        assert_eq!(
            msg.data["type"], "acp_context_usage",
            "renderer switches on this exact tag"
        );
        assert_eq!(msg.data["data"]["used"], 26_420);
        assert_eq!(msg.data["data"]["size"], 1_000_000, "drives context_limit");
        assert_eq!(msg.data["conversation_id"], "conv-1");
    }

    /// The gate is shared by the snapshot and the live broadcast, so a report can
    /// never be pushed to the UI without also being stored.
    #[test]
    fn informative_usage_gates_zero_reports_only() {
        assert!(
            informative_usage(&usage(0, Some(1.0), Some(200_000))).is_none(),
            "a zero-token report says nothing about occupancy"
        );
        assert!(informative_usage(&usage(11_030, None, Some(258_400))).is_some());
    }

    /// Defect 3: after a conversation switch the task is rebuilt with no in-memory
    /// usage, so `get_usage` must serve the value straight out of the snapshot —
    /// otherwise the indicator sits blank until the next turn produces fresh usage.
    #[tokio::test]
    async fn get_usage_serves_the_persisted_snapshot_on_a_cold_task() {
        let (repo, _db) = seeded_repo().await;
        persist_side_effects(repo.as_ref(), "user-1", "conv-1", &usage(47_579, None, Some(258_400))).await;

        // A freshly built task: nothing in memory, only the repo.
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/tmp".into(),
            Arc::new(super::pump_tests::StaticCapsBackend),
            Some(repo.clone()),
        );
        let served = task.get_usage().await.unwrap().expect("cold task serves the snapshot");
        assert_eq!(served["used"], 47_579);
        assert_eq!(served["size"], 258_400);
    }

    #[tokio::test]
    async fn backend_bound_persists_resume_anchor() {
        let (repo, _db) = seeded_repo().await;
        persist_side_effects(
            repo.as_ref(),
            "user-1",
            "conv-1",
            &SessionEvent::BackendBound {
                backend_session_id: Some("bsid-abc".into()),
            },
        )
        .await;
        let row = repo
            .get_for_user("user-1", "conv-1")
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(
            row.session_id.as_deref(),
            Some("bsid-abc"),
            "BackendBound must write the resume anchor build_session_instance reads back"
        );
    }

    #[tokio::test]
    async fn backend_bound_none_does_not_clobber_anchor() {
        let (repo, _db) = seeded_repo().await;
        repo.update_session_id_for_user("user-1", "conv-1", "bsid-existing")
            .await
            .unwrap();
        persist_side_effects(
            repo.as_ref(),
            "user-1",
            "conv-1",
            &SessionEvent::BackendBound {
                backend_session_id: None,
            },
        )
        .await;
        let row = repo
            .get_for_user("user-1", "conv-1")
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(
            row.session_id.as_deref(),
            Some("bsid-existing"),
            "BackendBound{{None}} (lost-backend self-heal) must leave the stored anchor intact"
        );
    }

    #[tokio::test]
    async fn config_changed_persists_mode_and_model() {
        let (repo, _db) = seeded_repo().await;
        persist_side_effects(
            repo.as_ref(),
            "user-1",
            "conv-1",
            &SessionEvent::ConfigChanged {
                mode: Some("plan".into()),
                model: Some("claude-opus-4-8".into()),
            },
        )
        .await;
        let state = repo
            .load_runtime_state_for_user("user-1", "conv-1")
            .await
            .unwrap()
            .expect("runtime state");
        assert_eq!(state.current_mode_id.as_deref(), Some("plan"));
        assert_eq!(state.current_model_id.as_deref(), Some("claude-opus-4-8"));
    }

    // #4: a `set_config_option("effort", ...)` must persist the level into
    // config_selections — claude emits NO ConfigChanged for effort, so unless
    // set_config_option writes it directly, effort is lost across respawn/resume.
    // This drives the real chokepoint (a task built around a NoStreamBackend + the
    // seeded repo) and asserts the level lands in the persisted config_selections.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_config_option_effort_persists_into_config_selections() {
        use super::pump_tests::StaticCapsBackend;
        let (repo, _db) = seeded_repo().await;
        let backend: Arc<dyn SessionBackend> = Arc::new(StaticCapsBackend);
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            Some(repo.clone()),
        );

        let resp = task.set_config_option("effort", "high").await.unwrap();
        assert!(
            matches!(
                resp.confirmation,
                aionui_api_types::ConfigOptionConfirmation::CommandAck
            ),
            "effort reports CommandAck (no picker current_value to observe)"
        );

        // Persisted under the effort key so build_session_instance can re-apply it.
        let state = repo
            .load_runtime_state_for_user("user-1", "conv-1")
            .await
            .unwrap()
            .expect("runtime state");
        let selections: std::collections::HashMap<String, String> = serde_json::from_str(
            state
                .config_selections_json
                .as_deref()
                .expect("config_selections persisted"),
        )
        .unwrap();
        assert_eq!(
            selections.get(EFFORT_CONFIG_KEY).map(String::as_str),
            Some("high"),
            "the chosen effort must be persisted into config_selections"
        );
    }

    /// A backend whose dispatch reports the session as gone (codex resume-poison:
    /// `bound_thread_within` fails fast after a rejected thread/resume).
    struct DeadSessionBackend;

    #[async_trait::async_trait]
    impl SessionBackend for DeadSessionBackend {
        async fn dispatch(&self, _c: Command) -> Result<CommandReceipt, BackendError> {
            Err(BackendError::SessionNotFound(
                "codex thread/resume failed: no rollout found for thread id th-dead".into(),
            ))
        }
        fn events(&self) -> BoxStream<'static, SessionEnvelope> {
            use futures_util::StreamExt as _;
            futures_util::stream::empty().boxed()
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
    }

    // ELECTRON-3Q0 fix B2: a DISPATCH-time dead-session error never becomes a
    // `TurnResult` on the event pump, so the stream-side self-heal
    // (`is_dead_resume_anchor`) cannot clear the anchor for it. send_message must
    // clear it directly and classify the failure as the retryable
    // UserAgentSessionNotFound so the turn orchestrator auto-replays (Fresh).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_dispatch_session_not_found_clears_anchor_and_classifies() {
        let (repo, _db) = seeded_repo().await;
        repo.update_session_id_for_user("user-1", "conv-1", "dead-anchor")
            .await
            .unwrap();
        let backend: Arc<dyn SessionBackend> = Arc::new(DeadSessionBackend);
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            Some(repo.clone()),
        );

        let err = crate::agent_task::IAgentTask::send_message(
            task.as_ref(),
            SendMessageData {
                content: "hi".into(),
                msg_id: "m1".into(),
                turn_id: None,
                files: Vec::new(),
                inject_skills: Vec::new(),
            },
        )
        .await
        .expect_err("dispatch fails with SessionNotFound");

        assert_eq!(
            err.code(),
            Some(aionui_api_types::AgentErrorCode::UserAgentSessionNotFound),
            "classified as the retryable session-not-found so TurnRecoveryPolicy replays once"
        );
        let row = repo
            .get_for_user("user-1", "conv-1")
            .await
            .unwrap()
            .expect("row exists");
        assert!(
            row.session_id.is_none(),
            "a dispatch-time dead session must clear the resume anchor — the replay/next send opens Fresh"
        );
    }

    // A backend that advertises per-model reasoning efforts (claude `supportedEffortLevels`),
    // used to prove the effort axis is surfaced to the frontend picker. The prior gap:
    // `get_config_options` emitted only mode+model, so the origin frontend's
    // `deriveSelectOption(..., 'thought_level', ['reasoning_effort'])` found nothing and the
    // top-right selector never showed a thinking/effort group even though claude advertises
    // it and the backend can set it.
    use aionui_session::{
        Admission, BackendError, Capabilities, Command, CommandReceipt, SessionBackend, SessionEnvelope,
    };
    use futures_util::stream::BoxStream;

    struct EffortCapsBackend;

    #[async_trait::async_trait]
    impl SessionBackend for EffortCapsBackend {
        async fn dispatch(&self, _c: Command) -> Result<CommandReceipt, BackendError> {
            Ok(CommandReceipt {
                accepted: true,
                admission: Admission::NoTurn,
                turn_gen: 0,
            })
        }
        fn events(&self) -> BoxStream<'static, SessionEnvelope> {
            use futures_util::StreamExt as _;
            futures_util::stream::empty().boxed()
        }
        fn capabilities(&self) -> Capabilities {
            use aionui_session::ModelInfo;
            Capabilities {
                available_models: vec![
                    ModelInfo {
                        id: "opus".into(),
                        name: "Opus".into(),
                        description: None,
                        reasoning_efforts: vec!["low".into(), "medium".into(), "high".into(), "max".into()],
                    },
                    ModelInfo {
                        id: "haiku".into(),
                        name: "Haiku".into(),
                        description: None,
                        reasoning_efforts: vec![],
                    },
                ],
                current_model: Some("opus".into()),
                ..Default::default()
            }
        }
    }

    // The effort ("thought level") axis MUST be surfaced as a config option so the origin
    // frontend renders it in the top-right model selector (parity with the ACP path's
    // `thought_level` option). Asserts the exact shape the frontend keys off: category
    // `thought_level`, id `reasoning_effort`, and the current model's advertised levels.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_config_options_surfaces_reasoning_effort_for_current_model() {
        let backend: Arc<dyn SessionBackend> = Arc::new(EffortCapsBackend);
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
        );
        let snapshot = task.get_config_options().await.unwrap();
        let effort = snapshot
            .config_options
            .iter()
            .find(|o| o.category.as_deref() == Some("thought_level"))
            .expect("effort axis must be surfaced as a thought_level config option");
        assert_eq!(
            effort.id, "reasoning_effort",
            "canonical id the frontend fallback matches"
        );
        assert_eq!(effort.option_type, "select");
        let values: Vec<&str> = effort.options.iter().map(|o| o.value.as_str()).collect();
        assert_eq!(
            values,
            vec!["low", "medium", "high", "max"],
            "the current model's advertised efforts"
        );
    }

    // A model with no advertised efforts (claude `haiku`) must NOT get an effort option —
    // an empty select would render a dead, choice-less group.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_config_options_omits_effort_when_current_model_has_none() {
        struct HaikuCurrentBackend;
        #[async_trait::async_trait]
        impl SessionBackend for HaikuCurrentBackend {
            async fn dispatch(&self, _c: Command) -> Result<CommandReceipt, BackendError> {
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: 0,
                })
            }
            fn events(&self) -> BoxStream<'static, SessionEnvelope> {
                use futures_util::StreamExt as _;
                futures_util::stream::empty().boxed()
            }
            fn capabilities(&self) -> Capabilities {
                use aionui_session::ModelInfo;
                Capabilities {
                    available_models: vec![ModelInfo {
                        id: "haiku".into(),
                        name: "Haiku".into(),
                        description: None,
                        reasoning_efforts: vec![],
                    }],
                    current_model: Some("haiku".into()),
                    ..Default::default()
                }
            }
        }
        let backend: Arc<dyn SessionBackend> = Arc::new(HaikuCurrentBackend);
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
        );
        let snapshot = task.get_config_options().await.unwrap();
        assert!(
            snapshot
                .config_options
                .iter()
                .all(|o| o.category.as_deref() != Some("thought_level")),
            "a model with no advertised efforts must not surface an (empty) effort option"
        );
    }

    // Setting effort must read back as Observed with the requested level (the frontend's
    // `hasObservedValue` contract), driven by the optimistic override — claude emits no
    // effort echo, so without the override the switch would downgrade to command_ack and
    // the frontend would reject it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_config_option_effort_returns_observed_via_override() {
        let (repo, _db) = seeded_repo().await;
        let backend: Arc<dyn SessionBackend> = Arc::new(EffortCapsBackend);
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            Some(repo),
        );
        let resp = task.set_config_option("reasoning_effort", "high").await.unwrap();
        assert!(
            matches!(resp.confirmation, aionui_api_types::ConfigOptionConfirmation::Observed),
            "effort switch must be Observed (optimistic override), not command_ack"
        );
        let effort = resp
            .config_options
            .as_ref()
            .and_then(|opts| opts.iter().find(|o| o.category.as_deref() == Some("thought_level")))
            .expect("effort option in the observed snapshot");
        assert_eq!(
            effort.current_value.as_deref(),
            Some("high"),
            "the requested level is highlighted"
        );
    }

    // ── Defect 2: dead-resume-anchor self-heal ────────────────────────────
    // A turn that fails *because* the stored backend session no longer resolves must
    // NULL that anchor, or every subsequent send re-resumes the same dead id and the
    // conversation wedges forever. This restores the self-heal the direct-CLI path
    // dropped (clean-slate `Orchestrator` BackendBound{None}; legacy ACP
    // rebuild_after_session_not_found → clear_session_id).

    fn errored_turn(text: &str) -> SessionEvent {
        use aionui_session::{StopReason, TurnOutcome};
        SessionEvent::TurnResult {
            is_error: true,
            api_error_status: None,
            result_text: text.into(),
            epoch: 0,
            outcome: TurnOutcome::Completed {
                stop_reason: StopReason::EndTurn,
            },
        }
    }

    #[tokio::test]
    async fn no_conversation_found_clears_dead_anchor() {
        let (repo, _db) = seeded_repo().await;
        repo.update_session_id_for_user("user-1", "conv-1", "dead-sid")
            .await
            .unwrap();
        persist_side_effects(
            repo.as_ref(),
            "user-1",
            "conv-1",
            &errored_turn("No conversation found with session ID dead-sid"),
        )
        .await;
        let row = repo
            .get_for_user("user-1", "conv-1")
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(
            row.session_id, None,
            "an unrecoverable resume error must null the dead anchor so the next turn opens Fresh"
        );
    }

    #[tokio::test]
    async fn error_during_execution_clears_dead_anchor() {
        let (repo, _db) = seeded_repo().await;
        repo.update_session_id_for_user("user-1", "conv-1", "dead-sid")
            .await
            .unwrap();
        persist_side_effects(
            repo.as_ref(),
            "user-1",
            "conv-1",
            &errored_turn("error_during_execution"),
        )
        .await;
        let row = repo
            .get_for_user("user-1", "conv-1")
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(
            row.session_id, None,
            "error_during_execution is a structural resume failure"
        );
    }

    #[tokio::test]
    async fn ordinary_error_keeps_anchor() {
        let (repo, _db) = seeded_repo().await;
        repo.update_session_id_for_user("user-1", "conv-1", "live-sid")
            .await
            .unwrap();
        // A normal tool/turn error is NOT a resume failure — the anchor is still good.
        persist_side_effects(
            repo.as_ref(),
            "user-1",
            "conv-1",
            &errored_turn("the Bash tool exited with code 1"),
        )
        .await;
        let row = repo
            .get_for_user("user-1", "conv-1")
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(
            row.session_id.as_deref(),
            Some("live-sid"),
            "an ordinary error must NOT clear a still-valid resume anchor"
        );
    }

    #[tokio::test]
    async fn cancelled_turn_keeps_anchor_even_with_matching_text() {
        use aionui_session::{CancelReason, TurnOutcome};
        let (repo, _db) = seeded_repo().await;
        repo.update_session_id_for_user("user-1", "conv-1", "live-sid")
            .await
            .unwrap();
        // claude reports a user interrupt as is_error with cancel-noise text; the
        // anchor is still good, so a cancel must never trigger the self-heal.
        persist_side_effects(
            repo.as_ref(),
            "user-1",
            "conv-1",
            &SessionEvent::TurnResult {
                is_error: true,
                api_error_status: None,
                result_text: "error_during_execution".into(),
                epoch: 0,
                outcome: TurnOutcome::Cancelled {
                    reason: CancelReason::UserCancel,
                },
            },
        )
        .await;
        let row = repo
            .get_for_user("user-1", "conv-1")
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(
            row.session_id.as_deref(),
            Some("live-sid"),
            "a user-cancelled turn must NOT clear the anchor"
        );
    }

    // Pure classification matrix for the FCIS core, independent of the DB.
    #[test]
    fn is_dead_resume_anchor_matrix() {
        use aionui_session::{CancelReason, StopReason, TurnOutcome};
        let completed = |is_error: bool, text: &str| SessionEvent::TurnResult {
            is_error,
            api_error_status: None,
            result_text: text.into(),
            epoch: 0,
            outcome: TurnOutcome::Completed {
                stop_reason: StopReason::EndTurn,
            },
        };
        // Structural resume failures → dead anchor.
        assert!(is_dead_resume_anchor(&completed(
            true,
            "No conversation found: dead-sid"
        )));
        assert!(is_dead_resume_anchor(&completed(true, "error_during_execution")));
        // is_error:false is never a dead anchor, even with matching text.
        assert!(!is_dead_resume_anchor(&completed(false, "No conversation found")));
        // Ordinary error text is not a resume failure.
        assert!(!is_dead_resume_anchor(&completed(true, "tool call failed")));
        // A user-cancel is excluded even when the noise text matches.
        assert!(!is_dead_resume_anchor(&SessionEvent::TurnResult {
            is_error: true,
            api_error_status: None,
            result_text: "error_during_execution".into(),
            epoch: 0,
            outcome: TurnOutcome::Cancelled {
                reason: CancelReason::UserCancel,
            },
        }));
        // Non-TurnResult events are never dead anchors.
        assert!(!is_dead_resume_anchor(&SessionEvent::BackendBound {
            backend_session_id: Some("x".into()),
        }));
    }
}

#[cfg(test)]
mod pump_tests {
    //! End-to-end pump tests over a scripted `SessionBackend`: they assert the
    //! forwarded `AgentStreamEvent` sequence for a realistic claude event stream,
    //! locking in the ACP-alignment fixes found by the live frame-by-frame A/B
    //! (Start emitted by send_message before dispatch; opening ConfigChanged
    //! suppressed; Finish carries the CLI session id learned from BackendBound).
    use super::*;
    use aionui_session::{
        Admission, BackendError, Capabilities, Command, CommandReceipt, SessionBackend, SessionEnvelope, SessionEvent,
    };
    use futures_util::stream::BoxStream;

    /// Emits a fixed script on `events()`; `dispatch(Send)` admits a turn.
    struct ScriptBackend(Vec<SessionEnvelope>);

    #[async_trait::async_trait]
    impl SessionBackend for ScriptBackend {
        async fn dispatch(&self, c: Command) -> Result<CommandReceipt, BackendError> {
            let admission = match c {
                Command::Send { .. } => Admission::Started,
                _ => Admission::NoTurn,
            };
            Ok(CommandReceipt {
                accepted: true,
                admission,
                turn_gen: 1,
            })
        }
        fn events(&self) -> BoxStream<'static, SessionEnvelope> {
            use futures_util::StreamExt as _;
            futures_util::stream::iter(self.0.clone()).boxed()
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
    }

    fn env(event: SessionEvent) -> SessionEnvelope {
        env_gen(1, event)
    }

    fn env_gen(turn_gen: u64, event: SessionEvent) -> SessionEnvelope {
        SessionEnvelope {
            session_id: "conv-1".into(),
            turn_gen,
            event,
        }
    }

    /// Like `ScriptBackend`, but holds every scripted frame until its `gate` is
    /// released — so a test can `subscribe()` to the task BEFORE any frame is
    /// pumped. This deterministically closes a subscribe-vs-pump race: the pump is
    /// spawned inside `build()` (it calls `events()` and starts polling at once), and
    /// on a `multi_thread` runtime it can otherwise emit — and DROP — frames before the
    /// test's `subscribe()` runs, because the broadcast channel keeps no pre-subscribe
    /// buffer. `drain_script` subscribes first, THEN releases the gate. This mirrors
    /// production, where the backend's event stream is subscribed at open but stays
    /// silent until the CLI emits (well after the frontend's WS subscribe).
    struct GatedScriptBackend {
        script: Vec<SessionEnvelope>,
        gate: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl SessionBackend for GatedScriptBackend {
        async fn dispatch(&self, c: Command) -> Result<CommandReceipt, BackendError> {
            let admission = match c {
                Command::Send { .. } => Admission::Started,
                _ => Admission::NoTurn,
            };
            Ok(CommandReceipt {
                accepted: true,
                admission,
                turn_gen: 1,
            })
        }
        fn events(&self) -> BoxStream<'static, SessionEnvelope> {
            use futures_util::StreamExt as _;
            let gate = self.gate.clone();
            let script = self.script.clone();
            // First poll parks on the gate; only once released does the scripted
            // sequence flow — so no frame can predate the test's subscribe().
            futures_util::stream::once(async move { gate.notified().await })
                .flat_map(move |_| futures_util::stream::iter(script.clone()))
                .boxed()
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
    }

    // Build a task over a gated script, subscribe, THEN release the gate, and collect
    // every frame the task forwards until its (finite) event stream drains. Subscribing
    // before releasing is what makes the collection deterministic — see
    // `GatedScriptBackend`.
    async fn drain_script(script: Vec<SessionEnvelope>) -> Vec<AgentStreamEvent> {
        let gate = Arc::new(tokio::sync::Notify::new());
        let backend: Arc<dyn SessionBackend> = Arc::new(GatedScriptBackend {
            script,
            gate: gate.clone(),
        });
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
        );
        let mut rx = crate::agent_task::IAgentTask::subscribe(task.as_ref());
        // Release only AFTER subscribing: the pump cannot emit before we listen.
        gate.notify_one();
        let mut out = Vec::new();
        // The scripted stream is finite; once the pump drains it, no more frames
        // arrive, so a short bounded poll settles the collection (no live agent).
        while let Ok(Ok(ev)) = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv()).await {
            out.push(ev);
        }
        // Keep the task (hence the backend + pump) alive through the whole drain.
        drop(task);
        out
    }

    fn frame_name(ev: &AgentStreamEvent) -> &'static str {
        match ev {
            AgentStreamEvent::Start(_) => "start",
            AgentStreamEvent::Text(_) => "content",
            AgentStreamEvent::Finish(_) => "finish",
            AgentStreamEvent::AcpConfigOption(_) => "config",
            AgentStreamEvent::AcpContextUsage(_) => "usage",
            AgentStreamEvent::SegmentBreak => "SegmentBreak",
            _ => "other",
        }
    }

    // A ConfigChanged never produces a stream frame (it would fall into origin
    // useAcpMessage's `default:` arm and light a spurious timer bar), and the
    // BackendBound session id still reaches the Finish frame. (Mirrors the live A/B
    // finding: the session path must NOT emit a stray acp_config_option.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn config_change_emits_no_frame_and_bound_id_reaches_finish() {
        let script = vec![
            env(SessionEvent::BackendBound {
                backend_session_id: Some("sid-xyz".into()),
            }),
            env(SessionEvent::ConfigChanged {
                mode: Some("default".into()),
                model: None,
            }),
            env(SessionEvent::MessageDelta {
                item_id: "m".into(),
                text: "hi".into(),
            }),
            env(SessionEvent::TurnResult {
                is_error: false,
                api_error_status: None,
                result_text: String::new(),
                epoch: 0,
                outcome: aionui_session::TurnOutcome::EndTurn,
            }),
        ];
        let frames = drain_script(script).await;
        let seq: Vec<&str> = frames.iter().map(frame_name).collect();
        // No leading "config"; the turn body + terminal come through.
        assert!(
            !seq.contains(&"config"),
            "opening ConfigChanged must be suppressed, got {seq:?}"
        );
        assert_eq!(seq, vec!["content", "finish"], "got {seq:?}");
        // The Finish carries the CLI session id learned from BackendBound.
        let finish = frames.iter().rev().find(|f| matches!(f, AgentStreamEvent::Finish(_)));
        let AgentStreamEvent::Finish(data) = finish.expect("finish present") else {
            unreachable!()
        };
        assert_eq!(
            data.session_id.as_deref(),
            Some("sid-xyz"),
            "resume anchor rides Finish"
        );
    }

    // THE FIX (stuck View-Steps spinner after interrupt): a tool call whose turn
    // ends without a terminal `ToolResult` (user cancel / crash / dropped result)
    // must be closed with a `Canceled` frame BEFORE the Finish — otherwise the
    // persisted tool_call row stays status "work" forever and the frontend spinner
    // (`hasRunningToolMessages`) never stops, surviving reloads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_turn_closes_open_tool_calls_as_canceled_before_finish() {
        use aionui_session::{CancelReason, SubagentKind, TurnOutcome};
        let script = vec![
            env(SessionEvent::TurnStarted { epoch: 1 }),
            env(SessionEvent::ToolCall {
                tool_use_id: "call-1".into(),
                name: "Bash".into(),
                subagent: SubagentKind::Inline,
                input: serde_json::Value::Null,
                parent_tool_use_id: None,
            }),
            env(SessionEvent::TurnResult {
                is_error: true,
                api_error_status: None,
                result_text: "error_during_execution".into(),
                epoch: 1,
                outcome: TurnOutcome::Cancelled {
                    reason: CancelReason::UserCancel,
                },
            }),
        ];
        let frames = drain_script(script).await;
        let canceled_pos = frames.iter().position(|f| {
            matches!(f, AgentStreamEvent::ToolCall(d)
                if d.call_id == "call-1" && d.status == ToolCallStatus::Canceled && d.name == "Bash")
        });
        let finish_pos = frames.iter().position(|f| matches!(f, AgentStreamEvent::Finish(_)));
        let canceled_pos = canceled_pos.expect("open tool call must be closed with a Canceled frame");
        let finish_pos = finish_pos.expect("cancelled turn still finishes");
        assert!(
            canceled_pos < finish_pos,
            "Canceled must precede Finish (relay breaks the turn on Finish)"
        );
    }

    // A tool call that DID receive its ToolResult must NOT get a trailing Canceled
    // frame at turn end — only calls left open are closed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_tool_call_is_not_recanceled_at_turn_end() {
        use aionui_session::SubagentKind;
        let script = vec![
            env(SessionEvent::TurnStarted { epoch: 1 }),
            env(SessionEvent::ToolCall {
                tool_use_id: "call-1".into(),
                name: "Bash".into(),
                subagent: SubagentKind::Inline,
                input: serde_json::Value::Null,
                parent_tool_use_id: None,
            }),
            env(SessionEvent::ToolResult {
                tool_use_id: "call-1".into(),
                is_error: false,
                content: vec![],
                parent_tool_use_id: None,
            }),
            env(SessionEvent::TurnResult {
                is_error: false,
                api_error_status: None,
                result_text: "done".into(),
                epoch: 1,
                outcome: aionui_session::TurnOutcome::EndTurn,
            }),
        ];
        let frames = drain_script(script).await;
        assert!(
            !frames
                .iter()
                .any(|f| { matches!(f, AgentStreamEvent::ToolCall(d) if d.status == ToolCallStatus::Canceled) }),
            "a resolved tool call must not be closed again as Canceled, got {frames:?}"
        );
    }

    // The FIX (async catalog-arrival push): a `CatalogUpdated` (the direct-CLI
    // analogue of ACP's `emit_snapshot_events`) MUST project to exactly one
    // `AcpConfigOption` frame carrying BOTH the model and mode categories — the
    // frontend's `useAcpConfigOptions` replaces its whole snapshot on this frame, so
    // omitting a sibling category would wipe that picker. Before this the catalog
    // arrived ~6s after open with no upward frame, so the model selector stayed
    // disabled. Unlike `ConfigChanged` (suppressed), this frame is the intended signal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn catalog_updated_projects_config_option_with_model_and_mode() {
        use aionui_session::{ModeInfo, ModelInfo};
        let script = vec![env(SessionEvent::CatalogUpdated {
            models: vec![
                ModelInfo {
                    id: "default".into(),
                    name: "Default".into(),
                    description: None,
                    reasoning_efforts: Vec::new(),
                },
                ModelInfo {
                    id: "opus".into(),
                    name: "Opus".into(),
                    description: None,
                    reasoning_efforts: Vec::new(),
                },
            ],
            modes: vec![ModeInfo {
                id: "plan".into(),
                name: "Plan".into(),
                description: None,
            }],
            slash_commands: Vec::new(),
        })];
        let frames = drain_script(script).await;
        let config = frames
            .iter()
            .find_map(|f| match f {
                AgentStreamEvent::AcpConfigOption(v) => Some(v),
                _ => None,
            })
            .expect("CatalogUpdated must project to an AcpConfigOption frame");
        let options = config
            .get("config_options")
            .and_then(|v| v.as_array())
            .expect("config_options array");
        let categories: Vec<&str> = options
            .iter()
            .filter_map(|o| o.get("category").and_then(|c| c.as_str()))
            .collect();
        assert!(
            categories.contains(&"model") && categories.contains(&"mode"),
            "both categories must ride the snapshot (else a sibling picker is wiped), got {categories:?}"
        );
        // The model category carries the parsed catalog so `canSwitch` derives true.
        let model_opt = options
            .iter()
            .find(|o| o.get("category").and_then(|c| c.as_str()) == Some("model"))
            .expect("model category");
        let model_values: Vec<&str> = model_opt
            .get("options")
            .and_then(|v| v.as_array())
            .expect("model options array")
            .iter()
            .filter_map(|o| o.get("value").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(
            model_values,
            vec!["default", "opus"],
            "the parsed model ids ride the frame"
        );
    }

    // The FIX (async slash-command arrival push): claude advertises its command
    // list in the same late `initialize` response that carries the model/mode
    // catalog. The frontend's mount-time REST read returns empty before that
    // lands, and the legacy ACP path recovers via a live `AvailableCommands`
    // push — so the direct-CLI pump MUST emit one too, or the `/` menu stays
    // empty until a manual refetch. A CatalogUpdated carrying slash_commands
    // projects to an AvailableCommands frame whose commands carry name+description.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn catalog_updated_projects_available_commands_frame() {
        use aionui_session::SlashCommandInfo;
        let script = vec![env(SessionEvent::CatalogUpdated {
            models: Vec::new(),
            modes: Vec::new(),
            slash_commands: vec![
                SlashCommandInfo {
                    name: "compact".into(),
                    description: Some("Compact the conversation".into()),
                },
                SlashCommandInfo {
                    name: "clear".into(),
                    description: None,
                },
            ],
        })];
        let frames = drain_script(script).await;
        let commands = frames
            .iter()
            .find_map(|f| match f {
                AgentStreamEvent::AvailableCommands(data) => Some(&data.commands),
                _ => None,
            })
            .expect("CatalogUpdated with slash_commands must project to an AvailableCommands frame");
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["compact", "clear"], "both commands ride the frame");
        assert_eq!(
            commands[0].description, "Compact the conversation",
            "description carries through"
        );
        assert_eq!(commands[1].description, "", "missing description becomes empty string");
    }

    // A CatalogUpdated with NO slash commands must not emit a spurious empty
    // AvailableCommands frame (which would clobber a REST-loaded menu).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn catalog_updated_without_slash_commands_emits_no_available_commands() {
        use aionui_session::ModelInfo;
        let script = vec![env(SessionEvent::CatalogUpdated {
            models: vec![ModelInfo {
                id: "opus".into(),
                name: "Opus".into(),
                description: None,
                reasoning_efforts: Vec::new(),
            }],
            modes: Vec::new(),
            slash_commands: Vec::new(),
        })];
        let frames = drain_script(script).await;
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, AgentStreamEvent::AvailableCommands(_))),
            "empty slash_commands must not emit an AvailableCommands frame"
        );
    }

    // send_message emits Start (before dispatch) stamped with the learned session id,
    // and PromptAccepted does NOT double-emit a Start.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_message_emits_single_leading_start_with_session_id() {
        // Pre-seed the backend-bound id via a script event, then let the pump learn it.
        let backend: Arc<dyn SessionBackend> = Arc::new(ScriptBackend(vec![env(SessionEvent::BackendBound {
            backend_session_id: Some("sid-abc".into()),
        })]));
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
        );
        // Let the pump process the BackendBound so session_id is known.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut rx = crate::agent_task::IAgentTask::subscribe(task.as_ref());
        crate::agent_task::IAgentTask::send_message(
            task.as_ref(),
            SendMessageData {
                content: "hi".into(),
                msg_id: "m1".into(),
                turn_id: None,
                files: Vec::new(),
                inject_skills: Vec::new(),
            },
        )
        .await
        .unwrap();
        // The very next frame must be exactly one Start, carrying the session id.
        let first = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv())
            .await
            .expect("a frame")
            .expect("ok");
        let AgentStreamEvent::Start(data) = first else {
            panic!("expected Start first, got {}", frame_name(&first));
        };
        assert_eq!(data.session_id.as_deref(), Some("sid-abc"));
    }

    /// Read the single `.json` dump written under `dir`.
    fn read_only_json_dump(dir: &std::path::Path) -> serde_json::Value {
        let path = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().map(|x| x == "json").unwrap_or(false))
            .expect("a dump file must exist");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    // DEV (`--dump-prompts`): send_message dumps the final input blocks as a
    // `session-cli-final-input` JSON, with the vendor label and raw text.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_message_dumps_final_input_when_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let backend: Arc<dyn SessionBackend> = Arc::new(ScriptBackend(vec![]));
        let task = SessionAgentTask::build(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
            CatalogPreload::default(),
            Some(SessionPromptDump {
                dir: tmp.path().to_path_buf(),
                backend: "claude",
            }),
            None,
        );
        crate::agent_task::IAgentTask::send_message(
            task.as_ref(),
            SendMessageData {
                content: "hello".into(),
                msg_id: "m-1".into(),
                turn_id: None,
                files: Vec::new(),
                inject_skills: Vec::new(),
            },
        )
        .await
        .unwrap();

        let dump = read_only_json_dump(tmp.path());
        assert_eq!(dump["kind"], "session-cli-final-input");
        assert_eq!(dump["backend"], "claude");
        assert_eq!(dump["conversation_id"], "conv-1");
        assert_eq!(dump["msg_id"], "m-1");
        assert_eq!(dump["input"]["content"][0]["type"], "text");
        assert_eq!(dump["input"]["content"][0]["text"], "hello");
    }

    // Multimodal blocks are dumped RAW: an Image block keeps its full base64 body
    // (no redaction / no byte-len summary), per the dev-only "keep original" rule.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_message_dumps_image_block_raw_base64() {
        use base64::Engine as _;
        let tmp = tempfile::tempdir().unwrap();
        let backend: Arc<dyn SessionBackend> = Arc::new(ScriptBackend(vec![]));
        let task = SessionAgentTask::build(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
            CatalogPreload::default(),
            Some(SessionPromptDump {
                dir: tmp.path().to_path_buf(),
                backend: "codex",
            }),
            None,
        );
        // Inject an image directly onto the task's dump path via a content slice
        // containing an Image block.
        task.dump_session_cli_final_input(
            &[ContentBlock::Image {
                data: vec![1u8, 2, 3],
                media_type: "image/png".into(),
            }],
            Some("m-img"),
        );

        let dump = read_only_json_dump(tmp.path());
        assert_eq!(dump["backend"], "codex");
        assert_eq!(dump["input"]["content"][0]["type"], "image");
        assert_eq!(dump["input"]["content"][0]["media_type"], "image/png");
        assert_eq!(
            dump["input"]["content"][0]["data"],
            base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3])
        );
    }

    // With `--dump-prompts` off (`prompt_dump == None`), send_message writes nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_message_no_dump_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let backend: Arc<dyn SessionBackend> = Arc::new(ScriptBackend(vec![]));
        let task = SessionAgentTask::build(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
            CatalogPreload::default(),
            None,
            None,
        );
        crate::agent_task::IAgentTask::send_message(
            task.as_ref(),
            SendMessageData {
                content: "hi".into(),
                msg_id: "m-2".into(),
                turn_id: None,
                files: Vec::new(),
                inject_skills: Vec::new(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_dir(tmp.path()).unwrap().count(),
            0,
            "no dump when --dump-prompts is off"
        );
    }

    // claude's non-blocking Workflow turn emits MULTIPLE `result` frames: a LAUNCH
    // result while subagents still run, then a TERMINAL result after every
    // `task_notification{completed}`. The pump must suppress the launch result's
    // Finish (else the relay closes and the workflow's completion message is lost)
    // and forward exactly ONE Finish — the terminal one, after the workflow drains.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn workflow_launch_result_finish_is_suppressed_until_workflow_completes() {
        use aionui_session::SubagentStatus;
        let script = vec![
            env(SessionEvent::ToolCall {
                tool_use_id: "toolu_wf".into(),
                name: "Task".into(),
                subagent: aionui_session::SubagentKind::Workflow,
                input: serde_json::Value::Null,
                parent_tool_use_id: None,
            }),
            // Workflow starts running (in-flight; task_started declares the
            // container kind → admitted to the suppression roster).
            env(SessionEvent::SubagentUpdate {
                r#ref: "task-1".into(),
                label: Some("wf".into()),
                status: SubagentStatus::Running,
                parent_ref: Some("toolu_wf".into()),
                kind: Some(aionui_session::SubagentTaskKind::WorkflowContainer),
            }),
            env(SessionEvent::MessageDelta {
                item_id: "m".into(),
                text: "launching workflow".into(),
            }),
            // LAUNCH result — arrives while the workflow is still in flight. Its
            // Finish MUST be suppressed.
            env(SessionEvent::TurnResult {
                is_error: false,
                api_error_status: None,
                result_text: String::new(),
                epoch: 0,
                outcome: aionui_session::TurnOutcome::EndTurn,
            }),
            // Workflow completes (matches the fixture invariant: completed precedes
            // the terminal result; task_notification carries no task_type).
            env(SessionEvent::SubagentUpdate {
                r#ref: "task-1".into(),
                label: Some("wf".into()),
                status: SubagentStatus::Completed,
                parent_ref: Some("toolu_wf".into()),
                kind: None,
            }),
            env(SessionEvent::MessageDelta {
                item_id: "m2".into(),
                text: "workflow done".into(),
            }),
            // TERMINAL result — workflow drained, so this Finish is forwarded.
            env(SessionEvent::TurnResult {
                is_error: false,
                api_error_status: None,
                result_text: String::new(),
                epoch: 0,
                outcome: aionui_session::TurnOutcome::EndTurn,
            }),
        ];
        let frames = drain_script(script).await;
        let seq: Vec<&str> = frames.iter().map(frame_name).collect();
        // Exactly ONE finish, and BOTH text segments (launch reply + completion
        // message) reach the frontend before it.
        let finish_count = frames
            .iter()
            .filter(|f| matches!(f, AgentStreamEvent::Finish(_)))
            .count();
        assert_eq!(
            finish_count, 1,
            "only the terminal result's Finish is forwarded, got {seq:?}"
        );
        let text_count = frames.iter().filter(|f| matches!(f, AgentStreamEvent::Text(_))).count();
        assert_eq!(
            text_count, 2,
            "both the launch reply and the workflow completion text survive, got {seq:?}"
        );
        // The single Finish is LAST — the completion text precedes it.
        assert!(
            matches!(frames.last(), Some(AgentStreamEvent::Finish(_))),
            "the terminal Finish comes after the workflow completion message, got {seq:?}"
        );
        // The suppressed launch result emits exactly one SegmentBreak so the relay
        // closes the launch text segment and the completion reply renders as a
        // separate bubble instead of being concatenated under one msg_id.
        let break_count = frames
            .iter()
            .filter(|f| matches!(f, AgentStreamEvent::SegmentBreak))
            .count();
        assert_eq!(
            break_count, 1,
            "the suppressed launch result emits one SegmentBreak, got {seq:?}"
        );
        // The SegmentBreak sits BETWEEN the launch text and the completion text.
        // (frame_name maps Text -> "content".)
        let first_text = seq.iter().position(|k| *k == "content").unwrap();
        let seg_break = seq.iter().position(|k| *k == "SegmentBreak").unwrap();
        let last_text = seq.iter().rposition(|k| *k == "content").unwrap();
        assert!(
            first_text < seg_break && seg_break < last_text,
            "SegmentBreak must separate the two text batches, got {seq:?}"
        );
    }

    // A workflow-launch result that is itself an ERROR is NOT suppressed — the user
    // must see a genuine failure even mid-workflow (suppression covers only clean
    // completion ordering, per the fixture invariant).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn errored_result_is_not_suppressed_even_with_inflight_workflow() {
        use aionui_session::SubagentStatus;
        let script = vec![
            env(SessionEvent::SubagentUpdate {
                r#ref: "task-1".into(),
                label: Some("wf".into()),
                status: SubagentStatus::Running,
                parent_ref: None,
                kind: Some(aionui_session::SubagentTaskKind::WorkflowContainer),
            }),
            env(SessionEvent::TurnResult {
                is_error: true,
                api_error_status: None,
                result_text: "provider exploded".into(),
                epoch: 0,
                outcome: aionui_session::TurnOutcome::EndTurn,
            }),
        ];
        let frames = drain_script(script).await;
        assert!(
            frames.iter().any(|f| matches!(f, AgentStreamEvent::Error(_))),
            "an error result terminates the turn even while a workflow is in flight, got {:?}",
            frames.iter().map(frame_name).collect::<Vec<_>>()
        );
    }

    // A user interrupt KILLS an in-flight workflow and the CLI emits NO result
    // frame afterwards — only task frames (verified: samples/claude-cli/2.1.220/
    // _all_workflow_interrupt.jsonl, scenario A: task_updated{killed} +
    // task_notification{stopped} per task, then silence). The suppressed launch
    // Finish is the turn's ONLY possible terminal, so the pump must emit it when
    // the roster drains via `Interrupted` — else the relay never breaks,
    // `cancelling` never clears, and the 15s UserCancelTimeout watchdog
    // force-kills a healthy session (ELECTRON-3RP/3RW).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupted_workflow_drain_settles_suppressed_finish() {
        use aionui_session::{SubagentStatus, SubagentTaskKind};
        // `kind` mirrors the wire: only `task_started` declares task_type
        // (workflow container vs bash child); `task_updated`/`task_notification`
        // frames carry None.
        let wf_update = |status: SubagentStatus, kind: Option<SubagentTaskKind>| {
            env(SessionEvent::SubagentUpdate {
                r#ref: "task-wf".into(),
                label: Some("wf".into()),
                status,
                parent_ref: Some("toolu_wf".into()),
                kind,
            })
        };
        let bash_update = |status: SubagentStatus, kind: Option<SubagentTaskKind>| {
            env(SessionEvent::SubagentUpdate {
                r#ref: "task-bash".into(),
                label: Some("local_bash".into()),
                status,
                parent_ref: Some("task-wf".into()),
                kind,
            })
        };
        let script = vec![
            env(SessionEvent::ToolCall {
                tool_use_id: "toolu_wf".into(),
                name: "Task".into(),
                subagent: aionui_session::SubagentKind::Workflow,
                input: serde_json::Value::Null,
                parent_tool_use_id: None,
            }),
            wf_update(SubagentStatus::Running, Some(SubagentTaskKind::WorkflowContainer)),
            env(SessionEvent::MessageDelta {
                item_id: "m".into(),
                text: "launching workflow".into(),
            }),
            // LAUNCH result — suppressed (workflow in flight): the turn now OWES
            // this Finish.
            env(SessionEvent::TurnResult {
                is_error: false,
                api_error_status: None,
                result_text: String::new(),
                epoch: 0,
                outcome: aionui_session::TurnOutcome::EndTurn,
            }),
            // The workflow's child bash is `local_bash` — visible in the display
            // roster but NOT admitted to the suppression roster.
            bash_update(SubagentStatus::Running, Some(SubagentTaskKind::Other)),
            // Interrupt kill sequence, frame-faithful to the 2.1.220 fixture:
            // task_updated{killed} maps to Running with NO task_type (kind None —
            // cannot re-insert), each followed by its task_notification{stopped}
            // → Interrupted. No result frame ever follows.
            wf_update(SubagentStatus::Running, None),
            wf_update(SubagentStatus::Interrupted, None),
            bash_update(SubagentStatus::Running, None),
            bash_update(SubagentStatus::Interrupted, None),
        ];
        let frames = drain_script(script).await;
        let seq: Vec<&str> = frames.iter().map(frame_name).collect();
        let finish_count = frames
            .iter()
            .filter(|f| matches!(f, AgentStreamEvent::Finish(_)))
            .count();
        assert_eq!(
            finish_count, 1,
            "the Interrupted drain settles the owed Finish exactly once, got {seq:?}"
        );
        assert!(
            matches!(frames.last(), Some(AgentStreamEvent::Finish(_))),
            "the settled Finish is the turn's terminal frame, got {seq:?}"
        );
        // The Task tool call left open by the kill is closed as Canceled BEFORE the
        // Finish, so the persisted row leaves "work" and the frontend spinner stops.
        let cancel_idx = frames
            .iter()
            .position(|f| matches!(f, AgentStreamEvent::ToolCall(d) if d.status == ToolCallStatus::Canceled && d.call_id == "toolu_wf"))
            .unwrap_or_else(|| panic!("open Task call must be closed as Canceled, got {seq:?}"));
        let finish_idx = frames
            .iter()
            .position(|f| matches!(f, AgentStreamEvent::Finish(_)))
            .unwrap();
        assert!(
            cancel_idx < finish_idx,
            "the Canceled tool close must precede the Finish (relay breaks at Finish), got {seq:?}"
        );
    }

    // A background bash (`local_bash`, e.g. Bash{run_in_background}) is NOT a
    // workflow container: it outlives the turn with no later terminal result, so
    // it must NEVER hold the turn open. Regression (live 2026-07-30): a bash-only
    // roster suppressed the launch Finish → turn wedged until the 15s watchdog,
    // and the user's Stop clicks could not drain it (interrupt emits no task
    // frames for a plain background bash).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_bash_does_not_suppress_turn_finish() {
        use aionui_session::{SubagentStatus, SubagentTaskKind};
        let script = vec![
            env(SessionEvent::ToolCall {
                tool_use_id: "toolu_bash".into(),
                name: "Bash".into(),
                subagent: aionui_session::SubagentKind::Inline,
                input: serde_json::Value::Null,
                parent_tool_use_id: None,
            }),
            env(SessionEvent::ToolResult {
                tool_use_id: "toolu_bash".into(),
                is_error: false,
                content: Vec::new(),
                parent_tool_use_id: None,
            }),
            // task_started for the background bash: kind = Other.
            env(SessionEvent::SubagentUpdate {
                r#ref: "task-bash".into(),
                label: Some("bash".into()),
                status: SubagentStatus::Running,
                parent_ref: Some("toolu_bash".into()),
                kind: Some(SubagentTaskKind::Other),
            }),
            env(SessionEvent::MessageDelta {
                item_id: "m".into(),
                text: "已启动，60 秒后完成".into(),
            }),
            // The turn's clean result: with only a bash in the roster this is the
            // turn's REAL terminal and its Finish must flow immediately — the
            // bash keeps running in the CLI, unrelated to turn lifecycle.
            env(SessionEvent::TurnResult {
                is_error: false,
                api_error_status: None,
                result_text: String::new(),
                epoch: 0,
                outcome: aionui_session::TurnOutcome::EndTurn,
            }),
        ];
        let frames = drain_script(script).await;
        let seq: Vec<&str> = frames.iter().map(frame_name).collect();
        assert!(
            matches!(frames.last(), Some(AgentStreamEvent::Finish(_))),
            "the clean result's Finish must flow while a background bash is alive, got {seq:?}"
        );
        assert!(
            !frames.iter().any(|f| matches!(f, AgentStreamEvent::SegmentBreak)),
            "no suppression SegmentBreak for a bash-only roster, got {seq:?}"
        );
    }

    // Interrupt-vs-natural-completion race: if a real terminal result trails the
    // synthetic settlement, its Finish must be swallowed (ONE Finish per turn —
    // the relay already broke) and no spurious empty-turn Tip may fire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trailing_real_finish_after_synthetic_settlement_is_swallowed() {
        use aionui_session::SubagentStatus;
        let script = vec![
            env(SessionEvent::SubagentUpdate {
                r#ref: "task-wf".into(),
                label: Some("wf".into()),
                status: SubagentStatus::Running,
                parent_ref: None,
                kind: Some(aionui_session::SubagentTaskKind::WorkflowContainer),
            }),
            env(SessionEvent::MessageDelta {
                item_id: "m".into(),
                text: "launching workflow".into(),
            }),
            // Launch result — suppressed.
            env(SessionEvent::TurnResult {
                is_error: false,
                api_error_status: None,
                result_text: String::new(),
                epoch: 0,
                outcome: aionui_session::TurnOutcome::EndTurn,
            }),
            // Kill drain → synthetic settlement (task_notification: no task_type).
            env(SessionEvent::SubagentUpdate {
                r#ref: "task-wf".into(),
                label: Some("wf".into()),
                status: SubagentStatus::Interrupted,
                parent_ref: None,
                kind: None,
            }),
            // Trailing real terminal (the race) — must NOT produce a second Finish.
            env(SessionEvent::TurnResult {
                is_error: false,
                api_error_status: None,
                result_text: String::new(),
                epoch: 0,
                outcome: aionui_session::TurnOutcome::EndTurn,
            }),
        ];
        let frames = drain_script(script).await;
        let seq: Vec<&str> = frames.iter().map(frame_name).collect();
        let finish_count = frames
            .iter()
            .filter(|f| matches!(f, AgentStreamEvent::Finish(_)))
            .count();
        assert_eq!(finish_count, 1, "trailing real Finish is swallowed, got {seq:?}");
        assert!(
            !frames.iter().any(|f| matches!(f, AgentStreamEvent::Tips(_))),
            "no spurious empty-turn Tip after the synthetic settlement, got {seq:?}"
        );
    }

    // THE FIX (live: dev 2026-07-31, conv ee61bd05 stuck "processing"): the
    // swallow guard armed by a cancel-drain settlement must disarm when the
    // envelope `turn_gen` advances. claude_conn NEVER emits `TurnStarted` (only
    // the codex/acp adapters synthesize it), so a reset keyed on that arm alone
    // is dead code on the claude stream — the guard leaked into the follow-up
    // turn and ate its real Finish, so the relay never broke and the turn stayed
    // open forever. The script mirrors the live log: gen 1 = workflow launch
    // (suppressed result) + kill drain (synthetic Finish); gen 2 = follow-up
    // message answered with claude's usual double terminal result.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn swallow_guard_disarms_when_turn_gen_advances() {
        use aionui_session::SubagentStatus;
        let script = vec![
            env_gen(
                1,
                SessionEvent::SubagentUpdate {
                    r#ref: "task-wf".into(),
                    label: Some("wf".into()),
                    status: SubagentStatus::Running,
                    parent_ref: None,
                    kind: Some(aionui_session::SubagentTaskKind::WorkflowContainer),
                },
            ),
            env_gen(
                1,
                SessionEvent::MessageDelta {
                    item_id: "m1".into(),
                    text: "launching workflow".into(),
                },
            ),
            // Launch result — suppressed while the workflow is in flight.
            env_gen(
                1,
                SessionEvent::TurnResult {
                    is_error: false,
                    api_error_status: None,
                    result_text: String::new(),
                    epoch: 0,
                    outcome: aionui_session::TurnOutcome::EndTurn,
                },
            ),
            // User cancel → kill drain → synthetic settlement Finish (arms the guard).
            env_gen(
                1,
                SessionEvent::SubagentUpdate {
                    r#ref: "task-wf".into(),
                    label: Some("wf".into()),
                    status: SubagentStatus::Interrupted,
                    parent_ref: None,
                    kind: None,
                },
            ),
            // ── Follow-up message: the reader bumps turn_gen on the accepted Send. ──
            env_gen(
                2,
                SessionEvent::PromptAccepted {
                    client_msg_id: "u-2".into(),
                },
            ),
            env_gen(
                2,
                SessionEvent::MessageDelta {
                    item_id: "m2".into(),
                    text: "follow-up answer".into(),
                },
            ),
            // claude's double terminal per turn (known-benign duplicate).
            env_gen(
                2,
                SessionEvent::TurnResult {
                    is_error: false,
                    api_error_status: None,
                    result_text: String::new(),
                    epoch: 0,
                    outcome: aionui_session::TurnOutcome::EndTurn,
                },
            ),
            env_gen(
                2,
                SessionEvent::TurnResult {
                    is_error: false,
                    api_error_status: None,
                    result_text: String::new(),
                    epoch: 0,
                    outcome: aionui_session::TurnOutcome::EndTurn,
                },
            ),
        ];
        let frames = drain_script(script).await;
        let seq: Vec<&str> = frames.iter().map(frame_name).collect();
        let last_content = seq
            .iter()
            .rposition(|f| *f == "content")
            .expect("follow-up turn text present");
        let last_finish = seq
            .iter()
            .rposition(|f| *f == "finish")
            .unwrap_or_else(|| panic!("no Finish at all — guard ate the follow-up turn's terminal, got {seq:?}"));
        assert!(
            last_finish > last_content,
            "the follow-up turn's real Finish must flow after its content (guard must disarm on gen advance), got {seq:?}"
        );
        let finish_count = frames
            .iter()
            .filter(|f| matches!(f, AgentStreamEvent::Finish(_)))
            .count();
        assert!(
            finish_count >= 2,
            "expected the settlement Finish AND the follow-up turn's Finish, got {seq:?}"
        );
    }

    // Cross-gen guard for the empty-turn Tip. The interrupt-vs-completion race can
    // leave a trailing real `TurnResult{is_error:false}` on the OLD gen AFTER the
    // synthetic settlement — with `saw_visible_output` already reset to false, that
    // terminal arms `pending_empty_turn_tip`, and the swallow guard `continue`s past
    // its Finish without draining it. This asserts that a Some(tip) so armed on gen 1
    // can never surface on the NEXT (real, output-bearing) turn's Finish as a spurious
    // ACP_EMPTY_TURN. The guarantee is structural: `pending_empty_turn_tip` is a
    // per-iteration binding (dropped at the end of each envelope), so it cannot cross
    // the gen boundary — this test pins that invariant against future refactors that
    // might hoist the binding out of the loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_turn_tip_armed_on_trailing_result_does_not_leak_to_next_turn() {
        use aionui_session::SubagentStatus;
        let script = vec![
            env_gen(
                1,
                SessionEvent::SubagentUpdate {
                    r#ref: "task-wf".into(),
                    label: Some("wf".into()),
                    status: SubagentStatus::Running,
                    parent_ref: None,
                    kind: Some(aionui_session::SubagentTaskKind::WorkflowContainer),
                },
            ),
            env_gen(
                1,
                SessionEvent::MessageDelta {
                    item_id: "m1".into(),
                    text: "launching workflow".into(),
                },
            ),
            // Launch result — suppressed while the workflow is in flight.
            env_gen(
                1,
                SessionEvent::TurnResult {
                    is_error: false,
                    api_error_status: None,
                    result_text: String::new(),
                    epoch: 0,
                    outcome: aionui_session::TurnOutcome::EndTurn,
                },
            ),
            // User cancel → kill drain → synthetic settlement Finish. Arms the swallow
            // guard AND resets `saw_visible_output` to false.
            env_gen(
                1,
                SessionEvent::SubagentUpdate {
                    r#ref: "task-wf".into(),
                    label: Some("wf".into()),
                    status: SubagentStatus::Interrupted,
                    parent_ref: None,
                    kind: None,
                },
            ),
            // Trailing real terminal (the race): with `saw_visible_output` false this
            // ARMS `pending_empty_turn_tip`, but the guard swallows its Finish — so the
            // tip is never drained on THIS turn. It must be dropped, not carried over.
            env_gen(
                1,
                SessionEvent::TurnResult {
                    is_error: false,
                    api_error_status: None,
                    result_text: String::new(),
                    epoch: 0,
                    outcome: aionui_session::TurnOutcome::EndTurn,
                },
            ),
            // ── Follow-up turn on the NEXT gen: it DOES produce visible output, so it
            // must never receive an empty-turn Tip. ──
            env_gen(
                2,
                SessionEvent::PromptAccepted {
                    client_msg_id: "u-2".into(),
                },
            ),
            env_gen(
                2,
                SessionEvent::MessageDelta {
                    item_id: "m2".into(),
                    text: "follow-up answer".into(),
                },
            ),
            env_gen(
                2,
                SessionEvent::TurnResult {
                    is_error: false,
                    api_error_status: None,
                    result_text: String::new(),
                    epoch: 0,
                    outcome: aionui_session::TurnOutcome::EndTurn,
                },
            ),
        ];
        let frames = drain_script(script).await;
        let seq: Vec<&str> = frames.iter().map(frame_name).collect();
        assert!(
            !frames.iter().any(|f| matches!(f, AgentStreamEvent::Tips(_))),
            "an empty-turn Tip armed on the trailing gen-1 result must not leak onto \
             the output-bearing gen-2 turn's Finish, got {seq:?}"
        );
        // Sanity: the follow-up turn's real Finish still flows after its content, so
        // the assertion above is testing a turn that actually reached a terminal.
        let last_content = seq
            .iter()
            .rposition(|f| *f == "content")
            .expect("follow-up turn text present");
        let last_finish = seq
            .iter()
            .rposition(|f| *f == "finish")
            .expect("follow-up turn Finish present");
        assert!(
            last_finish > last_content,
            "follow-up turn's Finish must flow after its content, got {seq:?}"
        );
    }

    /// Backend that reports one scripted pending permission — models a permission
    /// raised before the client subscribed, which the REST /confirmations recovery
    /// path must be able to rebuild.
    struct PendingPermBackend(aionui_session::PendingPermissionView);

    #[async_trait::async_trait]
    impl SessionBackend for PendingPermBackend {
        async fn dispatch(&self, _c: Command) -> Result<CommandReceipt, BackendError> {
            Ok(CommandReceipt {
                accepted: true,
                admission: Admission::NoTurn,
                turn_gen: 0,
            })
        }
        fn events(&self) -> BoxStream<'static, SessionEnvelope> {
            use futures_util::StreamExt as _;
            futures_util::stream::empty().boxed()
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        fn pending_permission_requests(&self) -> Vec<aionui_session::PendingPermissionView> {
            vec![self.0.clone()]
        }
    }

    // get_confirmations must recover a pending AskUserQuestion as a question card
    // (its options), not an empty/allow-deny card — else a page refresh loses the
    // question and the turn hangs. (Regression guard: get_confirmations used to
    // return an empty Vec unconditionally.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_confirmations_recovers_pending_ask_user_question() {
        let backend: Arc<dyn SessionBackend> = Arc::new(PendingPermBackend(aionui_session::PendingPermissionView {
            request_id: "req-recover".into(),
            tool_name: "AskUserQuestion".into(),
            questions: Some(serde_json::json!({
                "questions": [{
                    "question": "Which?",
                    "options": [{"label": "A"}, {"label": "B"}]
                }]
            })),
        }));
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
        );
        let confs = task.get_confirmations();
        assert_eq!(confs.len(), 1, "the pending permission must be recovered");
        assert_eq!(
            confs[0].call_id, "req-recover",
            "card id == request_id for live/recovered de-dup"
        );
        let labels: Vec<&str> = confs[0].options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["A", "B"],
            "recovered as the question's options, not allow/deny"
        );
    }

    // A pending ordinary tool permission recovers as the generic allow/deny card.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_confirmations_recovers_generic_permission() {
        let backend: Arc<dyn SessionBackend> = Arc::new(PendingPermBackend(aionui_session::PendingPermissionView {
            request_id: "req-tool".into(),
            tool_name: "Bash".into(),
            questions: None,
        }));
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
        );
        let confs = task.get_confirmations();
        assert_eq!(confs.len(), 1);
        let vals: Vec<String> = confs[0]
            .options
            .iter()
            .filter_map(|o| o.value.as_str().map(str::to_owned))
            .collect();
        assert_eq!(vals, vec!["allow", "allow_always", "reject"]);
    }

    /// Backend whose capabilities advertise modes+models but whose current_* never
    /// changes — models the claude constraint that an in-band switch is NOT reflected
    /// in capabilities(). Proves set_config_option's optimistic override makes the
    /// response satisfy the frontend's Observed contract regardless.
    pub(super) struct StaticCapsBackend;

    #[async_trait::async_trait]
    impl SessionBackend for StaticCapsBackend {
        async fn dispatch(&self, _c: Command) -> Result<CommandReceipt, BackendError> {
            Ok(CommandReceipt {
                accepted: true,
                admission: Admission::NoTurn,
                turn_gen: 0,
            })
        }
        fn events(&self) -> BoxStream<'static, SessionEnvelope> {
            use futures_util::StreamExt as _;
            futures_util::stream::empty().boxed()
        }
        fn capabilities(&self) -> Capabilities {
            use aionui_session::{ModeInfo, ModelInfo};
            Capabilities {
                available_modes: vec![
                    ModeInfo {
                        id: "default".into(),
                        name: "Default".into(),
                        description: None,
                    },
                    ModeInfo {
                        id: "plan".into(),
                        name: "Plan".into(),
                        description: None,
                    },
                ],
                current_mode: Some("default".into()),
                available_models: vec![
                    ModelInfo {
                        id: "opus".into(),
                        name: "Opus".into(),
                        description: None,
                        reasoning_efforts: vec![],
                    },
                    ModelInfo {
                        id: "sonnet".into(),
                        name: "Sonnet".into(),
                        description: None,
                        reasoning_efforts: vec![],
                    },
                ],
                current_model: Some("opus".into()),
                ..Default::default()
            }
        }
    }

    // set_config_option("mode") must return Observed with the requested value even
    // though capabilities().current_mode never moves — the optimistic override drives
    // the observed re-read. (Regression guard for the "switching mode → command_ack"
    // error the origin frontend rejects.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_config_option_mode_returns_observed_via_override() {
        let backend: Arc<dyn SessionBackend> = Arc::new(StaticCapsBackend);
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
        );
        let resp = task.set_config_option("mode", "plan").await.unwrap();
        assert!(
            matches!(resp.confirmation, aionui_api_types::ConfigOptionConfirmation::Observed),
            "mode switch must be Observed, got {:?}",
            resp.confirmation
        );
        let opts = resp.config_options.expect("config_options present");
        let mode_opt = opts.iter().find(|o| o.id == "mode").expect("mode option");
        assert_eq!(
            mode_opt.current_value.as_deref(),
            Some("plan"),
            "current_value reflects the switch"
        );
    }

    // Same for model — critical because claude gives set_model NO confirmation wire,
    // so ONLY the override can make it read back as observed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_config_option_model_returns_observed_via_override() {
        let backend: Arc<dyn SessionBackend> = Arc::new(StaticCapsBackend);
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
        );
        let resp = task.set_config_option("model", "sonnet").await.unwrap();
        assert!(
            matches!(resp.confirmation, aionui_api_types::ConfigOptionConfirmation::Observed),
            "model switch must be Observed, got {:?}",
            resp.confirmation
        );
        // And get_model reflects the override too (picker highlight follows).
        let m = task.get_model().await.unwrap().model_info.expect("model_info");
        assert_eq!(m.current_model_id.as_deref(), Some("sonnet"));
    }

    // #3: a runtime switch to a value NOT in the advertised catalog is REJECTED
    // (bad_request), not silently dropped and not dispatched — the user's chosen
    // reject-and-report behavior. Non-empty catalog that omits the value → reject.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_config_option_rejects_invalid_mode_and_model() {
        let backend: Arc<dyn SessionBackend> = Arc::new(StaticCapsBackend);
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
        );

        let mode_err = task
            .set_config_option("mode", "no-such-mode")
            .await
            .expect_err("a mode outside the catalog must be rejected");
        assert!(
            matches!(mode_err, AgentError::BadRequest(_)),
            "invalid mode → BadRequest, got {mode_err:?}"
        );

        let model_err = task
            .set_config_option("model", "no-such-model")
            .await
            .expect_err("a model outside the catalog must be rejected");
        assert!(
            matches!(model_err, AgentError::BadRequest(_)),
            "invalid model → BadRequest, got {model_err:?}"
        );

        // The optimistic overrides must NOT have moved (nothing was dispatched).
        assert!(
            task.runtime.mode_override().is_none() && task.runtime.model_override().is_none(),
            "a rejected switch must not set an optimistic override"
        );
    }

    // codex ToolOutputDelta (streamed command stdout) must surface as tool_call
    // frames carrying the CUMULATIVE output (the frontend REPLACES output on merge,
    // so sending raw deltas would show only the last chunk). Each frame keys on the
    // item_id so the frontend appends to the right tool.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tool_output_delta_accumulates_cumulative_output() {
        let script = vec![
            env(SessionEvent::ToolOutputDelta {
                item_id: "call_0".into(),
                text: "line-1\n".into(),
            }),
            env(SessionEvent::ToolOutputDelta {
                item_id: "call_0".into(),
                text: "line-2\n".into(),
            }),
        ];
        let frames = drain_script(script).await;
        let outputs: Vec<String> = frames
            .iter()
            .filter_map(|f| match f {
                AgentStreamEvent::ToolCall(d) if d.call_id == "call_0" => d.output.clone(),
                _ => None,
            })
            .collect();
        // Two frames: first the 1st chunk, then the cumulative 1st+2nd (not just "line-2").
        assert_eq!(outputs, vec!["line-1\n".to_string(), "line-1\nline-2\n".to_string()]);
    }

    // ── Defect 1: process-reap on task drop ───────────────────────────────
    // Faithfully models `ClaudeSessionBackend`: `events()` subscribes to a
    // broadcast `Sender` the backend struct OWNS, so the event stream stays open
    // (pending, never Closed) exactly as long as the backend is alive — and reaping
    // the child CLI happens in the backend's `Drop`. If `spawn_event_pump` captured a
    // backend `Arc`, that Arc would keep `event_tx` alive after the task Arc is
    // dropped, so the stream would never Close, the pump loop would never exit, the
    // backend would never Drop, and the child CLI would leak. This test proves the
    // pump holds ONLY the stream: dropping the sole task Arc must fire the backend's
    // Drop (i.e. reap) promptly.
    struct ReapBackend {
        // Owning this sender keeps `events()` subscribers pending while the backend
        // lives, mirroring the real backend's `event_tx` field.
        event_tx: broadcast::Sender<SessionEnvelope>,
        // Fired from `Drop` — stands in for "the child process was reaped".
        reap_signal: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    #[async_trait::async_trait]
    impl SessionBackend for ReapBackend {
        async fn dispatch(&self, _c: Command) -> Result<CommandReceipt, BackendError> {
            Ok(CommandReceipt {
                accepted: true,
                admission: Admission::NoTurn,
                turn_gen: 0,
            })
        }
        fn events(&self) -> BoxStream<'static, SessionEnvelope> {
            use futures_util::StreamExt as _;
            // Subscribe here (like ClaudeSessionBackend::events): the returned stream
            // captures ONLY the Receiver, never `self`. It yields nothing and only
            // ends when every Sender — i.e. the field below — is dropped.
            let rx = self.event_tx.subscribe();
            futures_util::stream::unfold(rx, |mut rx| async move {
                match rx.recv().await {
                    Ok(env) => Some((env, rx)),
                    Err(_) => None,
                }
            })
            .boxed()
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
    }

    impl Drop for ReapBackend {
        fn drop(&mut self) {
            if let Some(tx) = self.reap_signal.lock().unwrap().take() {
                let _ = tx.send(());
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_task_reaps_backend() {
        let (reaped_tx, reaped_rx) = tokio::sync::oneshot::channel();
        let (event_tx, _keep) = broadcast::channel(8);
        let backend: Arc<dyn SessionBackend> = Arc::new(ReapBackend {
            event_tx,
            reap_signal: std::sync::Mutex::new(Some(reaped_tx)),
        });
        // `_keep` is dropped here, so the ONLY remaining Sender is the backend's field
        // — the reap now hinges purely on the backend being dropped.
        drop(_keep);
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
        );
        // Let the pump subscribe and settle into its await.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Drop the sole strong task Arc. Post-fix, this drops `task.backend` (the only
        // long-lived backend Arc) → ReapBackend::drop fires. Pre-fix, the pump held a
        // backend Arc and this would hang.
        drop(task);

        tokio::time::timeout(std::time::Duration::from_secs(2), reaped_rx)
            .await
            .expect("backend must be dropped (reaped) promptly after the task Arc is dropped")
            .expect("reap signal delivered");
    }

    // Build a persisted-handshake value in the SHAPE `spawn_catalog_writeback` stores
    // (the top-level `{available_models:[{id,label}], current_model_id}` column shape).
    fn handshake_with_catalog() -> aionui_api_types::AgentHandshake {
        aionui_api_types::AgentHandshake {
            available_models: Some(serde_json::json!({
                "available_models": [
                    {"id": "opus", "label": "Opus"},
                    {"id": "sonnet", "label": "Sonnet"},
                ],
                "current_model_id": "sonnet",
            })),
            available_modes: Some(serde_json::json!({
                "available_modes": [
                    {"id": "default", "name": "Default"},
                    {"id": "plan", "name": "Plan"},
                ],
                "current_mode_id": "plan",
            })),
            ..Default::default()
        }
    }

    // Cold-start resume: the backend's live capabilities() is still empty (initialize
    // round-trip not landed), but the persisted-handshake preload populates the picker
    // so it is NOT blank. This is the fix for "session-port history-open shows an empty
    // model list for ~seconds while ACP's persisted preload keeps it filled".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preload_serves_catalog_when_live_capabilities_empty() {
        // ScriptBackend has empty capabilities() → live catalog is absent.
        let backend: Arc<dyn SessionBackend> = Arc::new(ScriptBackend(Vec::new()));
        let task = SessionAgentTask::new_with_preload(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
            &handshake_with_catalog(),
            None,
            None,
        );

        // get_model serves the preloaded catalog + persisted current model.
        let m = task
            .get_model()
            .await
            .unwrap()
            .model_info
            .expect("model_info from preload");
        assert_eq!(
            m.available_models.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["opus", "sonnet"],
        );
        assert_eq!(m.current_model_id.as_deref(), Some("sonnet"));

        // get_config_options renders both mode+model selects from the preload.
        let opts = task.get_config_options().await.unwrap().config_options;
        let model_opt = opts
            .iter()
            .find(|o| o.id == "model")
            .expect("model select from preload");
        assert_eq!(
            model_opt.options.iter().map(|o| o.value.as_str()).collect::<Vec<_>>(),
            vec!["opus", "sonnet"],
        );
        assert_eq!(model_opt.current_value.as_deref(), Some("sonnet"));
        let mode_opt = opts.iter().find(|o| o.id == "mode").expect("mode select from preload");
        assert_eq!(mode_opt.current_value.as_deref(), Some("plan"));

        // mode() serves the preloaded current mode.
        assert_eq!(task.mode().await.unwrap().mode, "plan");
    }

    // The live catalog OVERWRITES the preload the instant it is present: even though a
    // (stale) preload is supplied, a backend with non-empty capabilities() serves the
    // live values — matching ACP's "fill-when-empty, live-overwrites" semantics and
    // preventing a stale persisted catalog from masking a fresh engine's model list.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_capabilities_overwrite_stale_preload() {
        use super::pump_tests::StaticCapsBackend;
        // Preload advertises a codex-shaped catalog; live StaticCapsBackend advertises
        // opus/sonnet with current=opus. Live must win on every axis.
        let stale = aionui_api_types::AgentHandshake {
            available_models: Some(serde_json::json!({
                "available_models": [{"id": "stale-model", "label": "Stale"}],
                "current_model_id": "stale-model",
            })),
            ..Default::default()
        };
        let backend: Arc<dyn SessionBackend> = Arc::new(StaticCapsBackend);
        let task = SessionAgentTask::new_with_preload(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
            &stale,
            None,
            None,
        );
        let m = task.get_model().await.unwrap().model_info.expect("model_info");
        assert_eq!(
            m.available_models.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["opus", "sonnet"],
            "live capabilities must overwrite the stale preload"
        );
        assert_eq!(m.current_model_id.as_deref(), Some("opus"));
    }

    // Cold-start pre-init: the backend has already seeded `current_model`/`current_mode`
    // from the RESOLVED snapshot (claude_conn spawn: `caps.current_model = config.model`,
    // config.model = persisted `current_model_id`), but `available_models` is still empty
    // (the initialize round-trip has not landed). The persisted-handshake preload's
    // current is STALE (frozen at the prior session's write-back, which does not re-run on
    // a mid-turn switch). The picker must show the backend's snapshot-seeded current — the
    // model claude actually runs — NOT the stale preload current, even though the LIST is
    // served from preload in this same window. This is the per-axis-independent fallback.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn caps_current_wins_over_stale_preload_while_list_is_empty() {
        // Backend: current seeded (user last switched to opus), but lists still empty.
        struct CurrentOnlyBackend;
        #[async_trait::async_trait]
        impl SessionBackend for CurrentOnlyBackend {
            async fn dispatch(&self, _c: Command) -> Result<CommandReceipt, BackendError> {
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: 0,
                })
            }
            fn events(&self) -> BoxStream<'static, SessionEnvelope> {
                use futures_util::StreamExt as _;
                futures_util::stream::empty().boxed()
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities {
                    current_model: Some("opus".into()),
                    current_mode: Some("default".into()),
                    ..Default::default()
                }
            }
        }

        // Preload (prior write-back) still says sonnet/plan — the pre-switch values.
        let backend: Arc<dyn SessionBackend> = Arc::new(CurrentOnlyBackend);
        let task = SessionAgentTask::new_with_preload(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
            &handshake_with_catalog(),
            None,
            None,
        );

        let m = task.get_model().await.unwrap().model_info.expect("model_info");
        // LIST comes from the preload (backend list is empty pre-init)...
        assert_eq!(
            m.available_models.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["opus", "sonnet"],
            "list falls back to preload while the backend list is empty"
        );
        // ...but the CURRENT is the backend's snapshot-seeded model, NOT the stale preload.
        assert_eq!(
            m.current_model_id.as_deref(),
            Some("opus"),
            "current_model must be the backend's snapshot-seeded value, not the stale preload's sonnet"
        );

        let opts = task.get_config_options().await.unwrap().config_options;
        let model_opt = opts.iter().find(|o| o.id == "model").expect("model select");
        assert_eq!(model_opt.current_value.as_deref(), Some("opus"));
        let mode_opt = opts.iter().find(|o| o.id == "mode").expect("mode select");
        assert_eq!(
            mode_opt.current_value.as_deref(),
            Some("default"),
            "current_mode must be the backend's snapshot-seeded value, not the stale preload's plan"
        );
        assert_eq!(task.mode().await.unwrap().mode, "default");
    }
}

/// Layer D force-kill regression (spec §10.1/§10.2/§10.3/§10.7, plan §5 T4–T6):
/// the direct-CLI `SessionAgentTask` kill path. Before this fix `kill` was a
/// Drop-only no-op that silently failed while an orchestrator held an `Arc`
/// clone of the task (ELECTRON-3RW). These tests pin the fixed behavior: a
/// `UserCancelTimeout` kill emits a clean `Finish` AND really terminates the
/// backend, even with an external `Arc` held; and it is idempotent + isolated
/// from the non-force-kill reasons.
#[cfg(test)]
mod force_kill_tests {
    use super::*;
    use crate::agent_task::{AgentInstance, IAgentTask};
    use crate::types::SendMessageData;
    use aionui_common::{AgentKillReason, ConversationStatus};
    use aionui_session::{
        Admission, BackendError, Capabilities, Command, CommandReceipt, SessionBackend, SessionEnvelope,
    };
    use futures_util::stream::BoxStream;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A backend whose `events()` NEVER terminates (the turn stays in flight —
    /// no natural `Finish`, exactly the workflow-in-progress window), and whose
    /// `terminate()` bumps a shared counter so the force-kill delegation is
    /// observable without a real process.
    struct TerminateCountingBackend {
        terminate_calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl SessionBackend for TerminateCountingBackend {
        async fn dispatch(&self, c: Command) -> Result<CommandReceipt, BackendError> {
            let admission = match c {
                Command::Send { .. } => Admission::Started,
                _ => Admission::NoTurn,
            };
            Ok(CommandReceipt {
                accepted: true,
                admission,
                turn_gen: 1,
            })
        }
        fn events(&self) -> BoxStream<'static, SessionEnvelope> {
            use futures_util::StreamExt as _;
            // Never yields → the turn never converges on its own; only kill can.
            futures_util::stream::pending().boxed()
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        async fn terminate(&self) {
            self.terminate_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn build_task_with_counter() -> (Arc<SessionAgentTask>, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        let backend: Arc<dyn SessionBackend> = Arc::new(TerminateCountingBackend {
            terminate_calls: Arc::clone(&counter),
        });
        let task = SessionAgentTask::new(
            AgentType::Acp,
            "conv-1".into(),
            "user-1".into(),
            "/w".into(),
            backend,
            None,
        );
        (task, counter)
    }

    /// Drive a turn to `Running` (emits `Start` on the runtime channel).
    async fn start_turn(task: &SessionAgentTask) {
        IAgentTask::send_message(
            task,
            SendMessageData {
                content: "hello".into(),
                msg_id: "m1".into(),
                turn_id: None,
                files: Vec::new(),
                inject_skills: Vec::new(),
            },
        )
        .await
        .expect("send accepted");
    }

    /// Return the next TERMINAL frame (`Finish`/`Error`), skipping lifecycle
    /// frames like `Start`. `None` if none arrives within the bounded window.
    async fn next_terminal(rx: &mut broadcast::Receiver<AgentStreamEvent>) -> Option<AgentStreamEvent> {
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                Ok(Ok(ev)) => match ev {
                    AgentStreamEvent::Finish(_) | AgentStreamEvent::Error(_) => return Some(ev),
                    _ => continue,
                },
                _ => return None,
            }
        }
    }

    /// T4: `UserCancelTimeout` on the Session path forces a clean `Finish` AND a
    /// real backend `terminate`, even while an external `Arc<SessionAgentTask>`
    /// (the orchestrator) is still held — the case the old Drop-only kill missed.
    #[tokio::test]
    async fn user_cancel_timeout_forces_clean_finish_and_terminate_with_external_arc() {
        let (task, counter) = build_task_with_counter();
        let mut rx = IAgentTask::subscribe(task.as_ref());
        start_turn(task.as_ref()).await;
        assert_eq!(
            IAgentTask::status(task.as_ref()),
            Some(ConversationStatus::Running),
            "turn should be in flight before kill"
        );

        // An orchestrator legitimately holds a clone of the Arc for the whole turn.
        let orchestrator_hold = Arc::clone(&task);

        let inst = AgentInstance::Session(Arc::clone(&task));
        inst.kill_and_wait(Some(AgentKillReason::UserCancelTimeout)).await;

        // (a) clean Finish broadcast — NOT a crash Error.
        let terminal = next_terminal(&mut rx).await.expect("a terminal frame after kill");
        assert!(
            matches!(terminal, AgentStreamEvent::Finish(_)),
            "kill must broadcast a clean Finish (not Error), got {terminal:?}"
        );
        // (b) runtime converged to Finished.
        assert_eq!(IAgentTask::status(task.as_ref()), Some(ConversationStatus::Finished));
        // (c) backend really terminated once — independent of the still-held Arc.
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "backend.terminate must be awaited exactly once"
        );
        assert!(
            Arc::strong_count(&task) >= 2,
            "orchestrator still holds the Arc — Drop did NOT fire, yet terminate ran"
        );

        drop(orchestrator_hold);
        drop(inst);
    }

    /// T5: idempotence — a repeated force-kill (or a late real Finish) does not
    /// re-broadcast; status stays `Finished`.
    #[tokio::test]
    async fn repeated_user_cancel_kill_does_not_double_broadcast() {
        let (task, _counter) = build_task_with_counter();
        let mut rx = IAgentTask::subscribe(task.as_ref());
        start_turn(task.as_ref()).await;

        let inst = AgentInstance::Session(Arc::clone(&task));
        inst.kill_and_wait(Some(AgentKillReason::UserCancelTimeout)).await;
        let first = next_terminal(&mut rx).await.expect("first Finish");
        assert!(matches!(first, AgentStreamEvent::Finish(_)));
        assert_eq!(IAgentTask::status(task.as_ref()), Some(ConversationStatus::Finished));

        // Second force-kill → emit_finish_once is a no-op in the Finished state.
        inst.kill_and_wait(Some(AgentKillReason::UserCancelTimeout)).await;
        let again = next_terminal(&mut rx).await;
        assert!(again.is_none(), "no second Finish broadcast, got {again:?}");
        assert_eq!(IAgentTask::status(task.as_ref()), Some(ConversationStatus::Finished));
    }

    /// T6: isolation — non-`UserCancelTimeout` reasons keep the original
    /// Drop-driven no-op: no injected Finish, no forced terminate, status
    /// unchanged.
    #[tokio::test]
    async fn non_user_cancel_reason_keeps_drop_driven_noop() {
        let (task, counter) = build_task_with_counter();
        let mut rx = IAgentTask::subscribe(task.as_ref());
        start_turn(task.as_ref()).await;

        let inst = AgentInstance::Session(Arc::clone(&task));
        inst.kill_and_wait(Some(AgentKillReason::IdleTimeout)).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "idle kill must NOT force backend terminate"
        );
        // The explicit `None` kill forwarder is likewise a Drop-driven no-op.
        assert!(IAgentTask::kill(task.as_ref(), None).is_ok());
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "explicit None kill must NOT force backend terminate"
        );

        // Neither non-force-kill broadcast a terminal frame; status stays Running.
        let terminal = next_terminal(&mut rx).await;
        assert!(
            terminal.is_none(),
            "non-UserCancel kill must not broadcast Finish/Error, got {terminal:?}"
        );
        assert_eq!(IAgentTask::status(task.as_ref()), Some(ConversationStatus::Running));
    }
}

#[cfg(test)]
mod catalog_writeback_tests {
    //! When a backend discovers models LATE. agy runs `agy models` off the open
    //! path (~3s cold, slower on a bad network), so the write-back must keep
    //! watching well past the point where modes are already known.
    use super::*;
    use aionui_session::{Admission, Capabilities, CommandReceipt, ModeInfo, ModelInfo};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `catalog_partial_from_caps` nests the list under its own key alongside
    /// `current_*`, so reach through that wrapper rather than the outer value.
    fn nested_len(field: Option<&serde_json::Value>, key: &str) -> usize {
        field
            .and_then(|v| v.get(key))
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    }

    /// Reports modes immediately and models only after `polls_before_models`
    /// calls to `capabilities()` — the write-back polls every 50ms, so the
    /// count sets how late discovery lands.
    struct LateModelsBackend {
        polls: AtomicUsize,
        polls_before_models: usize,
    }

    #[async_trait::async_trait]
    impl SessionBackend for LateModelsBackend {
        async fn dispatch(&self, _c: Command) -> Result<CommandReceipt, BackendError> {
            Ok(CommandReceipt {
                accepted: true,
                admission: Admission::NoTurn,
                turn_gen: 0,
            })
        }
        fn events(&self) -> BoxStream<'static, SessionEnvelope> {
            use futures_util::StreamExt as _;
            futures_util::stream::empty().boxed()
        }
        fn capabilities(&self) -> Capabilities {
            let n = self.polls.fetch_add(1, Ordering::SeqCst);
            let available_models = if n >= self.polls_before_models {
                vec![ModelInfo {
                    id: "gemini-3.6-flash-high".into(),
                    name: "gemini-3.6-flash-high".into(),
                    description: None,
                    reasoning_efforts: Vec::new(),
                }]
            } else {
                Vec::new()
            };
            Capabilities {
                available_modes: vec![ModeInfo {
                    id: "default".into(),
                    name: "Default".into(),
                    description: None,
                }],
                available_models,
                ..Default::default()
            }
        }
    }

    /// Drain the channel until a message carrying models arrives, or the
    /// write-back gives up. Returns every message it saw.
    async fn collect_until_models(
        rx: &mut tokio::sync::mpsc::Receiver<crate::registry::CatalogSyncMessage>,
    ) -> Vec<crate::registry::CatalogSyncMessage> {
        let mut seen = Vec::new();
        while let Some(msg) = rx.recv().await {
            let has_models = nested_len(msg.handshake.available_models.as_ref(), "available_models") > 0;
            seen.push(msg);
            if has_models {
                break;
            }
        }
        seen
    }

    #[tokio::test(start_paused = true)]
    async fn models_discovered_after_the_interim_publish_still_reach_the_catalog() {
        // 200 polls ≈ 10s: past the interim publish, well inside the model
        // window. Before this fix the write-back stopped at 5s and the model
        // picker stayed empty for the whole session with no error anywhere.
        let backend = Arc::new(LateModelsBackend {
            polls: AtomicUsize::new(0),
            polls_before_models: 200,
        });
        let (tx, mut rx) = crate::registry::catalog_channel_for_test(16);
        spawn_catalog_writeback("agent-1".into(), "user-1".into(), backend, tx);

        let seen = collect_until_models(&mut rx).await;
        let last = seen.last().expect("write-back published nothing at all");
        assert_eq!(
            nested_len(last.handshake.available_models.as_ref(), "available_models"),
            1,
            "late-discovered models never reached the catalog; saw {} message(s)",
            seen.len()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_backend_that_never_reports_models_still_publishes_its_modes() {
        // claude/codex fill capabilities from the handshake and may legitimately
        // have no model list. Waiting for the longer model window must not stop
        // their modes from landing.
        let backend = Arc::new(LateModelsBackend {
            polls: AtomicUsize::new(0),
            polls_before_models: usize::MAX,
        });
        let (tx, mut rx) = crate::registry::catalog_channel_for_test(16);
        spawn_catalog_writeback("agent-2".into(), "user-2".into(), backend, tx);

        let msg = rx.recv().await.expect("modes were never published");
        assert_eq!(msg.agent_metadata_id, "agent-2");
        assert!(
            nested_len(msg.handshake.available_modes.as_ref(), "available_modes") > 0,
            "expected the modes-only partial to be published"
        );
    }
}
