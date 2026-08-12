//! 007 §C1 (ACP variant): `AcpConnection` / `AcpSessionBackend` — the third
//! `SessionBackend`, over the **Agent Client Protocol** (`session/*` JSON-RPC)
//! spoken by ACP CLIs / bridges (hermes, codex-acp, claude-acp, opencode, …).
//!
//! Like codex (`codex_conn`), this talks RAW JSON-RPC over the `AgentIo` byte
//! duplex — it does NOT use the `agent-client-protocol` SDK (that crate lives in
//! the upper `aionui-ai-agent` Domain crate; `aionui-session` must stay
//! transport-agnostic and SDK-free, so a backend here parses `serde_json::Value`
//! and matches on field strings). The reader-task / dispatch / capabilities
//! contract is identical in shape to codex; the wire dialect differs.
//!
//! THE ONE STRUCTURAL DIFFERENCE vs codex (and why this file exists): an ACP
//! turn's TERMINAL is the `session/prompt` REQUEST's RESPONSE (`{stopReason}`),
//! out-of-band of the `session/update` notification stream — there is NO
//! `turn/completed` notification (verified against real hermes/codex-acp wire).
//! So `dispatch(Send)` does NOT await the prompt response inline; it records
//! `rpc_id → client_msg_id` in `pending_prompts` and the reader claims that
//! response, synthesizes the `TurnResult` from its `stopReason`, and (because the
//! reader is the single ordered consumer of stdout) every `session/update` delta
//! has already been folded before the terminal — no cross-task race. This is the
//! codex GAP-A pending-sends pattern, reused for the terminal rather than the ack.
//!
//! Freeze-blocker parity with codex:
//!  - A1 (anti-panic): `session/update` is matched on the `sessionUpdate` STRING,
//!    never deserialized into a closed SDK enum. Unknown variant → `AdapterSpecific`.
//!  - A2/A3 (reverse-RPC deadlock): the reader writes a JSON-RPC response for
//!    every server-initiated request. `session/request_permission` surfaces as
//!    `Permission` (answered by `dispatch(AnswerPermission)`); all other reverse
//!    methods (`fs/*`, `terminal/*`, …) get an immediate `-32601` so the channel
//!    never hangs (matches the SDK's own auto-reject of unhandled reverse RPC).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aionui_process::Spawner;
use futures_util::stream::{BoxStream, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use super::suspend::{ProcHandle, SuspendController, spawn_idle_timer};
use super::types::{
    Admission, BackendError, CancelTarget, Command, CommandReceipt, ContentBlock, PermissionDecision, SessionEnvelope,
    SessionSpec,
};
use super::{BackendConnection, SessionBackend, SessionConfig};
use crate::adapter::AgentIo;
use crate::capability::{
    BlockSet, Capabilities, CapabilityTier, CommandSet, ModeInfo, ModelInfo, PromptAcceptedSource, SignalSet,
};
use crate::event::{PermissionKind, SessionEvent, StopReason, SubagentKind, TruncationKind, TurnOutcome};

/// Connection-level factory for ACP. Holds the injected `Spawner` (S14 — never
/// raw-spawn) and the command that launches the ACP CLI/bridge. One process per
/// logical session (P1; ACP `session/new` could multiplex, a later refinement).
pub struct AcpConnection {
    spawner: Arc<dyn Spawner>,
    /// The command that starts the ACP agent (e.g. `hermes acp`, the codex-acp
    /// bridge binary). Connection-static; per-session `cwd`/`extra_args` come
    /// from `SessionConfig` at `open_session`.
    command: aionui_common::CommandSpec,
}

impl AcpConnection {
    pub fn new(spawner: Arc<dyn Spawner>, command: aionui_common::CommandSpec) -> Self {
        Self { spawner, command }
    }
}

#[async_trait::async_trait]
impl BackendConnection for AcpConnection {
    async fn open_session(
        &self,
        spec: SessionSpec,
        config: SessionConfig,
    ) -> Result<Arc<dyn SessionBackend>, BackendError> {
        let logical_id = match &spec {
            SessionSpec::Fresh { session_id } => session_id.clone(),
            SessionSpec::Resume { session_id, .. } => session_id.clone(),
            // Clean-slate ACP backend is not factory-wired yet; the live ACP
            // fork path is AcpAgentManager's `session/fork`. Explicit reject —
            // never silently open fresh under a fork spec.
            SessionSpec::Fork { .. } => {
                return Err(BackendError::Transport(
                    "acp_conn does not support SessionSpec::Fork (use the ACP manager fork path)".into(),
                ));
            }
        };

        // Live spawn via the injected Spawner (records into the unified registry
        // so the supervisor reaps the ACP child + its bun/node tree on a
        // crash-restart — feature 006, automatic here because we go through
        // Spawner/ManagedProcess rather than the legacy CliAgentProcess path).
        let mut cmd = self.command.clone();
        cmd.args.extend(config.extra_args.iter().cloned());
        if let Some(cwd) = &config.cwd {
            cmd.cwd = Some(cwd.clone());
        }
        let proc = self
            .spawner
            .spawn(cmd, &[], "aionui-session")
            .await
            .map_err(|e| BackendError::from_spawn("acp spawn failed", e))?;
        let io: Box<dyn AgentIo> = Box::new(crate::adapter::ManagedProcessIo::new(proc));
        // F-4 wake recipe: a Dormant→dispatch wake re-spawns the ACP CLI and
        // replays the resume handshake (`session/load` against the bound sid).
        // idle_ttl=None (default) → never suspends → identical to pre-F-4.
        let wake = AcpWakeRecipe {
            spawner: Some(self.spawner.clone()),
            command: Some(self.command.clone()),
            config: config.clone(),
        };
        let mut backend = AcpSessionBackend::spawn_with_wake(logical_id, io, wake, config.idle_ttl_ms).await;

        // Seed the immutable capability snapshot's current model/mode from config
        // (parity with codex/claude). SetModel/SetMode updates flow via
        // ConfigChanged, not by mutating this open-time snapshot (§5.5).
        backend.capabilities.current_model = config.model.clone();
        backend.capabilities.current_mode = config.mode.clone();
        *backend.current_model.lock().await = config.model.clone();

        // JSON-RPC handshake (shared with wake_handle so the wire shape is in one
        // place). Resume pre-seeds the acp_session_id binding so the first prompt
        // has the sid without waiting on the load response.
        let resume_sid = match &spec {
            SessionSpec::Fresh { .. } => None,
            // lost backend session → fresh under the same logical id (§4.1).
            SessionSpec::Resume { backend_session_id, .. } => backend_session_id.clone(),
            // Unreachable: the logical-id match above already rejected Fork.
            SessionSpec::Fork { .. } => unreachable!("acp_conn rejects SessionSpec::Fork at open"),
        };
        backend.run_handshake(resume_sid.as_deref()).await?;

        let backend = Arc::new(backend);
        // G6: the handshake above is fire-and-forget; the agent's OBSERVED mode/model
        // lands asynchronously in the reader. Spawn a one-shot task that waits for it
        // and re-aligns the agent to the DESIRED config (decision A — backend self-heals
        // on open). Skip entirely when nothing is desired (no spurious set_* / no task).
        if backend.wake.config.mode.is_some() || backend.wake.config.model.is_some() {
            let reconcile = Arc::clone(&backend);
            tokio::spawn(async move { reconcile.reconcile_startup_config().await });
        }
        Ok(backend as Arc<dyn SessionBackend>)
    }

    async fn close_session(&self, _session_id: &str) -> Result<(), BackendError> {
        // No connection-level per-session registry to release: `open_session`
        // returns a self-owned `AcpSessionBackend` held by the conversation layer.
        // Graceful close happens when the conversation drops its handle —
        // `AcpSessionBackend::drop` aborts the reader and reaps the ACP subprocess
        // (kill_on_drop). A mid-turn cancel is a separate, turn-scoped concern
        // (`session/cancel` via `dispatch(Cancel)`), not a session teardown.
        // Idempotent.
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        acp_capabilities()
    }
}

/// `initialize` params (ACP). `protocolVersion` + client-side capabilities; we
/// advertise the reverse-RPC we actually handle (`session/request_permission`).
fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false } }
    })
}

/// `session/new` params: workspace cwd + the resolved MCP servers (Wave 0c).
fn new_session_params(config: &SessionConfig) -> Value {
    json!({
        "cwd": config.cwd.clone().unwrap_or_else(|| ".".into()),
        "mcpServers": build_mcp_servers(&config.init.mcp_servers),
    })
}

/// `session/load` params: the ACP session id to re-attach + cwd + MCP servers.
///
/// RESUME RE-INJECTS MCP (Wave 0c): a resumed ACP session re-runs `session/load`,
/// so the servers must be supplied here too — otherwise a resumed conversation
/// silently loses every MCP tool (the pre-0c `mcpServers: []` regression).
fn load_session_params(sid: &str, config: &SessionConfig) -> Value {
    json!({
        "sessionId": sid,
        "cwd": config.cwd.clone().unwrap_or_else(|| ".".into()),
        "mcpServers": build_mcp_servers(&config.init.mcp_servers),
    })
}

/// Serialize neutral [`McpServerSpec`]s into the ACP `session/new`|`session/load`
/// `mcpServers[]` JSON — BYTE-IDENTICAL to what the ACP SDK `McpServer` produces
/// (so the agent sees the same wire the legacy path sent): a Stdio entry is
/// untagged `{name, command, args, env:[{name,value}]}`; Http/Sse carry a `type`
/// discriminator + `{name, url, headers:[{name,value}]}`. Pure `serde_json`, no
/// ACP SDK (acp_conn stays SDK-free, §module header). env/headers are emitted in
/// the spec's order — the app boundary already sorts them (parity with the legacy
/// `session_server_to_sdk_mcp_server` deterministic sort) so hermetic assertions
/// are stable.
fn build_mcp_servers(servers: &[crate::backend::McpServerSpec]) -> Vec<Value> {
    use crate::backend::McpTransport;
    let kv = |pairs: &[(String, String)]| -> Vec<Value> {
        pairs.iter().map(|(k, v)| json!({ "name": k, "value": v })).collect()
    };
    servers
        .iter()
        .map(|s| match &s.transport {
            McpTransport::Stdio { command, args, env } => json!({
                "name": s.name,
                "command": command,
                "args": args,
                "env": kv(env),
            }),
            McpTransport::Http { url, headers } => json!({
                "type": "http",
                "name": s.name,
                "url": url,
                "headers": kv(headers),
            }),
            McpTransport::Sse { url, headers } => json!({
                "type": "sse",
                "name": s.name,
                "url": url,
                "headers": kv(headers),
            }),
        })
        .collect()
}

/// ACP's declared capabilities (§C5.5 parity). tier=Parsed (ACP is fully
/// structured JSON-RPC). ACP supports per-turn permission, mode/model switching,
/// and (when advertised) auth; it has NO native steer / tool-scoped cancel /
/// rewind / checkpoint-list on the base wire, so those are advertised false and
/// `dispatch` rejects them with `CommandNotSupported` (the cap-behavior invariant).
pub fn acp_capabilities() -> Capabilities {
    Capabilities {
        tier: CapabilityTier::Parsed,
        emits: SignalSet {
            // ACP has no liveness heartbeat notification; the turn terminal is the
            // prompt response (no idle-timeout in AionCore anyway, post-007).
            heartbeat: false,
            tool_lifecycle: true,
            terminal_result: true,
        },
        supported_commands: CommandSet {
            // ACP base wire has no turn/steer-equivalent.
            steer: false,
            // No tool-scoped cancel (only whole-session `session/cancel`).
            cancel_tool: false,
            answer_permission: true,
            // No standard mid-session auth reverse-RPC in the base ACP turn loop
            // we drive (auth is a connection-level concern); advertise false.
            answer_auth: false,
            acknowledge: true,
            set_mode: true,
            set_model: true,
            rewind: false,
            list_checkpoints: false,
            query_session_info: false,
        },
        prompt_blocks: BlockSet {
            // ACP baseline mandates Text + ResourceLink; image/audio are optional
            // (PromptCapabilities). We advertise the safe baseline + image.
            text: true,
            image: true,
            audio: false,
            resource: true,
            at_mention: false,
        },
        // ACP's `session/prompt` response is BOTH the accept ack and the terminal
        // (one return). We synthesize PromptAccepted optimistically when the prompt
        // is written to the wire, so the conversation's pending queue drains
        // immediately rather than only at turn end.
        prompt_accepted: PromptAcceptedSource::Synthesized,
        available_models: Vec::new(),
        available_modes: Vec::new(),
        current_model: None,
        current_mode: None,
        current_effort: None,
        auth_methods: Vec::new(),
        // 009 R2: ACP is one session/prompt at a time — no proactive next-turn
        // input path from the conv layer. can_queue degrades to false (= can_send).
        accepts_proactive_input: false,
        // #101: static default empty; filled from the `available_commands_update`
        // session/update (capabilities() merges the discovered set on read).
        slash_commands: Vec::new(),
    }
}

/// Per-session ACP handle. `&self`-concurrent (stdin write behind a Mutex).
pub struct AcpSessionBackend {
    session_id: String,
    /// Base capability snapshot: the static `acp_capabilities()` + open-time
    /// current_model/current_mode (seeded mutably in `open_session` before the
    /// backend is Arc'd, parity with codex/claude). Immutable after open; the
    /// reader-discovered available models/modes are merged in `capabilities()`.
    capabilities: Capabilities,
    /// Reader-filled available models/modes (from the `session/new`|`load`
    /// response). Behind a sync Mutex so the sync `capabilities()` merges them
    /// without awaiting (the static base cannot carry per-session discovery).
    discovered: Arc<std::sync::Mutex<Discovered>>,
    rpc_id: AtomicU64,
    /// Live turn epoch (bumped on dispatch(Send), read by the reader to stamp).
    turn_gen: Arc<AtomicU64>,
    stdin: Arc<Mutex<Option<aionui_process::BoxedStdin>>>,
    event_tx: broadcast::Sender<SessionEnvelope>,
    /// F-4 self-suspend controller owning the live `{reader, io}` pair. ACP CLIs/
    /// bridges are persistent (stdout never EOFs mid-session), so the reader is
    /// aborted on suspend AND on Drop (`abort_on_drop`, M5) to reap the child. When
    /// idle_ttl=None (default) the slot stays Active for life — pre-F-4 parity.
    suspend: Arc<SuspendController>,
    /// Per-backend idle timer (Some only when idle_ttl is set). Aborted on Drop.
    idle_timer: Option<tokio::task::JoinHandle<()>>,
    /// What a Dormant→dispatch wake needs to re-spawn the ACP CLI + replay the
    /// resume handshake (`session/load` against the bound acp_session_id).
    wake: AcpWakeRecipe,
    /// Shared reader inputs, cloned into the open-time reader AND every post-wake
    /// reader so they drain into the same broadcast/atomics/bindings.
    reader_state: AcpReaderState,
    /// F-4 turn-active flag (shared with the reader via `reader_state`): set on
    /// dispatch(Send), cleared by the reader at the terminal. The idle timer reads
    /// it so a streaming turn is never suspended mid-flight.
    turn_in_flight: Arc<std::sync::atomic::AtomicBool>,
    /// Logical session_id ← ACP backend session id binding. Filled by the reader
    /// when it claims the `session/new` response (which carries `sessionId`) or the
    /// `session/load` response (which does NOT — the sid is taken from
    /// `pending_resume_sid`). NOT pre-seeded on Resume: the ACP spec requires the
    /// client to wait for the full `session/load` response before prompting (the
    /// agent replays history via `session/update` first, then responds), so binding
    /// only on the response makes `bound_session()` gate the first prompt correctly.
    /// Never escapes upward except via BackendBound (the resume anchor). Two-id (§4.1).
    acp_session_id: Arc<Mutex<Option<String>>>,
    /// The resume sid an in-flight `session/load` is re-attaching to. Set in
    /// `run_handshake`'s Resume branch (instead of pre-seeding `acp_session_id`) and
    /// consumed by the reader when the load RESPONSE arrives: `session/load` returns
    /// no `sessionId`, so the reader binds `acp_session_id` to THIS value on success.
    pending_resume_sid: Arc<Mutex<Option<String>>>,
    /// The current model id (for SetMode/SetModel tracking + ConfigChanged).
    current_model: Arc<Mutex<Option<String>>>,
    /// rpc_id of the in-flight `session/new`|`session/load` request, so the reader
    /// claims its response → binds the ACP session id + fills discovery.
    pending_open: Arc<Mutex<Option<u64>>>,
    /// rpc_id of the in-flight `initialize` request, so the reader claims its
    /// RESPONSE and parses `authMethods[]` into `Discovered.auth_methods` (the auth
    /// capability the agent advertises at connect — hermes carries it, claude does
    /// not). Symmetric with `pending_open`.
    pending_init: Arc<Mutex<Option<u64>>>,
    /// rpc_id → client_msg_id for in-flight `session/prompt` requests. The reader
    /// claims the response, reads its `stopReason`, and synthesizes the terminal
    /// `TurnResult` (THE ACP-specific terminal path — see the module header).
    pending_prompts: Arc<Mutex<HashMap<u64, PendingPrompt>>>,
    /// rpc_id → `"mode→<v>"` / `"model→<v>"` label for in-flight `session/set_mode` /
    /// `session/set_model` requests. The reader claims the response: a JSON-RPC ERROR
    /// (e.g. opencode `-32602 model not found`) is surfaced as a `Notice{Warning}` +
    /// error log instead of being silently dropped; a SUCCESS (incl opencode's empty
    /// `{}`) drives `ConfigChanged` with the labelled value ITSELF (the response is the
    /// authoritative "applied" signal) and advances the discovered current_mode/model —
    /// it does NOT wait for an echo notification, because opencode 1.16.2 sends none
    /// (the "set doesn't stick" prod bug). claude-acp's later `config_option_update`
    /// echo carries the same value (reducer idempotent) — the two paths never conflict.
    pending_set: Arc<Mutex<HashMap<u64, String>>>,
    /// request_id → the options the agent OFFERED on a `session/request_permission`
    /// (each `(optionId, kind)`, kind ∈ allow_once/allow_always/reject_once/
    /// reject_always). The response MUST echo one of THOSE real optionIds — we used
    /// to hardcode "allow_once"/"cancelled", which bridges reject (→ tool silently
    /// denied) or mis-route (deny→client-abort). dispatch(AnswerPermission) looks up
    /// this set and picks the optionId matching the decision's kind.
    pending_perm_options: PendingPermOptions,
    /// Wave 0c-F: the composed first-message preamble (`[Assistant Rules]` block
    /// from `SessionConfig.init.preset_context`). ACP has no system-prompt wire
    /// field, so the preset is delivered by prepending this to the FIRST
    /// `session/prompt`. `take()`-drained on the first `dispatch(Send)` so it is
    /// applied exactly once; `None` = no preset (the pre-0c first prompt unchanged).
    pending_preamble: Mutex<Option<String>>,
}

/// Per-request_id offered permission options: `(optionId, kind)` pairs the agent
/// sent on `session/request_permission`, so the answer echoes a REAL optionId by kind.
type PendingPermOptions = Arc<Mutex<HashMap<String, Vec<(String, String)>>>>;

/// What a pending `session/prompt` carries so the reader can synthesize the
/// terminal when its response lands.
#[derive(Clone)]
struct PendingPrompt {
    /// The turn epoch this prompt opened (stamped on the synthesized TurnResult).
    turn_gen: u64,
}

/// Reader-discovered models/modes (from the `session/new`|`load` response).
/// `capabilities()` merges these into the returned snapshot.
#[derive(Default, Clone)]
struct Discovered {
    models: Vec<ModelInfo>,
    modes: Vec<ModeInfo>,
    current_model: Option<String>,
    current_mode: Option<String>,
    /// Auth method ids advertised in the `initialize` RESPONSE (`authMethods[].id`).
    /// hermes returns e.g. `[bedrock, hermes-setup]`; claude ACP returns none.
    /// Non-empty ⇒ `capabilities().auth_methods` + `answer_auth` cap flip true.
    auth_methods: Vec<String>,
    /// #101: slash commands from the `available_commands_update` session/update
    /// (`update.availableCommands[{name, description, input?}]` — wire-pinned from
    /// hermes + claude-acp captures). `capabilities()` merges them on read.
    slash_commands: Vec<crate::capability::SlashCommandInfo>,
    /// G4: the ids of GENERIC config options the agent advertised in the
    /// `session/new|load` response `configOptions[]` (and `config_option_update`
    /// notifications) — e.g. claude-acp `effort` (category `thought_level`). `mode`/
    /// `model` are EXCLUDED (they have dedicated set_mode/set_model arms). A
    /// non-empty set is what gates `Command::SetConfigOption` (advertised ⟺ settable),
    /// keeping the cap-behavior invariant honest. Ids only — the rich catalog
    /// (values/labels) surfaces to the UI via the existing modes/models path; this
    /// is purely the dispatch allowlist.
    config_options: Vec<String>,
}

impl AcpSessionBackend {
    /// Test-support seam: build over an injected `AgentIo` replaying an ACP
    /// JSON-RPC fixture WITHOUT spawning a real CLI — proves the
    /// parse/reverse-RPC/dispatch contract end-to-end.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn build_with_io(session_id: impl Into<String>, io: Box<dyn AgentIo>) -> Self {
        Self::spawn(session_id.into(), io).await
    }

    /// Test-support seam: build a SUSPENDABLE backend with a caller-supplied
    /// `Spawner` + ACP `command` (to observe the wake re-spawn) and an `idle_ttl_ms`.
    /// Lets a test drive the suspend→wake path: the idle slot suspends, and the
    /// next dispatch wakes via the supplied spawner (asserting the resume re-spawn).
    #[cfg(any(test, feature = "test-support"))]
    pub async fn build_with_io_suspending(
        session_id: impl Into<String>,
        io: Box<dyn AgentIo>,
        spawner: Arc<dyn Spawner>,
        command: aionui_common::CommandSpec,
        idle_ttl_ms: i64,
    ) -> Self {
        let wake = AcpWakeRecipe {
            spawner: Some(spawner),
            command: Some(command),
            config: SessionConfig::default(),
        };
        Self::spawn_with_wake(session_id.into(), io, wake, Some(idle_ttl_ms)).await
    }

    /// Test-support seam: pre-bind the ACP session id (what the `session/new`
    /// response does on the live path). `open_session` runs the handshake;
    /// `build_with_io` skips it, so a hermetic `dispatch(Send)` / terminal test
    /// uses this to satisfy `bound_session()` without a live process.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn bind_for_test(&self, acp_session_id: impl Into<String>) {
        *self.acp_session_id.lock().await = Some(acp_session_id.into());
    }

    /// Test-support seam: register a pending `session/new`|`session/load` rpc id
    /// so a hermetic fixture can replay that response and exercise the reader's
    /// open-response path (bind ACP sid → `BackendBound{Some}` + discovery fill).
    /// On the live path `open_session` sets this; `build_with_io` skips it.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_pending_open_for_test(&self, rpc_id: u64) {
        *self.pending_open.lock().await = Some(rpc_id);
    }

    /// Test-support seam: register a pending `initialize` rpc id so a hermetic
    /// fixture can replay that response and exercise the reader's auth-discovery
    /// path (parse `authMethods[]` → `Discovered.auth_methods` → capabilities()).
    /// On the live path `run_handshake` sets this; `build_with_io` skips it.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_pending_init_for_test(&self, rpc_id: u64) {
        *self.pending_init.lock().await = Some(rpc_id);
    }

    /// Test-support seam: stash the in-flight resume sid, marking the reader as
    /// inside a `session/load` replay window (what `run_handshake`'s Resume branch
    /// sets). Lets a hermetic fixture prove the reader SUPPRESSES the historical
    /// `session/update` replay from the UI event stream while a resume load is
    /// in flight. `build_with_io` skips the handshake, so a test seeds it here.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_pending_resume_sid_for_test(&self, sid: impl Into<String>) {
        *self.pending_resume_sid.lock().await = Some(sid.into());
    }

    /// Test-support seam: register a pending `session/set_model`|`session/set_mode`
    /// rpc id + label so a hermetic fixture can replay an error response and assert
    /// the reader surfaces a `Notice` (not a silent drop). On the live path
    /// `dispatch(SetModel/SetMode)` registers it.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_pending_set_for_test(&self, rpc_id: u64, label: impl Into<String>) {
        self.pending_set.lock().await.insert(rpc_id, label.into());
    }

    /// Test-support seam: set the Wave 0c-F first-message preamble (the production
    /// path composes it from `SessionConfig.init.preset_context` in
    /// `spawn_with_wake`; `build_with_io` uses an inert config, so a hermetic test
    /// seeds it here to exercise the first-prompt prepend).
    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_pending_preamble_for_test(&self, preamble: impl Into<String>) {
        *self.pending_preamble.lock().await = Some(preamble.into());
    }

    #[cfg(any(test, feature = "test-support"))]
    async fn spawn(session_id: String, io: Box<dyn AgentIo>) -> Self {
        Self::spawn_with_wake(session_id, io, AcpWakeRecipe::inert(), None).await
    }

    /// G6 test seam: build a backend whose wake recipe carries DESIRED mode/model
    /// (what the conversation wanted), so `reconcile_startup_config` has something
    /// to align the agent's OBSERVED values to. No spawner (never suspends).
    #[cfg(any(test, feature = "test-support"))]
    pub async fn build_with_io_and_desired(
        session_id: impl Into<String>,
        io: Box<dyn AgentIo>,
        desired_mode: Option<String>,
        desired_model: Option<String>,
    ) -> Self {
        let wake = AcpWakeRecipe {
            spawner: None,
            command: None,
            config: SessionConfig {
                mode: desired_mode,
                model: desired_model,
                ..SessionConfig::default()
            },
        };
        Self::spawn_with_wake(session_id.into(), io, wake, None).await
    }

    /// G6 test seam: seed the reader-discovered (OBSERVED) current mode/model, as if
    /// the `session/new|load` response had already been parsed, so a test can drive
    /// `reconcile_startup_config` without a live handshake round-trip.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn seed_observed_for_test(&self, mode: Option<String>, model: Option<String>) {
        let mut disc = self.discovered.lock().unwrap_or_else(|e| e.into_inner());
        disc.current_mode = mode;
        disc.current_model = model;
    }

    /// Spawn + (optionally) enable F-4 idle self-suspend. `wake` carries what a
    /// Dormant→dispatch wake needs (the ACP command + spawner + config);
    /// idle_ttl=None = never suspend (the `spawn` default).
    async fn spawn_with_wake(
        session_id: String,
        io: Box<dyn AgentIo>,
        wake: AcpWakeRecipe,
        idle_ttl_ms: Option<i64>,
    ) -> Self {
        let io: Arc<dyn AgentIo> = Arc::from(io);
        let turn_gen = Arc::new(AtomicU64::new(0));
        let acp_session_id = Arc::new(Mutex::new(None));
        let pending_resume_sid = Arc::new(Mutex::new(None));
        let current_model = Arc::new(Mutex::new(None));
        let pending_open = Arc::new(Mutex::new(None));
        let pending_init = Arc::new(Mutex::new(None));
        let pending_prompts = Arc::new(Mutex::new(HashMap::new()));
        let pending_set = Arc::new(Mutex::new(HashMap::new()));
        let pending_perm_options = Arc::new(Mutex::new(HashMap::new()));
        let discovered = Arc::new(std::sync::Mutex::new(Discovered::default()));
        let turn_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (event_tx, _) = broadcast::channel(1024);

        let (stdin, stdout) = match io.take_stdio().await {
            Some((stdin, stdout)) => (Some(stdin), Some(stdout)),
            None => (None, None),
        };
        let stdin = Arc::new(Mutex::new(stdin));

        let reader_state = AcpReaderState {
            session_id: session_id.clone(),
            turn_gen: turn_gen.clone(),
            event_tx: event_tx.clone(),
            acp_session_id: acp_session_id.clone(),
            pending_resume_sid: pending_resume_sid.clone(),
            current_model: current_model.clone(),
            pending_open: pending_open.clone(),
            pending_init: pending_init.clone(),
            pending_prompts: pending_prompts.clone(),
            pending_set: pending_set.clone(),
            pending_perm_options: pending_perm_options.clone(),
            discovered: discovered.clone(),
            stdin: stdin.clone(),
            turn_in_flight: turn_in_flight.clone(),
        };
        let reader = start_acp_reader(&reader_state, stdout, io.clone());

        // Wave 0c-F: compose the first-message preamble from the preset context.
        // ACP has no system-prompt field, so a non-empty preset is delivered by
        // prepending an `[Assistant Rules]` block to the first prompt (light-mode
        // format, byte-identical to the legacy first_message_injector).
        let pending_preamble = Mutex::new(
            wake.config
                .init
                .preset_context
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|ctx| format!("[Assistant Rules]\n{ctx}\n[/Assistant Rules]")),
        );

        let suspend = Arc::new(SuspendController::active(
            ProcHandle::new(reader, io),
            idle_ttl_ms,
            aionui_common::now_ms(),
        ));
        let idle_timer = {
            let tif = turn_in_flight.clone();
            // 009 R6 cleanup path 3: emit BackendSuspended on idle-reap → orchestrator
            // clears the workflow_roster (a running workflow's task_notification will
            // never arrive once the process is reaped).
            let etx = event_tx.clone();
            let sid = session_id.clone();
            let tgen = turn_gen.clone();
            spawn_idle_timer(
                &suspend,
                idle_check_interval_ms(idle_ttl_ms),
                aionui_common::now_ms,
                move || tif.load(std::sync::atomic::Ordering::SeqCst),
                move || {
                    let _ = etx.send(SessionEnvelope {
                        session_id: sid.clone(),
                        turn_gen: tgen.load(std::sync::atomic::Ordering::SeqCst),
                        event: SessionEvent::BackendSuspended,
                    });
                },
            )
        };

        Self {
            session_id,
            capabilities: acp_capabilities(),
            discovered,
            rpc_id: AtomicU64::new(0),
            turn_gen,
            stdin,
            event_tx,
            suspend,
            idle_timer,
            wake,
            reader_state,
            turn_in_flight,
            acp_session_id,
            pending_resume_sid,
            pending_perm_options,
            current_model,
            pending_open,
            pending_init,
            pending_prompts,
            pending_set,
            pending_preamble,
        }
    }

    /// Write one JSON-RPC frame (request or response) as a single line.
    async fn write_frame(&self, frame: Value) -> Result<(), BackendError> {
        write_frame_to(&self.stdin, frame).await
    }

    fn next_rpc_id(&self) -> u64 {
        self.rpc_id.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Resolve the bound ACP session id, waiting briefly for the session/new|load
    /// response the reader binds. Every `session/prompt` + `session/set_*` needs it.
    async fn bound_session(&self) -> Result<String, BackendError> {
        // Regression-by-rewrite (audit + codex-500 twin): this was a hardcoded
        // 40×50ms=2s busy-poll returning a bare Transport → opaque 500 — the IDENTICAL
        // bug codex's bound_thread had, and a downgrade from legacy ACP's 30s
        // (aionui-agent-rest INIT_TIMEOUT_SECS). A cold start / untrusted project slows
        // agent init past 2s → 500. Use the shared handshake budget (30s, env-overridable)
        // and the RETRYABLE HandshakeTimeout so the user sees "agent starting, retry".
        self.bound_session_within(super::handshake_budget()).await
    }

    /// Inner (test seam, mirrors codex bound_thread_within): poll for the acp_session_id
    /// binding within `budget`; tests pass a tiny budget to exercise the timeout branch.
    async fn bound_session_within(&self, budget: std::time::Duration) -> Result<String, BackendError> {
        let polls = (budget.as_millis() / 50).max(1) as u64;
        for _ in 0..polls {
            if let Some(sid) = self.acp_session_id.lock().await.clone() {
                return Ok(sid);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Err(BackendError::HandshakeTimeout(
            "acp sessionId not bound (session/new response not received within handshake budget)".into(),
        ))
    }

    /// G4: did the agent advertise a GENERIC config option with this id (from the
    /// discovered `configOptions[]`)? Gates `Command::SetConfigOption` so the
    /// cap-behavior invariant holds (advertised ⟺ settable; an agent with no generic
    /// options rejects). `mode`/`model` are intentionally NOT here — they route to
    /// the dedicated set_mode/set_model arms before reaching SetConfigOption.
    fn has_config_option(&self, option_id: &str) -> bool {
        self.discovered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .config_options
            .iter()
            .any(|id| id == option_id)
    }

    /// G6 (imperative shell): after `open_session`'s fire-and-forget handshake, the
    /// reader parses the `session/new|load` response ASYNCHRONOUSLY into `discovered`
    /// (the agent's OBSERVED current mode/model — which is the agent's OWN default on
    /// a Resume, not necessarily what this conversation wanted). This one-shot task
    /// waits for that response to land, then re-aligns the agent to the DESIRED
    /// mode/model (the conversation's config) by dispatching the EXISTING, tested
    /// SetMode/SetModel write-half. Decision A (backend self-heals on open) — the app
    /// stays zero-change; the pure `reconcile_plan` owns the diff logic. Best-effort:
    /// any dispatch error is logged, never fatal (a turn can still proceed).
    async fn reconcile_startup_config(self: &Arc<Self>) {
        // Wait for the open-response to be parsed: the reader clears `pending_open`
        // once it consumes the session/new|load result (same bound as bound_session).
        let mut parsed = false;
        for _ in 0..40 {
            if self.pending_open.lock().await.is_none() {
                parsed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        if !parsed {
            // No open-response within the window — nothing observed to reconcile
            // against; a later turn surfaces any real failure. Stay silent (no spam).
            return;
        }
        let caps = self.capabilities(); // merges discovered (observed) over the seed
        let desired_mode = self.wake.config.mode.clone();
        let desired_model = self.wake.config.model.clone();
        // `capabilities()` reflects the OBSERVED current_* once discovered is filled,
        // but the open-time seed also wrote desired into the base snapshot — so read
        // observed from `discovered` directly to avoid comparing desired against itself.
        let (obs_mode, obs_model) = {
            let disc = self.discovered.lock().unwrap_or_else(|e| e.into_inner());
            (disc.current_mode.clone(), disc.current_model.clone())
        };
        let plan = reconcile_plan(
            caps.supported_commands.set_mode,
            desired_mode.as_deref(),
            obs_mode.as_deref(),
            caps.supported_commands.set_model,
            desired_model.as_deref(),
            obs_model.as_deref(),
        );
        for cmd in plan {
            let what = match &cmd {
                Command::SetMode { mode } => format!("mode→{mode}"),
                Command::SetModel { model } => format!("model→{model}"),
                _ => String::new(),
            };
            if let Err(e) = self.dispatch(cmd).await {
                tracing::warn!(session_id = %self.session_id, %what, error = %e, "G6 startup reconcile dispatch failed");
            } else {
                tracing::info!(session_id = %self.session_id, %what, "G6: reconciled resumed session to desired config");
            }
        }
    }

    /// Replay the JSON-RPC handshake over the (already-connected) stdin: an
    /// `initialize`, then `session/load` (Resume — pre-seeds the acp_session_id
    /// binding) or `session/new` (Fresh / lost-Resume). Capability discovery rides
    /// the session/new|load RESPONSE (the reader claims it by the `pending_open`
    /// rpc id). Shared by `open_session` (initial open) and `wake_handle` (idle-wake
    /// re-attach), so the wire shape lives in one place.
    async fn run_handshake(&self, resume_sid: Option<&str>) -> Result<(), BackendError> {
        let init_id = self.next_rpc_id();
        // Register the initialize rpc id so the reader claims its RESPONSE and parses
        // `authMethods[]` (the advertised auth capability) into Discovered.
        self.pending_init.lock().await.replace(init_id);
        self.write_frame(json!({
            "jsonrpc": "2.0", "id": init_id, "method": "initialize",
            "params": initialize_params()
        }))
        .await?;
        let id = self.next_rpc_id();
        self.pending_open.lock().await.replace(id);
        match resume_sid {
            Some(sid) => {
                // Do NOT pre-seed `acp_session_id` here. The ACP spec requires the
                // client to wait for the full `session/load` response (the agent
                // replays history via `session/update` first) before prompting;
                // pre-seeding would let `bound_session()` release the first prompt
                // before opencode has loaded the session → `-32602 session not found`.
                // Stash the sid so the reader binds it when the load RESPONSE arrives
                // (session/load returns no `sessionId` of its own).
                *self.pending_resume_sid.lock().await = Some(sid.to_string());
                // Clear any existing binding so `bound_session()` blocks until THIS
                // load completes. At first open it is already None; on an idle-wake
                // re-attach (wake_handle shares run_handshake) it holds the pre-suspend
                // sid — clearing it makes the post-wake gate wait for the fresh load
                // too, not release a prompt against a not-yet-reloaded session.
                *self.acp_session_id.lock().await = None;
                self.write_frame(json!({
                    "jsonrpc": "2.0", "id": id, "method": "session/load",
                    "params": load_session_params(sid, &self.wake.config)
                }))
                .await?;
            }
            None => {
                self.write_frame(json!({
                    "jsonrpc": "2.0", "id": id, "method": "session/new",
                    "params": new_session_params(&self.wake.config)
                }))
                .await?;
            }
        }
        Ok(())
    }

    /// Wake from Dormant: re-spawn the ACP CLI/bridge, re-take its stdio, swap the
    /// fresh stdin into the retained slot, start a new reader on the SAME
    /// event_tx/turn_gen/bindings, and replay the resume handshake against the
    /// bound acp_session_id (the resume anchor that survived the suspend). Only
    /// reached when idle_ttl is set AND the slot was suspended (a test backend has
    /// no spawner → `inert()` → never enabled).
    async fn wake_handle(&self) -> Result<ProcHandle, BackendError> {
        let (Some(spawner), Some(command)) = (self.wake.spawner.as_ref(), self.wake.command.as_ref()) else {
            return Err(BackendError::Transport(
                "acp wake: no spawner/command (suspension not enabled)".into(),
            ));
        };
        let mut cmd = command.clone();
        cmd.args.extend(self.wake.config.extra_args.iter().cloned());
        if let Some(cwd) = &self.wake.config.cwd {
            cmd.cwd = Some(cwd.clone());
        }
        let proc = spawner
            .spawn(cmd, &[], "aionui-session")
            .await
            .map_err(|e| BackendError::from_spawn("acp resume-spawn failed", e))?;
        let io: Arc<dyn AgentIo> = Arc::from(Box::new(crate::adapter::ManagedProcessIo::new(proc)) as Box<dyn AgentIo>);
        let (stdin, stdout) = match io.take_stdio().await {
            Some((stdin, stdout)) => (Some(stdin), Some(stdout)),
            None => (None, None),
        };
        *self.stdin.lock().await = stdin;
        let reader = start_acp_reader(&self.reader_state, stdout, io.clone());
        // Replay the handshake against the bound sid (resume re-attach via
        // session/load). The binding survived the suspend. On a handshake failure,
        // abort the just-started reader so its AgentIo clone releases and the
        // freshly-spawned child is reaped (kill_on_drop) — else it leaks (the
        // controller never takes ownership of a failed wake's handle).
        let resume_sid = self.acp_session_id.lock().await.clone();
        if let Err(e) = self.run_handshake(resume_sid.as_deref()).await {
            reader.abort();
            return Err(e);
        }
        Ok(ProcHandle::new(reader, io))
    }
}

/// Shared frame writer (used by the backend + the reader's reverse-RPC responses,
/// both behind the same stdin Mutex so writes never interleave).
async fn write_frame_to(
    stdin: &Arc<Mutex<Option<aionui_process::BoxedStdin>>>,
    frame: Value,
) -> Result<(), BackendError> {
    let mut guard = stdin.lock().await;
    let w = guard
        .as_mut()
        .ok_or_else(|| BackendError::Transport("acp stdin unavailable".into()))?;
    let mut line = serde_json::to_vec(&frame).map_err(|e| BackendError::Transport(e.to_string()))?;
    line.push(b'\n');
    use tokio::io::AsyncWriteExt;
    w.write_all(&line)
        .await
        .map_err(|e| BackendError::Transport(e.to_string()))?;
    w.flush().await.map_err(|e| BackendError::Transport(e.to_string()))?;
    Ok(())
}

/// Reader-task context (grouped to avoid a too-many-arguments fn).
struct ReaderCtx {
    session_id: String,
    stdout: Option<aionui_process::BoxedStdout>,
    io: Arc<dyn AgentIo>,
    turn_gen: Arc<AtomicU64>,
    event_tx: broadcast::Sender<SessionEnvelope>,
    acp_session_id: Arc<Mutex<Option<String>>>,
    pending_resume_sid: Arc<Mutex<Option<String>>>,
    current_model: Arc<Mutex<Option<String>>>,
    pending_open: Arc<Mutex<Option<u64>>>,
    pending_init: Arc<Mutex<Option<u64>>>,
    pending_prompts: Arc<Mutex<HashMap<u64, PendingPrompt>>>,
    pending_set: Arc<Mutex<HashMap<u64, String>>>,
    pending_perm_options: PendingPermOptions,
    discovered: Arc<std::sync::Mutex<Discovered>>,
    stdin: Arc<Mutex<Option<aionui_process::BoxedStdin>>>,
    turn_in_flight: Arc<std::sync::atomic::AtomicBool>,
}

/// The process-independent share of `ReaderCtx`: everything cloned into the
/// open-time reader AND every post-wake reader (the per-process `stdout`/`io` are
/// supplied per spawn). The acp_session_id binding survives a suspend, so a wake
/// re-attaches via `session/load`.
#[derive(Clone)]
struct AcpReaderState {
    session_id: String,
    turn_gen: Arc<AtomicU64>,
    event_tx: broadcast::Sender<SessionEnvelope>,
    acp_session_id: Arc<Mutex<Option<String>>>,
    pending_resume_sid: Arc<Mutex<Option<String>>>,
    current_model: Arc<Mutex<Option<String>>>,
    pending_open: Arc<Mutex<Option<u64>>>,
    pending_init: Arc<Mutex<Option<u64>>>,
    pending_prompts: Arc<Mutex<HashMap<u64, PendingPrompt>>>,
    pending_set: Arc<Mutex<HashMap<u64, String>>>,
    pending_perm_options: PendingPermOptions,
    discovered: Arc<std::sync::Mutex<Discovered>>,
    stdin: Arc<Mutex<Option<aionui_process::BoxedStdin>>>,
    /// F-4 turn-active flag: set on dispatch(Send), cleared by the reader at a turn
    /// terminal (synthesized TurnResult / Detached). The idle timer reads it so a
    /// streaming turn is never suspended mid-flight.
    turn_in_flight: Arc<std::sync::atomic::AtomicBool>,
}

/// Spawn an ACP JSON-RPC reader over `stdout`/`io` using the shared state. Used
/// both at open (`spawn_with_wake`) and on every idle-wake (`wake_handle`).
fn start_acp_reader(
    state: &AcpReaderState,
    stdout: Option<aionui_process::BoxedStdout>,
    io: Arc<dyn AgentIo>,
) -> tokio::task::JoinHandle<()> {
    let state = state.clone();
    tokio::spawn(async move {
        reader_task(ReaderCtx {
            session_id: state.session_id,
            stdout,
            io,
            turn_gen: state.turn_gen,
            event_tx: state.event_tx,
            acp_session_id: state.acp_session_id,
            pending_resume_sid: state.pending_resume_sid,
            current_model: state.current_model,
            pending_open: state.pending_open,
            pending_init: state.pending_init,
            pending_prompts: state.pending_prompts,
            pending_set: state.pending_set,
            pending_perm_options: state.pending_perm_options,
            discovered: state.discovered,
            stdin: state.stdin,
            turn_in_flight: state.turn_in_flight,
        })
        .await;
    })
}

/// What `AcpSessionBackend::wake_handle` needs to re-spawn the ACP CLI/bridge and
/// replay the resume handshake. `inert()` (no spawner) is used for test-built
/// backends, which never suspend, so it is never consulted.
struct AcpWakeRecipe {
    spawner: Option<Arc<dyn Spawner>>,
    command: Option<aionui_common::CommandSpec>,
    config: SessionConfig,
}

impl AcpWakeRecipe {
    #[cfg(any(test, feature = "test-support"))]
    fn inert() -> Self {
        Self {
            spawner: None,
            command: None,
            config: SessionConfig::default(),
        }
    }
}

/// The idle-check cadence for a ttl: poll at ~ttl/4 (bounded 1s..=30s). Only
/// consulted when idle_ttl is Some (else no timer is spawned).
fn idle_check_interval_ms(idle_ttl_ms: Option<i64>) -> u64 {
    match idle_ttl_ms {
        Some(ttl) => ((ttl / 4).clamp(1_000, 30_000)) as u64,
        None => 30_000,
    }
}

/// The long-lived JSON-RPC reader: each line is a `session/update` notification,
/// a `session/request_permission` (or other) reverse-RPC, or a response to one of
/// our requests (`session/new`|`load`|`prompt`|`set_*`). Single ordered consumer
/// of stdout, so deltas always fold before the prompt response (the terminal).
async fn reader_task(ctx: ReaderCtx) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let ReaderCtx {
        session_id,
        stdout,
        io,
        turn_gen,
        event_tx,
        acp_session_id,
        pending_resume_sid,
        current_model,
        pending_open,
        pending_init,
        pending_prompts,
        pending_set,
        pending_perm_options,
        discovered,
        stdin,
        turn_in_flight,
    } = ctx;

    let Some(stdout) = stdout else {
        emit(
            &event_tx,
            &session_id,
            turn_gen.load(Ordering::SeqCst),
            // Startup double-take guard: stdio was never available, so there is
            // no meaningful stderr to attribute — G2 summary stays None.
            SessionEvent::Detached {
                exit: None,
                redacted_summary: None,
            },
        );
        return;
    };

    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(frame): Result<Value, _> = serde_json::from_str(line) else {
                    emit(
                        &event_tx,
                        &session_id,
                        turn_gen.load(Ordering::SeqCst),
                        SessionEvent::AdapterSpecific {
                            tag: "acp_unparseable".into(),
                            payload: json!({ "raw": line }),
                        },
                    );
                    continue;
                };

                let method = frame.get("method").and_then(Value::as_str);
                let has_id = frame.get("id").is_some();
                match (method, has_id) {
                    // reverse-RPC (server-initiated request): permission → surface;
                    // everything else → -32601 unblock (A2/A3).
                    (Some(m), true) => {
                        handle_reverse_rpc(
                            m,
                            &frame,
                            &session_id,
                            &turn_gen,
                            &event_tx,
                            &stdin,
                            &pending_perm_options,
                        )
                        .await;
                    }
                    // notification (`session/update` + others) → SessionEvent(s).
                    (Some(m), false) => {
                        let cur = turn_gen.load(Ordering::SeqCst);
                        let params = frame.get("params").unwrap_or(&Value::Null);
                        if m == "session/update" {
                            // A resumed session replays its FULL history as `session/update`
                            // notifications between the `session/load` request and its
                            // RESPONSE (ACP spec ordering — see run_handshake). Those events
                            // are historical: conversation_blocks is the SSOT and the frontend
                            // renders history from it, so broadcasting the replay would
                            // duplicate UI blocks AND spuriously light the turn-active
                            // indicator (a resumed conv has no live turn — e.g. warmup on a
                            // model change). Mirror the agent-rest `replay_suppression` guard:
                            // still run map_update for its metadata side-effects (slash-command
                            // / config-option catalog land in `discovered`), but suppress the
                            // UI emit while the resume load is in flight. The window is
                            // resume-only (`pending_resume_sid` is set on session/load, never on
                            // a fresh session/new — which has no history to replay) and holds
                            // only replay (bound_session() blocks the first prompt until the
                            // load RESPONSE takes the sid), so no live turn output is dropped.
                            let replaying = pending_resume_sid.lock().await.is_some();
                            let events = map_update(params, &current_model, &discovered).await;
                            if !replaying {
                                for ev in events {
                                    emit(&event_tx, &session_id, cur, ev);
                                }
                            }
                        }
                        // Other notifications are FSM-orthogonal / unknown → opaque.
                    }
                    // response to one of OUR requests (id + result/error, no method).
                    _ => {
                        let rid = frame.get("id").and_then(Value::as_u64);
                        let Some(rid) = rid else { continue };

                        // initialize response → parse advertised authMethods[] into
                        // Discovered (capabilities() merges → auth_methods + answer_auth).
                        let is_init = *pending_init.lock().await == Some(rid);
                        if is_init {
                            *pending_init.lock().await = None;
                            if let Some(result) = frame.get("result") {
                                handle_initialize_response(result, &discovered);
                            } else if frame.get("error").is_some() {
                                // 9a-ACP: a connect-time `initialize` ERROR (the agent rejects
                                // the handshake — typically NOT logged in / unsupported protocol)
                                // was previously swallowed (no else arm) → the first prompt hung
                                // in bound_session() then failed opaquely. Synthesize an error
                                // terminal so the reducer (still Starting) routes it to
                                // Error{Backend{message}}, carrying the cause to the 9c
                                // classifier (→ CheckAgentLogin). Peek stderr so a generic
                                // JSON-RPC message gets the allowlisted auth cause enriched in.
                                emit_connect_error(&frame, &io, &event_tx, &session_id, &turn_gen).await;
                                // Clear the open marker too: session/new|load will never come
                                // after a failed initialize, so unblock any waiter (bound_session
                                // / reconcile) instead of letting it spin out its window.
                                *pending_open.lock().await = None;
                            }
                            continue;
                        }

                        // session/new|load response → bind ACP sid + discovery + BackendBound.
                        let is_open = *pending_open.lock().await == Some(rid);
                        if is_open {
                            if let Some(result) = frame.get("result") {
                                handle_open_response(
                                    result,
                                    &session_id,
                                    &turn_gen,
                                    &event_tx,
                                    &acp_session_id,
                                    &pending_resume_sid,
                                    &discovered,
                                )
                                .await;
                            } else if frame.get("error").is_some() {
                                // 9a-ACP: a connect-time `session/new`|`session/load` ERROR
                                // (auth required / bad resume sid / setup rejected) was
                                // previously swallowed → opaque hang. Synthesize an error
                                // terminal (reducer Starting → Error{Backend{message}}), with
                                // the stderr cause enriched in.
                                emit_connect_error(&frame, &io, &event_tx, &session_id, &turn_gen).await;
                            }
                            // G6: clear `pending_open` ONLY AFTER discovery is filled (or the
                            // error is emitted), so the startup-reconcile task (which waits on
                            // `pending_open` clearing) observes the parsed `current_mode/model`,
                            // never a half-filled race.
                            *pending_open.lock().await = None;
                            continue;
                        }

                        // session/set_mode | session/set_model response.
                        //
                        // A JSON-RPC ERROR (e.g. opencode `-32602 model not found`) is
                        // surfaced as a Notice{Warning} + error log so a FAILED set is
                        // visible instead of being silently reported as success.
                        //
                        // A SUCCESS response (any non-error, including opencode's empty
                        // `{}`) is itself the authoritative "applied" signal: emit
                        // ConfigChanged with the just-set value and authoritatively
                        // advance the discovered current_mode/model. We must NOT wait for
                        // an echo notification — claude-acp echoes `config_option_update`
                        // (LIVE-VERIFIED), but opencode 1.16.2 sends `{}` + ZERO
                        // notifications (LIVE round-trip probe), so an echo-only path
                        // leaves the selector stuck on the open-time value forever
                        // ("set doesn't stick"). The label is `"mode→<v>"` / `"model→<v>"`.
                        // If an agent later DOES echo, its ConfigChanged carries the same
                        // value (reducer is idempotent) — the two paths never conflict.
                        if let Some(label) = pending_set.lock().await.remove(&rid) {
                            if let Some(err) = frame.get("error") {
                                let message = err
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("set rejected")
                                    .to_string();
                                tracing::error!(
                                    conversation_id = %session_id,
                                    set = %label,
                                    "ACP set_mode/set_model rejected by agent: {message}"
                                );
                                emit(
                                    &event_tx,
                                    &session_id,
                                    turn_gen.load(Ordering::SeqCst),
                                    SessionEvent::Notice {
                                        // NoticeLevel has no Error tier; Warning is the
                                        // highest user-facing level (the error-ness is in
                                        // the message + the error! log above).
                                        level: crate::event::NoticeLevel::Warning,
                                        message: format!("{label} failed: {message}"),
                                        localized: None,
                                        supersedes_key: None,
                                    },
                                );
                            } else if let Some((kind, value)) = label.split_once('\u{2192}') {
                                // Success: authoritatively converge on the set value.
                                let value = value.to_string();
                                {
                                    let mut disc = discovered.lock().unwrap_or_else(|e| e.into_inner());
                                    match kind {
                                        "mode" => disc.current_mode = Some(value.clone()),
                                        "model" => disc.current_model = Some(value.clone()),
                                        _ => {}
                                    }
                                }
                                let config_changed = match kind {
                                    "mode" => Some(SessionEvent::ConfigChanged {
                                        mode: Some(value),
                                        model: None,
                                    }),
                                    "model" => Some(SessionEvent::ConfigChanged {
                                        mode: None,
                                        model: Some(value),
                                    }),
                                    _ => None,
                                };
                                if let Some(ev) = config_changed {
                                    emit(&event_tx, &session_id, turn_gen.load(Ordering::SeqCst), ev);
                                }
                            }
                            continue;
                        }

                        // session/prompt response → THE ACP TERMINAL. Synthesize
                        // TurnResult from stopReason (success/error/cancelled).
                        let pending = pending_prompts.lock().await.remove(&rid);
                        if let Some(pending) = pending {
                            // F-4: the prompt-response terminal ends the turn → clear
                            // the turn-active flag so the idle timer may suspend.
                            turn_in_flight.store(false, Ordering::SeqCst);
                            // G1-B: only peek stderr for an ERROR terminal (the cause
                            // of a generic JSON-RPC error lives there); a success
                            // response never needs it — avoids a peek every turn.
                            let stderr_tail = if frame.get("error").is_some() {
                                Some(io.peek_stderr(crate::adapter::STDERR_PEEK_LINES).await)
                            } else {
                                None
                            };
                            // Terminal usage: the session/prompt RESPONSE carries
                            // result.usage{inputTokens,outputTokens,totalTokens, cost?}
                            // on bridges that report it (claude-agent-acp, hermes). It
                            // was previously DROPPED (only the streaming usage_update,
                            // which lacks the per-direction split, was read). Emit a
                            // UsageDelta BEFORE the terminal (mirrors claude C-2).
                            if let Some(usage_ev) = parse_acp_result_usage(&frame) {
                                emit(&event_tx, &session_id, pending.turn_gen, usage_ev);
                            }
                            let ev = synth_turn_result(&frame, pending.turn_gen, stderr_tail.as_deref());
                            emit(&event_tx, &session_id, pending.turn_gen, ev);
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    // F-4: the reader loop ended (process exited / stdout EOF) → any in-flight turn
    // is terminal. Clear the turn-active flag so the idle timer is unblocked.
    turn_in_flight.store(false, Ordering::SeqCst);

    // Backend session gone (process exited / stdout EOF): signal the live binding
    // is dead (the threadId/sid stays the resume anchor; conversation persisted it).
    let was_bound = acp_session_id.lock().await.is_some();
    if was_bound {
        emit(
            &event_tx,
            &session_id,
            turn_gen.load(Ordering::SeqCst),
            SessionEvent::BackendBound {
                backend_session_id: None,
            },
        );
    }
    let exit = io.wait_for_exit().await;
    // G2: redact the stderr tail at the backend boundary so a crash carries a
    // user-facing reason (allowlisted, ≤240 chars) without leaking raw stderr.
    let redacted_summary = crate::adapter::redact_exit_stderr(io.as_ref()).await;
    emit(
        &event_tx,
        &session_id,
        turn_gen.load(Ordering::SeqCst),
        SessionEvent::Detached { exit, redacted_summary },
    );
}

fn emit(tx: &broadcast::Sender<SessionEnvelope>, session_id: &str, turn_gen: u64, event: SessionEvent) {
    let _ = tx.send(SessionEnvelope {
        session_id: session_id.to_string(),
        turn_gen,
        event,
    });
}

/// 9a-ACP: synthesize + emit the terminal for a connect-time handshake ERROR
/// (`initialize` / `session/new` / `session/load` returning a JSON-RPC `error`
/// instead of a `result`). Reuses [`synth_turn_result`] (the error-frame arm →
/// `TurnResult{is_error:true, message, api_error_status}`) so the connect error
/// shares the turn-error shape; the reducer, still in `Starting`, routes it to
/// `Error{Backend{message}}` (reducer.rs R16/3.9) — carrying the cause to the 9c
/// classifier (auth → `CheckAgentLogin`) instead of the opaque hang the missing
/// `else` arm caused. Peeks stderr (the agent logs the real cause there without
/// echoing it in the JSON-RPC error) so a generic message is enriched (G1-B/S0).
/// Stamped at the live `turn_gen` (0 at connect): the `Starting` error-TurnResult
/// arm applies no epoch guard, so the cause is never dropped.
async fn emit_connect_error(
    frame: &Value,
    io: &Arc<dyn AgentIo>,
    event_tx: &broadcast::Sender<SessionEnvelope>,
    session_id: &str,
    turn_gen: &Arc<AtomicU64>,
) {
    let stderr_tail = io.peek_stderr(crate::adapter::STDERR_PEEK_LINES).await;
    let cur = turn_gen.load(Ordering::SeqCst);
    let ev = synth_turn_result(frame, cur, Some(stderr_tail.as_str()));
    emit(event_tx, session_id, cur, ev);
}

/// Handle the `initialize` RESPONSE: parse the advertised `authMethods[]` (each
/// `{id, name?, description?}`, ACP `AuthMethod`) into `Discovered.auth_methods`.
/// `capabilities()` merges them → non-empty flips `auth_methods` + the
/// `answer_auth` cap true. hermes advertises `[bedrock, hermes-setup]`; claude
/// ACP advertises none → stays empty (honest, unchanged). Sync-only (no await /
/// no emit): auth methods are a capability fact, not a stream event.
fn handle_initialize_response(result: &Value, discovered: &Arc<std::sync::Mutex<Discovered>>) {
    let methods: Vec<String> = result
        .get("authMethods")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if !methods.is_empty() {
        let mut disc = discovered.lock().unwrap_or_else(|e| e.into_inner());
        disc.auth_methods = methods;
    }
}

/// Handle the `session/new` | `session/load` RESPONSE: bind the ACP session id
/// (→ BackendBound resume anchor) and fill discovered models/modes into the
/// capability snapshot (ACP carries them in this one response, unlike codex).
async fn handle_open_response(
    result: &Value,
    session_id: &str,
    turn_gen: &Arc<AtomicU64>,
    event_tx: &broadcast::Sender<SessionEnvelope>,
    acp_session_id: &Arc<Mutex<Option<String>>>,
    pending_resume_sid: &Arc<Mutex<Option<String>>>,
    discovered: &Arc<std::sync::Mutex<Discovered>>,
) {
    let cur = turn_gen.load(Ordering::SeqCst);
    // Bind the ACP session id. `session/new` carries the freshly-minted `sessionId`
    // in its result; `session/load` does NOT (it returns config/null) — its sid is
    // the one we asked to re-attach, stashed in `pending_resume_sid`. Binding here
    // (on the RESPONSE, not pre-seeded) is what makes `bound_session()` block the
    // first prompt until the load actually completed (ACP spec ordering).
    // The stashed resume sid is consumed (one-shot per handshake), then used as the
    // fallback when the response carries no `sessionId` (the session/load case).
    let resume_sid = pending_resume_sid.lock().await.take();
    let bound_sid = result
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(resume_sid);
    if let Some(sid) = bound_sid {
        *acp_session_id.lock().await = Some(sid.clone());
        emit(
            event_tx,
            session_id,
            cur,
            SessionEvent::BackendBound {
                backend_session_id: Some(sid),
            },
        );
    }
    // models / modes. TWO wire shapes across ACP agents (both LIVE-pinned):
    //   - claude-acp 0.33.x: TOP-LEVEL `result.models`/`result.modes`
    //     (SessionModelState{availableModels[],currentModelId} / SessionModeState).
    //   - opencode 1.16.2: inside `result.configOptions[]` as items `{id:"model"|"mode",
    //     currentValue, options:[{value,name,description}]}` (value = provider-prefixed
    //     full id, e.g. `amazon-bedrock/openai.gpt-5.5`). NO top-level models/modes.
    // We parse BOTH (top-level first, then configOptions fallback) so model/mode are
    // never dropped → config-options non-empty → the picker has real values and a
    // set_model does not -32602 on a stale hardcoded id (opencode prod bug). See
    // protocols/design/aioncore-opencode-acp-configoptions-model-mode-prompt.md.
    let mut disc = discovered.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(models) = result.get("models") {
        let avail = parse_models(models);
        if !avail.is_empty() {
            disc.models = avail;
        }
        if let Some(cur_id) = models.get("currentModelId").and_then(Value::as_str) {
            disc.current_model = Some(cur_id.to_string());
        }
    }
    if let Some(modes) = result.get("modes") {
        let avail = parse_modes(modes);
        if !avail.is_empty() {
            disc.modes = avail;
        }
        if let Some(cur_id) = modes.get("currentModeId").and_then(Value::as_str) {
            disc.current_mode = Some(cur_id.to_string());
        }
    }
    // opencode fallback: extract model/mode from configOptions[] when the top-level
    // keys were absent/empty. `options[]{value,name,description}` → ModelInfo/ModeInfo
    // (value→id); `currentValue` → current_model/current_mode.
    let config_options = result.get("configOptions");
    if disc.models.is_empty()
        && let Some((opts, current)) = config_option_select(config_options, "model")
    {
        disc.models = opts
            .into_iter()
            .map(|(value, name, description)| ModelInfo {
                id: value,
                name,
                description,
                reasoning_efforts: Vec::new(),
            })
            .collect();
        if disc.current_model.is_none() {
            disc.current_model = current;
        }
    }
    if disc.modes.is_empty()
        && let Some((opts, current)) = config_option_select(config_options, "mode")
    {
        disc.modes = opts
            .into_iter()
            .map(|(value, name, description)| ModeInfo {
                id: value,
                name,
                description,
            })
            .collect();
        if disc.current_mode.is_none() {
            disc.current_mode = current;
        }
    }
    // G4: generic config option ids (NOT mode/model — those have dedicated arms),
    // e.g. claude-acp `effort`. Gates Command::SetConfigOption (advertised ⟺ settable).
    let generic = parse_generic_config_option_ids(config_options);
    if !generic.is_empty() {
        disc.config_options = generic;
    }
}

/// opencode shape: pull the `configOptions[]` item with `id == want` (e.g. "model" /
/// "mode") and return its `(options:[(value,name,description)], currentValue)`. None
/// if absent. The `value` token is what `dispatch(SetModel/SetMode)` sends back
/// (opencode validates against it), so it becomes the `id`. (claude-acp carries
/// model/mode top-level instead, handled before this is reached.)
#[allow(clippy::type_complexity)]
fn config_option_select(
    config_options: Option<&Value>,
    want: &str,
) -> Option<(Vec<(String, String, Option<String>)>, Option<String>)> {
    let arr = config_options.and_then(Value::as_array)?;
    let item = arr.iter().find(|o| o.get("id").and_then(Value::as_str) == Some(want))?;
    let options = item
        .get("options")
        .and_then(Value::as_array)
        .map(|opts| {
            opts.iter()
                .filter_map(|o| {
                    let value = o.get("value").and_then(Value::as_str)?.to_string();
                    let name = o.get("name").and_then(Value::as_str).unwrap_or(&value).to_string();
                    let description = o.get("description").and_then(Value::as_str).map(str::to_string);
                    Some((value, name, description))
                })
                .collect()
        })
        .unwrap_or_default();
    let current = item.get("currentValue").and_then(Value::as_str).map(str::to_string);
    Some((options, current))
}

/// G4: from a `configOptions[]` array (session/new|load response OR a
/// `config_option_update` notification), the ids that are NOT `mode`/`model` — the
/// generic options the dedicated set_mode/set_model arms do not cover. Wire-pinned
/// to the claude-acp 0.33.1 shape (`[{id, category, type, currentValue, options}]`).
fn parse_generic_config_option_ids(config_options: Option<&Value>) -> Vec<String> {
    let Some(arr) = config_options.and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|o| o.get("id").and_then(Value::as_str))
        .filter(|id| *id != "mode" && *id != "model")
        .map(str::to_owned)
        .collect()
}

/// ACP `SessionModelState.availableModels[]` → `ModelInfo`. Each
/// `{modelId, name}` (camelCase ACP wire).
fn parse_models(models: &Value) -> Vec<ModelInfo> {
    models
        .get("availableModels")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("modelId").and_then(Value::as_str)?;
                    Some(ModelInfo {
                        id: id.to_string(),
                        name: m.get("name").and_then(Value::as_str).unwrap_or(id).to_string(),
                        description: m.get("description").and_then(Value::as_str).map(str::to_string),
                        reasoning_efforts: Vec::new(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// ACP `SessionModeState.availableModes[]` → `ModeInfo`. Each `{id, name}`.
fn parse_modes(modes: &Value) -> Vec<ModeInfo> {
    modes
        .get("availableModes")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("id").and_then(Value::as_str)?;
                    Some(ModeInfo {
                        id: id.to_string(),
                        name: m.get("name").and_then(Value::as_str).unwrap_or(id).to_string(),
                        description: m.get("description").and_then(Value::as_str).map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Server-initiated request (reverse-RPC). `session/request_permission` → surface
/// as `Permission{Tool}` (answered by `dispatch(AnswerPermission)` writing the
/// keyed response). Any other reverse method → immediate `-32601` so the channel
/// never deadlocks (A2/A3 — matches the SDK's own auto-reject + the ACP-audit
/// finding that unhandled reverse RPC must be clean-rejected, not dropped).
async fn handle_reverse_rpc(
    method: &str,
    frame: &Value,
    session_id: &str,
    turn_gen: &Arc<AtomicU64>,
    event_tx: &broadcast::Sender<SessionEnvelope>,
    stdin: &Arc<Mutex<Option<aionui_process::BoxedStdin>>>,
    pending_perm_options: &PendingPermOptions,
) {
    let cur = turn_gen.load(Ordering::SeqCst);
    let id = frame.get("id").cloned().unwrap_or(Value::Null);
    match method {
        "session/request_permission" => {
            // The wire `id` is the request_id the conversation answers. The backend
            // does NOT decide — it parses G3 context (MCP server name + answerable
            // options) into `metadata` so the conversation facade can consult its
            // injected PermissionAuthorizer (team-MCP allowlist auto-approval). A
            // human still decides for everything the authorizer does not auto-approve.
            let metadata = parse_permission_metadata(frame.get("params"));
            // Remember the OFFERED options (real optionId + kind) keyed by request_id,
            // so dispatch(AnswerPermission) can echo a REAL optionId picked by kind
            // (Approved→allow_once, AllowAlways→allow_always, Denied→reject_once) — the
            // agent rejects a fabricated id. Stored before the emit so the answer (which
            // may race back fast) always finds it.
            if let Some(opts) = frame
                .get("params")
                .and_then(|p| p.get("options"))
                .and_then(Value::as_array)
            {
                let parsed: Vec<(String, String)> = opts
                    .iter()
                    .filter_map(|o| {
                        let oid = o.get("optionId").and_then(Value::as_str)?.to_string();
                        let kind = o.get("kind").and_then(Value::as_str).unwrap_or("").to_string();
                        Some((oid, kind))
                    })
                    .collect();
                if !parsed.is_empty() {
                    pending_perm_options.lock().await.insert(id.to_string(), parsed);
                }
            }
            // Carry the raised toolCall's `title` and `rawInput` so the permission
            // card can show the approver WHAT they are approving (AionUi issue
            // #3779 — a title-less card read as a bare "Permission request" and the
            // user approved blind). Same wire fields `parse_permission_metadata`
            // already reads (`toolCall.title` / `toolCall.rawInput`, ACP
            // session/request_permission params). Display-to-the-approver, not
            // logging — TIO-13 still applies to log output.
            let perm_tool_call = frame.get("params").and_then(|p| p.get("toolCall"));
            emit(
                event_tx,
                session_id,
                cur,
                SessionEvent::Permission {
                    request_id: id.to_string(),
                    kind: PermissionKind::Tool,
                    metadata,
                    tool_name: perm_tool_call
                        .and_then(|tc| tc.get("title"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    input: perm_tool_call.and_then(|tc| tc.get("rawInput")).cloned(),
                },
            );
        }
        _ => {
            write_reverse_error(stdin, &id, -32601, "method not handled by aionui-session").await;
            emit(
                event_tx,
                session_id,
                cur,
                SessionEvent::AdapterSpecific {
                    tag: "acp_reverse_rpc".into(),
                    payload: json!({ "method": method, "id": id }),
                },
            );
        }
    }
}

/// G3: parse the `session/request_permission` params into the NON-authoritative
/// `Permission.metadata` the conversation uses for auto-approval. Extracts the MCP
/// server name (codex-style `toolCall.rawInput.server_name`, else claude-style
/// `mcp__<server>__<tool>` title prefix — ported from the legacy permission_router
/// `extract_mcp_server_name`) and the answerable `options` (optionId + kind), so
/// the conversation can both decide (server allowlist) and pick an allow option
/// without re-reading the wire. Returns `None` when nothing useful is present.
fn parse_permission_metadata(params: Option<&Value>) -> Option<Value> {
    let params = params?;
    let tool_call = params.get("toolCall");
    let server_name = extract_mcp_server_name(tool_call);
    let options: Vec<Value> = params
        .get("options")
        .and_then(Value::as_array)
        .map(|opts| {
            opts.iter()
                .filter_map(|o| {
                    let option_id = o.get("optionId").and_then(Value::as_str)?;
                    Some(json!({
                        "option_id": option_id,
                        "kind": o.get("kind").and_then(Value::as_str).unwrap_or(""),
                        // CT-PERM-OPTIONS: carry the human label so the conversation
                        // can render a clickable option (not just an opaque id).
                        "name": o.get("name").and_then(Value::as_str).unwrap_or(""),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    if server_name.is_none() && options.is_empty() {
        return None;
    }
    let mut meta = serde_json::Map::new();
    if let Some(name) = server_name {
        meta.insert("server_name".into(), Value::String(name));
    }
    if !options.is_empty() {
        meta.insert("options".into(), Value::Array(options));
    }
    Some(Value::Object(meta))
}

/// MCP server name from a `toolCall`: prefer `rawInput.server_name` (codex shape),
/// else the `mcp__<server>__<tool>` title prefix (claude shape). Mirrors the legacy
/// `extract_mcp_server_name` two-source order.
fn extract_mcp_server_name(tool_call: Option<&Value>) -> Option<String> {
    let tool_call = tool_call?;
    let from_raw = tool_call
        .get("rawInput")
        .and_then(|ri| ri.get("server_name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if from_raw.is_some() {
        return from_raw;
    }
    let title = tool_call.get("title").and_then(Value::as_str)?;
    let rest = title.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some(server.to_owned())
}

/// Write a JSON-RPC ERROR response to unblock a reverse-RPC (A2/A3). Best-effort.
async fn write_reverse_error(
    stdin: &Arc<Mutex<Option<aionui_process::BoxedStdin>>>,
    id: &Value,
    code: i64,
    message: &str,
) {
    if id.is_null() {
        return;
    }
    let frame = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    let _ = write_frame_to(stdin, frame).await;
}

/// Map an ACP `session/update` notification `params` → canonical SessionEvent(s).
/// A1 anti-panic: match on the `sessionUpdate` STRING, never deserialize a closed
/// enum. Unknown `sessionUpdate` → `AdapterSpecific`. `update` shapes verified
/// against real hermes/codex-acp wire (camelCase fields).
async fn map_update(
    params: &Value,
    current_model: &Arc<Mutex<Option<String>>>,
    discovered: &Arc<std::sync::Mutex<Discovered>>,
) -> Vec<SessionEvent> {
    let update = params.get("update").unwrap_or(&Value::Null);
    let kind = update.get("sessionUpdate").and_then(Value::as_str).unwrap_or("");
    match kind {
        "agent_message_chunk" => {
            let text = update
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            vec![SessionEvent::MessageDelta {
                item_id: "acp:text".into(),
                text: text.to_string(),
            }]
        }
        "agent_thought_chunk" => {
            let text = update
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            vec![SessionEvent::ThoughtDelta {
                item_id: "acp:think".into(),
                text: text.to_string(),
            }]
        }
        "tool_call" => {
            let id = update.get("toolCallId").and_then(Value::as_str).unwrap_or("");
            let title = update.get("title").and_then(Value::as_str).unwrap_or("tool");
            vec![SessionEvent::ToolCall {
                tool_use_id: id.to_string(),
                name: title.to_string(),
                subagent: SubagentKind::Inline,
                // Gap #4 / H2: carry the ACP tool ARGUMENTS (`rawInput`, the spec's
                // optional pre-parsed tool input). Absent → Value::Null. TIO-13: never
                // logged at info.
                input: update.get("rawInput").cloned().unwrap_or(Value::Null),
                // 009 H5: ACP has no per-frame subagent attribution wire — main agent.
                parent_tool_use_id: None,
            }]
        }
        "tool_call_update" => {
            // A COMPLETED/FAILED tool is substantive output (ToolResult). Other
            // statuses (pending/in_progress) carry no FSM meaning.
            let status = update.get("status").and_then(Value::as_str).unwrap_or("");
            if matches!(status, "completed" | "failed") {
                let id = update.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                vec![SessionEvent::ToolResult {
                    tool_use_id: id.to_string(),
                    // 009 R7/H3: ACP status:failed → is_error (was dropped → a failed
                    // tool rendered as success, §12.9 TIO-8/9).
                    is_error: status == "failed",
                    // 009 R8: carry the tool OUTPUT from the ACP `content[]` (text /
                    // image / diff) + `locations[]` (touched file paths). A content
                    // item's inner ContentBlock may be an image (image-file Read /
                    // screenshot / vision tool) — decoded to ToolResultContent::Image,
                    // not dropped (protocol-audit fix; the prior "ACP never inlines
                    // image bytes" claim was false).
                    content: parse_acp_tool_content(update),
                    // 009 H5: ACP has no per-frame subagent attribution — main agent.
                    parent_tool_use_id: None,
                }]
            } else {
                Vec::new()
            }
        }
        "usage_update" => {
            // Two ACP shapes, both handled (do not assume cost-free / split-free):
            //  - hermes: {used, size} (cumulative token count, no split, no cost).
            //  - claude-agent-acp: richer {inputTokens, outputTokens, totalTokens,
            //    cost:{amount, currency}}. cost was previously DROPPED (hardcoded None)
            //    + the per-direction split ignored. cost_usd is consumed downstream
            //    (turn_finalizer). Read whichever fields are present.
            let input = update.get("inputTokens").and_then(Value::as_u64).unwrap_or(0);
            let output = update.get("outputTokens").and_then(Value::as_u64).unwrap_or(0);
            let total = update
                .get("totalTokens")
                .and_then(Value::as_u64)
                .or_else(|| update.get("used").and_then(Value::as_u64))
                .unwrap_or(input + output);
            let cost_usd = update.get("cost").and_then(|c| c.get("amount")).and_then(Value::as_f64);
            vec![SessionEvent::UsageDelta {
                input_tokens: input,
                output_tokens: output,
                total_tokens: total,
                cost_usd,
                // ACP spells the context window `size` (the UsageUpdate shape the
                // frontend indicator reads back as its denominator).
                context_window: update.get("size").and_then(Value::as_u64),
                // ACP's UsageUpdate carries no per-turn split, so the detail
                // line stays empty and the renderer omits it.
                breakdown: crate::event::UsageBreakdown::default(),
            }]
        }
        "current_mode_update" => {
            // Real ACP SessionUpdate variant (schema 0.12.0 client.rs:103 CurrentModeUpdate{currentModeId}).
            let mode = update.get("currentModeId").and_then(Value::as_str).map(str::to_string);
            vec![SessionEvent::ConfigChanged { mode, model: None }]
        }
        // NOTE: there is NO `current_model_update` SessionUpdate in ACP. The official
        // schema (agent-client-protocol-schema 0.12.0) defines CurrentModeUpdate but no
        // CurrentModelUpdate — the current model lives in SessionModelState.current_model_id
        // inside the session/new|load|set RESULT (read at open + on set_model), not in a
        // streaming notification. A prior arm parsed `current_model_update` by symmetry
        // with current_mode_update — a guessed frame that does not exist on the wire
        // (contracts README #9). REMOVED. Real mid-session model changes ride
        // `config_option_update` (LIVE-VERIFIED for claude-acp; handled below) → ConfigChanged.
        // If some non-standard agent ever emits a literal `current_model_update`, it falls
        // through to the `_ =>` AdapterSpecific catch-all (lossless, no invented parse).
        "available_commands_update" => {
            // #101: fill the discovered slash-command catalog from
            // `update.availableCommands[{name, description, input?}]` (wire-pinned:
            // hermes + claude-acp). FSM-orthogonal (no SessionEvent) — still returns
            // AdapterSpecific below so the event surface is unchanged. Anti-panic:
            // filter_map over the array, never deserialize a closed enum (A1 doctrine).
            if let Some(cmds) = update.get("availableCommands").and_then(Value::as_array) {
                let parsed: Vec<crate::capability::SlashCommandInfo> = cmds
                    .iter()
                    .filter_map(|c| {
                        let name = c.get("name").and_then(Value::as_str)?.to_string();
                        Some(crate::capability::SlashCommandInfo {
                            name,
                            description: c.get("description").and_then(Value::as_str).map(str::to_string),
                        })
                    })
                    .collect();
                discovered.lock().unwrap_or_else(|e| e.into_inner()).slash_commands = parsed;
            }
            vec![SessionEvent::AdapterSpecific {
                tag: format!("acp_update:{kind}"),
                payload: update.clone(),
            }]
        }
        "config_option_update" => {
            // Refresh the discovered GENERIC config-option ids (so the SetConfigOption
            // gate stays accurate), AND surface a mode/model change as ConfigChanged.
            //
            // LIVE-VERIFIED (claude-agent-acp, acp_claude_bridge_set_mode_config_change_behavior):
            // some ACP agents (claude-acp) route mode/model changes through
            // config_option_update — `configOptions:[{id:"mode",currentValue:"plan"},
            // {id:"model",currentValue:"default"}, ...]` — NOT through
            // current_mode_update/current_model_update. Earlier this arm dropped the
            // mode/model currentValue (only tracked generic ids), so a claude-acp mode
            // switch never reached ConfigChanged → the frontend selector would not
            // update (README discipline #10: sense the change however the agent reports
            // it). Extract the mode/model currentValue here so BOTH report shapes are
            // covered. (acp is not prod-wired yet — backend_router fail-loud — so this
            // had no production impact; it is fixed now that the real wire is captured.)
            let opts = update.get("configOptions");
            let generic = parse_generic_config_option_ids(opts);
            if !generic.is_empty() {
                discovered.lock().unwrap_or_else(|e| e.into_inner()).config_options = generic;
            }
            let current_value = |id: &str| -> Option<String> {
                opts.and_then(Value::as_array)?
                    .iter()
                    .find(|o| o.get("id").and_then(Value::as_str) == Some(id))
                    .and_then(|o| o.get("currentValue").and_then(Value::as_str))
                    .map(str::to_string)
            };
            let mode = current_value("mode");
            let model = current_value("model");
            let mut events = Vec::new();
            if mode.is_some() || model.is_some() {
                if let Some(m) = &model {
                    *current_model.lock().await = Some(m.clone());
                }
                events.push(SessionEvent::ConfigChanged { mode, model });
            }
            // Keep the opaque AdapterSpecific too (carries the full options catalog for
            // any generic-option consumer; the ConfigChanged above is additive).
            events.push(SessionEvent::AdapterSpecific {
                tag: format!("acp_update:{kind}"),
                payload: update.clone(),
            });
            events
        }
        "plan" => {
            // LC-8a: ACP to-do plan snapshot. `update.entries[{content, status,
            // priority?}]` (wire-pinned acp-zed/copilot) → SessionEvent::Plan. ACP is
            // the superset shape; snake_case `in_progress`→InProgress, priority maps.
            // FSM-orthogonal (the reducer no-ops it); a full-replace snapshot.
            let entries: Vec<crate::event::PlanEntry> = update
                .get("entries")
                .and_then(Value::as_array)
                .map(|es| {
                    es.iter()
                        .filter_map(|e| {
                            let content = e.get("content").and_then(Value::as_str)?.to_string();
                            let status = map_plan_status(e.get("status").and_then(Value::as_str).unwrap_or(""));
                            let priority = map_plan_priority(e.get("priority").and_then(Value::as_str));
                            Some(crate::event::PlanEntry {
                                content,
                                status,
                                priority,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            // ACP plan carries no explanation (codex-only field).
            vec![SessionEvent::Plan {
                entries,
                explanation: None,
            }]
        }
        // session_info_update / user chunk / unknown → FSM-orthogonal opaque
        // (never panic on an unknown variant).
        _ => vec![SessionEvent::AdapterSpecific {
            tag: format!("acp_update:{kind}"),
            payload: update.clone(),
        }],
    }
}

/// LC-8a: normalize an ACP/codex plan-step status string → canonical `PlanStatus`
/// (I8). camelCase `inProgress` (codex) AND snake_case `in_progress` (ACP) both map
/// to `InProgress`; unknown → `Pending` (never panic).
fn map_plan_status(s: &str) -> crate::event::PlanStatus {
    use crate::event::PlanStatus;
    match s {
        "inProgress" | "in_progress" => PlanStatus::InProgress,
        "completed" => PlanStatus::Completed,
        _ => PlanStatus::Pending,
    }
}

/// LC-8a: map an ACP plan-step priority string → `PlanPriority` (None when absent /
/// unknown — codex never sets one).
fn map_plan_priority(s: Option<&str>) -> Option<crate::event::PlanPriority> {
    use crate::event::PlanPriority;
    match s {
        Some("high") => Some(PlanPriority::High),
        Some("medium") => Some(PlanPriority::Medium),
        Some("low") => Some(PlanPriority::Low),
        _ => None,
    }
}

/// Synthesize the terminal `TurnResult` from a `session/prompt` RESPONSE. This is
/// the ACP-specific terminal: the response's `stopReason` (success/limit/refusal/
/// cancelled) IS the turn outcome — there is no `turn/completed` notification.
/// `epoch: 0` would let the orchestrator restamp, but we already know the turn
/// epoch (the prompt's `pending_prompts` entry), so we stamp it directly.
/// G1-B/C: synthesize the ACP terminal TurnResult from the prompt-response frame.
/// `stderr_tail` (G1-B) is the backend's recent stderr — passed in by the reader
/// (which owns `io`) so this stays a pure, testable function. When the JSON-RPC
/// error message is generic, the allowlisted stderr cause (S0) is appended; the
/// classifier (G1-C) maps a known-generic message to a friendlier user tip.
/// Parse a `session/prompt` RESPONSE's `result.usage` into a terminal UsageDelta
/// (claude-agent-acp / hermes carry it: {inputTokens, outputTokens, totalTokens,
/// cost:{amount}}). Previously DROPPED — only the streaming usage_update was read,
/// which lacks the per-direction split. None when no usage present or all-zero.
fn parse_acp_result_usage(frame: &Value) -> Option<SessionEvent> {
    let usage = frame.get("result")?.get("usage")?;
    let input = usage.get("inputTokens").and_then(Value::as_u64).unwrap_or(0);
    let output = usage.get("outputTokens").and_then(Value::as_u64).unwrap_or(0);
    let total = usage
        .get("totalTokens")
        .and_then(Value::as_u64)
        .unwrap_or(input + output);
    let cost_usd = usage.get("cost").and_then(|c| c.get("amount")).and_then(Value::as_f64);
    if total == 0 && cost_usd.is_none() {
        return None;
    }
    Some(SessionEvent::UsageDelta {
        input_tokens: input,
        output_tokens: output,
        total_tokens: total,
        cost_usd,
        // ACP spells the context window `size` (see the notification path above).
        context_window: usage.get("size").and_then(Value::as_u64),
        // ACP's end-of-turn usage carries no per-turn split either.
        breakdown: crate::event::UsageBreakdown::default(),
    })
}

fn synth_turn_result(frame: &Value, turn_gen: u64, stderr_tail: Option<&str>) -> SessionEvent {
    // A JSON-RPC error response (the prompt itself failed) → error terminal.
    if let Some(err) = frame.get("error") {
        let raw = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("ACP prompt failed");
        let message = enrich_acp_error_message(raw, stderr_tail);
        let api_error_status = err
            .get("data")
            .and_then(|d| d.get("httpStatusCode"))
            .and_then(Value::as_u64)
            .map(|n| n as u16);
        return SessionEvent::TurnResult {
            is_error: true,
            api_error_status,
            result_text: message,
            epoch: turn_gen,
            outcome: TurnOutcome::Failed,
        };
    }
    // A normal response carries `result.stopReason`.
    let stop = frame
        .get("result")
        .and_then(|r| r.get("stopReason"))
        .and_then(Value::as_str)
        .unwrap_or("end_turn");
    let outcome = map_stop_reason(stop);
    // ACP has no result text; the empty-turn signal is "no substantive output"
    // (the reducer decides from folded deltas) AND empty result_text. A non-error
    // stop reason carries an empty result_text → reducer's saw-substantive gate
    // (deltas already folded by this single-reader ordering) decides Idle vs Empty.
    SessionEvent::TurnResult {
        is_error: false,
        api_error_status: None,
        result_text: String::new(),
        epoch: turn_gen,
        outcome,
    }
}

/// G1-B/C: turn a raw ACP JSON-RPC error message into a user-facing one. When the
/// message is generic (`internal` / `unknown` / `failed` / empty-ish), the real
/// cause is almost always on the agent's stderr (codex-acp/claude-acp log it as a
/// tracing event WITHOUT echoing it in the JSON-RPC error). If the allowlisted S0
/// extractor finds a cause there, append it: `"<raw> (<redacted cause>)"`. A
/// non-generic message (already specific) is returned unchanged. Pure function:
/// the reader peeks stderr and passes it in.
fn enrich_acp_error_message(raw: &str, stderr_tail: Option<&str>) -> String {
    if !is_generic_error_message(raw) {
        return raw.to_string();
    }
    if let Some(tail) = stderr_tail
        && let Some(cause) = aionui_common::error_extract::extract_error_message(tail)
    {
        return format!("{raw} ({cause})");
    }
    raw.to_string()
}

/// A JSON-RPC error message is "generic" (worth enriching from stderr) when it
/// carries no actionable detail — the ACP bridges emit these for upstream failures
/// whose real cause is only on stderr. Matches the legacy `acp_error_public_message`
/// `AgentInternal` intent without needing the typed SDK error.
fn is_generic_error_message(msg: &str) -> bool {
    let m = msg.trim().to_lowercase();
    m.is_empty()
        || m == "acp prompt failed"
        || m.contains("internal error")
        || m.contains("internal server error")
        || m == "internal"
        || m == "unknown"
        || m == "failed"
        || m == "unknown error"
}

/// G6 (functional core): decide which SetMode/SetModel commands re-align a freshly
/// opened/resumed ACP session's OBSERVED mode/model (what the agent came up with,
/// from the session/new|load response) to the DESIRED values (the conversation's
/// config). Pure + total — the thin shell (`reconcile_startup_config`) dispatches
/// the returned commands.
///
/// A dimension is reconciled ONLY when the agent ADVERTISED it (`*_supported`) — so
/// a mode-less agent never gets a spurious set_mode — AND a desired value exists AND
/// it differs from observed (including observed=None: the agent advertised the
/// dimension but did not report a current, so we still align to desired). Mirrors
/// the legacy `agent_reconcile::plan_reconcile` intent without its typed plan struct.
fn reconcile_plan(
    modes_supported: bool,
    desired_mode: Option<&str>,
    observed_mode: Option<&str>,
    models_supported: bool,
    desired_model: Option<&str>,
    observed_model: Option<&str>,
) -> Vec<Command> {
    let mut out = Vec::new();
    if modes_supported
        && let Some(want) = desired_mode
        && observed_mode != Some(want)
    {
        out.push(Command::SetMode { mode: want.to_string() });
    }
    if models_supported
        && let Some(want) = desired_model
        && observed_model != Some(want)
    {
        out.push(Command::SetModel {
            model: want.to_string(),
        });
    }
    out
}

/// Map an ACP `stopReason` (snake_case wire) → `TurnOutcome`.
fn map_stop_reason(stop: &str) -> TurnOutcome {
    match stop {
        "end_turn" => TurnOutcome::Completed {
            stop_reason: StopReason::EndTurn,
        },
        "max_tokens" => TurnOutcome::Completed {
            stop_reason: StopReason::Truncated(TruncationKind::MaxTokens),
        },
        "max_turn_requests" => TurnOutcome::Completed {
            stop_reason: StopReason::Truncated(TruncationKind::MaxTurns),
        },
        "refusal" => TurnOutcome::Completed {
            stop_reason: StopReason::Refused { category: None },
        },
        "cancelled" => TurnOutcome::Cancelled {
            reason: crate::event::CancelReason::UserCancel,
        },
        // Unknown future stopReason → treat as a clean end (never panic).
        _ => TurnOutcome::Completed {
            stop_reason: StopReason::EndTurn,
        },
    }
}

/// Map our multimodal `ContentBlock`s → ACP `session/prompt.prompt` content
/// blocks (camelCase wire). Text + image (advertised); others dropped (UI gated
/// by prompt_blocks).
fn build_prompt_blocks(content: &[ContentBlock]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(json!({ "type": "text", "text": t })),
            ContentBlock::Image { data, media_type } => {
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(data);
                Some(json!({ "type": "image", "mimeType": media_type, "data": b64 }))
            }
            ContentBlock::ResourceLink { uri, mime_type } => Some(json!({
                "type": "resource_link", "uri": uri, "mimeType": mime_type
            })),
            _ => None,
        })
        .collect()
}

/// 009 R8: extract a completed ACP `tool_call_update`'s OUTPUT into the
/// backend-neutral `ToolResultContent` Vec. ACP carries it two ways:
///  - `content[]` items — `{type:"content", content:{type:"text", text}}` → `Text`;
///    `{type:"diff", path, oldText, newText}` → `FilePath` with the diff text.
///  - `locations[]` — `{path}` files the tool touched → `FilePath` references.
///
/// A `content` item's inner ContentBlock may be an image (image-file Read /
/// screenshot / vision tool) → decoded to `Image` (base64 `data` + `mimeType`);
/// audio / resource_link / unknown have no neutral mapping yet and are dropped.
/// Wire is camelCase; we also accept snake_case defensively.
fn parse_acp_tool_content(update: &Value) -> Vec<crate::event::ToolResultContent> {
    use crate::event::ToolResultContent;
    let mut out = Vec::new();
    if let Some(items) = update.get("content").and_then(Value::as_array) {
        for it in items {
            match it.get("type").and_then(Value::as_str) {
                Some("content") => {
                    // A ToolCallContent{type:content, content:<ContentBlock>}. The
                    // inner ContentBlock is text OR image (ACP ContentBlock variants) —
                    // previously only text was read, so a tool returning an image
                    // (image-file Read, screenshot, vision tool) silently dropped its
                    // bytes. Carry the image block too (same {data:base64, mimeType}
                    // shape we send in build_prompt_blocks).
                    let inner = it.get("content");
                    match inner.and_then(|c| c.get("type")).and_then(Value::as_str) {
                        Some("text") | None => {
                            if let Some(text) = inner.and_then(|c| c.get("text")).and_then(Value::as_str) {
                                out.push(ToolResultContent::Text(text.to_string()));
                            }
                        }
                        Some("image") => {
                            use base64::Engine as _;
                            let media_type = inner
                                .and_then(|c| c.get("mimeType"))
                                .and_then(Value::as_str)
                                .unwrap_or("image/png")
                                .to_string();
                            if let Some(bytes) = inner
                                .and_then(|c| c.get("data"))
                                .and_then(Value::as_str)
                                .and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok())
                            {
                                out.push(ToolResultContent::Image {
                                    media_type,
                                    data: bytes,
                                });
                            }
                        }
                        _ => {} // audio / resource_link / unknown — no neutral mapping yet
                    }
                }
                Some("diff") => {
                    if let Some(path) = it.get("path").and_then(Value::as_str) {
                        let pick = |a: &str, b: &str| {
                            it.get(a)
                                .or_else(|| it.get(b))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        };
                        out.push(ToolResultContent::FilePath {
                            path: path.to_string(),
                            mime: None,
                            old_text: pick("oldText", "old_text"),
                            new_text: pick("newText", "new_text"),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(locs) = update.get("locations").and_then(Value::as_array) {
        for loc in locs {
            if let Some(path) = loc.get("path").and_then(Value::as_str) {
                // Avoid duplicating a path already carried by a diff item.
                let dup = out
                    .iter()
                    .any(|c| matches!(c, ToolResultContent::FilePath { path: p, .. } if p == path));
                if !dup {
                    out.push(ToolResultContent::FilePath {
                        path: path.to_string(),
                        mime: None,
                        old_text: None,
                        new_text: None,
                    });
                }
            }
        }
    }
    out
}

#[async_trait::async_trait]
impl SessionBackend for AcpSessionBackend {
    async fn dispatch(&self, command: Command) -> Result<CommandReceipt, BackendError> {
        match command {
            Command::Send { content, metadata } => {
                // §C6 Layer-2: reject any block kind ACP does not advertise
                // (prompt_blocks: text + image + resource) BEFORE wire-write —
                // never silently drop it ("adapter authoritatively rejects → CommandNotSupported,
                // never a silent drop"). An audio / at_mention block is rejected, keyed on
                // its `content_block:<kind>` name (parity with codex/claude).
                let blocks = self.capabilities().prompt_blocks;
                if let Some(bad) = content.iter().find(|b| !blocks.allows(b)) {
                    return Err(BackendError::CommandNotSupported {
                        command: crate::capability::block_kind_name(bad),
                    });
                }
                // F-4: ensure the ACP CLI is awake before the wire write. idle_ttl=
                // None (default) → slot always Active → one uncontended lock, no
                // re-spawn (pre-F-4 parity). When suspended, re-spawn + replay the
                // session/load handshake first.
                self.suspend
                    .ensure_awake(aionui_common::now_ms(), || self.wake_handle())
                    .await?;
                // F-4: mark the turn in flight so the idle timer won't suspend the
                // ACP CLI mid-turn (the reader clears it at the prompt-response terminal).
                self.turn_in_flight.store(true, Ordering::SeqCst);
                let sid = self.bound_session().await?;
                let cur_gen = self.turn_gen.fetch_add(1, Ordering::SeqCst) + 1;
                let id = self.next_rpc_id();
                // Record the pending prompt so the reader synthesizes the terminal
                // from its response's stopReason (the ACP terminal path).
                self.pending_prompts
                    .lock()
                    .await
                    .insert(id, PendingPrompt { turn_gen: cur_gen });
                // Wave 0c-F: on the FIRST send, prepend the preset `[Assistant Rules]`
                // block as a leading text block (ACP has no system-prompt field).
                // Drained once via take() so later turns are unaffected.
                let mut prompt = build_prompt_blocks(&content);
                if let Some(preamble) = self.pending_preamble.lock().await.take() {
                    prompt.insert(0, json!({ "type": "text", "text": preamble }));
                }
                self.write_frame(json!({
                    "jsonrpc": "2.0", "id": id, "method": "session/prompt",
                    "params": { "sessionId": sid, "prompt": prompt }
                }))
                .await?;
                // PromptAccepted is Synthesized: the prompt is on the wire, so the
                // conversation's pending queue can drain now (the terminal arrives
                // later via the response). Only when a client_msg_id was supplied.
                if let Some(cmid) = metadata.client_msg_id {
                    emit(
                        &self.event_tx,
                        &self.session_id,
                        cur_gen,
                        SessionEvent::PromptAccepted { client_msg_id: cmid },
                    );
                }
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::Started,
                    turn_gen: cur_gen,
                })
            }
            Command::Cancel { target } => {
                if let CancelTarget::Tool(_) = target {
                    return Err(BackendError::CommandNotSupported { command: "cancel_tool" });
                }
                // ACP `session/cancel` is a NOTIFICATION (no id, no response). The
                // agent confirms by returning stopReason:cancelled on the in-flight
                // prompt response (→ the reader synthesizes TurnResult{Cancelled}).
                let sid = self.bound_session().await?;
                self.write_frame(json!({
                    "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": sid }
                }))
                .await?;
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            Command::SetMode { mode } => {
                // F-4: between-turn config write → wake a suspended session first.
                self.suspend
                    .ensure_awake(aionui_common::now_ms(), || self.wake_handle())
                    .await?;
                let sid = self.bound_session().await?;
                let id = self.next_rpc_id();
                // Register so the reader surfaces a JSON-RPC error response (e.g. an
                // invalid modeId) as a Notice{Error} instead of silently dropping it.
                self.pending_set.lock().await.insert(id, format!("mode→{mode}"));
                self.write_frame(json!({
                    "jsonrpc": "2.0", "id": id, "method": "session/set_mode",
                    "params": { "sessionId": sid, "modeId": mode }
                }))
                .await?;
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            Command::SetModel { model } => {
                // F-4: between-turn config write → wake a suspended session first.
                self.suspend
                    .ensure_awake(aionui_common::now_ms(), || self.wake_handle())
                    .await?;
                let sid = self.bound_session().await?;
                *self.current_model.lock().await = Some(model.clone());
                let id = self.next_rpc_id();
                // Register so the reader surfaces a JSON-RPC error response (e.g.
                // opencode `-32602 model not found` on a stale/invalid id) as a
                // Notice{Error} instead of silently reporting success (the prod bug).
                self.pending_set.lock().await.insert(id, format!("model→{model}"));
                self.write_frame(json!({
                    "jsonrpc": "2.0", "id": id, "method": "session/set_model",
                    "params": { "sessionId": sid, "modelId": model }
                }))
                .await?;
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            Command::AnswerPermission {
                request_id,
                decision,
                selected,
                answers: _, // ACP outcome is a single optionId; no per-question set
            } => {
                // Write the reverse-RPC RESPONSE keyed by the request id we surfaced
                // as Permission.request_id. ACP `RequestPermissionResponse.outcome` is
                // `{outcome:"selected", optionId}` (a REAL optionId the agent offered)
                // or `{outcome:"cancelled"}` (a client-side abort, NOT a deny).
                //
                // FIX (protocol audit): we must echo a REAL optionId from the offered
                // set, chosen by KIND — NOT a hardcoded "allow_once"/"cancelled" (every
                // bridge rejected the fabricated allow_once → "Always Allow" silently
                // denied the tool; "cancelled" routed a DENY to the abort path, not a
                // clean reject). The offered options were stashed at request time.
                let id: Value = serde_json::from_str(&request_id).unwrap_or(Value::String(request_id.clone()));
                let offered = self.pending_perm_options.lock().await.remove(&request_id);
                // Want-kind by decision: AllowAlways→allow_always (fallback allow_once),
                // Approved→allow_once, Denied→reject_once (fallback reject_always).
                let pick_by_kind = |offered: &[(String, String)], wants: &[&str]| -> Option<String> {
                    wants
                        .iter()
                        .find_map(|w| offered.iter().find(|(_, kind)| kind == w).map(|(oid, _)| oid.clone()))
                };
                let outcome = if let Some(sel) = selected {
                    // An explicit user pick (pick-one card) — the conversation already
                    // resolved it to a real offered optionId.
                    json!({ "outcome": "selected", "optionId": sel })
                } else if let Some(offered) = offered.as_deref() {
                    let chosen = match decision {
                        PermissionDecision::AllowAlways => pick_by_kind(offered, &["allow_always", "allow_once"]),
                        PermissionDecision::Approved => pick_by_kind(offered, &["allow_once", "allow_always"]),
                        PermissionDecision::Denied => pick_by_kind(offered, &["reject_once", "reject_always"]),
                    };
                    match chosen {
                        Some(oid) => json!({ "outcome": "selected", "optionId": oid }),
                        // No matching option offered → cancelled is the only honest fallback.
                        None => json!({ "outcome": "cancelled" }),
                    }
                } else {
                    // No options were captured (defensive — shouldn't happen for a real
                    // request_permission). Approve→cancelled would be wrong; only deny/
                    // unknown falls back to cancelled.
                    json!({ "outcome": "cancelled" })
                };
                self.write_frame(json!({
                    "jsonrpc": "2.0", "id": id, "result": { "outcome": outcome }
                }))
                .await?;
                // 009 RA -1 (codex·ACP symmetry): the reducer leaves requires-action
                // ONLY on PermissionResolved. ACP previously wrote the wire response
                // but never broadcast PermissionResolved, so `waiting_on_approval`
                // stayed +1 and can_send was stuck false until the whole turn folded
                // to Idle (and with multiple pending permissions the count could never
                // reach 0). Mirror the claude/codex peers. The ACP reverse-RPC response
                // IS the resolve (no separate out-of-band resolved notification), so
                // emit exactly once here, keyed Tool (session/request_permission is the
                // only Permission source; Auth goes through connection-level authenticate).
                let cur_gen = self.turn_gen.load(Ordering::SeqCst);
                emit(
                    &self.event_tx,
                    &self.session_id,
                    cur_gen,
                    SessionEvent::PermissionResolved {
                        request_id: request_id.clone(),
                        kind: PermissionKind::Tool,
                    },
                );
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: cur_gen,
                })
            }
            Command::Acknowledge { .. } => {
                // Conversation-side fold (done-unseen → seen). No ACP wire. Accept.
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            // ACP base wire has no equivalent → reject (cap advertises false).
            Command::Steer { .. } => Err(BackendError::CommandNotSupported { command: "steer" }),
            // G4: ACP exposes generic config options (e.g. claude-acp `effort`,
            // category `thought_level`) via `session/set_config_option {sessionId,
            // optionId, value}` (the SDK method, written as raw JSON-RPC). mode/model
            // still go through their dedicated set_mode/set_model arms (the conversation
            // routes id=="mode"/"model" there); anything else (effort/custom) lands
            // here instead of being rejected. Gated on the agent ADVERTISING the option
            // (cap-behavior invariant: an agent that discovered no config options
            // rejects, so the surface stays honest).
            Command::SetConfigOption { option_id, value } => {
                if !self.has_config_option(&option_id) {
                    return Err(BackendError::CommandNotSupported {
                        command: "set_config_option",
                    });
                }
                self.suspend
                    .ensure_awake(aionui_common::now_ms(), || self.wake_handle())
                    .await?;
                let sid = self.bound_session().await?;
                let id = self.next_rpc_id();
                // Register the pending set so the reader surfaces a JSON-RPC ERROR
                // response (e.g. an invalid option value) as a Notice{Warning} instead
                // of silently dropping it — same visibility set_mode/set_model already
                // have. The label kind is `config:<optionId>` (NOT mode/model), so the
                // reader's success branch emits no ConfigChanged: generic config options
                // have no field on the ConfigChanged event (it is mode/model-only), and
                // their convergence rides the agent's own config_option_update echo +
                // the discovered-options refresh — only the FAILURE path needed wiring.
                self.pending_set
                    .lock()
                    .await
                    .insert(id, format!("config:{option_id}\u{2192}{value}"));
                self.write_frame(json!({
                    "jsonrpc": "2.0", "id": id, "method": "session/set_config_option",
                    "params": { "sessionId": sid, "optionId": option_id, "value": value }
                }))
                .await?;
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            // ACP has no structured-question channel (elicitation unimplemented,
            // 0/24 live agents emit it — 2026-08-04 sweep); Ask is never raised
            // here, so an answer has nothing to land on.
            Command::AnswerAsk { .. } => Err(BackendError::CommandNotSupported { command: "answer_ask" }),
            Command::AnswerAuth { method_id, .. } => {
                // D2: only when the agent advertised authMethods at initialize
                // (answer_auth cap dynamically true) do we honor this. ACP auth is a
                // CONNECTION-level `authenticate` request keyed by methodId — NOT a
                // reverse-RPC response (unlike AnswerPermission). The credentials are
                // supplied out-of-band by the chosen method (hermes bedrock = ambient
                // runtime creds; hermes-setup = interactive terminal), so we send only
                // the methodId. (K2 — whether an in-flight turn can resume after a
                // mid-session re-auth — is unresolved; this connection-level write is
                // the open-time / between-turn auth the cap honestly advertises.)
                if !self.capabilities().supported_commands.answer_auth {
                    return Err(BackendError::CommandNotSupported { command: "answer_auth" });
                }
                self.suspend
                    .ensure_awake(aionui_common::now_ms(), || self.wake_handle())
                    .await?;
                let id = self.next_rpc_id();
                self.write_frame(json!({
                    "jsonrpc": "2.0", "id": id, "method": "authenticate",
                    "params": { "methodId": method_id }
                }))
                .await?;
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            Command::Rewind { .. } => Err(BackendError::CommandNotSupported { command: "rewind" }),
            Command::ListCheckpoints => Err(BackendError::CommandNotSupported {
                command: "list_checkpoints",
            }),
            // ACP has no cumulative usage/cost query wire → reject (cap=false).
            Command::QuerySessionInfo { .. } => Err(BackendError::CommandNotSupported {
                command: "query_session_info",
            }),
        }
    }

    fn events(&self) -> BoxStream<'static, SessionEnvelope> {
        let rx = self.event_tx.subscribe();
        futures_util::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(env) => return Some((env, rx)),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
        .boxed()
    }

    fn capabilities(&self) -> Capabilities {
        // Merge the reader-discovered models/modes (from the session/new|load
        // response) into the open-time base snapshot. Read-only sync lock — no
        // await (parity with codex's capabilities()).
        let mut caps = self.capabilities.clone();
        let disc = self.discovered.lock().unwrap_or_else(|e| e.into_inner());
        if !disc.models.is_empty() {
            caps.available_models = disc.models.clone();
        }
        if !disc.modes.is_empty() {
            caps.available_modes = disc.modes.clone();
        }
        if disc.current_model.is_some() {
            caps.current_model = disc.current_model.clone();
        }
        if disc.current_mode.is_some() {
            caps.current_mode = disc.current_mode.clone();
        }
        // Auth methods advertised in the initialize response (D2). When the agent
        // advertises any (hermes), surface them AND flip `answer_auth` true so the
        // cap-behavior invariant holds (advertised ⟺ dispatch does not reject). claude
        // ACP advertises none → stays empty + answer_auth false (honest, unchanged).
        if !disc.auth_methods.is_empty() {
            caps.auth_methods = disc.auth_methods.clone();
            caps.supported_commands.answer_auth = true;
        }
        // #101: merge the discovered slash commands (available_commands_update).
        if !disc.slash_commands.is_empty() {
            caps.slash_commands = disc.slash_commands.clone();
        }
        caps
    }
}

impl AcpSessionBackend {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl Drop for AcpSessionBackend {
    /// M5: abort the live reader (via the controller's mirrored AbortHandle, no
    /// await) so its `Arc<dyn AgentIo>` clone is released and the ACP subprocess is
    /// reaped (kill_on_drop). ACP CLIs/bridges are persistent (stdout never EOFs
    /// mid-session), so without this the reader would block forever on
    /// `next_line()`, orphaning the child. Also stop the idle timer if running.
    fn drop(&mut self) {
        self.suspend.abort_on_drop();
        if let Some(timer) = &self.idle_timer {
            timer.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{McpServerSpec, McpTransport};
    use crate::testing::FakeAgentIo;

    /// PROPERTY (§F.3 input field-value boundary for the acp `map_update` entry,
    /// sibling of codex `prop_map_item_*` and claude `prop_parse_assistant_*`): for
    /// ANY `session/update` params shape — arbitrary `sessionUpdate` kind (known /
    /// unknown / absent) and arbitrary payload — `map_update`:
    ///   1. NEVER panics (the ACP update is matched on the kind STRING, never a closed
    ///      SDK enum, so future/malformed variants are data not a crash — A1);
    ///   2. an UNKNOWN kind → exactly an `AdapterSpecific{tag:"acp_update:<kind>"}`.
    ///
    /// Sweeps the kind value space directly (map_update is a near-pure async fn over
    /// &Value + two default-constructible state holders).
    #[test]
    fn prop_map_update_never_panics_unknown_kind_is_adapter_specific() {
        use proptest::prelude::*;
        const KNOWN: &[&str] = &[
            "agent_message_chunk",
            "agent_thought_chunk",
            "tool_call",
            "tool_call_update",
            "usage_update",
            "current_mode_update",
            "current_model_update",
            "available_commands_update",
            "plan",
            "session_info_update",
        ];
        let unknown = "[a-z][a-z_]{0,14}".prop_filter("unknown", |s| !KNOWN.contains(&s.as_str()));
        let kind_strat = prop_oneof![
            Just(None),
            prop::sample::select(KNOWN).prop_map(|s| Some(s.to_string())),
            unknown.prop_map(Some),
        ];
        let rt = tokio::runtime::Runtime::new().unwrap();

        proptest!(|(kind in kind_strat)| {
            let mut update = serde_json::json!({"blob": 7});
            if let Some(k) = &kind { update["sessionUpdate"] = serde_json::Value::String(k.clone()); }
            let params = serde_json::json!({"sessionId": "s", "update": update});
            let model = Arc::new(Mutex::new(None));
            let disc = Arc::new(std::sync::Mutex::new(Discovered::default()));

            let events = rt.block_on(map_update(&params, &model, &disc)); // (1) must not panic

            if let Some(k) = &kind
                && !k.is_empty()
                && !KNOWN.contains(&k.as_str())
            {
                prop_assert!(
                    events.iter().any(|e| matches!(
                        e,
                        SessionEvent::AdapterSpecific { tag, .. } if tag == &format!("acp_update:{k}")
                    )),
                    "unknown sessionUpdate {k:?} must surface as AdapterSpecific, got {events:?}"
                );
            }
        });
    }

    // ── Wave 0c: MCP injection into session/new + session/load ──
    //
    // The pre-0c regression: both params functions hardcoded `mcpServers: []`, so
    // a clean-slate ACP session dropped EVERY user/guide/team MCP server (a fresh
    // session AND a resumed one). These pin the fix: the resolved MCP servers from
    // SessionConfig.init are serialized into the session/new + session/load frames,
    // byte-identical to the ACP SDK `McpServer` wire shape (verified empirically:
    // Stdio is untagged {name,command,args,env:[{name,value}]}).

    fn cfg_with_mcp() -> SessionConfig {
        SessionConfig {
            cwd: Some("/work".into()),
            init: crate::backend::SessionInit {
                mcp_servers: vec![
                    McpServerSpec {
                        name: "fs".into(),
                        transport: McpTransport::Stdio {
                            command: "/usr/bin/node".into(),
                            args: vec!["server.js".into()],
                            env: vec![("TOKEN".into(), "x".into())],
                        },
                    },
                    McpServerSpec {
                        name: "remote".into(),
                        transport: McpTransport::Http {
                            url: "https://mcp.example/api".into(),
                            headers: vec![("Authorization".into(), "Bearer y".into())],
                        },
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn new_session_params_injects_resolved_mcp_servers() {
        let params = new_session_params(&cfg_with_mcp());
        assert_eq!(params["cwd"], "/work");
        let servers = params["mcpServers"].as_array().expect("mcpServers array");
        assert_eq!(
            servers.len(),
            2,
            "both resolved servers are injected (was hardcoded [])"
        );
        // Stdio: untagged {name,command,args,env:[{name,value}]} — SDK byte-parity.
        assert_eq!(servers[0]["name"], "fs");
        assert_eq!(servers[0]["command"], "/usr/bin/node");
        assert_eq!(servers[0]["args"][0], "server.js");
        assert_eq!(servers[0]["env"][0]["name"], "TOKEN");
        assert_eq!(servers[0]["env"][0]["value"], "x");
        assert!(servers[0].get("type").is_none(), "Stdio is untagged (no type field)");
        // Http: {type:http,name,url,headers:[{name,value}]}.
        assert_eq!(servers[1]["type"], "http");
        assert_eq!(servers[1]["url"], "https://mcp.example/api");
        assert_eq!(servers[1]["headers"][0]["name"], "Authorization");
    }

    #[test]
    fn parse_generic_config_option_ids_excludes_mode_and_model() {
        // Wire-pinned to the claude-acp 0.33.1 shape (mode + model + effort).
        let opts = json!([
            { "id": "mode", "category": "mode", "type": "select", "currentValue": "default", "options": [] },
            { "id": "model", "category": "model", "type": "select", "currentValue": "opus", "options": [] },
            { "id": "effort", "category": "thought_level", "type": "select", "currentValue": "xhigh", "options": [] },
        ]);
        let ids = parse_generic_config_option_ids(Some(&opts));
        assert_eq!(ids, vec!["effort"], "mode/model excluded (dedicated arms); effort kept");
        assert!(parse_generic_config_option_ids(None).is_empty());
        // Only mode/model → no generic options.
        let only_mm = json!([{ "id": "mode" }, { "id": "model" }]);
        assert!(parse_generic_config_option_ids(Some(&only_mm)).is_empty());
    }

    /// opencode 1.16.2: model/mode are inside `configOptions[]` (NOT top-level), each
    /// `{id, currentValue, options:[{value,name,description}]}`. `config_option_select`
    /// extracts the options + currentValue so `handle_open_response` can fill
    /// disc.models/modes (the fix for "config-options empty → set_model -32602").
    #[test]
    fn config_option_select_extracts_opencode_model_and_mode() {
        // The LIVE opencode shape (value = provider-prefixed full id).
        let opts = json!([
            { "id": "model", "category": "model", "type": "select", "currentValue": "opencode/big-pickle",
              "options": [
                { "value": "amazon-bedrock/anthropic.claude-opus-4-8", "name": "Claude Opus 4.8" },
                { "value": "amazon-bedrock/openai.gpt-5.5", "name": "GPT-5.5", "description": "fast" },
              ]},
            { "id": "mode", "category": "mode", "type": "select", "currentValue": "build",
              "options": [
                { "value": "build", "name": "build" },
                { "value": "plan", "name": "plan", "description": "Plan mode." },
              ]},
        ]);
        let (models, cur_model) = config_option_select(Some(&opts), "model").expect("model option present");
        assert_eq!(
            cur_model.as_deref(),
            Some("opencode/big-pickle"),
            "currentValue → current_model"
        );
        assert_eq!(models.len(), 2);
        assert_eq!(
            models[1].0, "amazon-bedrock/openai.gpt-5.5",
            "value → id (the token set_model sends back)"
        );
        assert_eq!(models[1].1, "GPT-5.5");
        assert_eq!(models[1].2.as_deref(), Some("fast"));

        let (modes, cur_mode) = config_option_select(Some(&opts), "mode").expect("mode option present");
        assert_eq!(cur_mode.as_deref(), Some("build"));
        assert_eq!(
            modes.iter().map(|m| m.0.as_str()).collect::<Vec<_>>(),
            vec!["build", "plan"]
        );

        // Absent / wrong shape → None (so the top-level path / empty fallback applies).
        assert!(config_option_select(Some(&opts), "effort").is_none());
        assert!(config_option_select(None, "model").is_none());
    }

    /// handle_open_response dual-shape: an opencode-style session/new (model/mode in
    /// configOptions[], NO top-level models/modes) fills disc.models/modes + current_*,
    /// AND a claude-acp-style top-level result still parses (regression). This is the
    /// end-to-end fix for the opencode "empty config-options" prod bug.
    #[tokio::test]
    async fn handle_open_response_parses_both_opencode_configoptions_and_toplevel() {
        use std::sync::Mutex as StdMutex;
        let mk = || {
            (
                Arc::new(AtomicU64::new(1)),
                broadcast::channel::<SessionEnvelope>(64).0,
                Arc::new(Mutex::new(None::<String>)),
                Arc::new(StdMutex::new(Discovered::default())),
            )
        };

        // (1) opencode shape: configOptions[] carries model + mode, no top-level keys.
        let (tg, tx, sid, disc) = mk();
        let opencode_result = json!({
            "sessionId": "oc-1",
            "configOptions": [
                { "id": "model", "currentValue": "opencode/big-pickle",
                  "options": [{ "value": "amazon-bedrock/openai.gpt-5.5", "name": "GPT-5.5" }] },
                { "id": "mode", "currentValue": "build",
                  "options": [{ "value": "build", "name": "build" }, { "value": "plan", "name": "plan" }] },
            ]
        });
        handle_open_response(
            &opencode_result,
            "s",
            &tg,
            &tx,
            &sid,
            &Arc::new(Mutex::new(None)),
            &disc,
        )
        .await;
        {
            let d = disc.lock().unwrap();
            assert_eq!(d.models.len(), 1, "opencode model extracted from configOptions");
            assert_eq!(d.models[0].id, "amazon-bedrock/openai.gpt-5.5");
            assert_eq!(d.current_model.as_deref(), Some("opencode/big-pickle"));
            assert_eq!(
                d.modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
                vec!["build", "plan"]
            );
            assert_eq!(d.current_mode.as_deref(), Some("build"));
        }

        // (2) claude-acp shape: top-level models/modes (regression — must still parse).
        let (tg, tx, sid, disc) = mk();
        let claude_result = json!({
            "sessionId": "cl-1",
            "models": { "currentModelId": "opus", "availableModels": [{ "modelId": "opus", "name": "Opus" }] },
            "modes": { "currentModeId": "default", "availableModes": [{ "id": "default", "name": "Default" }] },
        });
        handle_open_response(&claude_result, "s", &tg, &tx, &sid, &Arc::new(Mutex::new(None)), &disc).await;
        {
            let d = disc.lock().unwrap();
            assert_eq!(d.models[0].id, "opus", "top-level model still parses");
            assert_eq!(d.current_model.as_deref(), Some("opus"));
            assert_eq!(d.modes[0].id, "default");
        }
    }

    #[tokio::test]
    async fn set_config_option_writes_frame_only_for_advertised_generic_option() {
        let fake = FakeAgentIo::never_exits(Vec::new());
        let captured = fake.captured_stdin();
        let backend = AcpSessionBackend::build_with_io("s", Box::new(fake)).await;
        backend.bind_for_test("acp-sid").await;
        // An UNADVERTISED option rejects (cap-behavior invariant: advertised ⟺ settable).
        let rejected = backend
            .dispatch(Command::SetConfigOption {
                option_id: "effort".into(),
                value: "low".into(),
            })
            .await;
        assert!(
            matches!(rejected, Err(BackendError::CommandNotSupported { command }) if command == "set_config_option"),
            "an unadvertised option must reject, got {rejected:?}"
        );
        // Discover `effort` (as the session/new response would) → now it is settable.
        {
            let mut disc = backend.discovered.lock().unwrap_or_else(|e| e.into_inner());
            disc.config_options = vec!["effort".into()];
        }
        backend
            .dispatch(Command::SetConfigOption {
                option_id: "effort".into(),
                value: "low".into(),
            })
            .await
            .expect("advertised option dispatches");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let raw = String::from_utf8(captured.lock().await.clone()).unwrap();
        let frame = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|f| f["method"] == "session/set_config_option")
            .expect("a session/set_config_option frame was written");
        assert_eq!(frame["params"]["optionId"], "effort");
        assert_eq!(frame["params"]["value"], "low");
        assert_eq!(frame["params"]["sessionId"], "acp-sid");
    }

    /// Acknowledge (user-ack of a done-unseen turn) has NO ACP wire — it folds at
    /// the conversation read layer. It must accept as NoTurn and write nothing.
    /// (acp was the only backend whose Acknowledge arm lacked a dedicated test;
    /// claude/codex already have one — closes acp's per-arm dispatch coverage.)
    #[tokio::test]
    async fn dispatch_acknowledge_is_local_noop_no_wire() {
        let fake = FakeAgentIo::never_exits(Vec::new());
        let captured = fake.captured_stdin();
        let backend = AcpSessionBackend::build_with_io("s", Box::new(fake)).await;
        backend.bind_for_test("acp-sid").await;

        let receipt = backend
            .dispatch(Command::Acknowledge {
                node_id: "node-1".into(),
            })
            .await
            .expect("Acknowledge is always accepted (never CommandNotSupported)");
        assert_eq!(
            receipt.admission,
            Admission::NoTurn,
            "Acknowledge folds at the conversation layer; it must not open a turn"
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let raw = String::from_utf8(captured.lock().await.clone()).unwrap();
        assert!(
            raw.is_empty() || !raw.contains(r#""method""#),
            "Acknowledge must write NO ACP request frame, got: {raw}"
        );
    }

    #[test]
    fn reconcile_plan_aligns_only_mismatched_advertised_dimensions() {
        // Both differ + advertised → both set commands, mode before model.
        let plan = reconcile_plan(true, Some("plan"), Some("default"), true, Some("opus"), Some("sonnet"));
        assert_eq!(plan.len(), 2);
        assert!(matches!(&plan[0], Command::SetMode { mode } if mode == "plan"));
        assert!(matches!(&plan[1], Command::SetModel { model } if model == "opus"));

        // Observed already matches desired → no command (idempotent re-open).
        assert!(reconcile_plan(true, Some("plan"), Some("plan"), true, Some("opus"), Some("opus")).is_empty());

        // Only the model drifted → only SetModel.
        let plan = reconcile_plan(true, Some("plan"), Some("plan"), true, Some("opus"), Some("sonnet"));
        assert_eq!(plan.len(), 1);
        assert!(matches!(&plan[0], Command::SetModel { model } if model == "opus"));

        // Dimension NOT advertised → never reconciled, even on mismatch (a mode-less
        // agent must not get a spurious set_mode).
        assert!(
            reconcile_plan(
                false,
                Some("plan"),
                Some("default"),
                false,
                Some("opus"),
                Some("sonnet")
            )
            .is_empty()
        );

        // No desired value → nothing to align to (the agent's own default stands).
        assert!(reconcile_plan(true, None, Some("default"), true, None, Some("sonnet")).is_empty());

        // Advertised + desired but observed=None (agent reported no current) → still
        // align to desired (the resumed agent came up without echoing a current).
        let plan = reconcile_plan(true, Some("plan"), None, false, None, None);
        assert_eq!(plan.len(), 1);
        assert!(matches!(&plan[0], Command::SetMode { mode } if mode == "plan"));
    }

    #[test]
    fn enrich_acp_error_appends_stderr_cause_for_generic_message_only() {
        let stderr = "ERROR codex_acp::thread: You've hit your usage limit, try again later";
        // Generic message → enriched with the allowlisted stderr cause.
        let enriched = enrich_acp_error_message("internal error", Some(stderr));
        assert!(
            enriched.starts_with("internal error ("),
            "generic message is enriched; got {enriched}"
        );
        assert!(
            enriched.contains("usage limit"),
            "the allowlisted cause is appended; got {enriched}"
        );
        // Specific message → unchanged (no spurious enrichment).
        let specific = enrich_acp_error_message("Tool 'foo' is not allowed in plan mode", Some(stderr));
        assert_eq!(specific, "Tool 'foo' is not allowed in plan mode");
        // Generic message but no allowlisted stderr → unchanged (S0 returns None).
        let no_cause = enrich_acp_error_message("internal error", Some("DEBUG some unrelated line"));
        assert_eq!(no_cause, "internal error");
        // No stderr at all → unchanged.
        assert_eq!(enrich_acp_error_message("internal error", None), "internal error");
    }

    #[test]
    fn is_generic_error_message_classifies_known_generic_strings() {
        assert!(is_generic_error_message("internal error"));
        assert!(is_generic_error_message("Internal Server Error"));
        assert!(is_generic_error_message("ACP prompt failed"));
        assert!(is_generic_error_message("  unknown  "));
        assert!(is_generic_error_message(""));
        assert!(!is_generic_error_message("rate limit exceeded"));
        assert!(!is_generic_error_message("session not found"));
    }

    #[test]
    fn synth_turn_result_error_frame_carries_enriched_message() {
        // A generic JSON-RPC error response + an allowlisted stderr cause → the
        // error TurnResult's result_text surfaces the cause (G1-B end-to-end).
        let frame = json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32603, "message": "internal error" } });
        let stderr = "ERROR codex_acp: connection refused while contacting upstream";
        let ev = synth_turn_result(&frame, 3, Some(stderr));
        match ev {
            SessionEvent::TurnResult {
                is_error, result_text, ..
            } => {
                assert!(is_error, "an error frame is a failed terminal");
                assert!(
                    result_text.contains("connection refused"),
                    "stderr cause enriched in; got {result_text}"
                );
            }
            other => panic!("expected TurnResult, got {other:?}"),
        }
        // A success frame ignores stderr entirely (no enrichment, no peek needed).
        let ok = json!({ "jsonrpc": "2.0", "id": 1, "result": { "stopReason": "end_turn" } });
        match synth_turn_result(&ok, 3, None) {
            SessionEvent::TurnResult { is_error, .. } => assert!(!is_error),
            other => panic!("expected TurnResult, got {other:?}"),
        }
    }

    #[test]
    fn parse_permission_metadata_extracts_claude_title_prefix_server() {
        // Claude-acp shape: server name rides the `mcp__<server>__<tool>` title.
        let params = json!({
            "options": [
                { "kind": "allow_always", "optionId": "allow_always", "name": "Always Allow" },
                { "kind": "reject_once", "optionId": "reject" },
            ],
            "toolCall": { "title": "mcp__aionui-team__team_members", "rawInput": {} },
        });
        let meta = parse_permission_metadata(Some(&params)).expect("metadata present");
        assert_eq!(meta["server_name"], "aionui-team");
        assert_eq!(meta["options"][0]["option_id"], "allow_always");
        assert_eq!(meta["options"][0]["kind"], "allow_always");
        // CT-PERM-OPTIONS: the human label rides through for the card.
        assert_eq!(meta["options"][0]["name"], "Always Allow");
        assert_eq!(
            meta["options"][1]["name"], "",
            "missing name → empty (finalizer falls back to id)"
        );
    }

    #[test]
    fn parse_permission_metadata_extracts_codex_raw_input_server() {
        // Codex-acp shape: server name in toolCall.rawInput.server_name (wins over title).
        let params = json!({
            "options": [{ "kind": "allow_once", "optionId": "allow" }],
            "toolCall": { "title": "mcp__other__x", "rawInput": { "server_name": "aionui-team-guide" } },
        });
        let meta = parse_permission_metadata(Some(&params)).expect("metadata present");
        assert_eq!(meta["server_name"], "aionui-team-guide", "rawInput.server_name wins");
    }

    #[test]
    fn parse_permission_metadata_none_when_no_server_and_no_options() {
        // A non-MCP tool with no parseable options → None (a human decides; no card label).
        let params = json!({ "toolCall": { "title": "Write file.txt", "rawInput": {} } });
        assert!(parse_permission_metadata(Some(&params)).is_none());
        assert!(parse_permission_metadata(None).is_none());
    }

    #[test]
    fn load_session_params_reinjects_mcp_on_resume() {
        // RESUME REGRESSION FIX: session/load must carry the SAME servers as
        // session/new, else a resumed conversation silently loses all MCP tools.
        let params = load_session_params("acp-sid-123", &cfg_with_mcp());
        assert_eq!(params["sessionId"], "acp-sid-123");
        assert_eq!(params["cwd"], "/work");
        let servers = params["mcpServers"].as_array().expect("mcpServers array");
        assert_eq!(servers.len(), 2, "resume re-injects MCP (was hardcoded [])");
        assert_eq!(servers[0]["name"], "fs");
    }

    #[test]
    fn session_params_empty_mcp_is_byte_identical_to_pre_0c() {
        // Default (no init) → mcpServers:[] — the pre-0c handshake is unchanged for
        // conversations with no MCP configured.
        let cfg = SessionConfig::default();
        assert_eq!(new_session_params(&cfg)["mcpServers"].as_array().unwrap().len(), 0);
        assert_eq!(
            load_session_params("s", &cfg)["mcpServers"].as_array().unwrap().len(),
            0
        );
    }

    /// M5 (codex/claude parity): dropping the backend aborts the reader so a
    /// hung/persistent ACP process (no stdout EOF) is reaped. Inline because it
    /// inspects the private `reader` handle.
    #[tokio::test]
    async fn dropping_backend_aborts_reader() {
        let backend = AcpSessionBackend::build_with_io("s", Box::new(FakeAgentIo::never_exits(Vec::new()))).await;
        let handle = backend
            .suspend
            .current_abort_handle()
            .expect("live reader has an abort handle");
        assert!(!handle.is_finished(), "reader live (blocked on read) before drop");
        drop(backend);
        for _ in 0..40 {
            if handle.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            handle.is_finished(),
            "dropping the backend aborts the reader (M5 parity)"
        );
    }

    /// Wave 0c-F: the preset `[Assistant Rules]` block is prepended to the FIRST
    /// `session/prompt` (ACP has no system-prompt field), and ONLY the first — a
    /// second turn carries the user content verbatim. Drives two Sends over a
    /// captured-stdin FakeAgentIo and parses the written prompt frames.
    #[tokio::test]
    async fn preset_preamble_prepended_to_first_prompt_only() {
        let fake = FakeAgentIo::never_exits(Vec::new());
        let captured = fake.captured_stdin();
        let backend = AcpSessionBackend::build_with_io("s", Box::new(fake)).await;
        backend.bind_for_test("acp-sid").await; // so bound_session() resolves
        backend
            .set_pending_preamble_for_test("[Assistant Rules]\nBe terse.\n[/Assistant Rules]")
            .await;

        for text in ["first", "second"] {
            backend
                .dispatch(Command::Send {
                    content: vec![ContentBlock::Text(text.into())],
                    metadata: super::super::types::CommandMeta::default(),
                })
                .await
                .expect("send dispatched");
        }

        // Parse the two session/prompt frames out of the captured stdin. Give the
        // async frame writes a beat to flush into the captured buffer.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let raw = String::from_utf8(captured.lock().await.clone()).unwrap();
        let prompts: Vec<serde_json::Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|f| f["method"] == "session/prompt")
            .collect();
        assert_eq!(prompts.len(), 2, "two prompts written, got raw: {raw}");

        // First prompt: leading [Assistant Rules] text block, THEN the user text.
        let first = prompts[0]["params"]["prompt"].as_array().unwrap();
        assert_eq!(first[0]["type"], "text");
        assert!(
            first[0]["text"].as_str().unwrap().contains("[Assistant Rules]"),
            "first prompt leads with the preset block, got {:?}",
            first[0]
        );
        assert_eq!(first[1]["text"], "first", "user content follows the preamble");

        // Second prompt: NO preamble — user content is the first block.
        let second = prompts[1]["params"]["prompt"].as_array().unwrap();
        assert_eq!(second[0]["text"], "second", "second turn has no preamble");
        assert!(
            !second
                .iter()
                .any(|b| b["text"].as_str().is_some_and(|t| t.contains("[Assistant Rules]"))),
            "the preamble is applied exactly once"
        );
    }

    /// G6 e2e: after open, a backend whose DESIRED mode/model differ from the agent's
    /// OBSERVED current (a resumed agent comes up at its own default) re-aligns by
    /// writing `session/set_mode` + `session/set_model` for the desired values. The
    /// idempotent case (observed already matches) writes nothing — proven separately
    /// in `reconcile_plan`'s unit test.
    #[tokio::test]
    async fn startup_reconcile_aligns_drifted_mode_and_model() {
        let fake = FakeAgentIo::never_exits(Vec::new());
        let captured = fake.captured_stdin();
        let backend =
            AcpSessionBackend::build_with_io_and_desired("s", Box::new(fake), Some("plan".into()), Some("opus".into()))
                .await;
        backend.bind_for_test("acp-sid").await; // so dispatch(SetMode/SetModel)'s bound_session resolves
        // The agent came up at its OWN default (drifted from desired).
        backend
            .seed_observed_for_test(Some("default".into()), Some("sonnet".into()))
            .await;
        // pending_open already None (build_with_io path) → reconcile sees the seeded observed.
        let backend = Arc::new(backend);
        backend.reconcile_startup_config().await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let raw = String::from_utf8(captured.lock().await.clone()).unwrap();
        let set_mode = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|f| f["method"] == "session/set_mode")
            .expect("a session/set_mode was written");
        assert_eq!(set_mode["params"]["modeId"], "plan", "reconciled to the DESIRED mode");
        let set_model = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|f| f["method"] == "session/set_model")
            .expect("a session/set_model was written");
        assert_eq!(
            set_model["params"]["modelId"], "opus",
            "reconciled to the DESIRED model"
        );
    }

    /// G6 e2e (negative): when the agent's OBSERVED current already matches DESIRED,
    /// the startup reconcile writes NOTHING (no spurious set_*). The complement of the
    /// drift test above — together they pin "align iff mismatch".
    #[tokio::test]
    async fn startup_reconcile_noop_when_already_aligned() {
        let fake = FakeAgentIo::never_exits(Vec::new());
        let captured = fake.captured_stdin();
        let backend =
            AcpSessionBackend::build_with_io_and_desired("s", Box::new(fake), Some("plan".into()), Some("opus".into()))
                .await;
        backend.bind_for_test("acp-sid").await;
        backend
            .seed_observed_for_test(Some("plan".into()), Some("opus".into()))
            .await;
        let backend = Arc::new(backend);
        backend.reconcile_startup_config().await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let raw = String::from_utf8(captured.lock().await.clone()).unwrap();
        assert!(
            !raw.contains("session/set_mode") && !raw.contains("session/set_model"),
            "already-aligned session must not write any set_*; got: {raw}"
        );
    }

    /// THE seam proof (codex parity): a real ACP turn — driven through
    /// `Orchestrator::run()` — locks during the turn and unlocks
    /// (`StateSnapshot.can_send=true`) at the terminal, with the SAME reducer/FSM
    /// codex+claude use. Critically this exercises ACP's UNIQUE terminal: the
    /// unlock comes from the `session/prompt` RESPONSE's `stopReason` (synthesized
    /// into a TurnResult by the reader), NOT a notification — proving that
    /// out-of-band terminal still folds through step() to the unlock.
    #[tokio::test]
    async fn acp_backend_folds_through_orchestrator_to_unlock() {
        use super::super::Orchestrator;
        use crate::state::SessionState;
        use futures_util::StreamExt as _;

        // GATED turn tail: an agent_message_chunk delta (substantive output) then
        // the session/prompt RESPONSE (id 1 — the first rpc id dispatch(Send)
        // mints) carrying stopReason:end_turn (the ACP terminal). Gated until
        // run() has subscribed + dispatch has registered pending_prompts[1].
        let tail = format!(
            "{}\n{}\n",
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"}}"#,
        )
        .into_bytes();
        let fake = FakeAgentIo::never_exits(Vec::new()).with_gated_tail(tail);
        let release = fake.stdout_releaser();
        let backend = AcpSessionBackend::build_with_io("sess-acp", Box::new(fake)).await;
        // Bind the ACP session id (the live path binds it from the session/new
        // response; build_with_io skips the handshake).
        backend.bind_for_test("acp-sid").await;

        let orch = std::sync::Arc::new(Orchestrator::new(256));
        let mut states = orch.subscribe_state("sess-acp");

        // send() dispatches Send (writes session/prompt, registers pending_prompts[1])
        // and lowers TurnStarted (Idle→Running, can_send=false).
        let receipt = orch
            .send(
                &backend,
                "sess-acp",
                vec![ContentBlock::Text("hi".into())],
                super::super::types::CommandMeta::default(),
            )
            .await
            .expect("send accepted");
        assert_eq!(receipt.admission, Admission::Started);

        let run = {
            let orch = orch.clone();
            tokio::spawn(async move { orch.run(&backend).await })
        };
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Open the gate so the scripted delta + prompt-response terminal drive in.
        release();

        let unlocked = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut saw_locked_running = false;
            while let Some(snap) = states.next().await {
                if snap.session_id != "sess-acp" {
                    continue;
                }
                if matches!(snap.state, SessionState::Running { .. }) && !snap.can_send {
                    saw_locked_running = true;
                }
                if matches!(snap.state, SessionState::Idle) && snap.can_send && saw_locked_running {
                    return true;
                }
            }
            false
        })
        .await
        .expect("must not hang");

        assert!(
            unlocked,
            "a real ACP turn folded through the orchestrator must lock during the turn and unlock \
             (can_send=true) at the session/prompt-response terminal"
        );
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), run).await;
    }

    /// 9a-ACP wire-out oracle: a connect-time `initialize` ERROR response (the
    /// agent rejects the handshake — e.g. NOT logged in) is no longer silently
    /// swallowed. The reader synthesizes an error `TurnResult` carrying the cause,
    /// with the allowlisted stderr auth reason ENRICHED into the generic JSON-RPC
    /// message (so the 9c classifier downstream → CheckAgentLogin). Pins the wire
    /// (the emitted event), not just internal state — the missing `else` arm was a
    /// silent-swallow that hung the first prompt opaquely.
    #[tokio::test]
    async fn connect_time_initialize_error_emits_enriched_error_terminal() {
        use futures_util::StreamExt as _;

        // The initialize ERROR response (id 1 — set as pending_init below). Generic
        // JSON-RPC message; the real auth cause is on stderr (the agent logs it
        // there without echoing it in the error), so it must be enriched in.
        let tail =
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32603,\"message\":\"internal error\"}}\n".to_vec();
        let fake = FakeAgentIo::never_exits(Vec::new())
            .with_gated_tail(tail)
            .with_stderr("ERROR claude_acp: Invalid API key · please run /login (unauthorized)");
        let release = fake.stdout_releaser();
        let backend = AcpSessionBackend::build_with_io("sess-init-err", Box::new(fake)).await;
        // The live path's run_handshake registers the initialize rpc id; build_with_io
        // skips the handshake, so register it here to mirror the live reader claim.
        backend.set_pending_init_for_test(1).await;

        let mut events = backend.events();
        // Subscribe BEFORE releasing so the synthesized terminal is observed.
        release();

        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::TurnResult { .. }) {
                    return Some(env.event);
                }
            }
            None
        })
        .await
        .expect("must not hang")
        .expect("a connect-time initialize error emits a TurnResult terminal");

        match ev {
            SessionEvent::TurnResult {
                is_error,
                result_text,
                outcome,
                ..
            } => {
                assert!(is_error, "connect-time initialize error is an error terminal");
                assert!(
                    matches!(outcome, TurnOutcome::Failed),
                    "outcome Failed, got {outcome:?}"
                );
                assert!(
                    result_text.contains("Invalid API key") || result_text.contains("unauthorized"),
                    "the allowlisted stderr auth cause is enriched into the message, got: {result_text}"
                );
            }
            other => panic!("expected TurnResult, got {other:?}"),
        }
    }

    /// 9a-ACP wire-out oracle (the `session/new` arm): a connect-time
    /// `session/new` ERROR (auth required / setup rejected) likewise synthesizes an
    /// error terminal instead of being swallowed, AND clears `pending_open` so a
    /// `bound_session()`/reconcile waiter unblocks immediately rather than spinning
    /// out its retry window.
    #[tokio::test]
    async fn connect_time_session_new_error_emits_error_terminal_and_clears_pending_open() {
        use futures_util::StreamExt as _;

        let tail =
            b"{\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32000,\"message\":\"authentication required\"}}\n"
                .to_vec();
        let fake = FakeAgentIo::never_exits(Vec::new()).with_gated_tail(tail);
        let release = fake.stdout_releaser();
        let backend = AcpSessionBackend::build_with_io("sess-open-err", Box::new(fake)).await;
        backend.set_pending_open_for_test(2).await;

        let mut events = backend.events();
        release();

        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::TurnResult { .. }) {
                    return Some(env.event);
                }
            }
            None
        })
        .await
        .expect("must not hang")
        .expect("a connect-time session/new error emits a TurnResult terminal");

        match ev {
            SessionEvent::TurnResult {
                is_error, result_text, ..
            } => {
                assert!(is_error, "connect-time session/new error is an error terminal");
                // Specific (non-generic) message is passed through verbatim (no stderr
                // enrichment needed — is_generic_error_message is false for it).
                assert!(
                    result_text.contains("authentication required"),
                    "the specific error message survives, got: {result_text}"
                );
            }
            other => panic!("expected TurnResult, got {other:?}"),
        }

        // pending_open cleared → a waiter (bound_session / reconcile) is unblocked.
        assert!(
            backend.pending_open.lock().await.is_none(),
            "the connect error clears pending_open so waiters don't spin out their window"
        );
    }

    /// A `session/set_model`|`set_mode` JSON-RPC ERROR response (e.g. opencode
    /// `-32602 model not found`) is surfaced as a `Notice{Warning}` (+ error log)
    /// instead of being silently dropped — the prod bug where a FAILED set was
    /// reported as a 200 command_ack and the selector never converged / no diagnosis.
    #[tokio::test]
    async fn acp_set_model_error_response_surfaces_notice_not_silent() {
        // A gated tail carrying the error response for the set's rpc id (7).
        let tail = concat!(
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32602,"message":"model not found: anthropic/claude-sonnet-4"}}"#,
            "\n",
        )
        .as_bytes()
        .to_vec();
        let fake = FakeAgentIo::never_exits(Vec::new()).with_gated_tail(tail);
        let release = fake.stdout_releaser();
        let backend = AcpSessionBackend::build_with_io("sess-set-err", Box::new(fake)).await;
        backend
            .set_pending_set_for_test(7, "model→anthropic/claude-sonnet-4")
            .await;

        let mut events = backend.events();
        release();

        let notice = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::Notice { level, message, .. } = env.event {
                    return Some((level, message));
                }
            }
            None
        })
        .await
        .expect("must not hang")
        .expect("a set error must surface a Notice (not be silently dropped)");
        assert_eq!(notice.0, crate::event::NoticeLevel::Warning);
        assert!(
            notice.1.contains("model→anthropic/claude-sonnet-4") && notice.1.contains("model not found"),
            "the Notice carries the label + agent error message, got: {}",
            notice.1
        );
        // pending_set entry consumed (no leak).
        assert!(
            backend.pending_set.lock().await.is_empty(),
            "the pending_set entry is claimed"
        );
    }

    /// Protocol-audit fix (MED): AnswerPermission must echo a REAL offered optionId
    /// chosen by KIND, not a hardcoded value. AllowAlways → the agent's allow_always
    /// optionId (was "allow_once" → bridge rejected → tool silently DENIED); Denied →
    /// the agent's reject_once optionId via {outcome:selected} (was {outcome:cancelled}
    /// → client-abort path, not a clean reject).
    #[tokio::test]
    async fn acp_answer_permission_echoes_real_offered_option_id_by_kind() {
        // A request_permission whose offered options use agent-specific ids ("ok"/"no")
        // distinct from the kinds — proves we pick by KIND, not by guessing the id.
        let req = concat!(
            r#"{"jsonrpc":"2.0","id":501,"method":"session/request_permission","params":{"#,
            r#""sessionId":"s","options":["#,
            r#"{"optionId":"ok","kind":"allow_once","name":"Allow"},"#,
            r#"{"optionId":"ok_always","kind":"allow_always","name":"Always Allow"},"#,
            r#"{"optionId":"no","kind":"reject_once","name":"Reject"}],"#,
            r#""toolCall":{"title":"Bash","rawInput":{"command":"rm -rf /"}}}}"#,
            "\n",
        )
        .as_bytes()
        .to_vec();
        let fake = FakeAgentIo::never_exits(Vec::new()).with_gated_tail(req);
        let captured = fake.captured_stdin();
        let release = fake.stdout_releaser();
        let backend = AcpSessionBackend::build_with_io("s", Box::new(fake)).await;
        let mut events = backend.events();
        release();

        // Wait for the Permission event so its request_id (the wire "501") is surfaced
        // and the offered options are stashed.
        let (req_id, perm_tool_name, perm_input) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::Permission {
                    request_id,
                    tool_name,
                    input,
                    ..
                } = env.event
                {
                    return Some((request_id, tool_name, input));
                }
            }
            None
        })
        .await
        .expect("must not hang")
        .expect("Permission surfaced");
        // AionUi issue #3779: the card must show what is being approved — the
        // toolCall's title and rawInput ride on the Permission event.
        assert_eq!(
            perm_tool_name.as_deref(),
            Some("Bash"),
            "toolCall.title rides as tool_name"
        );
        let perm_input = perm_input.expect("toolCall.rawInput rides as input");
        assert_eq!(perm_input["command"], "rm -rf /", "rawInput reaches the card verbatim");

        // AllowAlways → must echo the real allow_always optionId "ok_always".
        backend
            .dispatch(Command::AnswerPermission {
                request_id: req_id.clone(),
                decision: PermissionDecision::AllowAlways,
                selected: None,
                answers: Vec::new(),
            })
            .await
            .expect("AnswerPermission(AllowAlways) accepted");
        // captured_stdin is drained by a background task — poll until the frame lands.
        let written = {
            let mut s = String::new();
            for _ in 0..40 {
                s = String::from_utf8_lossy(&captured.lock().await.clone()).to_string();
                if s.contains("outcome") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            s
        };
        assert!(
            written.contains(r#""optionId":"ok_always""#) && written.contains(r#""outcome":"selected""#),
            "AllowAlways must echo the real allow_always optionId (ok_always), not a hardcoded id. got: {written}"
        );
        assert!(
            !written.contains(r#""optionId":"allow_once""#),
            "must NOT send the fabricated allow_once id (bridges reject it). got: {written}"
        );
    }

    /// Protocol-audit fix (MED): an image ContentBlock inside a tool_call_update
    /// content item must be decoded to ToolResultContent::Image, not dropped (the
    /// prior code read only `.text`, so an image-file Read / screenshot / vision tool
    /// lost its bytes — the false "ACP never inlines image bytes" claim).
    #[test]
    fn parse_acp_tool_content_carries_image_block_not_just_text() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]);
        let update = json!({
            "content": [
                { "type": "content", "content": { "type": "text", "text": "here is the screenshot" } },
                { "type": "content", "content": { "type": "image", "mimeType": "image/png", "data": b64 } },
            ]
        });
        let content = parse_acp_tool_content(&update);
        assert!(
            content
                .iter()
                .any(|c| matches!(c, crate::event::ToolResultContent::Text(t) if t.contains("screenshot"))),
            "text block still carried, got {content:?}"
        );
        assert!(
            content.iter().any(|c| matches!(c,
                crate::event::ToolResultContent::Image { media_type, data }
                    if media_type == "image/png" && data == &[1u8, 2, 3])),
            "image ContentBlock must decode to ToolResultContent::Image (was dropped), got {content:?}"
        );
    }

    /// F-4 default: build_with_io → idle_ttl=None → never suspends (no timer, slot
    /// Active for life). Protects the parse/dispatch contract from any F-4 cost.
    #[tokio::test]
    async fn f4_off_by_default_no_suspension() {
        let backend = AcpSessionBackend::build_with_io("s", Box::new(FakeAgentIo::never_exits(Vec::new()))).await;
        assert!(backend.idle_timer.is_none(), "no idle timer when idle_ttl is None");
        assert!(backend.suspend.is_active().await, "slot Active");
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert!(backend.suspend.is_active().await, "stays Active (production parity)");
    }

    /// F-4 suspend→wake: a configured idle_ttl suspends the idle ACP CLI; the next
    /// dispatch(Send) wakes by re-spawning the ACP command through the injected
    /// spawner (then replaying session/load against the bound sid). FakeSpawner
    /// records the spawn then Errs, so dispatch surfaces the wake error — the
    /// hermetic proof the resume re-spawn ran with the configured command.
    #[tokio::test]
    async fn f4_suspend_then_wake_respawns_through_spawner() {
        use crate::testing::FakeSpawner;
        let spawner = Arc::new(FakeSpawner::new());
        let command = aionui_common::CommandSpec {
            command: "hermes".into(),
            args: vec!["acp".into()],
            env: Vec::new(),
            cwd: None,
        };
        let backend = AcpSessionBackend::build_with_io_suspending(
            "acp-resume-1",
            Box::new(FakeAgentIo::never_exits(Vec::new())),
            spawner.clone(),
            command,
            40,
        )
        .await;
        // The resume anchor that survives the suspend (live path binds it from the
        // session/new response; seed it here).
        backend.bind_for_test("acp-sid-anchor").await;
        assert!(backend.idle_timer.is_some(), "idle timer spawned when ttl is Some");

        assert!(
            backend
                .suspend
                .suspend_if_idle(aionui_common::now_ms() + 10_000, false)
                .await
        );
        assert!(!backend.suspend.is_active().await, "now Dormant");

        let err = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("wake".into())],
                metadata: super::super::types::CommandMeta::default(),
            })
            .await
            .expect_err("FakeSpawner cannot make a real process → wake Errs");
        assert!(
            matches!(&err, BackendError::Transport(m) if m.contains("resume-spawn failed")),
            "dispatch surfaced the wake re-spawn error, got {err:?}"
        );
        assert_eq!(spawner.call_count(), 1, "wake routed through the injected spawner once");
        let spec = spawner.last_command().await.expect("a spawn was recorded");
        assert_eq!(
            spec.command.to_str(),
            Some("hermes"),
            "wake re-spawns the configured ACP command"
        );
        assert!(
            spec.args.iter().any(|a| a == "acp"),
            "wake re-spawns `hermes acp`, got {:?}",
            spec.args
        );
        drop(backend);
    }

    // ======================================================================
    // Field-value coverage (2026-06-17 audit): two reachable session-layer
    // field VALUES the field-value audit found unasserted — both produced by
    // map_update, directly unit-testable here.
    // ======================================================================

    /// Field-value gap: `UsageDelta` with `input_tokens==0 && output_tokens==0`.
    /// The ACP `usage_update` wire (hermes: `{used, size}`) carries only a
    /// cumulative `used`, so map_update emits input=0/output=0/total=used — a real
    /// production zero-token edge no test pinned (every other UsageDelta test uses
    /// nonzero input/output). A regression that started defaulting input/output to
    /// `used` (double-counting) would be caught here.
    #[tokio::test]
    async fn acp_usage_update_emits_zero_input_output_with_cumulative_total() {
        let current_model = Arc::new(Mutex::new(None));
        let discovered = Arc::new(std::sync::Mutex::new(Discovered::default()));
        let params = serde_json::json!({
            "update": { "sessionUpdate": "usage_update", "used": 4096, "size": 200000 }
        });
        let events = map_update(&params, &current_model, &discovered).await;
        assert_eq!(events.len(), 1, "usage_update → exactly one UsageDelta, got {events:?}");
        match &events[0] {
            SessionEvent::UsageDelta {
                input_tokens,
                output_tokens,
                total_tokens,
                cost_usd,
                ..
            } => {
                assert_eq!(
                    *input_tokens, 0,
                    "hermes {{used,size}} shape carries no split → input==0"
                );
                assert_eq!(*output_tokens, 0, "hermes shape carries no split → output==0");
                assert_eq!(*total_tokens, 4096, "cumulative `used` rides total_tokens");
                assert_eq!(*cost_usd, None, "the hermes shape carries no cost");
            }
            other => panic!("expected UsageDelta, got {other:?}"),
        }
    }

    /// claude-agent-acp's RICHER usage_update shape — per-direction split + cost — must
    /// be carried (was dropped: input/output ignored, cost hardcoded None).
    #[tokio::test]
    async fn acp_usage_update_carries_split_and_cost_when_present() {
        let current_model = Arc::new(Mutex::new(None));
        let discovered = Arc::new(std::sync::Mutex::new(Discovered::default()));
        let params = serde_json::json!({
            "update": {
                "sessionUpdate": "usage_update",
                "inputTokens": 1200, "outputTokens": 340, "totalTokens": 1540,
                "cost": { "amount": 0.011, "currency": "USD" }
            }
        });
        let events = map_update(&params, &current_model, &discovered).await;
        match events.first() {
            Some(SessionEvent::UsageDelta {
                input_tokens,
                output_tokens,
                total_tokens,
                cost_usd,
                ..
            }) => {
                assert_eq!(*input_tokens, 1200);
                assert_eq!(*output_tokens, 340);
                assert_eq!(*total_tokens, 1540);
                assert_eq!(*cost_usd, Some(0.011), "cost.amount must reach cost_usd (was dropped)");
            }
            other => panic!("expected UsageDelta, got {other:?}"),
        }
    }

    /// The terminal session/prompt RESPONSE's result.usage → a UsageDelta (the
    /// per-direction split that the streaming usage_update lacks). Was dropped.
    #[test]
    fn acp_result_usage_emits_terminal_usage_delta() {
        let frame = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": { "stopReason": "end_turn",
                "usage": { "inputTokens": 900, "outputTokens": 50, "totalTokens": 950,
                           "cost": { "amount": 0.007 } } }
        });
        match parse_acp_result_usage(&frame) {
            Some(SessionEvent::UsageDelta {
                input_tokens,
                output_tokens,
                total_tokens,
                cost_usd,
                ..
            }) => {
                assert_eq!((input_tokens, output_tokens, total_tokens), (900, 50, 950));
                assert_eq!(cost_usd, Some(0.007));
            }
            other => panic!("expected UsageDelta, got {other:?}"),
        }
        // No usage in result → None (no spurious event).
        let bare = serde_json::json!({ "result": { "stopReason": "end_turn" } });
        assert!(parse_acp_result_usage(&bare).is_none());
    }

    /// A `ConfigChanged { mode: None, model: Some(_) }` (model-only change) is real,
    /// but it does NOT come from a `current_model_update` SessionUpdate — that variant
    /// does not exist in ACP (schema 0.12.0 defines CurrentModeUpdate, never
    /// CurrentModelUpdate; the model lives in SessionModelState inside session results).
    /// A prior arm parsed `current_model_update` by guessed symmetry; it is removed.
    /// This pins the HONEST behavior: a (non-existent) current_model_update frame falls
    /// through to AdapterSpecific, NOT ConfigChanged. The REAL model-only ConfigChanged
    /// is asserted by `acp_config_option_update_with_mode_emits_config_changed` (the
    /// config_option_update path claude-acp actually uses, LIVE-VERIFIED).
    #[tokio::test]
    async fn acp_nonexistent_current_model_update_does_not_emit_config_changed() {
        let current_model = Arc::new(Mutex::new(None));
        let discovered = Arc::new(std::sync::Mutex::new(Discovered::default()));
        let params = serde_json::json!({
            "update": { "sessionUpdate": "current_model_update", "currentModelId": "gpt-5.5" }
        });
        let events = map_update(&params, &current_model, &discovered).await;
        assert!(
            !events.iter().any(|e| matches!(e, SessionEvent::ConfigChanged { .. })),
            "a `current_model_update` frame (a guessed, non-existent ACP variant) must NOT be parsed into a \
             ConfigChanged — it falls through to AdapterSpecific. Real model changes ride config_option_update."
        );
        assert!(
            events.iter().any(
                |e| matches!(e, SessionEvent::AdapterSpecific { tag, .. } if tag.contains("current_model_update"))
            ),
            "the unknown sessionUpdate is preserved opaquely (lossless A1 catch-all), not silently dropped"
        );
    }

    /// Some ACP agents (claude-agent-acp) report mode/model changes via
    /// `config_option_update` (NOT current_mode/model_update). The wire below is the
    /// REAL claude-acp shape captured live (acp_claude_bridge_set_mode_config_change_behavior):
    /// `configOptions:[{id:"mode",currentValue:"plan"},{id:"model",currentValue:"default"},...]`.
    /// The arm must extract the mode/model currentValue → ConfigChanged (else a
    /// claude-acp mode switch is dropped and the frontend selector never updates —
    /// README discipline #10). Generic option ids are still tracked + the full opaque
    /// AdapterSpecific is still emitted.
    #[tokio::test]
    async fn acp_config_option_update_with_mode_emits_config_changed() {
        let current_model = Arc::new(Mutex::new(None));
        let discovered = Arc::new(std::sync::Mutex::new(Discovered::default()));
        // Real claude-acp wire (trimmed to the load-bearing fields).
        let params = serde_json::json!({
            "update": {
                "sessionUpdate": "config_option_update",
                "configOptions": [
                    { "id": "mode", "category": "mode", "type": "select", "currentValue": "plan",
                      "options": [{"value": "default"}, {"value": "plan"}] },
                    { "id": "model", "category": "model", "type": "select", "currentValue": "opus",
                      "options": [{"value": "default"}, {"value": "opus"}] }
                ]
            }
        });
        let events = map_update(&params, &current_model, &discovered).await;
        let cc = events
            .iter()
            .find_map(|e| match e {
                SessionEvent::ConfigChanged { mode, model } => Some((mode.clone(), model.clone())),
                _ => None,
            })
            .expect("config_option_update carrying mode/model must emit a ConfigChanged");
        assert_eq!(cc.0.as_deref(), Some("plan"), "mode.currentValue → ConfigChanged.mode");
        assert_eq!(
            cc.1.as_deref(),
            Some("opus"),
            "model.currentValue → ConfigChanged.model"
        );
        assert_eq!(
            current_model.lock().await.as_deref(),
            Some("opus"),
            "config_option_update model also updates the tracked model"
        );
        // The opaque catalog event is still emitted (additive, not replaced).
        assert!(
            events.iter().any(
                |e| matches!(e, SessionEvent::AdapterSpecific { tag, .. } if tag.contains("config_option_update"))
            ),
            "the full options catalog still rides AdapterSpecific"
        );
    }

    /// #101: an `available_commands_update` session/update fills the discovered
    /// slash-command catalog (`update.availableCommands[{name, description}]` —
    /// wire-pinned from hermes + claude-acp captures). It stays FSM-orthogonal
    /// (AdapterSpecific event, no FSM signal), and `capabilities()` merges the catalog.
    #[tokio::test]
    async fn acp_available_commands_update_fills_slash_commands() {
        let current_model = Arc::new(Mutex::new(None));
        let discovered = Arc::new(std::sync::Mutex::new(Discovered::default()));
        let params = serde_json::json!({
            "update": {
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    { "name": "help", "description": "List available commands" },
                    { "name": "model", "description": "Switch models", "input": { "hint": "model name" } },
                    { "name": "reset" }
                ]
            }
        });
        let events = map_update(&params, &current_model, &discovered).await;
        // Event surface unchanged: opaque AdapterSpecific (no FSM signal).
        assert!(
            matches!(&events[..], [SessionEvent::AdapterSpecific { tag, .. }] if tag == "acp_update:available_commands_update"),
            "available_commands_update stays an opaque AdapterSpecific, got {events:?}"
        );
        // Catalog filled (name + optional description; a command without a description → None).
        let cmds = discovered.lock().unwrap().slash_commands.clone();
        assert_eq!(cmds.len(), 3, "three commands parsed");
        assert_eq!(cmds[0].name, "help");
        assert_eq!(cmds[0].description.as_deref(), Some("List available commands"));
        assert_eq!(cmds[2].name, "reset");
        assert_eq!(cmds[2].description, None, "a command without a description → None");
    }

    /// LC-8a: ACP `session/update{sessionUpdate:"plan"}` → `SessionEvent::Plan`.
    /// entries[].{content,status,priority} direct map; snake_case `in_progress`→
    /// InProgress; priority parsed (high/medium/low). No explanation (codex-only).
    #[tokio::test]
    async fn acp_plan_update_maps_to_plan_event() {
        use crate::event::{PlanPriority, PlanStatus};
        let current_model = Arc::new(Mutex::new(None));
        let discovered = Arc::new(std::sync::Mutex::new(Discovered::default()));
        let params = serde_json::json!({
            "update": {
                "sessionUpdate": "plan",
                "entries": [
                    { "content": "investigate", "status": "completed", "priority": "high" },
                    { "content": "implement", "status": "in_progress", "priority": "medium" },
                    { "content": "verify", "status": "pending" },
                ]
            }
        });
        let events = map_update(&params, &current_model, &discovered).await;
        match &events[..] {
            [SessionEvent::Plan { entries, explanation }] => {
                assert_eq!(entries.len(), 3);
                assert_eq!(entries[0].content, "investigate");
                assert_eq!(entries[0].status, PlanStatus::Completed);
                assert_eq!(entries[0].priority, Some(PlanPriority::High));
                assert_eq!(
                    entries[1].status,
                    PlanStatus::InProgress,
                    "snake_case in_progress normalized"
                );
                assert_eq!(entries[1].priority, Some(PlanPriority::Medium));
                assert_eq!(entries[2].status, PlanStatus::Pending);
                assert!(entries[2].priority.is_none(), "absent priority → None");
                assert!(explanation.is_none(), "ACP plan carries no explanation");
            }
            other => panic!("expected one Plan event, got {other:?}"),
        }
    }

    /// Regression-by-rewrite (codex-500 TWIN): acp's bound_session had the IDENTICAL
    /// hardcoded 2s busy-poll → bare Transport→500 bug, a downgrade from legacy ACP's
    /// 30s. When session/new never binds, it must now return the RETRYABLE
    /// HandshakeTimeout (not Transport). Tiny injected budget = deterministic.
    #[tokio::test]
    async fn bound_session_timeout_is_handshake_timeout_not_transport() {
        let fake = FakeAgentIo::never_exits(Vec::new());
        let backend = AcpSessionBackend::build_with_io("acp-noses", Box::new(fake)).await;
        let err = backend
            .bound_session_within(std::time::Duration::from_millis(120))
            .await
            .expect_err("no session/new binding → must time out");
        assert!(
            matches!(err, BackendError::HandshakeTimeout(_)),
            "acp handshake timeout must be RETRYABLE HandshakeTimeout (not Transport→500), got {err:?}"
        );
    }

    /// Positive: a late-arriving session binding (past the old 2s) within budget binds.
    #[tokio::test]
    async fn bound_session_binds_when_session_arrives_within_budget() {
        let fake = FakeAgentIo::never_exits(Vec::new());
        let backend = AcpSessionBackend::build_with_io("acp-late", Box::new(fake)).await;
        backend.bind_for_test("s-late").await; // simulate session/new arriving
        let sid = backend
            .bound_session_within(std::time::Duration::from_secs(2))
            .await
            .expect("a within-budget binding must succeed");
        assert_eq!(sid, "s-late");
    }

    /// Regression (opencode `-32602 session not found` on reopen): a `session/load`
    /// RESPONSE carries NO `sessionId` (just config/null). The reader must still bind
    /// the resume anchor — from `pending_resume_sid`, stashed by `run_handshake`
    /// instead of pre-seeding `acp_session_id`. Without this the resume binding never
    /// lands, OR (the original bug) it was pre-seeded so the first `session/prompt`
    /// fired before opencode finished `session/load` → session-not-found. Binding only
    /// on the response is what gates the prompt until the load completes.
    #[tokio::test]
    async fn session_load_response_binds_resume_sid_without_sessionid_in_result() {
        let tg = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = broadcast::channel(8);
        let acp_session_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        // run_handshake's resume branch stashes the sid here (NOT into acp_session_id).
        let pending_resume_sid: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(Some("ses_resume_me".into())));
        let disc = Arc::new(std::sync::Mutex::new(Discovered::default()));
        // A real opencode session/load result: configOptions, but NO `sessionId`.
        let load_result = json!({
            "configOptions": [
                { "id": "mode", "currentValue": "build",
                  "options": [{ "value": "build", "name": "build" }] }
            ]
        });

        // Before the response: acp_session_id is unbound, so bound_session() would block.
        assert!(
            acp_session_id.lock().await.is_none(),
            "must NOT be pre-seeded before the load response"
        );

        handle_open_response(&load_result, "s", &tg, &tx, &acp_session_id, &pending_resume_sid, &disc).await;

        assert_eq!(
            acp_session_id.lock().await.as_deref(),
            Some("ses_resume_me"),
            "the load response must bind the resume sid (taken from pending_resume_sid, since the result has none)"
        );
        assert!(
            pending_resume_sid.lock().await.is_none(),
            "the resume sid is consumed one-shot"
        );
        match rx.try_recv() {
            Ok(env) => assert!(
                matches!(env.event, SessionEvent::BackendBound { backend_session_id: Some(ref s) } if s == "ses_resume_me"),
                "must emit BackendBound with the resume sid, got {:?}",
                env.event
            ),
            Err(e) => panic!("expected a BackendBound event, got {e:?}"),
        }
    }

    /// Regression (opencode timer-bar on model change): a resumed ACP session
    /// replays its FULL history as `session/update` notifications between the
    /// `session/load` request and its RESPONSE. Those are historical — the frontend
    /// renders history from conversation_blocks (the SSOT) — so the reader must
    /// SUPPRESS them from the UI event stream while the resume load is in flight;
    /// otherwise a warmup-triggered resume (e.g. picking a model on a cold conv)
    /// streams the replay as live deltas → duplicate blocks + a spurious turn-active
    /// timer bar. The suppression is UI-only: `map_update`'s metadata side-effects
    /// (slash-command / config-option catalog into `discovered`) still run, and once
    /// the load RESPONSE takes the resume sid the window closes and live deltas flow.
    #[tokio::test]
    async fn session_load_replay_is_suppressed_from_ui_but_metadata_survives() {
        use futures_util::StreamExt as _;

        // Scripted tail (reader is the single ordered consumer, so order holds):
        //  1. replayed assistant text  → MessageDelta (MUST be suppressed)
        //  2. available_commands_update → fills discovered.slash_commands (side-effect
        //     MUST survive) + AdapterSpecific (MUST be suppressed)
        //  3. session/load RESPONSE id:1 → binds resume sid, emits BackendBound,
        //     takes pending_resume_sid → CLOSES the replay window (ordering sentinel)
        //  4. post-load assistant text → MessageDelta (MUST now be emitted)
        let tail = format!(
            "{}\n{}\n{}\n{}\n",
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"REPLAYED-HISTORY"}}}}"#,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"compact","description":"shrink context"}]}}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"configOptions":[{"id":"mode","currentValue":"build","options":[{"value":"build","name":"build"}]}]}}"#,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"LIVE-DELTA"}}}}"#,
        )
        .into_bytes();
        let fake = FakeAgentIo::never_exits(Vec::new()).with_gated_tail(tail);
        let release = fake.stdout_releaser();
        let backend = AcpSessionBackend::build_with_io("sess-replay", Box::new(fake)).await;
        // Open the resume-load window (run_handshake's Resume branch does this live)
        // and register the load-response rpc id so id:1 hits handle_open_response.
        backend.set_pending_resume_sid_for_test("ses_resume_me").await;
        backend.set_pending_open_for_test(1).await;

        let mut events = backend.events();
        release();

        // Collect until the LIVE post-load delta arrives; assert the REPLAYED delta
        // and the replayed AdapterSpecific never surfaced, and BackendBound preceded
        // the live delta (the window closed exactly at the load response).
        let (saw_replay_ui, saw_backend_bound_before_live) =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let mut saw_replay = false;
                let mut saw_bound = false;
                while let Some(env) = events.next().await {
                    match &env.event {
                        SessionEvent::MessageDelta { text, .. } if text == "REPLAYED-HISTORY" => {
                            saw_replay = true;
                        }
                        SessionEvent::AdapterSpecific { tag, .. } if tag == "acp_update:available_commands_update" => {
                            // The replayed metadata's opaque event must also be suppressed.
                            saw_replay = true;
                        }
                        SessionEvent::BackendBound { .. } => saw_bound = true,
                        SessionEvent::MessageDelta { text, .. } if text == "LIVE-DELTA" => {
                            return (saw_replay, saw_bound);
                        }
                        _ => {}
                    }
                }
                (saw_replay, saw_bound)
            })
            .await
            .expect("must not hang; the live post-load delta must arrive");

        assert!(
            !saw_replay_ui,
            "the session/load history replay must be suppressed from the UI event stream"
        );
        assert!(
            saw_backend_bound_before_live,
            "the load response (BackendBound) must close the replay window before the live delta"
        );
        // The metadata side-effect must have survived the suppression.
        let cmds = backend
            .discovered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slash_commands
            .clone();
        assert!(
            cmds.iter().any(|c| c.name == "compact"),
            "replayed available_commands_update must still fill discovered.slash_commands \
             (suppression is UI-only), got {cmds:?}"
        );
    }
}
