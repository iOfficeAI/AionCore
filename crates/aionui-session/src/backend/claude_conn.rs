//! 007 §C5 (claude variant): `ClaudeConnection` / `ClaudeSessionBackend` — the
//! NEW symmetric-seam impl that WRAPS the existing `ClaudeAdapter` spawn+parse
//! logic so its behavior is verbatim-unchanged (claude is already in production;
//! the hard acceptance is "parse output zero-diff"). This is the strangler's
//! claude lane: the legacy `adapter`/`run_turn` path stays compiled in parallel
//! behind the `legacy-session` feature; the orchestrator selects this path.
//!
//! Shape: claude is a 1:1 connection→session backend (one spawned process per
//! session, no multiplexing). A long-lived reader task drains the persistent
//! process's stdout, feeds bytes through `ClaudeAdapter::parse_chunk`, stamps
//! the live `turn_gen`, wraps each event in a `SessionEnvelope`, and broadcasts
//! it on `events()`. `dispatch(Send)` delivers the prompt over the retained
//! stdin + flush, bumps `turn_gen`, and synthesizes `PromptAccepted`
//! (Synthesized — claude has no native prompt-ack wire signal).

use std::sync::Arc;

use aionui_process::Spawner;
use tokio::sync::{Mutex, broadcast};

use super::suspend::{ProcHandle, SuspendController, spawn_idle_timer};
use super::types::{
    Admission, BackendError, CancelTarget, Command, CommandReceipt, PendingPermissionView, SessionEnvelope, SessionSpec,
};
use super::{BackendConnection, SessionBackend, SessionConfig};
use crate::adapter::{AgentIo, BackendAdapter, ClaudeAdapter, SessionSpec as LegacySessionSpec};
use crate::capability::Capabilities;
use crate::event::SessionEvent;
use futures_util::stream::{BoxStream, StreamExt};

/// Connection-level factory for claude. Holds the injected `Spawner` (the only
/// way to spawn — never raw `Command`, S14) + a default `SessionConfig`. claude
/// is 1:1, so `open_session` spawns one process and returns one backend handle.
pub struct ClaudeConnection {
    spawner: Arc<dyn Spawner>,
}

impl ClaudeConnection {
    pub fn new(spawner: Arc<dyn Spawner>) -> Self {
        Self { spawner }
    }

    /// Map the two-id `SessionSpec` (§4.1) to `(logical_id, claude_session_id,
    /// legacy_spec)`:
    /// - `logical_id` — our demux key, stamped on every envelope; the backend's
    ///   `session_id`. Often a prefixed id (`conv_<uuid_v7>`) — NOT a bare UUID.
    /// - `claude_session_id` — the bare-UUID id claude is spawned with
    ///   (`--session-id`) and resumed with (`--resume`). MUST be a valid UUID or
    ///   claude exits 1 `Invalid session ID` (see [`claude_session_id_for`]).
    /// - `legacy_spec` — what `ClaudeAdapter::start_turn` uses for the initial
    ///   spawn (Fresh `--session-id` / Resume `--resume`).
    ///
    /// On a lost-backend Resume (`backend_session_id: None`) we rebind a FRESH
    /// valid-UUID claude session (the old on-disk session is gone).
    fn to_legacy_spec(spec: &SessionSpec) -> (String, String, LegacySessionSpec) {
        match spec {
            SessionSpec::Fresh { session_id } => {
                let claude_id = claude_session_id_for(session_id);
                (
                    session_id.clone(),
                    claude_id.clone(),
                    LegacySessionSpec::Fresh(claude_id),
                )
            }
            SessionSpec::Resume {
                session_id,
                backend_session_id,
            } => match backend_session_id {
                // claude echoed this id in `system/init` (BackendBound) so it is
                // already a valid UUID — resume it verbatim.
                Some(bid) => (session_id.clone(), bid.clone(), LegacySessionSpec::Resume(bid.clone())),
                // lost backend session → rebind a fresh valid-UUID claude session.
                None => {
                    let claude_id = claude_session_id_for(session_id);
                    (
                        session_id.clone(),
                        claude_id.clone(),
                        LegacySessionSpec::Fresh(claude_id),
                    )
                }
            },
        }
    }
}

/// Derive the bare-UUID id to spawn/resume claude with (`--session-id` /
/// `--resume`). claude REQUIRES a valid UUID here: a non-UUID makes it exit code
/// 1 with `Error: Invalid session ID. Must be a valid UUID.` (the message lands
/// on stderr, which the `Detached` event does not surface, so it looks like an
/// empty silent crash → `Error{Crashed}`).
///
/// Our logical session id is the conversation id (`conv_<uuid_v7>` — prefixed,
/// NOT a bare UUID), so the seam must mint one rather than forward it verbatim.
/// If the logical id already parses as a UUID (the F1 factory mints a bare
/// `Uuid::new_v4()` upstream, so production ids pass through UNCHANGED), use it
/// as-is; otherwise mint a fresh v4. claude echoes whichever id it was given in
/// `system/init` → `BackendBound` → persisted as `backend_session_id`, so the
/// minted id becomes the cross-process resume anchor and the wake recipe resumes
/// the SAME id (decoupling the on-disk claude id from the logical demux key,
/// §4.1).
fn claude_session_id_for(logical_id: &str) -> String {
    match uuid::Uuid::parse_str(logical_id) {
        Ok(_) => logical_id.to_string(),
        Err(_) => uuid::Uuid::new_v4().to_string(),
    }
}

/// Prepend `head` flags before `tail`, returning a new owned arg vec. Used so the
/// init-surface flags are positioned before any caller-supplied `extra_args` (a
/// caller flag that duplicates one then wins by appearing later on the CLI).
fn prepend_args(head: &[String], tail: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(head.len() + tail.len());
    out.extend_from_slice(head);
    out.extend_from_slice(tail);
    out
}

/// Translate the neutral [`SessionConfig`] init surface into claude CLI flags
/// (S18/D13 parity with the legacy F1 `prelude.rs`, which is NOT on the clean-slate
/// route). Each flag is omitted when its source is empty, so a default/empty config
/// produces no flags (pre-0c spawn byte-identical):
/// - `init.mcp_servers` → `--mcp-config <json>` + `--strict-mcp-config` (the latter
///   ONLY alongside `--mcp-config`: it makes the session ignore the machine's
///   ambient `~/.claude` servers, which we must NOT do when we inject none).
/// - `init.preset_context` → `--system-prompt` (composed `[Assistant Rules]` /
///   skills index / team-guide text, already assembled by the app boundary).
/// - `model` → `--model`; `mode` → `--permission-mode` (claude has no in-band
///   switch at spawn; a UI switch persists + evicts so the rebuild re-applies here).
///
/// claude's `--mcp-config` uses a MAP shape `{"mcpServers":{"<name>":{…}}}` (NOT the
/// ACP array), so this builds its own JSON rather than reusing `acp_conn`'s array
/// serializer. stdio → `{command,args,env:{k:v}}`; http/sse → `{type,url,headers:{…}}`.
pub(crate) fn build_claude_init_args(config: &SessionConfig) -> Vec<String> {
    let mut args = Vec::new();

    if let Some(preset) = config
        .init
        .preset_context
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        args.push("--system-prompt".to_string());
        args.push(preset.to_string());
    }

    if let Some(mcp_json) = build_claude_mcp_config(&config.init.mcp_servers) {
        args.push("--mcp-config".to_string());
        args.push(mcp_json);
        args.push("--strict-mcp-config".to_string());
    }

    // SECURITY (fail-CLOSED): ALWAYS pass --permission-mode. Omitting it makes claude
    // headless default to `bypassPermissions` — LIVE-PROBED: `system/init` reports
    // `permissionMode: bypassPermissions` and Write/Bash auto-run with NO `can_use_tool`
    // prompt. config.mode is `None` for an ordinary claude session (the create path
    // does not seed it; an interactive switch is in-band + persisted to extra, read
    // back into config.mode on the next spawn), so gating the flag on `Some` silently
    // downgraded every default session to the most-permissive mode. Default to
    // "default" (standard prompts) so a session with no explicit choice is gated, not
    // bypassed. `default`/`acceptEdits`/`bypassPermissions`/`plan`/`dontAsk`/`auto` are
    // claude's exact accepted wire values — the whitelist is a SUPERSET of the advertised
    // picker (which omits `auto`; see `claude_permission_modes`) so a resumed session that
    // carries `auto` is not downgraded/crashed.
    // VALIDATE before the flag reaches the spawn: an invalid `--permission-mode`
    // makes claude exit 1 at spawn (LIVE-PROBED), which surfaces as an opaque
    // "agent crashed" with no diagnosis. `config.mode` is sourced from unconstrained
    // storage (a persisted `current_mode_id`, an assistant default), so a stale/
    // generic alias that survived normalization would harden into a spawn crash. The
    // dead-until-now `is_valid_claude_permission_mode` is the exact seed-time
    // whitelist for this; an unrecognized value falls back to the fail-CLOSED
    // "default" (a WARN records the drop) rather than crashing the process. Mirrors
    // the ACP path's `clear_invalid_desired_mode` (drop-if-not-in-catalog) — a
    // protection the port had wired but never called.
    let mode = config
        .mode
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|m| {
            let ok = crate::adapter::is_valid_claude_permission_mode(m);
            if !ok {
                tracing::warn!(
                    requested_mode = %m,
                    "claude: ignoring unrecognized --permission-mode (would crash spawn); \
                     falling back to \"default\""
                );
            }
            ok
        })
        .unwrap_or("default");
    args.push("--permission-mode".to_string());
    args.push(mode.to_string());

    // UNLOCK runtime bypass WITHOUT architecting away the spawn-time mode. claude has
    // TWO distinct flags (LIVE-PROBED 2.1.185):
    //   --dangerously-skip-permissions       FORCES init=bypassPermissions, OVERRIDING
    //                                         --permission-mode (would re-open the
    //                                         fail-open hole this fn closes — DO NOT use).
    //   --allow-dangerously-skip-permissions  ONLY enables `bypassPermissions` as a
    //                                         reachable mode; it does NOT change the
    //                                         initial mode. With it + `--permission-mode
    //                                         default`, `default` still ENFORCES (Write
    //                                         prompts), AND a later in-band
    //                                         `set_permission_mode bypassPermissions` is
    //                                         ACCEPTED instead of rejected with "session
    //                                         was not launched with
    //                                         --dangerously-skip-permissions".
    // Mirrors the official @agentclientprotocol/claude-agent-acp adapter, which passes
    // the SDK's `allowDangerouslySkipPermissions` separately from `permissionMode`.
    // Without this flag the user can never switch to bypass at runtime (claude rejects
    // the in-band switch). bypass is unavailable as root (claude ignores it there); we
    // pass the flag unconditionally and let the in-band control_response surface the
    // rejection (the dispatch reconciles on the reply), keeping this builder a pure,
    // syscall-free fn.
    args.push("--allow-dangerously-skip-permissions".to_string());

    // TEMPORARY: disable AskUserQuestion until the multi-question interactive card is
    // ported to the current frontend. claude's AskUserQuestion can ask several
    // questions at once (`{questions:[…]}`), but the active frontend only renders a
    // single-question permission card, so a multi-question ask would silently drop all
    // but the first. Rather than ship that half-answer behaviour, deny the tool at
    // spawn time — claude then falls back to plain-text questions, which render fully.
    // Mirrors the official @agentclientprotocol/claude-agent-acp adapter, which
    // likewise lists `AskUserQuestion` in `disallowedTools` for the same reason
    // ("not a great way to expose this over ACP at the moment"). Remove once the
    // frontend gains a multi-question renderer.
    args.push("--disallowed-tools".to_string());
    args.push("AskUserQuestion".to_string());

    if let Some(model) = config.model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }

    args
}

/// Serialize neutral [`McpServerSpec`]s into claude's `--mcp-config` inline JSON
/// (`{"mcpServers":{"<name>":{…}}}`). `None` when empty so the flag is omitted.
/// Pure `serde_json`, no ACP SDK — `aionui-session` stays SDK-free.
fn build_claude_mcp_config(servers: &[super::McpServerSpec]) -> Option<String> {
    use super::McpTransport;
    use serde_json::{Map, Value, json};
    if servers.is_empty() {
        return None;
    }
    let kv = |pairs: &[(String, String)]| -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect()
    };
    let mut map = Map::new();
    for s in servers {
        let entry = match &s.transport {
            McpTransport::Stdio { command, args, env } => json!({
                "command": command,
                "args": args,
                "env": Value::Object(kv(env)),
            }),
            McpTransport::Http { url, headers } => json!({
                "type": "http",
                "url": url,
                "headers": Value::Object(kv(headers)),
            }),
            McpTransport::Sse { url, headers } => json!({
                "type": "sse",
                "url": url,
                "headers": Value::Object(kv(headers)),
            }),
        };
        map.insert(s.name.clone(), entry);
    }
    Some(json!({ "mcpServers": Value::Object(map) }).to_string())
}

#[async_trait::async_trait]
impl BackendConnection for ClaudeConnection {
    async fn open_session(
        &self,
        spec: SessionSpec,
        config: SessionConfig,
    ) -> Result<Arc<dyn SessionBackend>, BackendError> {
        let (logical_id, claude_session_id, legacy_spec) = Self::to_legacy_spec(&spec);
        let adapter = ClaudeAdapter::new();

        // Build the init flags claude is spawned with from the session-init surface
        // (MCP / preset / model / permission-mode). The clean-slate seam owns this
        // (the legacy F1 `ClaudeCodeManager` did it via prelude.rs; that path is not
        // on the clean-slate route, so without this the spawned CLI would receive
        // NONE of the user's MCP servers / preset context / model / mode). Prepended
        // to any caller-supplied `extra_args` so an explicit caller flag still wins
        // by position. The SAME args are threaded into the wake recipe so a
        // crash/idle-reap respawn re-applies them (R16 continuity).
        let init_args = build_claude_init_args(&config);
        let spawn_args = prepend_args(&init_args, &config.extra_args);

        // Spawn the persistent process via the legacy adapter (reuses the exact
        // flag-building + spawn path, so behavior is verbatim).
        let io = adapter
            .start_turn(
                self.spawner.as_ref(),
                &legacy_spec,
                config.cwd.as_deref(),
                &spawn_args,
                &config.spawn_env,
                config.cli_program.as_deref(),
            )
            .await
            .map_err(|e| BackendError::from_spawn("claude spawn failed", e))?;

        // F-4 wake recipe: re-spawn on wake by RESUMING the SAME claude session id
        // we spawned with (`--session-id <claude_session_id>` on Fresh → `--resume
        // <claude_session_id>`), so the on-disk session is re-attached. This id is
        // the bare-UUID claude id (NOT the logical demux key), so the resume target
        // is always a valid UUID (§4.1). The init flags + spawn env are carried
        // verbatim so the re-spawned process gets the same MCP / preset / model /
        // mode AND the same provider env (#103).
        let wake = ClaudeWakeRecipe {
            spawner: self.spawner.clone(),
            claude_session_id,
            cwd: config.cwd.clone(),
            extra_args: spawn_args,
            env: config.spawn_env.clone(),
            cli_program: config.cli_program.clone(),
        };
        let backend = ClaudeSessionBackend::spawn(logical_id, adapter, io, config, wake).await;
        // #98/#101: ask claude for its discovery catalog (selectable models + slash
        // commands) up-front via `control_request{initialize}`. The response flows
        // back through the reader → `discovered_caps` → `capabilities()`. Best-effort:
        // a write failure (e.g. no stdin on a degenerate spawn) is non-fatal — the
        // catalog just stays empty (the model/slash pickers degrade, the turn path is
        // unaffected). Sent BEFORE any prompt so the catalog is usually present by the
        // first `capabilities()` read; a late response is merged on the next read
        // (same late-discovery contract as codex `model/list`).
        backend.request_initialize().await;
        Ok(Arc::new(backend))
    }

    async fn close_session(&self, _session_id: &str) -> Result<(), BackendError> {
        // claude is 1:1; dropping the backend handle drops the process (001
        // on-drop hook). Nothing connection-level to release.
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        ClaudeAdapter::new().capabilities()
    }
}

/// Per-session claude handle. `&self`-concurrent: the retained stdin is behind a
/// `Mutex` (a microsecond frame-write lock, NOT a per-turn lock), and `turn_gen`
/// is an atomic the dispatch path bumps + the reader task reads.
pub struct ClaudeSessionBackend {
    session_id: String,
    capabilities: Capabilities,
    /// Retained stdin for prompt/control delivery. `BoxedStdin` taken once from
    /// the process; behind a Mutex so concurrent dispatches serialize at the
    /// byte-frame level only. `Arc` so a wake (`wake_handle`) can swap in the fresh
    /// woken process's stdin (the slot survives suspension; the BoxedStdin inside
    /// is replaced).
    stdin: Arc<Mutex<Option<aionui_process::BoxedStdin>>>,
    /// The legacy adapter, retained for `deliver_prompt`/`write_control_response`
    /// (pure transport framing). Behind a Mutex because those take `&mut stdin`.
    adapter: Arc<ClaudeAdapter>,
    /// Live turn epoch (single-writer = dispatch; single-reader = the reader
    /// task stamps it onto each envelope). See §5.4.
    turn_gen: Arc<std::sync::atomic::AtomicU64>,
    /// Broadcast of wrapped events; `events()` resubscribes.
    event_tx: broadcast::Sender<SessionEnvelope>,
    /// F-4 self-suspend controller: owns the live `{reader, io}` pair and the
    /// Active⇄Dormant slot. When `idle_ttl` is set, the idle timer closes the
    /// process after inactivity and `dispatch` re-spawns (`--resume`) it via
    /// `wake()`. When None (default), the slot stays Active for life — the reader
    /// behaves exactly as before F-4 (aborted on Drop via `abort_on_drop`).
    suspend: Arc<SuspendController>,
    /// The per-backend idle timer (Some only when `idle_ttl` is set). Aborted on Drop.
    idle_timer: Option<tokio::task::JoinHandle<()>>,
    /// Everything needed to re-spawn (`--resume`) the claude process on wake from
    /// Dormant: the injected spawner, the resume spec, and the cwd/args. Resume
    /// keys on the SAME claude session id, so the FSM sees a continuous session.
    wake: ClaudeWakeRecipe,
    /// Shared reader-task inputs, cloned into the open-time reader AND every
    /// post-wake reader so they all drain into the same event_tx/turn_gen.
    reader_state: ClaudeReaderState,
    /// F-4 turn-active flag (shared with the reader via `reader_state`): set on
    /// dispatch(Send), cleared by the reader at the terminal. The idle timer reads
    /// it so a streaming turn is never suspended mid-flight.
    turn_in_flight: Arc<std::sync::atomic::AtomicBool>,
    /// Pending permission registry keyed by `request_id` (the control correlation
    /// key). The reader populates it from each raw `can_use_tool` control_request
    /// (storing the tool_use_id + tool_name + input that claude requires echoed in
    /// the response); `dispatch(AnswerPermission)` consumes it to build the keyed
    /// `control_response`. This is the 007-seam analogue of F1's `ControlChannel`
    /// (adapter-private side-channel — it does NOT change the backend-agnostic
    /// `SessionEvent::Permission`, which deliberately carries only `request_id`).
    /// Shared `Arc<Mutex>` between the reader and dispatch (short synchronous use).
    pending_perms: Arc<std::sync::Mutex<std::collections::HashMap<String, PendingPerm>>>,
    /// B-CLAUDE-INIT: the current model captured from the `system/init` frame's
    /// `model` field (the authoritative current model claude broadcasts at spawn).
    /// The reader fills it (sniffing the raw init frame, NOT via parse_chunk → keeps
    /// the zero-diff parse contract); `capabilities()` merges it into
    /// `current_model` when config did not already supply one. None until init
    /// arrives / when config wins.
    discovered_model: Arc<std::sync::Mutex<Option<String>>>,
    /// #98/#101: the selectable model list + slash commands captured from the
    /// `control_request{initialize}` RESPONSE (`response.models[]` /
    /// `response.commands[]`). Unlike `discovered_model` (the `system/init` DATA
    /// frame's single current model), this is the full CATALOG — claude's only
    /// channel for it (the bare `--print` data frames never carry a model list; the
    /// SDK/ACP `supportedModels()` just forwards this same control response). The
    /// reader sniffs the control_response and fills this; `capabilities()` merges it
    /// into `available_models`/`slash_commands` on read. Empty until the response
    /// lands (a freshly-opened backend reads empty, like codex pre-`model/list`).
    discovered_caps: Arc<std::sync::Mutex<DiscoveredCaps>>,
    /// G2 (in-band config switch): control_requests (`set_model` /
    /// `set_permission_mode`) deferred because they arrived mid-turn. Writing one
    /// while a turn is Running would reinitialize the CLI session and TRUNCATE the
    /// in-flight turn (raw-CLI limitation), so `dispatch(SetMode/SetModel)` QUEUES
    /// the frame here and `dispatch(Send)` drains it — in order, over the same
    /// stdin lock, BEFORE the prompt — so a queued switch applies to the NEXT turn
    /// and can never land after-and-truncate it. De-duped by subtype (last-write-
    /// wins). Mirrors F1's `pending_controls`.
    pending_controls: Arc<Mutex<Vec<serde_json::Value>>>,
    /// Monotonic counter minting `control_request` request_ids (no uuid dep). The
    /// CLI echoes it in its success control_response (observed by the reader, not
    /// awaited — the switch applies to the next turn).
    control_seq: Arc<std::sync::atomic::AtomicU64>,
    /// CP-1: the last effort level set via `SetConfigOption{effort}`. claude does NOT
    /// echo effort back (unlike model/mode), so the backend remembers it here and
    /// `capabilities()` surfaces it as `current_effort` for the picker. `None` until
    /// the user picks one. A `std::sync::Mutex` (NOT tokio) so the sync `capabilities()`
    /// can read it without awaiting — mirrors `discovered_model`/`discovered_caps`.
    current_effort: Arc<std::sync::Mutex<Option<String>>>,
    /// The last permission mode set via `SetMode` (control_request{set_permission_mode}).
    /// `capabilities()` surfaces it as `current_mode` so the picker highlights the
    /// active mode after a switch (init seeds `current_mode` from config; this carries
    /// the RUNTIME override). Mirrors `current_effort`; `None` until the user switches.
    current_mode_override: Arc<std::sync::Mutex<Option<String>>>,
    /// #99 reject-surfacing: carries the `ctl-N` request_id of an in-flight
    /// `set_config_option(effort)` (→ a label like `"effort→high"`) so the reader can
    /// surface a REJECTION (claude returns `control_response{subtype:"error"}` for a
    /// bad effort value) as a `Notice{Warning}` instead of silently dropping it (no
    /// handler matched it before — `sniff_mode_reject` hard-filters on "permission
    /// mode"). SUCCESS is silent: claude does not echo effort, and
    /// `capabilities().current_effort` already tracks it optimistically. A
    /// `std::sync::Mutex` (NOT tokio) so the sync reader `process_batch` closure can
    /// lock it without awaiting — mirrors `current_mode_override`.
    pending_set_config: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
}

/// One outstanding claude `can_use_tool` request, stored so `AnswerPermission` can
/// build the keyed `control_response` (claude blocks the tool until it arrives).
#[derive(Clone)]
struct PendingPerm {
    /// The assistant tool_use block id — echoed back as `toolUseID` (required).
    tool_use_id: String,
    /// Tool name; `AskUserQuestion` (the only interactive tool on claude headless)
    /// needs the chosen answer injected into `updatedInput.answers`.
    tool_name: String,
    /// The original tool input (for AskUserQuestion: `{questions:[…]}`).
    input: serde_json::Value,
}

/// Everything `ClaudeSessionBackend::wake_handle` needs to re-spawn (`--resume`)
/// the claude process after an idle suspend. Resume keys on the SAME bare-UUID
/// claude session id claude was started with (`--session-id <claude_session_id>`
/// on Fresh → `--resume <claude_session_id>`), so the on-disk session is
/// re-attached and the FSM sees a continuous session (§4.1). This is the claude
/// on-disk id (a valid UUID), DISTINCT from the logical demux key. For a
/// test-built backend (`build_with_io`, no real spawner) suspension is never
/// enabled, so it is never consulted.
struct ClaudeWakeRecipe {
    spawner: Arc<dyn Spawner>,
    claude_session_id: String,
    cwd: Option<String>,
    extra_args: Vec<String>,
    /// #103: the spawn env captured at open time (e.g. cc-switch provider env) so
    /// a resume-respawn re-applies the SAME env (R16 continuity — a woken process
    /// must reach the same provider as the original).
    env: Vec<aionui_common::EnvVar>,
    /// The bundled-CLI path captured at open time so a resume-respawn uses the
    /// SAME binary (R16 continuity). `None` ⇒ bare "claude" via PATH.
    cli_program: Option<std::path::PathBuf>,
}

/// #98/#101: the discovery catalog captured from the `control_request{initialize}`
/// response — the selectable model list + slash commands claude advertises (the
/// `system/init` data frame carries neither; this control response is the source the
/// SDK/ACP `supportedModels()`/`supportedCommands()` forward). Filled by the reader
/// on the control_response, merged by `capabilities()` on read. Default empty.
#[derive(Clone, Default)]
struct DiscoveredCaps {
    models: Vec<crate::capability::ModelInfo>,
    slash_commands: Vec<crate::capability::SlashCommandInfo>,
}

/// Shared state the reader task drains into — held by the backend, cloned into
/// each reader (the live one + every post-wake one). Grouped so `spawn` and
/// `wake_handle` start identical readers without a 7-arg call duplicated twice.
#[derive(Clone)]
struct ClaudeReaderState {
    session_id: String,
    turn_gen: Arc<std::sync::atomic::AtomicU64>,
    event_tx: broadcast::Sender<SessionEnvelope>,
    pending_perms: Arc<std::sync::Mutex<std::collections::HashMap<String, PendingPerm>>>,
    discovered_model: Arc<std::sync::Mutex<Option<String>>>,
    /// #98/#101: shared catalog the reader fills from the initialize control_response.
    discovered_caps: Arc<std::sync::Mutex<DiscoveredCaps>>,
    want_init_model: bool,
    /// F-4 turn-active flag: set true on dispatch(Send), cleared by the reader at a
    /// turn terminal (TurnResult / Detached). The idle timer reads it so a streaming
    /// turn is never suspended mid-flight (see SuspendController::suspend_if_idle).
    turn_in_flight: Arc<std::sync::atomic::AtomicBool>,
    /// The OBSERVED permission mode (mirror of `ClaudeSessionBackend.current_mode_override`,
    /// shared Arc). The reader reconciles it to claude's authoritative
    /// `set_permission_mode` control_response — success echoes the applied mode
    /// (normal→default), error (e.g. a root-rejected bypass) clears the optimistic
    /// value. This is claude's observed-mode track, the analogue of codex's
    /// `thread/settings/updated` and ACP's `session/update` reconcile.
    current_mode_override: Arc<std::sync::Mutex<Option<String>>>,
    /// #99: shared map of in-flight `set_config_option(effort)` ctl-ids → label, so
    /// `sniff_set_config_reject` can surface a rejection as a `Notice{Warning}`
    /// (shared Arc with `ClaudeSessionBackend.pending_set_config`).
    pending_set_config: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
}

/// Spawn a claude stdout reader over `stdout`/`io` using the shared state. Used
/// both at open (`spawn`) and on every idle-wake (`wake_handle`), so the reader
/// wiring lives in exactly one place.
fn start_claude_reader(
    state: &ClaudeReaderState,
    stdout: Option<aionui_process::BoxedStdout>,
    io: Arc<dyn AgentIo>,
) -> tokio::task::JoinHandle<()> {
    let state = state.clone();
    tokio::spawn(async move {
        reader_task(
            state.session_id,
            stdout,
            io,
            state.turn_gen,
            state.event_tx,
            state.pending_perms,
            state.discovered_model,
            state.discovered_caps,
            state.want_init_model,
            state.turn_in_flight,
            state.current_mode_override,
            state.pending_set_config,
        )
        .await;
    })
}

impl ClaudeSessionBackend {
    /// `take_stdio()` is ONE-SHOT and returns BOTH halves, so we take it exactly
    /// once here: stdin is retained behind a Mutex for delivery, stdout is moved
    /// into the long-lived reader task. (A failed take → an immediate terminal
    /// Detached so the FSM never hangs.)
    async fn spawn(
        session_id: String,
        adapter: ClaudeAdapter,
        io: Box<dyn AgentIo>,
        config: SessionConfig,
        wake: ClaudeWakeRecipe,
    ) -> Self {
        let capabilities = {
            let mut caps = adapter.capabilities();
            caps.current_model = config.model.clone();
            caps.current_mode = config.mode.clone();
            caps
        };
        let adapter = Arc::new(adapter);
        let io: Arc<dyn AgentIo> = Arc::from(io);
        let turn_gen = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let pending_perms = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let discovered_model = Arc::new(std::sync::Mutex::new(None));
        let discovered_caps = Arc::new(std::sync::Mutex::new(DiscoveredCaps::default()));
        let turn_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Shared with the reader so it can reconcile the OBSERVED mode to claude's
        // `set_permission_mode` control_response (the observed-mode track).
        let current_mode_override = Arc::new(std::sync::Mutex::new(None));
        // #99: shared with the reader so a rejected set_config_option(effort) surfaces
        // a Notice instead of being silently dropped.
        let pending_set_config = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        // B-CLAUDE-INIT: only let the wire fill current_model when config did NOT
        // supply one (config is authoritative; the init frame is the fallback).
        let want_init_model = config.model.is_none();
        let (event_tx, _) = broadcast::channel(1024);

        let stdio = io.take_stdio().await;
        let (stdin, stdout) = match stdio {
            Some((stdin, stdout)) => (Some(stdin), Some(stdout)),
            None => (None, None),
        };

        let reader_state = ClaudeReaderState {
            session_id: session_id.clone(),
            turn_gen: turn_gen.clone(),
            event_tx: event_tx.clone(),
            pending_perms: pending_perms.clone(),
            discovered_model: discovered_model.clone(),
            discovered_caps: discovered_caps.clone(),
            want_init_model,
            turn_in_flight: turn_in_flight.clone(),
            current_mode_override: current_mode_override.clone(),
            pending_set_config: pending_set_config.clone(),
        };
        let reader = start_claude_reader(&reader_state, stdout, io.clone());

        // F-4: own the live {reader, io} in the SuspendController. idle_ttl=None
        // (the default) → no idle timer, slot stays Active for life (production
        // parity). idle_ttl=Some → spawn the per-backend idle timer, which never
        // suspends while a turn is in flight (turn_in_flight gate).
        let suspend = Arc::new(SuspendController::active(
            ProcHandle::new(reader, io),
            config.idle_ttl_ms,
            aionui_common::now_ms(),
        ));
        let idle_timer = {
            let tif = turn_in_flight.clone();
            // 009 R6 cleanup path 3: on an idle-reap suspend, emit BackendSuspended
            // so the orchestrator clears this session's workflow_roster (the process
            // is gone — a running workflow will never deliver its task_notification).
            let etx = event_tx.clone();
            let sid = session_id.clone();
            let tgen = turn_gen.clone();
            spawn_idle_timer(
                &suspend,
                idle_check_interval_ms(config.idle_ttl_ms),
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
            capabilities,
            stdin: Arc::new(Mutex::new(stdin)),
            adapter,
            turn_gen,
            event_tx,
            suspend,
            idle_timer,
            wake,
            reader_state,
            turn_in_flight,
            pending_perms,
            discovered_model,
            discovered_caps,
            pending_controls: Arc::new(Mutex::new(Vec::new())),
            control_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            current_effort: Arc::new(std::sync::Mutex::new(None)),
            current_mode_override,
            pending_set_config,
        }
    }

    /// Wake from Dormant: re-spawn claude with `--resume <claude_session_id>`,
    /// re-take its stdio, swap the fresh stdin into the retained slot, and start a
    /// new reader on the SAME `event_tx`/`turn_gen` — so subscribers and the FSM
    /// never notice the process was recycled. Returns the new `{reader, io}` for
    /// the controller's slot. Only reached when `idle_ttl` is set AND the slot was
    /// suspended (a test backend has no spawner → never enabled).
    async fn wake_handle(&self) -> Result<ProcHandle, BackendError> {
        let legacy_spec = LegacySessionSpec::Resume(self.wake.claude_session_id.clone());
        let io = self
            .adapter
            .start_turn(
                self.wake.spawner.as_ref(),
                &legacy_spec,
                self.wake.cwd.as_deref(),
                &self.wake.extra_args,
                &self.wake.env,
                self.wake.cli_program.as_deref(),
            )
            .await
            .map_err(|e| BackendError::from_spawn("claude resume-spawn failed", e))?;
        let io: Arc<dyn AgentIo> = Arc::from(io);
        let (stdin, stdout) = match io.take_stdio().await {
            Some((stdin, stdout)) => (Some(stdin), Some(stdout)),
            None => (None, None),
        };
        // Swap the fresh stdin into the retained slot so the next `deliver_prompt`
        // writes to the woken process (the old stdin dropped with the old io).
        *self.stdin.lock().await = stdin;
        let reader = start_claude_reader(&self.reader_state, stdout, io.clone());
        Ok(ProcHandle::new(reader, io))
    }

    /// Wire a user permission answer to claude's blocking `can_use_tool` request
    /// (MAJOR-1). Looks up the pending request by `request_id`, builds the keyed
    /// `control_response` claude requires (echoing toolUseID; for AskUserQuestion
    /// injecting the answer into `updatedInput.answers`), writes it over the
    /// retained stdin, and broadcasts `PermissionResolved{request_id}` so the FSM
    /// leaves the requires-action sub-state. Mirrors F1's `answer_permission`.
    async fn answer_permission(
        &self,
        request_id: &str,
        decision: super::types::PermissionDecision,
        selected: Option<&str>,
        answers: &[super::types::QuestionAnswer],
    ) -> Result<CommandReceipt, BackendError> {
        use std::sync::atomic::Ordering;
        let pending = self
            .pending_perms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(request_id);
        let Some(pending) = pending else {
            return Err(BackendError::Transport(format!(
                "no pending permission for request_id {request_id}"
            )));
        };
        let response = build_control_response(request_id, &pending, decision, selected, answers);
        {
            let mut guard = self.stdin.lock().await;
            let stdin = guard
                .as_mut()
                .ok_or_else(|| BackendError::Transport("claude stdin unavailable".into()))?;
            self.adapter
                .write_control_response(stdin, &response)
                .await
                .map_err(|e| BackendError::Transport(format!("write control_response: {e}")))?;
        }
        // RA -1: the reducer leaves requires-action only on PermissionResolved.
        let cur_gen = self.turn_gen.load(Ordering::SeqCst);
        let _ = self.event_tx.send(SessionEnvelope {
            session_id: self.session_id.clone(),
            turn_gen: cur_gen,
            event: SessionEvent::PermissionResolved {
                request_id: request_id.to_string(),
                kind: crate::event::PermissionKind::Tool,
            },
        });
        Ok(CommandReceipt {
            accepted: true,
            admission: Admission::NoTurn,
            turn_gen: cur_gen,
        })
    }

    /// G2: send a host→CLI `control_request` (set_model / set_permission_mode) over
    /// the retained stdin, OR queue it if a turn is in flight. The `turn_in_flight`
    /// flag is the backend's in-band proxy for "Running" (set on Send, cleared by
    /// the reader at the terminal): a switch written mid-turn reinitializes the CLI
    /// session and truncates the in-flight turn, so we defer it to the next prompt
    /// drain. De-duped by subtype (last-write-wins) so repeated switches of the
    /// same kind collapse. Mirrors F1's `write_control_request`.
    /// Returns the minted `ctl-N` request_id so a caller that needs reader-side
    /// reconcile (e.g. `SetConfigOption(effort)` → reject-surfacing) can register it.
    /// Callers that don't care discard it with `let _ =`. The id is returned whether
    /// the frame was written immediately or queued (claude echoes it verbatim in the
    /// control_response either way, after the queue drains).
    /// Whether `value` is an effort level the CURRENT model advertises
    /// (`supportedEffortLevels` → `reasoning_efforts`). The ACP `is_*_valid` semantic
    /// ported to effort: an EMPTY / not-yet-discovered catalog is permissive (the
    /// initialize control_response has not landed, or the model advertises no efforts —
    /// we cannot invalidate against an absent catalog). Only a NON-empty catalog that
    /// omits `value` returns false. Resolves the current model from the discovery
    /// catalog (matching `capabilities()` current_model precedence: config snapshot
    /// first, then the system/init discovered model), falling back to the sole model or
    /// the union of all advertised efforts when the current model is ambiguous.
    fn effort_is_supported(&self, value: &str) -> bool {
        let discovered = self.discovered_caps.lock().unwrap_or_else(|e| e.into_inner());
        if discovered.models.is_empty() {
            // Catalog not yet discovered → cannot validate → permissive.
            return true;
        }
        // Resolve the current model id the same way `capabilities()` does.
        let current = self
            .capabilities
            .current_model
            .clone()
            .or_else(|| self.discovered_model.lock().unwrap_or_else(|e| e.into_inner()).clone());
        // Efforts of the current model if we can pin it; otherwise the union across all
        // advertised models (don't reject a level some selectable model supports when
        // the current model is unknown).
        let efforts: Vec<&str> = match current
            .as_deref()
            .and_then(|id| discovered.models.iter().find(|m| m.id == id))
        {
            Some(model) => model.reasoning_efforts.iter().map(String::as_str).collect(),
            None => discovered
                .models
                .iter()
                .flat_map(|m| m.reasoning_efforts.iter().map(String::as_str))
                .collect(),
        };
        // A model (or the union) with no advertised efforts → permissive (absent
        // catalog can't invalidate, same as ACP empty-catalog semantics).
        efforts.is_empty() || efforts.contains(&value)
    }

    async fn write_or_queue_control(&self, request: serde_json::Value) -> Result<String, BackendError> {
        use std::sync::atomic::Ordering;
        let request_id = format!("ctl-{}", self.control_seq.fetch_add(1, Ordering::SeqCst) + 1);
        let frame = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": request,
        });
        if self.turn_in_flight.load(Ordering::SeqCst) {
            let subtype = control_subtype(&frame);
            let mut q = self.pending_controls.lock().await;
            q.retain(|f| control_subtype(f) != subtype);
            q.push(frame);
            return Ok(request_id);
        }
        self.write_control_frame(&frame).await?;
        Ok(request_id)
    }

    /// G-A: interrupt the in-flight turn — write `control_request{subtype:"interrupt"}`
    /// over the retained stdin IMMEDIATELY (NOT queued: unlike set_model, an interrupt
    /// is only meaningful while a turn is running, which is exactly when cancel fires).
    /// SDK parity with `query.interrupt()`; probe-verified (claude 2.1.168) to end the
    /// turn ~immediately without killing the persistent process. Best-effort — a
    /// stdin-closed error means the process already exited (the turn ends on teardown),
    /// so we log at debug and let the cancel succeed (the FSM has already unlocked).
    async fn interrupt_turn(&self) {
        use std::sync::atomic::Ordering;
        let request_id = format!("ctl-{}", self.control_seq.fetch_add(1, Ordering::SeqCst) + 1);
        let frame = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "interrupt" },
        });
        if let Err(e) = self.write_control_frame(&frame).await {
            tracing::debug!(
                session_id = %self.session_id,
                error = %e,
                "claude interrupt not written (stdin closed?); the turn ends on teardown"
            );
        }
    }

    /// Drain any queued in-band control_requests over the stdin lock — IN ORDER,
    /// BEFORE the prompt — so a switch queued mid-turn applies to THIS next turn and
    /// can never land after-and-truncate it. Called at the head of `dispatch(Send)`.
    async fn drain_pending_controls(&self) -> Result<(), BackendError> {
        let drained: Vec<serde_json::Value> = {
            let mut q = self.pending_controls.lock().await;
            if q.is_empty() {
                return Ok(());
            }
            std::mem::take(&mut *q)
        };
        for frame in drained {
            self.write_control_frame(&frame).await?;
        }
        Ok(())
    }

    /// #98/#101: send `control_request{initialize}` so claude replies with its
    /// discovery catalog (`response.models[]` + `commands[]`). The reader sniffs the
    /// success control_response into `discovered_caps`; `capabilities()` merges it.
    /// Best-effort (no turn in flight at open, so it writes immediately, not queued);
    /// a write error is swallowed — an empty catalog degrades the pickers, never the
    /// turn path.
    async fn request_initialize(&self) {
        use std::sync::atomic::Ordering;
        let request_id = format!("ctl-{}", self.control_seq.fetch_add(1, Ordering::SeqCst) + 1);
        let frame = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "initialize" },
        });
        if let Err(e) = self.write_control_frame(&frame).await {
            tracing::debug!(error = %e, "claude initialize control_request not sent (catalog stays empty)");
        }
    }

    /// Frame + flush one control_request over the retained stdin (same NDJSON path
    /// as a control_response). The CLI's success control_response is observed by the
    /// reader, not awaited here (the switch applies to the next turn).
    async fn write_control_frame(&self, frame: &serde_json::Value) -> Result<(), BackendError> {
        let mut guard = self.stdin.lock().await;
        let stdin = guard
            .as_mut()
            .ok_or_else(|| BackendError::Transport("claude stdin unavailable".into()))?;
        self.adapter
            .write_control_response(stdin, frame)
            .await
            .map_err(|e| BackendError::Transport(format!("write control_request: {e}")))
    }
}

/// The `request.subtype` of a control_request frame (set_model / set_permission_mode),
/// used to de-dup the pending-controls queue (last-write-wins per kind).
fn control_subtype(frame: &serde_json::Value) -> Option<String> {
    frame
        .get("request")
        .and_then(|r| r.get("subtype"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Build the keyed claude `control_response` for a permission decision. Echoes
/// `request_id` INSIDE `response` (claude's correlation key) + `toolUseID`. For
/// AskUserQuestion an allow injects the chosen answer(s) into
/// `updatedInput.answers` (claude silently drops any unanswered question — NOT a
/// re-ask — so an under-answer is silent data loss). The coarse `PermissionDecision`
/// maps Approved/AllowAlways→allow, Denied→deny.
///
/// AskUserQuestion answers (task #83, live-captured 2.1.178):
/// - `answers` (the FULL per-question set) wins when non-empty: every question
///   claude asked is keyed by its TEXT, the value is the chosen label (single) or a
///   JSON array of labels (multiSelect — claude's zod preprocess joins it with ", ").
/// - else degrade to the single-question path: the explicit `selected` label, else
///   the first option (a plain allow with no specific pick). Keeps single-question /
///   single-select working unchanged.
///
/// Mirrors F1 control.rs:build_permission_result.
fn build_control_response(
    request_id: &str,
    pending: &PendingPerm,
    decision: super::types::PermissionDecision,
    selected: Option<&str>,
    answers: &[super::types::QuestionAnswer],
) -> serde_json::Value {
    use super::types::PermissionDecision;
    use serde_json::json;
    let allow = matches!(decision, PermissionDecision::Approved | PermissionDecision::AllowAlways);
    let result = if !allow {
        json!({ "behavior": "deny", "message": "User rejected the request.", "toolUseID": pending.tool_use_id })
    } else if pending.tool_name == "AskUserQuestion" {
        let answers_map = build_ask_user_question_answers(&pending.input, selected, answers);
        let mut updated = pending.input.clone();
        if let serde_json::Value::Object(map) = &mut updated {
            map.insert("answers".to_string(), answers_map);
        } else {
            updated = json!({ "answers": answers_map });
        }
        json!({ "behavior": "allow", "updatedInput": updated, "toolUseID": pending.tool_use_id })
    } else {
        // claude's stdio control-response schema REQUIRES `updatedInput` (a record) on
        // the allow branch (unlike the SDK's in-process canUseTool schema where it is
        // .optional()). Omitting it makes claude's ZodError reject the whole union
        // (`updatedInput: expected record, received undefined`) → "Tool permission
        // request failed" → the approved tool never runs (Write/Bash etc. all fail).
        // Echo the original tool input unchanged ("run with this input"); fall back to
        // an empty object if (defensively) it is not a record so the frame stays valid.
        let updated_input = if pending.input.is_object() {
            pending.input.clone()
        } else {
            json!({})
        };
        json!({ "behavior": "allow", "updatedInput": updated_input, "toolUseID": pending.tool_use_id })
    };
    json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": request_id, "response": result }
    })
}

/// Build the `updatedInput.answers` object claude's AskUserQuestion reads (live
/// wire 2.1.178, keyed by question TEXT; multi-select value = JSON array of labels
/// which claude joins with ", "). Two sources, in order:
///   1. `answers` (full per-question set) — used verbatim when non-empty. A single
///      label serializes as a string, multiple as an array.
///   2. degrade to the single-question path when `answers` is empty: answer ONLY
///      the first question with `selected` (else its first option). This preserves
///      the prior single-question / single-select behavior (plain allow, ACP, etc.).
fn build_ask_user_question_answers(
    input: &serde_json::Value,
    selected: Option<&str>,
    answers: &[super::types::QuestionAnswer],
) -> serde_json::Value {
    use serde_json::json;
    if !answers.is_empty() {
        let map: serde_json::Map<String, serde_json::Value> = answers
            .iter()
            .map(|a| {
                // One label → bare string; many → array (claude accepts either and
                // joins arrays with ", "). An empty `labels` degrades to "".
                let value = match a.labels.as_slice() {
                    [] => json!(""),
                    [one] => json!(one),
                    many => json!(many),
                };
                (a.question.clone(), value)
            })
            .collect();
        return serde_json::Value::Object(map);
    }
    // Degrade: single-question path keyed by the FIRST question's text.
    let q0 = input
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .and_then(|qs| qs.first());
    let question = q0
        .and_then(|q| q.get("question").and_then(serde_json::Value::as_str))
        .unwrap_or("");
    let label = selected
        .map(str::to_string)
        .or_else(|| {
            q0.and_then(|q| q.get("options").and_then(serde_json::Value::as_array))
                .and_then(|opts| opts.first())
                .and_then(|o| o.get("label").and_then(serde_json::Value::as_str))
                .map(str::to_string)
        })
        .unwrap_or_default();
    json!({ question: label })
}

impl Drop for ClaudeSessionBackend {
    /// Parity with codex M5: abort the live reader so its `Arc<dyn AgentIo>` clone
    /// is released and a mid-turn-dropped / hung claude process is reaped
    /// (kill_on_drop) instead of leaking. `abort_on_drop` reaches the controller's
    /// mirrored AbortHandle without awaiting the async slot (Drop cannot await).
    /// Also stop the per-backend idle timer if one was running.
    fn drop(&mut self) {
        self.suspend.abort_on_drop();
        if let Some(timer) = &self.idle_timer {
            timer.abort();
        }
    }
}

/// The idle-check cadence for a given ttl: poll at ~ttl/4 (bounded 1s..=30s) so a
/// suspend fires within a quarter-ttl of going idle without a busy loop. Only
/// consulted when `idle_ttl` is Some (else no timer is spawned).
fn idle_check_interval_ms(idle_ttl_ms: Option<i64>) -> u64 {
    match idle_ttl_ms {
        Some(ttl) => ((ttl / 4).clamp(1_000, 30_000)) as u64,
        None => 30_000,
    }
}

/// DIAGNOSTIC (env-gated, default OFF): when `AIONUI_CLAUDE_WIRE_DUMP` is set, log
/// the RAW stdin/stdout bytes claude exchanges. This is the only way to settle
/// "send accepted but no output frames" — it shows whether the CLI returned ANY
/// bytes after a prompt (CLI hang) vs returned bytes the parser dropped. OFF by
/// default because it logs full prompt/output content (the AGENTS.md sensitive-payload
/// rule forbids that in normal production); it is a deliberate debugging switch a
/// developer turns on to reproduce, never enabled by default.
fn claude_wire_dump_enabled() -> bool {
    std::env::var("AIONUI_CLAUDE_WIRE_DUMP").is_ok_and(|v| v != "0" && !v.is_empty())
}

/// Emit one raw-bytes wire line (direction + conv + turn_gen + byte count + a
/// lossy-UTF8 preview, truncated). Only called when the dump gate is on.
fn dump_wire(direction: &str, session_id: &str, turn_gen: u64, bytes: &[u8]) {
    const MAX: usize = 4096;
    let preview = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX)]);
    tracing::info!(
        target: "aionui_session::claude_wire",
        direction,
        conversation_id = %session_id,
        turn_gen,
        byte_len = bytes.len(),
        truncated = bytes.len() > MAX,
        preview = %preview,
        "claude wire bytes"
    );
}

/// The long-lived stdout reader: drain → parse → wrap (stamp live turn_gen) →
/// broadcast. Owns its own `ClaudeAdapter` parse buffer (persists across turns,
/// the persistent process's stdout does not EOF between turns). On EOF/exit it
/// surfaces `Detached{exit}` so the FSM resolves (no `wait_for_exit` on the seam).
#[allow(clippy::too_many_arguments)]
async fn reader_task(
    session_id: String,
    stdout: Option<aionui_process::BoxedStdout>,
    io: Arc<dyn AgentIo>,
    turn_gen: Arc<std::sync::atomic::AtomicU64>,
    event_tx: broadcast::Sender<SessionEnvelope>,
    pending_perms: Arc<std::sync::Mutex<std::collections::HashMap<String, PendingPerm>>>,
    discovered_model: Arc<std::sync::Mutex<Option<String>>>,
    discovered_caps: Arc<std::sync::Mutex<DiscoveredCaps>>,
    want_init_model: bool,
    turn_in_flight: Arc<std::sync::atomic::AtomicBool>,
    current_mode_override: Arc<std::sync::Mutex<Option<String>>>,
    pending_set_config: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
) {
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncReadExt;

    let Some(mut stdout) = stdout else {
        // stdio could not be taken → emit a terminal Detached so nothing hangs.
        // Startup double-take guard: no stderr to attribute → G2 summary None.
        turn_in_flight.store(false, Ordering::SeqCst);
        let cur_gen = turn_gen.load(Ordering::SeqCst);
        let _ = event_tx.send(SessionEnvelope {
            session_id,
            turn_gen: cur_gen,
            event: SessionEvent::Detached {
                exit: None,
                redacted_summary: None,
            },
        });
        return;
    };

    let mut parser = ClaudeAdapter::new();
    let mut chunk = [0u8; 4096];
    // Startup-only zero-frame liveness check (resume-hang); armed below at the read
    // loop. `seen_frame` is process-level + ONE-SHOT — see the loop comment for the
    // full rationale and the deliberate "single turns are never timed" scope.
    let mut seen_frame = false;
    // Process one batch of parsed frames (from `frame_lines` OR `flush_tail`):
    // sniff the raw frame for permission/init/subagent side-channels, then
    // broadcast each canonical event. Shared so the EOF tail-flush (009 R1a)
    // runs the IDENTICAL processing as the live loop — a truncated final frame
    // must not be handled any differently than a `\n`-terminated one.
    // Bug-A fix (claude-only, proactive=true): the epoch of the wire turn currently
    // OPEN, locked at its `system/init` (claude's authoritative turn-open boundary,
    // §3.5). A `TurnResult` is stamped with THIS, not the read-time `turn_gen`, so a
    // trailing result from a turn that was cancelled/superseded by a proactive resend
    // keeps its OWN (older) turn's epoch and the reducer's cross-turn guard
    // (result_epoch < since_epoch) drops it. Read-time stamping mis-attributed it: the
    // resend's eager turn_gen bump lands BEFORE the late result is read (probe
    // `_all_zerogap_cancel.jsonl` C: same-ms), so the cancelled turn's `is_error`
    // result was stamped the NEW turn's epoch → not dropped → spurious Error bubble.
    // `init`↔`result` is 1:1 and ordered (§3.5), and the late result is always read
    // BEFORE the next turn's init, so turn_open_epoch is still the old turn's value
    // when it arrives. 0 = no turn opened yet → fall back to read-time (no regression
    // for the first frames). See protocols/design/claude-midturn-input-turn-gen-design.md §4-A.
    let mut turn_open_epoch: u64 = 0;
    let mut process_batch = |batch: Vec<(Option<serde_json::Value>, Vec<SessionEvent>)>| {
        let cur_gen = turn_gen.load(Ordering::SeqCst);
        for (raw, events) in batch {
            if let Some(v) = &raw {
                // Lock the open-turn epoch at the authoritative turn-open boundary
                // (system/init). Every subsequent result of THIS turn is stamped with
                // it until the next turn's init re-locks it (bug-A, see above).
                if v.get("type").and_then(serde_json::Value::as_str) == Some("system")
                    && v.get("subtype").and_then(serde_json::Value::as_str) == Some("init")
                {
                    turn_open_epoch = cur_gen;
                }
                register_or_clear_pending(v, &pending_perms);
                // B-CLAUDE-INIT: sniff the system/init frame for the current
                // model + MCP server statuses (the init broadcast the legacy
                // parse_system drops). Done on the RAW frame so parse_chunk's
                // event stream stays zero-diff. Emits Provisioning per MCP
                // server (parity with codex mcpServerStatus→Provisioning).
                sniff_init(v, want_init_model, &discovered_model, &event_tx, &session_id, cur_gen);
                // #98/#101: sniff the `control_request{initialize}` RESPONSE for the
                // selectable model list + slash commands (claude's only catalog
                // channel — the data init frame above carries neither). Fills
                // discovered_caps; capabilities() merges it on read. Done on the RAW
                // frame (parse_chunk drops control frames to opaque).
                sniff_control_initialize(v, &discovered_caps, &event_tx, &session_id, cur_gen);
                // AUTHORITATIVE mode signal (design §9.10.1 option A / README #10):
                // claude stamps `permissionMode` on system/init AND system/status. This
                // single inbound path confirms EVERY mode change — user-driven (a
                // set_permission_mode also yields a system/status) AND autonomous (claude
                // exits plan mode on its own → emits ONLY system/status). It replaces the
                // old optimistic dispatch emit (de-optimistic'd). normal→default normalized;
                // dedups so a repeated init/status echo of the same mode is silent.
                sniff_mode(v, &current_mode_override, &event_tx, &session_id, cur_gen);
                // The ONE case system/status can't cover: a REJECTED set_permission_mode
                // (claude refused → no status, only a control_response error). Clears the
                // stale override + surfaces mode_switch_rejected.
                sniff_mode_reject(v, &current_mode_override, &event_tx, &session_id, cur_gen);
                // #99: the analogue for set_config_option(effort). An effort REJECTION
                // (claude returns control_response{error} for a bad effort value) matched
                // no handler before and was SILENTLY DROPPED. Routed by the ctl-id we
                // minted + registered in pending_set_config → surface a Notice{Warning}.
                // SUCCESS is silent (claude does not echo effort); the entry is just removed.
                sniff_set_config_reject(v, &pending_set_config, &event_tx, &session_id, cur_gen);
                // NO set_model reader-side reconcile (design §9.10.1, Optimistic tier).
                // LIVE-PROBED (2.1.187, protocols/samples/claude-cli/2.1.187/_all_set_model.jsonl):
                // claude's set_model control_response is a BARE {subtype:"success"} — no
                // model echoed, a bogus id ALSO returns success — AND an in-band set_model
                // emits NO fresh system/init (two set_model sends → zero subsequent init).
                // So there is NO inbound signal at all to confirm/reconcile the switch (the
                // official Agent SDK treats set_model as fire-and-forget for the same reason).
                // dispatch(SetModel) emits ConfigChanged{model} optimistically and that is
                // final in-band; a bad id surfaces only when the next turn USES it (API 404).
                // (Do NOT add a reconcile keyed on a fresh system/init — it never arrives
                // in-band; a prior comment wrongly claimed "read back from the next turn's
                // system/init", disproved.) set_permission_mode is different: its ack DOES
                // echo response.mode (sniff_set_mode_response / sniff_mode real).
                //
                // PARTIAL correction (gap-reaudit): the binary 2.1.191 set_model handler
                // HAS a synchronous error branch for ids that fail an allowlist — but
                // `Na` returns true for ANY id when NO model allowlist is configured, so
                // on our Bedrock path (no allowlist) even a bogus id returns success
                // (matches the 2.1.187 probe). A real reject control_response{subtype:error}
                // therefore ONLY fires in allowlist/restricted-model orgs — a shape we have
                // NOT live-probed. Per the "no parser for an unprobed shape" discipline we do
                // NOT speculatively wire a sniff_set_model_reject; FOLLOW-UP gated on capturing
                // that reject frame in a restricted-model environment, then route it by the
                // `ctl-N` request_id we mint (not by guessing the error string).
                // QuerySessionInfo reply: claude answers our in-band
                // `control_request{get_context_usage|get_session_cost}` with a success
                // control_response keyed by our `ctl-qsi-{usage|cost}-N` request_id.
                // Sniff it → SessionEvent::SessionInfo (the cumulative context-budget /
                // cost snapshot the user asked for). Done on the RAW frame.
                sniff_session_info(v, &event_tx, &session_id, cur_gen);
                // Subagent roster: claude emits system/task_* frames for
                // Task/Workflow subagents (§6b b1). Translate them to
                // SubagentUpdate so the reducer upserts Running.subagents —
                // which drives has_activity (a subagent still running keeps
                // the spinner on even while the main turn blocks on approval).
                // Done on the RAW frame (parse_chunk drops task_* to opaque).
                sniff_task(v, &event_tx, &session_id, cur_gen);
                // B (regression A): claude's NATIVE prompt-ack. A replayed user frame
                // (--replay-user-messages) carrying OUR stamped `uuid` means claude has
                // truly consumed that message into THIS turn → emit PromptAccepted so
                // the conversation drains the matching pending head Sent→Accepted. Only
                // a frame whose uuid is a non-empty string we could have stamped fires;
                // claude-MINTED user frames (tool_result, the [Request interrupted]
                // ghost) carry claude's own uuid and simply won't match any outstanding
                // client_msg_id downstream (drain_pending_on is a precise single-id
                // match → no-op), so this stays safe even though we can't tell them
                // apart at the wire. Done on the RAW frame.
                sniff_replay_prompt_ack(v, &event_tx, &session_id, cur_gen);
            }
            for ev in events {
                // F-4: a terminal clears the turn-active flag so the idle
                // timer may suspend the now-idle process (it was held
                // resident for the whole turn). Cleared BEFORE the broadcast
                // so the flag is already false when subscribers react.
                if matches!(ev, SessionEvent::TurnResult { .. }) {
                    turn_in_flight.store(false, Ordering::SeqCst);
                }
                // Bug-A: a TurnResult is stamped with the OPEN turn's locked epoch
                // (set at this turn's system/init), NOT the read-time turn_gen — so a
                // late result from a turn superseded by a proactive resend keeps its
                // own (older) epoch and the reducer's cross-turn guard drops it. All
                // OTHER events keep the read-time epoch (they belong to the live turn
                // and carry no cross-turn staleness). turn_open_epoch==0 (no init yet)
                // falls back to cur_gen (first-frames / no regression). The
                // orchestrator's restamp_epoch then propagates this into
                // TurnResult.epoch (it copies env.turn_gen when the adapter left 0).
                let env_gen = if matches!(ev, SessionEvent::TurnResult { .. }) && turn_open_epoch != 0 {
                    turn_open_epoch
                } else {
                    cur_gen
                };
                let _ = event_tx.send(SessionEnvelope {
                    session_id: session_id.clone(),
                    turn_gen: env_gen,
                    event: ev,
                });
            }
        }
    };
    // Startup-only zero-frame liveness (resume-hang). A claude `--resume <id>` whose
    // on-disk session is a broken/empty husk hangs the spawned process (0% CPU,
    // sleeping) — it emits NO stream-json frame and never EOFs, so this read would
    // park forever, the turn never terminates, and the UI locks permanently. The
    // existing crash self-heal can't help (it keys on an Error terminal, which a
    // hung non-exiting process never produces).
    //
    // The guard is deliberately STARTUP-ONLY: we bound the read by `handshake_budget`
    // ONLY until the process has produced its very first frame; once it proves it is
    // alive (any frame: system/init, replay, anything), `seen_frame` latches true and
    // the read goes UNBOUNDED for the rest of the process's life. A long single turn
    // is NEVER timed — by owner decision (a turn that thinks/tools for minutes is
    // normal and must not be killed). This catches the real wedge (a spawn/resume
    // that hangs before emitting anything) without risking a healthy in-progress turn.
    //
    // On the hung verdict we surface a terminal Detached{exit:None} (→ reducer
    // Error{Crashed} → UI unlocks; the husk is reaped by the next get_or_build
    // eviction's kill_on_drop). We do NOT call wait_for_exit on a hang — a live
    // process would block it forever. `read` is cancel-safe so the bounded read
    // loses no bytes if the timer elapses.
    let mut hung = false;
    // Set when the parse+broadcast path panicked (see the catch_unwind in the read
    // loop). Like `hung`, it routes to a terminal Detached without waiting on the
    // process (which is still alive) so the turn ends as a crash instead of hanging.
    let mut panicked = false;
    // Windows pipe-EOF gap (F48-adjacent): claude's stdout write handle can be
    // inherited by a surviving grandchild (a detached MCP/tool descendant). When the
    // user kills the claude leaf, the pipe's write end is NOT fully closed while such
    // a descendant lives, so `stdout.read()` NEVER returns 0 — the reader would park
    // forever, `Detached` would never fire, and the UI would wedge at `pending` with
    // no error. (macOS has close-on-exec on the fd, so EOF is prompt there and this
    // race never wins — but the guard is unconditional: it is correct on every OS and
    // simply never fires when EOF/error already terminate first.) So we cannot rely on
    // EOF alone; we race the unbounded read against the process's exit watch
    // (`io.wait_for_exit()`, backed by a cancel-safe `watch::Receiver` over the direct
    // child's `child.wait()` — orthogonal to the stdout pipe). When the exit leg wins,
    // `proc_exited` carries the status so the terminal `Detached` reuses it instead of
    // re-awaiting `wait_for_exit` (which would race a second borrow / re-resolve).
    let mut proc_exited: Option<Option<crate::event::ExitStatusLite>> = None;
    loop {
        // DIAGNOSTIC: mark each read-loop iteration entry. If the log shows this line
        // but then NO matching stdout/eof/error outcome for a long time, the reader is
        // blocked inside `stdout.read().await` waiting for bytes claude never sends
        // (the suspected resume stall) — vs the loop not running at all.
        if claude_wire_dump_enabled() {
            tracing::debug!(
                target: "aionui_session::claude_wire",
                direction = "read",
                conversation_id = %session_id,
                outcome = "awaiting",
                seen_frame,
                "claude stdout read: awaiting bytes"
            );
        }
        let read = if seen_frame {
            // Proven alive → unbounded read (a long turn is never timed), BUT raced
            // against the process's exit watch so a Windows pipe-EOF stall (a surviving
            // grandchild holding the write end → no `Ok(0)` ever) still terminates the
            // turn. Both `select!` legs are cancel-safe: `stdout.read` is; `wait_for_exit`
            // is a `watch::Receiver::changed()` (loses nothing when the read leg wins).
            tokio::select! {
                biased;
                // Prefer the read: while bytes are flowing we must drain them (a turn
                // that also just exited still has its `result` frame to deliver). The
                // exit leg only wins once the read is genuinely parked with no bytes.
                r = stdout.read(&mut chunk) => r.map_err(|_| ()),
                exit = io.wait_for_exit() => {
                    // The direct child exited but stdout has not EOF'd (the Windows
                    // inherited-handle case). Do NOT tear down yet: the pipe buffer may
                    // still hold the final `result` frame. Bounded-drain it (EOF may
                    // never come, so we cannot wait for `Ok(0)`), then break to the
                    // existing terminal path with the captured exit status.
                    if claude_wire_dump_enabled() {
                        tracing::info!(
                            target: "aionui_session::claude_wire",
                            direction = "read",
                            conversation_id = %session_id,
                            outcome = "process_exited",
                            "claude process exited while stdout still open (no EOF); bounded-draining tail"
                        );
                    }
                    loop {
                        match tokio::time::timeout(std::time::Duration::from_millis(200), stdout.read(&mut chunk)).await {
                            // More buffered bytes: process them exactly as the live loop
                            // would (same panic net → `panicked` short-circuits the drain).
                            Ok(Ok(n)) if n > 0 => {
                                if claude_wire_dump_enabled() {
                                    dump_wire("stdout", &session_id, turn_gen.load(Ordering::SeqCst), &chunk[..n]);
                                }
                                let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    parser.frame_lines(&chunk[..n])
                                }));
                                match parsed {
                                    Ok(batch) => process_batch(batch),
                                    Err(_) => {
                                        tracing::error!(
                                            target: "aionui_session::backend::claude_conn",
                                            conversation_id = %session_id,
                                            "claude frame parser panicked during post-exit drain; ending turn as crash"
                                        );
                                        panicked = true;
                                        break;
                                    }
                                }
                            }
                            // Drain complete (EOF, error, or the 200ms budget elapsed
                            // with no more bytes) → stop draining.
                            _ => break,
                        }
                    }
                    // Remember the captured status so the terminal path below reuses it
                    // (do NOT re-await wait_for_exit). `Some(None)` = exited, status
                    // unknown (WaitErrored) — still a real terminal, distinct from `hung`.
                    proc_exited = Some(exit);
                    break;
                }
            }
        } else {
            // Startup window: bound the FIRST frame by the handshake budget.
            match tokio::time::timeout(super::handshake_budget(), stdout.read(&mut chunk)).await {
                Ok(r) => r.map_err(|_| ()),
                Err(_) => {
                    // Budget elapsed before the process emitted ANY frame → wedged
                    // startup (e.g. a broken --resume). Terminal Detached unsticks it.
                    // DIAGNOSTIC: this is the silent "startup read timed out" path —
                    // distinguishes "claude produced NO bytes at all" from a parse issue.
                    if claude_wire_dump_enabled() {
                        tracing::info!(
                            target: "aionui_session::claude_wire",
                            direction = "read",
                            conversation_id = %session_id,
                            outcome = "startup_timeout",
                            "claude stdout read: startup budget elapsed with zero frames"
                        );
                    }
                    hung = true;
                    break;
                }
            }
        };
        match read {
            Ok(0) => {
                // DIAGNOSTIC: EOF — claude closed stdout (process winding down). Logged
                // because a silent EOF mid-conversation (vs claude staying alive but
                // quiet) is a completely different root cause.
                if claude_wire_dump_enabled() {
                    tracing::info!(
                        target: "aionui_session::claude_wire",
                        direction = "read",
                        conversation_id = %session_id,
                        outcome = "eof",
                        "claude stdout read: EOF (stdout closed)"
                    );
                }
                break; // EOF: process winding down
            }
            Ok(n) => {
                // DIAGNOSTIC: raw stdout bytes BEFORE parsing — shows exactly what the
                // CLI returned (incl. frames the parser would drop to opaque).
                if claude_wire_dump_enabled() {
                    dump_wire("stdout", &session_id, turn_gen.load(Ordering::SeqCst), &chunk[..n]);
                }
                seen_frame = true; // proven alive — disarm the startup guard for life
                // `frame_lines` gives BOTH the raw frame Value AND the parsed
                // events from ONE parse (no double-parse).
                //
                // Panic-safety net (class defence, NOT a root-cause substitute):
                // a panic anywhere in the parse+broadcast path (e.g. a byte-index
                // String op that splits a UTF-8 char, an unchecked index on wire
                // data) would otherwise unwind THIS task silently — dropping stdout
                // WITHOUT emitting a terminal, so the pump blocks forever and the
                // conversation is wedged at `pending`. Catching it here downgrades
                // ANY future parser panic to "this turn crashed" (terminal Detached
                // below → reducer Error{Crashed} → UI unlocks) instead of a permanent
                // hang. `AssertUnwindSafe` is sound: on a caught panic we STOP reading
                // and tear down, so no partially-mutated parser/state is reused.
                let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.frame_lines(&chunk[..n])));
                match parsed {
                    Ok(batch) => process_batch(batch),
                    Err(_) => {
                        // error level: a parser panic is a contract violation we must be
                        // able to diagnose in production. No payload (no frame bytes /
                        // prompt) — only the fact + location context via the panic hook.
                        tracing::error!(
                            target: "aionui_session::backend::claude_conn",
                            conversation_id = %session_id,
                            "claude frame parser panicked; ending turn as crash (see panic hook for location)"
                        );
                        panicked = true;
                        break; // → terminal Detached path
                    }
                }
            }
            Err(()) => {
                // DIAGNOSTIC: stdout read errored (pipe broken / process gone) — a
                // distinct terminal cause from a clean EOF or a quiet-but-alive process.
                if claude_wire_dump_enabled() {
                    tracing::info!(
                        target: "aionui_session::claude_wire",
                        direction = "read",
                        conversation_id = %session_id,
                        outcome = "error",
                        "claude stdout read: I/O error (pipe broken)"
                    );
                }
                break; // read error → terminal
            }
        }
    }

    // 009 R1a: drain-before-honor a truncated final frame. If the process died
    // mid-write (OOM/SIGKILL during the `result` line), the trailing half-line
    // is still in the parser buffer; flush it as a final frame BEFORE the
    // terminal Detached so its content/result is not silently lost (and the turn
    // is not misclassified as empty). A clean EOF on a `\n` boundary flushes
    // nothing. Skipped on a parse panic: the parser buffer holds the very bytes
    // that just panicked, so re-parsing them via flush_tail would panic AGAIN
    // (this time uncaught) — go straight to the terminal.
    if !panicked {
        process_batch(parser.flush_tail());
    }

    // EOF/exit is terminal too → clear the turn flag (the process is gone).
    turn_in_flight.store(false, Ordering::SeqCst);
    // A zero-frame hang OR a parse panic leaves the process ALIVE — `wait_for_exit`
    // would block forever, so skip it and report `exit: None` (the reducer maps a
    // None-exit Detached to Error{Crashed}, same as an unknown-status exit). The husk
    // process is reaped by the next get_or_build eviction (kill_on_drop). If the exit
    // watch ALREADY won the read race (`proc_exited`), reuse that captured status — do
    // NOT re-await `wait_for_exit` (the process is gone; re-awaiting is redundant and
    // the status is in hand). Otherwise (clean EOF / read error) wait for and redact
    // the exit as before. `peek_stderr` is still safe on either path (it reads the
    // buffered tail, never blocks on the process).
    let exit = if hung || panicked {
        None
    } else if let Some(captured) = proc_exited {
        captured
    } else {
        io.wait_for_exit().await
    };
    // G2: redact the stderr tail at the backend boundary so a crash carries a
    // user-facing reason (allowlisted, ≤240 chars) without leaking raw stderr.
    let redacted_summary = crate::adapter::redact_exit_stderr(io.as_ref()).await;
    let cur_gen = turn_gen.load(Ordering::SeqCst);
    let _ = event_tx.send(SessionEnvelope {
        session_id,
        turn_gen: cur_gen,
        event: SessionEvent::Detached { exit, redacted_summary },
    });
}

/// Sniff a raw claude frame: register a `can_use_tool` control_request into the
/// pending-permission map (so `AnswerPermission` can build the keyed response),
/// and clear it on a `control_cancel_request` (claude retracted it). Mirrors F1's
/// `ControlChannel::register`/`cancel`. No-op for any other frame.
fn register_or_clear_pending(
    frame: &serde_json::Value,
    pending: &Arc<std::sync::Mutex<std::collections::HashMap<String, PendingPerm>>>,
) {
    use serde_json::Value;
    match frame.get("type").and_then(Value::as_str) {
        Some("control_request") => {
            let request = frame.get("request");
            if request.and_then(|r| r.get("subtype")).and_then(Value::as_str) != Some("can_use_tool") {
                return;
            }
            let Some(request_id) = frame.get("request_id").and_then(Value::as_str) else {
                return;
            };
            let request = request.unwrap();
            let tool_use_id = request.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
            if tool_use_id.is_empty() {
                return; // can't echo toolUseID → can't answer; parse degrades it to opaque too
            }
            pending.lock().unwrap_or_else(|e| e.into_inner()).insert(
                request_id.to_string(),
                PendingPerm {
                    tool_use_id: tool_use_id.to_string(),
                    tool_name: request
                        .get("tool_name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    input: request.get("input").cloned().unwrap_or(Value::Null),
                },
            );
        }
        Some("control_cancel_request") => {
            // claude retracted the request → drop the pending (it can no longer be
            // answered; the b-side PermissionResolved already clears the FSM count).
            if let Some(request_id) = frame.get("request_id").and_then(Value::as_str) {
                pending.lock().unwrap_or_else(|e| e.into_inner()).remove(request_id);
            }
        }
        _ => {}
    }
}

/// B-CLAUDE-INIT: sniff a raw `system/init` frame for discovery data the legacy
/// `parse_system` drops. Captures `model` into `discovered_model` (only when
/// `want_init_model`, i.e. config supplied none) and emits a `Provisioning` event
/// per `mcp_servers[]` entry (connected→ToolsReady, failed→LoadFailed,
/// needs-auth→Degraded) — parity with codex `mcpServerStatus→Provisioning`, so a
/// failed/needs-auth MCP server is visible on the claude seam too. No-op for any
/// non-init frame. Done on the raw frame (NOT parse_chunk) to keep zero-diff.
fn sniff_init(
    frame: &serde_json::Value,
    want_init_model: bool,
    discovered_model: &Arc<std::sync::Mutex<Option<String>>>,
    event_tx: &broadcast::Sender<SessionEnvelope>,
    session_id: &str,
    turn_gen: u64,
) {
    use serde_json::Value;
    if frame.get("type").and_then(Value::as_str) != Some("system")
        || frame.get("subtype").and_then(Value::as_str) != Some("init")
    {
        return;
    }
    if want_init_model && let Some(model) = frame.get("model").and_then(Value::as_str) {
        *discovered_model.lock().unwrap_or_else(|e| e.into_inner()) = Some(model.to_string());
    }
    // Addendum 9 parity (codex thread/started, acp session/new|load): lower the
    // authoritative on-disk session id from the init frame as BackendBound, so the
    // conversation persists it as the resume anchor. Emit ONLY when it differs from
    // the logical id we spawned with (a no-rotation session stays silent — the
    // common case where claude was started with `--session-id <logical_id>`); a
    // DIFFERENT id means claude rotated/resumed under another on-disk id, which is
    // the value a later `--resume` must target.
    if let Some(sid) = frame.get("session_id").and_then(Value::as_str)
        && sid != session_id
    {
        let _ = event_tx.send(SessionEnvelope {
            session_id: session_id.to_string(),
            turn_gen,
            event: SessionEvent::BackendBound {
                backend_session_id: Some(sid.to_string()),
            },
        });
    }
    if let Some(servers) = frame.get("mcp_servers").and_then(Value::as_array) {
        for s in servers {
            let name = s.get("name").and_then(Value::as_str).unwrap_or("");
            let phase = match s.get("status").and_then(Value::as_str).unwrap_or("") {
                "connected" => crate::event::ProvisioningPhase::ToolsReady,
                "failed" => crate::event::ProvisioningPhase::LoadFailed {
                    reason: format!("mcp server '{name}' failed"),
                },
                "needs-auth" | "needs_auth" => crate::event::ProvisioningPhase::Degraded {
                    reason: format!("mcp server '{name}' needs auth"),
                },
                // pending/unknown → still provisioning
                _ => crate::event::ProvisioningPhase::ToolsWaiting,
            };
            let _ = event_tx.send(SessionEnvelope {
                session_id: session_id.to_string(),
                turn_gen,
                event: SessionEvent::Provisioning { phase },
            });
        }
    }
}

/// Sniff the AUTHORITATIVE mode signal: claude stamps `permissionMode` on BOTH its
/// `system/init` (turn/session start) AND `system/status` (any mode change) frames.
/// This is the UNIFIED inbound mode-truth — it fires for a user-driven set
/// (`set_permission_mode` also produces a system/status) AND for an AUTONOMOUS change
/// (claude exits plan mode on its own after an approved ExitPlanMode → emits ONLY a
/// system/status, no control_response). LIVE-PROBED 2.1.187
/// (protocols/samples/claude-cli/2.1.187/_all_autonomous_mode.jsonl: set plan →
/// system/status{plan}; autonomous exit → system/status{bypassPermissions}).
///
/// Design §9.10.1 option A (de-optimistic): mode is confirmed by THIS inbound signal,
/// NOT by an optimistic dispatch emit — so dispatch(SetMode) no longer emits
/// ConfigChanged; this is the single path, covering active + autonomous with no gap.
/// (Contracts README discipline #10: never sense only our-own-triggered changes —
/// claude's autonomous plan-exit was dropped because no sniffer read system/status.)
///
/// `normal` → `default` normalization (claude's internal name for our `default`,
/// matching `sniff_set_mode_response`). Adopts the value as the authoritative
/// `current_mode_override` (the picker re-read surface) + emits `ConfigChanged{mode}`.
/// No-op for any non-system frame or a system frame without `permissionMode`.
fn sniff_mode(
    frame: &serde_json::Value,
    current_mode_override: &Arc<std::sync::Mutex<Option<String>>>,
    event_tx: &broadcast::Sender<SessionEnvelope>,
    session_id: &str,
    turn_gen: u64,
) {
    use serde_json::Value;
    if frame.get("type").and_then(Value::as_str) != Some("system") {
        return;
    }
    let Some(raw) = frame.get("permissionMode").and_then(Value::as_str) else {
        return;
    };
    let mode = if raw == "normal" { "default" } else { raw };
    // Reconcile only on a real change so a repeated init/status echo of the same mode
    // does not spam ConfigChanged (it is reducer-ignored, but keep the stream clean).
    {
        let mut cur = current_mode_override.lock().unwrap_or_else(|e| e.into_inner());
        if cur.as_deref() == Some(mode) {
            return;
        }
        *cur = Some(mode.to_string());
    }
    let _ = event_tx.send(SessionEnvelope {
        session_id: session_id.to_string(),
        turn_gen,
        event: SessionEvent::ConfigChanged {
            mode: Some(mode.to_string()),
            model: None,
        },
    });
}

/// #98/#101: sniff a raw `control_response{subtype:"success"}` for the
/// `initialize` reply's discovery catalog and fill `discovered_caps`. The
/// `response` object carries `models[{value, displayName, description,
/// supportsEffort, supportedEffortLevels[]}]` and `commands[{name, description}]`
/// — the selectable model list + slash commands claude advertises (live-probed
/// 2.1.181; fixture protocols/samples/claude-cli/2.1.181/control_initialize_response).
/// This is claude's ONLY catalog channel: the `system/init` DATA frame carries
/// only the current model, and the SDK/ACP `supportedModels()` just forwards this
/// same control response.
///
/// No request_id correlation: `initialize` is the only control_request we send that
/// yields a `models`/`commands`-bearing success response, so a success frame with
/// those keys is unambiguously the initialize reply. No-op for any other frame
/// (can_use_tool success, set_model ack, etc. carry no `models`). Done on the RAW
/// frame (parse_chunk drops control frames to opaque) — keeps the parse zero-diff.
fn sniff_control_initialize(
    frame: &serde_json::Value,
    discovered_caps: &Arc<std::sync::Mutex<DiscoveredCaps>>,
    event_tx: &broadcast::Sender<SessionEnvelope>,
    session_id: &str,
    turn_gen: u64,
) {
    use crate::capability::{ModelInfo, SlashCommandInfo};
    use serde_json::Value;
    if frame.get("type").and_then(Value::as_str) != Some("control_response") {
        return;
    }
    let Some(response) = frame.get("response") else {
        return;
    };
    if response.get("subtype").and_then(Value::as_str) != Some("success") {
        return;
    }
    // The success payload nests the actual init response under `response`.
    let Some(inner) = response.get("response") else {
        return;
    };
    // Only the initialize reply carries `models`; skip any other success response.
    let models = inner.get("models").and_then(Value::as_array);
    let commands = inner.get("commands").and_then(Value::as_array);
    if models.is_none() && commands.is_none() {
        return;
    }
    let parsed_models: Vec<ModelInfo> = models
        .map(|models| {
            models
                .iter()
                .filter_map(|m| {
                    let id = m.get("value").and_then(Value::as_str)?.to_string();
                    let mut reasoning_efforts: Vec<String> = m
                        .get("supportedEffortLevels")
                        .and_then(Value::as_array)
                        .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_string).collect())
                        .unwrap_or_default();
                    // Surface the synthetic `ultracode` level (xhigh + standing dynamic
                    // workflow orchestration) — the CLI's own effort-picker entry — but
                    // only for xhigh-capable models, mirroring the CLI gate. It rides the
                    // same picker + `effort_is_supported` path as real levels; only the
                    // dispatch wire differs (see `ULTRACODE_LEVEL`).
                    if reasoning_efforts.iter().any(|e| e == XHIGH_LEVEL)
                        && !reasoning_efforts.iter().any(|e| e == ULTRACODE_LEVEL)
                    {
                        reasoning_efforts.push(ULTRACODE_LEVEL.to_string());
                    }
                    Some(ModelInfo {
                        name: m.get("displayName").and_then(Value::as_str).unwrap_or(&id).to_string(),
                        description: m.get("description").and_then(Value::as_str).map(str::to_string),
                        reasoning_efforts,
                        id,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let parsed_commands: Vec<SlashCommandInfo> = commands
        .map(|commands| {
            commands
                .iter()
                .filter_map(|c| {
                    let name = c.get("name").and_then(Value::as_str)?.to_string();
                    Some(SlashCommandInfo {
                        name,
                        description: c.get("description").and_then(Value::as_str).map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    {
        let mut caps = discovered_caps.lock().unwrap_or_else(|e| e.into_inner());
        if models.is_some() {
            caps.models = parsed_models.clone();
        }
        if commands.is_some() {
            caps.slash_commands = parsed_commands.clone();
        }
    }
    // Signal the async catalog arrival so the conversation re-projects the picker
    // (the ACP `emit_snapshot_events` analogue). Without this the frontend, which
    // read an empty `config_options` on open, never re-fetches and the model
    // selector stays disabled. Carry claude's fixed permission modes too: the
    // frontend replaces the WHOLE config_options snapshot on this frame, so omitting
    // modes would wipe the (synchronously-available) mode picker — a fresh regression.
    let _ = event_tx.send(SessionEnvelope {
        session_id: session_id.to_string(),
        turn_gen,
        event: SessionEvent::CatalogUpdated {
            models: parsed_models,
            modes: crate::adapter::claude_permission_modes(),
            slash_commands: parsed_commands,
        },
    });
}

/// Handle a REJECTED `set_permission_mode` — the ONE mode signal `sniff_mode` cannot
/// cover. A successful mode change (active or autonomous) is confirmed by the inbound
/// `system/status{permissionMode}` that `sniff_mode` reads (design §9.10.1 option A).
/// But a REJECTED change emits NO system/status (claude refused, so the mode did not
/// change) — it only comes back as a `control_response{subtype:error}`, e.g. a
/// root-rejected bypass ("session was not launched with --dangerously-skip-permissions").
/// We CLEAR any stale override so `capabilities()` reflects the mode claude actually
/// enforces (no lying picker) + surface `AdapterSpecific{tag:"mode_switch_rejected"}`.
///
/// (The success arm of this function was REMOVED: with dispatch(SetMode) de-optimistic'd,
/// the success reconcile is owned solely by `sniff_mode` via system/status — the single
/// inbound path covering user-driven AND autonomous changes, README discipline #10.)
///
/// Self-identifying: a "permission mode" error string distinguishes a mode rejection
/// from other control errors. No-op for any non-error / non-mode frame.
fn sniff_mode_reject(
    frame: &serde_json::Value,
    current_mode_override: &Arc<std::sync::Mutex<Option<String>>>,
    event_tx: &broadcast::Sender<SessionEnvelope>,
    session_id: &str,
    turn_gen: u64,
) {
    use serde_json::Value;
    if frame.get("type").and_then(Value::as_str) != Some("control_response") {
        return;
    }
    let response = frame.get("response").unwrap_or(&Value::Null);
    if response.get("subtype").and_then(Value::as_str) != Some("error") {
        return;
    }
    let err = response.get("error").and_then(Value::as_str).unwrap_or("");
    // Only act on a permission-mode rejection (other control errors — e.g. a failed
    // set_model — are not ours to reconcile here).
    if !err.contains("permission mode") {
        return;
    }
    // The switch did not take → clear any override so capabilities() falls back to the
    // mode claude actually enforces (no lying picker).
    *current_mode_override.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let _ = event_tx.send(SessionEnvelope {
        session_id: session_id.to_string(),
        turn_gen,
        event: SessionEvent::AdapterSpecific {
            tag: "mode_switch_rejected".to_string(),
            payload: serde_json::json!({ "error": err }),
        },
    });
}

/// #99: reconcile a `control_response` for an in-flight `set_config_option(effort)`.
/// claude does NOT echo effort, so a SUCCESS is silent (`capabilities().current_effort`
/// already tracks it optimistically) — we only claim the pending entry. A REJECTION
/// (`control_response{subtype:"error"}` for a bad effort value) matched NO handler
/// before (`sniff_mode_reject` hard-filters on "permission mode") and was silently
/// dropped, so the user never learned the set failed; here we surface it as a
/// `Notice{Warning}` carrying the label + claude's error string. Routed strictly by the
/// `ctl-N` request_id we minted in the SetConfigOption arm, so it never disturbs the
/// permission-mode path (a permission-mode reject has no pending_set_config entry).
fn sniff_set_config_reject(
    frame: &serde_json::Value,
    pending_set_config: &Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    event_tx: &broadcast::Sender<SessionEnvelope>,
    session_id: &str,
    turn_gen: u64,
) {
    use serde_json::Value;
    if frame.get("type").and_then(Value::as_str) != Some("control_response") {
        return;
    }
    let response = frame.get("response").unwrap_or(&Value::Null);
    let Some(request_id) = response.get("request_id").and_then(Value::as_str) else {
        return;
    };
    let is_error = response.get("subtype").and_then(Value::as_str) == Some("error");
    // Claim (remove) the entry only if THIS response is for one of our effort sets.
    let Some(label) = pending_set_config
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(request_id)
    else {
        return;
    };
    if !is_error {
        // Success is silent — claude does not echo effort; the optimistic
        // current_effort already reflects it. Just drop the pending entry (done above).
        return;
    }
    let err = response.get("error").and_then(Value::as_str).unwrap_or("set rejected");
    tracing::error!(
        session_id = %session_id,
        set = %label,
        "claude set_config_option(effort) rejected: {err}"
    );
    let _ = event_tx.send(SessionEnvelope {
        session_id: session_id.to_string(),
        turn_gen,
        event: SessionEvent::Notice {
            level: crate::event::NoticeLevel::Warning,
            message: format!("{label} failed: {err}"),
        },
    });
}

// (set_model has NO reader-side reconcile — see the reader-loop note at the
//  sniff_set_mode_response call site + design §9.10.1: claude's set_model ack is a
//  bare success with no model echo, so the switch is Optimistic, confirmed by the
//  next turn's system/init. A parser here would be permanently inert + self-confirming.)

/// Request-id prefix tagging a `QuerySessionInfo` control_request so the reader can
/// route its success control_response to the right `SessionInfoKind` (claude echoes
/// the request_id verbatim, but the response body for usage vs cost is structurally
/// different, so we disambiguate on the id we minted).
const QSI_USAGE_PREFIX: &str = "ctl-qsi-usage-";
const QSI_COST_PREFIX: &str = "ctl-qsi-cost-";

/// The synthetic reasoning-effort level that mirrors the claude CLI's own interactive
/// effort-picker entry `"ultracode (xhigh + dynamic workflow orchestration; this session
/// only)"`. It is NOT a model-advertised `supportedEffortLevels` value — `fill_discovery`
/// injects it into a model's `reasoning_efforts` (so it surfaces in the picker and passes
/// `effort_is_supported`) ONLY when that model advertises `xhigh`, matching the CLI's gate
/// (`ultracode` requires an xhigh-capable model + dynamic workflows). On dispatch it does
/// NOT ride the `effortLevel` field: it is sent as the dedicated boolean
/// `apply_flag_settings{settings:{ultracode:true}}` — LIVE-PROBED 2.1.206
/// (samples/claude-cli/2.1.206/ultracode_wire.result.md): the flag returns
/// control_response{success} and `get_settings.applied` reads back `{effort:"xhigh",
/// ultracode:true}`, whereas sending `effortLevel:"ultracode"` would be rejected by our
/// own `effort_is_supported` gate since it is absent from `supportedEffortLevels`.
const ULTRACODE_LEVEL: &str = "ultracode";
/// The base effort level `ultracode` extends (and which the CLI auto-forces when the flag
/// is set). Used to gate ultracode injection to xhigh-capable models.
const XHIGH_LEVEL: &str = "xhigh";

/// Sniff the success control_response to a `QuerySessionInfo` (G): claude answers
/// `control_request{get_context_usage}` with `response.response.{totalTokens,
/// maxTokens, categories[]}` and `{get_session_cost}` with `response.response.text`
/// (live-confirmed 2.1.186, samples/claude-cli/2.1.186). Routed by the
/// `ctl-qsi-{usage|cost}-N` request_id we minted. No-op for any other frame.
/// B (regression A): claude's NATIVE prompt-ack via `--replay-user-messages`. When
/// claude consumes one of OUR user messages into a turn it replays that frame with
/// the `uuid` WE stamped (= the conversation's `client_msg_id`, see
/// `ClaudeAdapter::deliver_prompt`). Emitting `PromptAccepted{client_msg_id: uuid}`
/// here drains the matching pending head Sent→Accepted (the bubble flips
/// sending→sent) only once claude has REALLY taken the message — replacing the old
/// flush-ok synthesized emit that lied for a proactively-queued (or cancel-dropped)
/// message. Probe-pinned echo: protocols/samples/claude-cli/2.1.187/_all_replay_uuid.jsonl.
///
/// Guard: only a `type:"user"` frame carrying a NON-EMPTY top-level `uuid` fires.
/// claude also replays frames it MINTED itself (tool_result user frames, the
/// `[Request interrupted]` ghost) with claude's OWN uuid; those won't match any
/// outstanding `client_msg_id` (the conversation's `drain_pending_on` is a precise
/// single-id match → no-op), so emitting for them is harmless. We do NOT try to
/// distinguish minted-vs-ours at the wire (the uuid namespace is the only signal,
/// and the downstream precise match is the real gate). A `tool_result`-bearing user
/// frame is skipped defensively (it is never one of our top-level prompts).
fn sniff_replay_prompt_ack(
    frame: &serde_json::Value,
    event_tx: &broadcast::Sender<SessionEnvelope>,
    session_id: &str,
    turn_gen: u64,
) {
    use serde_json::Value;
    if frame.get("type").and_then(Value::as_str) != Some("user") {
        return;
    }
    let Some(uuid) = frame.get("uuid").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        return;
    };
    // Defensive: a user frame whose content is a tool_result is a claude-minted
    // continuation, never one of our top-level prompts — skip it (its uuid is
    // claude's and would no-op downstream anyway, but skipping avoids the spurious
    // event entirely).
    let is_tool_result = frame
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
        .unwrap_or(false);
    if is_tool_result {
        return;
    }
    let _ = event_tx.send(SessionEnvelope {
        session_id: session_id.to_string(),
        turn_gen,
        event: SessionEvent::PromptAccepted {
            client_msg_id: uuid.to_string(),
        },
    });
}

fn sniff_session_info(
    frame: &serde_json::Value,
    event_tx: &broadcast::Sender<SessionEnvelope>,
    session_id: &str,
    turn_gen: u64,
) {
    use serde_json::Value;
    if frame.get("type").and_then(Value::as_str) != Some("control_response") {
        return;
    }
    let response = frame.get("response").unwrap_or(&Value::Null);
    if response.get("subtype").and_then(Value::as_str) != Some("success") {
        return;
    }
    let request_id = response.get("request_id").and_then(Value::as_str).unwrap_or("");
    let inner = response.get("response").unwrap_or(&Value::Null);

    let event = if request_id.starts_with(QSI_USAGE_PREFIX) {
        let used = inner.get("totalTokens").and_then(Value::as_u64).unwrap_or(0);
        let max = inner.get("maxTokens").and_then(Value::as_u64).unwrap_or(0);
        let categories = inner
            .get("categories")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| {
                        let name = c.get("name").and_then(Value::as_str)?.to_string();
                        let tokens = c.get("tokens").and_then(Value::as_u64).unwrap_or(0);
                        Some(crate::event::ContextUsageCategory { name, tokens })
                    })
                    .collect()
            })
            .unwrap_or_default();
        SessionEvent::SessionInfo {
            context_usage: Some(crate::event::ContextUsage { used, max, categories }),
            cost_text: None,
        }
    } else if request_id.starts_with(QSI_COST_PREFIX) {
        let text = inner.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        SessionEvent::SessionInfo {
            context_usage: None,
            cost_text: Some(text),
        }
    } else {
        return; // not a QuerySessionInfo reply (initialize / set_mode / can_use_tool ack)
    };

    let _ = event_tx.send(SessionEnvelope {
        session_id: session_id.to_string(),
        turn_gen,
        event,
    });
}

/// Translate a raw claude `system/task_*` frame into a `SubagentUpdate` (§6b b1).
/// claude emits these for Task/Workflow subagents; the reducer upserts them into
/// `Running.subagents` (keyed by `r#ref`), which `has_foreground_activity` reads
/// so the spinner stays on while a subagent runs. No-op for any non-task frame.
///
/// Wire (verified against `tests/fixtures/claude_2.1.169_single_tool_turn.ndjson`):
/// - `task_started`     {task_id, tool_use_id, subagent_type?, workflow_name?} → Running
/// - `task_progress`    {task_id, ...}                                         → Running (still alive)
/// - `task_notification`{task_id, status: completed|failed|stopped}            → terminal
///
/// `task_id` is the stable lifecycle key (= `r#ref`); `tool_use_id` is the parent
/// ToolCall (= `parent_ref`); `subagent_type`/`workflow_name` is the label.
fn sniff_task(
    frame: &serde_json::Value,
    event_tx: &broadcast::Sender<SessionEnvelope>,
    session_id: &str,
    turn_gen: u64,
) {
    use crate::event::SubagentStatus;
    use serde_json::Value;
    if frame.get("type").and_then(Value::as_str) != Some("system") {
        return;
    }
    let subtype = frame.get("subtype").and_then(Value::as_str).unwrap_or("");
    let status = match subtype {
        "task_started" | "task_progress" | "task_updated" => SubagentStatus::Running,
        "task_notification" => match frame.get("status").and_then(Value::as_str) {
            Some("failed") => SubagentStatus::Errored,
            Some("stopped") => SubagentStatus::Interrupted,
            _ => SubagentStatus::Completed, // "completed" or unknown terminal
        },
        _ => return, // not a task frame
    };
    let Some(task_id) = frame.get("task_id").and_then(Value::as_str) else {
        return; // no stable ref → cannot upsert
    };
    let label = frame
        .get("workflow_name")
        .or_else(|| frame.get("subagent_type"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let parent_ref = frame.get("tool_use_id").and_then(Value::as_str).map(str::to_string);
    let _ = event_tx.send(SessionEnvelope {
        session_id: session_id.to_string(),
        turn_gen,
        event: SessionEvent::SubagentUpdate {
            r#ref: task_id.to_string(),
            label,
            status,
            parent_ref,
        },
    });

    // 009 R6b / H1: emit RICH per-agent detail from `workflow_progress[]`. The
    // workflow (task_id) is 1:N over its per-agent children (workflow_agent
    // entries), each carrying display fields the panel renders. Keyed by `agentId`
    // (present once running) falling back to `label` (the start frame has only
    // index/label); parent_ref = task_id (the container). Each child is a
    // SubagentDetail; the orchestrator folds them into workflow_roster.
    if let Some(agents) = frame.get("workflow_progress").and_then(Value::as_array) {
        for a in agents
            .iter()
            .filter(|a| a.get("type").and_then(Value::as_str) == Some("workflow_agent"))
        {
            let label = a.get("label").and_then(Value::as_str);
            let Some(agent_ref) = a.get("agentId").and_then(Value::as_str).or(label).map(str::to_string) else {
                continue; // no stable ref for this agent
            };
            let loop_state = match a.get("state").and_then(Value::as_str) {
                Some("start") => Some(crate::state::WorkflowLoopState::Start),
                Some("progress") => Some(crate::state::WorkflowLoopState::Progress),
                Some("done") => Some(crate::state::WorkflowLoopState::Done),
                _ => None,
            };
            let _ = event_tx.send(SessionEnvelope {
                session_id: session_id.to_string(),
                turn_gen,
                event: SessionEvent::SubagentDetail {
                    r#ref: agent_ref,
                    parent_ref: Some(task_id.to_string()),
                    label: label.map(str::to_string),
                    loop_state,
                    model: a.get("model").and_then(Value::as_str).map(str::to_string),
                    tokens: a.get("tokens").and_then(Value::as_u64),
                    tool_calls: a.get("toolCalls").and_then(Value::as_u64),
                    last_tool_name: a.get("lastToolName").and_then(Value::as_str).map(str::to_string),
                },
            });
        }
    }
}

#[async_trait::async_trait]
impl SessionBackend for ClaudeSessionBackend {
    /// Force-kill path (`UserCancelTimeout`): delegate to the suspend
    /// controller's unconditional teardown (abort reader → group-kill the
    /// claude CLI process tree), so the process dies even while an orchestrator
    /// still holds an `Arc` to this backend.
    async fn terminate(&self) {
        self.suspend.terminate().await;
    }

    async fn dispatch(&self, command: Command) -> Result<CommandReceipt, BackendError> {
        use std::sync::atomic::Ordering;
        match command {
            Command::Send { content, metadata } => {
                // OBSERVABILITY: dispatch entered (turn driver reached the backend).
                // The chain solo-send→facade→dispatch→deliver_prompt→stdin was a black
                // hole; these three markers (entered / about-to-write / delivered) pin
                // WHERE a no-output turn stalls. Shape only (block count, not text).
                tracing::info!(
                    conversation_id = %self.session_id,
                    block_count = content.len(),
                    "claude dispatch(Send): entered"
                );
                // §C6 Layer-2: reject any block kind this backend does not
                // advertise BEFORE wire-write — never silently drop it
                // ("adapter authoritatively rejects → CommandNotSupported, never a silent drop"). claude
                // headless `--print` carries text + image (native base64 block) +
                // resource (ResourceLink → Read-tool path ref); audio/at_mention
                // are rejected, keyed on their `content_block:<kind>` name.
                let blocks = self.capabilities().prompt_blocks;
                if let Some(bad) = content.iter().find(|b| !blocks.allows(b)) {
                    return Err(BackendError::CommandNotSupported {
                        command: crate::capability::block_kind_name(bad),
                    });
                }
                // F-4: ensure the process is awake before any wire write. When
                // idle_ttl=None (default) the slot is always Active → this is a
                // single uncontended lock + atomic store (no wake, no spawn), so
                // the dispatch path stays byte-identical to pre-F-4. When the slot
                // was idle-suspended, this re-spawns claude with `--resume` first.
                self.suspend
                    .ensure_awake(aionui_common::now_ms(), || self.wake_handle())
                    .await?;
                // G2: drain any queued in-band config switch (set_mode/set_model)
                // BEFORE marking the turn in flight + writing the prompt, so a switch
                // queued mid-previous-turn applies to THIS turn (and cannot land after
                // the prompt and truncate it). Drains over the same stdin lock, in
                // order. Done while still "idle" (turn_in_flight not yet set).
                self.drain_pending_controls().await?;
                // F-4: mark the turn in flight so the idle timer won't suspend the
                // process mid-turn (the reader clears it at the terminal). Set after
                // a successful wake, before the wire write.
                self.turn_in_flight.store(true, Ordering::SeqCst);
                {
                    // first-send-race-500 #2: is this the FIRST send on a process
                    // that has never accepted one? `turn_gen` is bumped only AFTER a
                    // successful delivery (below), so `== 0` ⇔ "no prompt has landed
                    // yet" ⇔ the process may still be completing startup. A
                    // deliver_prompt failure THERE is the claude analog of codex/acp's
                    // bound-thread/bound-session handshake miss: the agent is still
                    // coming up (the home-page warmup/send race hits a just-spawned
                    // claude before its control plane is ready). Classify it as the
                    // RETRYABLE HandshakeTimeout (→ session_bridge → BackendUnavailable
                    // → 502 "agent starting, retry") instead of a bare Transport→500.
                    // A failure AFTER the first successful send stays Transport (an
                    // established process that drops a write is genuinely broken — an
                    // honest terminal, not a startup race). We do NOT retry the write:
                    // a not-ready-but-alive process buffers stdin (the write would
                    // succeed), so a write error means the pipe is broken = process
                    // gone — where a retry is futile AND risks a corrupt frame (a
                    // partial write_all + a retried full frame). The reader's
                    // Detached→Error{Crashed}→evict path self-heals a dead process; the
                    // client's retry then rebuilds Fresh.
                    let starting = self.turn_gen.load(Ordering::SeqCst) == 0;
                    let wrap = |e: String| {
                        if starting {
                            BackendError::HandshakeTimeout(format!("claude still starting: {e}"))
                        } else {
                            BackendError::Transport(format!("deliver_prompt: {e}"))
                        }
                    };
                    let mut guard = self.stdin.lock().await;
                    let stdin = guard.as_mut().ok_or_else(|| wrap("stdin unavailable".into()))?;
                    tracing::info!(
                        conversation_id = %self.session_id,
                        first_send = starting,
                        "claude dispatch(Send): writing prompt to stdin"
                    );
                    self.adapter
                        .deliver_prompt(stdin, &content, metadata.client_msg_id.as_deref())
                        .await
                        .map_err(|e| wrap(e.to_string()))?;
                } // stdin lock released (microsecond frame-write lock, §5.4)
                tracing::info!(
                    conversation_id = %self.session_id,
                    "claude dispatch(Send): prompt delivered to stdin (awaiting CLI frames)"
                );
                // turn_gen++ on accept (§5.4): still bumped here — it drives the
                // orchestrator's Idle→Running latch (TurnStarted{epoch: receipt.turn_gen})
                // and the per-turn epoch. PromptAccepted is NO LONGER synthesized here.
                //
                // Bug-A / B (regression A): claude has a REAL prompt-ack after all — it
                // echoes our user-frame `uuid` (= client_msg_id) in the
                // `--replay-user-messages` frame ONLY when it actually consumes that
                // message into a turn (LIVE-pinned, see protocols/design/
                // claude-midturn-input-turn-gen-design.md §3.3). The reader's
                // `sniff_replay_prompt_ack` emits PromptAccepted on that echo. This
                // replaces the old flush-ok "Synthesized" emit, which lied for a
                // proactively-queued message (flush succeeds the instant we write, but
                // claude may sit on it for seconds, or DROP it if the turn is cancelled
                // before it is drained — the bubble must not flip to sent until claude
                // really took it). This brings claude to codex-parity (Native ack).
                let cur_gen = self.turn_gen.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::Started,
                    turn_gen: cur_gen,
                })
            }
            Command::Cancel { target } => {
                match target {
                    CancelTarget::Turn | CancelTarget::Session => {
                        // G-A: ACTUALLY interrupt the in-flight turn over the retained
                        // stdin (claude is a LONG-LIVED process; the orchestrator's
                        // lowered Cancel folds Running→Idle on OUR side, but without
                        // this write claude keeps running the whole turn in the
                        // background — wasted tokens, "cancel that didn't cancel").
                        // Write `control_request{subtype:"interrupt"}` IMMEDIATELY (not
                        // queued like set_model — the turn IS in flight, that is the
                        // point; SDK parity: query.interrupt(), probe-verified 2.1.168
                        // ends the turn ~immediately). The trailing late `result` claude
                        // emits is dropped by the reducer's epoch guard (restamp_epoch +
                        // result_epoch < since_epoch), so it never lands in a new turn.
                        // Best-effort: a stdin-closed write error means the process is
                        // already gone (the turn ends on teardown) — log, do not fail
                        // the cancel (the FSM already unlocked).
                        self.interrupt_turn().await;
                    }
                    CancelTarget::Tool(_) => {
                        return Err(BackendError::CommandNotSupported { command: "cancel_tool" });
                    }
                }
                let cur_gen = self.turn_gen.load(Ordering::SeqCst);
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: cur_gen,
                })
            }
            // cap=false ↔ dispatch-rejects (Layer-2: authoritatively reject, never a
            // silent drop). Rewind is NOT WIRED YET — deferred, not impossible: the
            // rewind_files protocol DOES exist in 2.1.191 (gap-reaudit correction), but
            // it needs a num_turns→user_message_id map + checkpoint infra we don't carry.
            // Reachable follow-up when rewind UX is wanted (probe shapes captured). Same
            // for ListCheckpoints.
            Command::Rewind { .. } => Err(BackendError::CommandNotSupported { command: "rewind" }),
            Command::ListCheckpoints => Err(BackendError::CommandNotSupported {
                command: "list_checkpoints",
            }),
            // G: query claude's cumulative session info over the in-band control plane
            // (get_context_usage / get_session_cost, live-confirmed 2.1.186). A
            // read-only query (Admission::NoTurn): mint a kind-tagged request_id so the
            // reader routes the success control_response → SessionEvent::SessionInfo.
            // Written immediately (not queued like set_mode): a query does not mutate
            // turn state, and we want the answer promptly.
            Command::QuerySessionInfo { kind } => {
                use std::sync::atomic::Ordering;
                let (subtype, prefix) = match kind {
                    super::types::SessionInfoKind::ContextUsage => ("get_context_usage", QSI_USAGE_PREFIX),
                    super::types::SessionInfoKind::SessionCost => ("get_session_cost", QSI_COST_PREFIX),
                };
                let request_id = format!("{prefix}{}", self.control_seq.fetch_add(1, Ordering::SeqCst) + 1);
                let frame = serde_json::json!({
                    "type": "control_request",
                    "request_id": request_id,
                    "request": { "subtype": subtype },
                });
                self.write_control_frame(&frame).await?;
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            Command::AnswerAuth { .. } => Err(BackendError::CommandNotSupported { command: "answer_auth" }),
            Command::Steer { .. } => Err(BackendError::CommandNotSupported { command: "steer" }),
            // G2: in-band config switch via control_request (probe-verified, mirrors
            // F1). set_permission_mode / set_model are written over the retained
            // stdin WITHOUT restarting the process; the switch applies to the NEXT
            // turn. Mid-turn writes would reinitialize + TRUNCATE the in-flight turn,
            // so they QUEUE (drained before the next prompt). On a successful
            // dispatch we emit ConfigChanged so the UI confirms immediately.
            Command::SetMode { mode } => {
                // DE-OPTIMISTIC (design §9.10.1 option A / README #10): we write the
                // set_permission_mode request and STOP — no optimistic ConfigChanged, no
                // optimistic override write. The confirmation comes from claude's inbound
                // `system/status{permissionMode}` (sniff_mode), which fires for BOTH this
                // user-driven switch AND an autonomous one (plan-exit). Routing both
                // through the single inbound signal means the UI never shows a mode claude
                // hasn't applied (no reverse drift), and the autonomous case is covered by
                // construction. A rejected switch comes back as a control_response error
                // (sniff_mode_reject). The picker re-read surface (`current_mode_override`)
                // is set by sniff_mode on the confirming status, not here.
                let _ = self
                    .write_or_queue_control(serde_json::json!({ "subtype": "set_permission_mode", "mode": mode }))
                    .await?;
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            Command::SetModel { model } => {
                // PURELY OPTIMISTIC by wire constraint (design §9.10.1). set_model is
                // in-band (no respawn), and LIVE-PROBED (2.1.187) claude gives it NO
                // confirmation channel whatsoever:
                //   - the set_model control_response is a bare {subtype:"success"} with no
                //     model echo (a bogus id also returns success) — no confirm/reject;
                //   - it does NOT emit a fresh `system/init` (init fires only on spawn/
                //     resume, NOT on an in-band set) — verified: `_all_set_model.jsonl` shows
                //     two set_model sends with ZERO subsequent system/init.
                // So there is NO inbound signal to reconcile the applied model against —
                // unlike set_permission_mode (which echoes via control_response + system/
                // status). The official Agent SDK treats set_model as fire-and-forget for
                // the same reason. We emit ConfigChanged{model} OPTIMISTICALLY (UI selector
                // updates at once) and STOP. Do NOT add a reconcile path keyed on a fresh
                // system/init — that frame never arrives in-band (a prior comment wrongly
                // claimed "reconciled from the next turn's system/init"; disproved). A bad
                // model id surfaces only when the NEXT turn actually tries to use it (API
                // 404). There is deliberately NO reader-side set_model response parser
                // (it would be permanently inert + self-confirming — README discipline #9).
                let _ = self
                    .write_or_queue_control(serde_json::json!({ "subtype": "set_model", "model": model.clone() }))
                    .await?;
                let cur_gen = self.turn_gen.load(Ordering::SeqCst);
                let _ = self.event_tx.send(SessionEnvelope {
                    session_id: self.session_id.clone(),
                    turn_gen: cur_gen,
                    event: SessionEvent::ConfigChanged {
                        mode: None,
                        model: Some(model),
                    },
                });
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: cur_gen,
                })
            }
            // #99: generic config option. EFFORT is the only one worth exposing on
            // current models (`supportedEffortLevels` per model, from initialize). The
            // binary (2.1.191) ALSO has a `set_max_thinking_tokens` control arm, but
            // budget_tokens thinking is deprecated on Opus/Sonnet 4.6+ in favor of
            // adaptive-thinking + effort, so we don't surface it (gap-reaudit: the prior
            // "only EFFORT exists" claim was wire-inaccurate; "only one worth exposing"
            // is the accurate framing). Effort is set via
            // `control_request{apply_flag_settings, settings:{effortLevel}}` —
            // LIVE-PROBED (2.1.181): shallow-merge, immediate, no restart (NOT
            // `set_effort`, which is Unsupported). Queued behind an in-flight turn
            // like set_mode/set_model. No ConfigChanged emit: that event carries only
            // mode/model (no effort field); the frontend confirms effort by re-reading
            // (get_settings). Any other option_id rejects (cap=false ↔ reject).
            Command::SetConfigOption { option_id, value } => match option_id.as_str() {
                "effort" | "reasoning_effort" | "thought_level" => {
                    // Validate against the current model's advertised effort catalog
                    // (`supportedEffortLevels` → `reasoning_efforts`) BEFORE sending —
                    // the ACP `clear_invalid_desired_*` semantic ported to effort. An
                    // unsupported level (e.g. a stale picker "max" against a model that
                    // only offers low/medium/high) would be rejected by claude next turn
                    // AND poison the optimistic `current_effort` we store below. Empty /
                    // unknown catalog → permissive (matches ACP `is_*_valid`: absent
                    // catalog can't invalidate). REJECT (not silent-drop): the caller
                    // asked for a level the model can't honor.
                    if !self.effort_is_supported(&value) {
                        return Err(BackendError::Transport(format!(
                            "effort level '{value}' is not supported by the current model"
                        )));
                    }
                    // `ultracode` is not an `effortLevel` value; it is the dedicated
                    // boolean flag `settings.ultracode` (which the CLI auto-forces to
                    // xhigh). Every other level rides `effortLevel`. LIVE-PROBED 2.1.206
                    // (samples/claude-cli/2.1.206/ultracode_wire.result.md).
                    let settings = if value == ULTRACODE_LEVEL {
                        serde_json::json!({ "ultracode": true })
                    } else {
                        serde_json::json!({ "effortLevel": value })
                    };
                    let request_id = self
                        .write_or_queue_control(serde_json::json!({
                            "subtype": "apply_flag_settings",
                            "settings": settings,
                        }))
                        .await?;
                    // #99: register the minted ctl-id so the reader surfaces a REJECTION
                    // (bad effort value → control_response{error}) as a Notice instead of
                    // silently dropping it. Success is silent (claude does not echo effort);
                    // the reader just removes the entry on a matching success.
                    self.pending_set_config
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(request_id, format!("effort\u{2192}{value}"));
                    // CP-1: claude does not echo effort back, so remember it here →
                    // `capabilities().current_effort` highlights the active level for
                    // the picker (the frontend confirms by re-reading get_config_options).
                    *self.current_effort.lock().unwrap_or_else(|e| e.into_inner()) = Some(value.clone());
                    let cur_gen = self.turn_gen.load(Ordering::SeqCst);
                    Ok(CommandReceipt {
                        accepted: true,
                        admission: Admission::NoTurn,
                        turn_gen: cur_gen,
                    })
                }
                _ => Err(BackendError::CommandNotSupported {
                    command: "set_config_option",
                }),
            },
            // AnswerPermission: wire the control_response (the F3 permission answer).
            Command::AnswerPermission {
                request_id,
                decision,
                selected,
                answers,
            } => {
                self.answer_permission(&request_id, decision, selected.as_deref(), &answers)
                    .await
            }
            // Acknowledge: a conversation-side fold (done-unseen → seen). NO claude
            // wire; accept as a local no-op (§C1).
            Command::Acknowledge { .. } => {
                let cur_gen = self.turn_gen.load(Ordering::SeqCst);
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: cur_gen,
                })
            }
        }
    }

    fn events(&self) -> BoxStream<'static, SessionEnvelope> {
        let rx = self.event_tx.subscribe();
        // Hand-roll a stream over the broadcast receiver via `unfold` (avoids a
        // tokio-stream dep). A `Lagged` recv error skips the gap and continues
        // (the orchestrator's own broadcast layer surfaces backpressure as U21);
        // a `Closed` error ends the stream.
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
        // B-CLAUDE-INIT: merge the init-discovered current_model when config did not
        // supply one (the snapshot's current_model is None in that case; the reader
        // fills discovered_model from the system/init frame). Read-only sync lock.
        let mut caps = self.capabilities.clone();
        if caps.current_model.is_none()
            && let Some(model) = self.discovered_model.lock().unwrap_or_else(|e| e.into_inner()).clone()
        {
            caps.current_model = Some(model);
        }
        // #98/#101: merge the initialize-response discovery catalog (selectable
        // models + slash commands). Empty until the control_response lands — a fresh
        // read sees [] (like codex pre-`model/list`); the conversation re-reads.
        let discovered = self.discovered_caps.lock().unwrap_or_else(|e| e.into_inner());
        if !discovered.models.is_empty() {
            caps.available_models = discovered.models.clone();
        }
        if !discovered.slash_commands.is_empty() {
            caps.slash_commands = discovered.slash_commands.clone();
        }
        // CP-1: surface the last-set effort (claude does not echo it back).
        if let Some(effort) = self.current_effort.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            caps.current_effort = Some(effort);
        }
        // Surface the last RUNTIME mode switch (init seeded current_mode from config;
        // a SetMode override supersedes it for the picker highlight).
        if let Some(mode) = self
            .current_mode_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            caps.current_mode = Some(mode);
        }
        caps
    }

    /// REST-recovery (`GET /confirmations`) source: the adapter's transient
    /// pending-permission registry IS the set of currently-unanswered permissions
    /// (insert on each `can_use_tool` control_request, remove on `AnswerPermission`
    /// and on `control_cancel_request`). Map each entry to a safe view — `request_id`
    /// (the card's id/call_id) + `tool_name` (the title). The raw tool `input` is NOT
    /// exposed (TIO-13: it carries command bodies / args). claude does not advertise
    /// options, so the recovered card's options default is synthesized frontend-side.
    fn pending_permission_requests(&self) -> Vec<PendingPermissionView> {
        self.pending_perms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(request_id, perm)| {
                // AskUserQuestion recovery: surface input.questions so the REST
                // /confirmations path rebuilds a question card (symmetric to the live
                // ConfirmationAdded projection in turn_finalizer). Only AskUserQuestion
                // carries `input`, so questions stays None for ordinary tools.
                let questions = if perm.tool_name == "AskUserQuestion" {
                    perm.input.get("questions").cloned()
                } else {
                    None
                };
                PendingPermissionView {
                    request_id: request_id.clone(),
                    tool_name: perm.tool_name.clone(),
                    questions,
                }
            })
            .collect()
    }
}

// Re-export the session_id accessor for tests / orchestration.
impl ClaudeSessionBackend {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// #99 test-support seam: pre-register a pending `set_config_option(effort)`
    /// ctl-id + label so a hermetic fixture can replay an error control_response and
    /// assert the reader surfaces a `Notice{Warning}` (not a silent drop). On the live
    /// path `dispatch(SetConfigOption{effort})` registers it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_pending_set_config_for_test(&self, request_id: impl Into<String>, label: impl Into<String>) {
        self.pending_set_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(request_id.into(), label.into());
    }

    /// Test-support seam (§C5 verification): build a backend over an injected
    /// `AgentIo` (a `FakeAgentIo` replaying fixtures) WITHOUT spawning a real
    /// process — proving the dispatch/reader/events wiring end-to-end. Gated so
    /// production never ships it.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn build_with_io(session_id: impl Into<String>, io: Box<dyn AgentIo>) -> Self {
        let session_id = session_id.into();
        // A test backend never suspends (config.idle_ttl_ms = None), so the wake
        // recipe is never consulted — but `spawn` needs one. Use a FakeSpawner.
        let wake = ClaudeWakeRecipe {
            spawner: Arc::new(crate::testing::FakeSpawner::new()),
            claude_session_id: session_id.clone(),
            cwd: None,
            extra_args: Vec::new(),
            env: Vec::new(),
            cli_program: None,
        };
        Self::spawn(session_id, ClaudeAdapter::new(), io, SessionConfig::default(), wake).await
    }

    /// Test-support seam: build a SUSPENDABLE backend over an injected `AgentIo`,
    /// with a caller-supplied `Spawner` (to observe the wake re-spawn) and an
    /// `idle_ttl_ms`. Lets a test drive the suspend→wake path hermetically: the
    /// idle timer suspends the idle slot, and the next dispatch wakes via the
    /// supplied spawner (asserting the `--resume <logical_id>` recipe).
    #[cfg(any(test, feature = "test-support"))]
    pub async fn build_with_io_suspending(
        session_id: impl Into<String>,
        io: Box<dyn AgentIo>,
        spawner: Arc<dyn Spawner>,
        idle_ttl_ms: i64,
    ) -> Self {
        let session_id = session_id.into();
        // Test backends drive the wake path directly over the supplied spawner; the
        // resume id is the test's session id verbatim (the assertion checks
        // `--resume <session_id>`), so it is NOT routed through claude_session_id_for.
        let wake = ClaudeWakeRecipe {
            spawner,
            claude_session_id: session_id.clone(),
            cwd: None,
            extra_args: Vec::new(),
            env: Vec::new(),
            cli_program: None,
        };
        let config = SessionConfig {
            idle_ttl_ms: Some(idle_ttl_ms),
            ..Default::default()
        };
        Self::spawn(session_id, ClaudeAdapter::new(), io, config, wake).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContentBlock;
    use crate::backend::types::CommandMeta;
    use crate::backend::{McpServerSpec, McpTransport, SessionInit};
    use crate::testing::FakeAgentIo;
    use futures_util::StreamExt;

    /// The seam MUST hand claude a bare valid UUID for `--session-id`/`--resume`
    /// (a non-UUID makes claude exit 1 "Invalid session ID"). A prefixed logical
    /// id (our `conv_<uuid_v7>` conversation id) is therefore minted into a fresh
    /// UUID; a logical id that already IS a UUID (the F1 factory mints one
    /// upstream) passes through verbatim so production behavior is unchanged.
    #[test]
    fn claude_session_id_minted_for_non_uuid_passthrough_for_uuid() {
        // Non-UUID logical id (prefixed conv id) → minted into a valid UUID.
        let minted = claude_session_id_for("conv_0192f0a1-1111-7abc-8def-000000000000");
        assert!(
            uuid::Uuid::parse_str(&minted).is_ok(),
            "a non-UUID logical id must be minted into a valid UUID, got {minted:?}"
        );
        let plain = claude_session_id_for("live-claude-xyz");
        assert!(
            uuid::Uuid::parse_str(&plain).is_ok(),
            "any non-UUID is minted, got {plain:?}"
        );

        // A bare UUID logical id passes through UNCHANGED (production F1 path).
        let uuid = "8cd37cd6-2e88-4c8d-847a-7b237ffa9710";
        assert_eq!(
            claude_session_id_for(uuid),
            uuid,
            "a logical id that is already a UUID must pass through verbatim"
        );
    }

    /// SECURITY regression: a default (empty-init, no explicit mode) SessionConfig
    /// produces EXACTLY `["--permission-mode", "default", "--allow-dangerously-skip-permissions"]`.
    /// `--permission-mode default` (NOT zero flags) keeps an unconfigured session gated —
    /// omitting it makes claude headless default to bypassPermissions (LIVE-PROBED).
    /// `--allow-dangerously-skip-permissions` only UNLOCKS a later in-band switch to
    /// bypass; it does NOT change the initial mode (default still enforces — LIVE-PROBED
    /// 2.1.185), so the fail-closed default is preserved while runtime bypass is reachable.
    #[test]
    fn build_claude_init_args_empty_config_defaults_permission_mode() {
        let config = SessionConfig::default();
        assert_eq!(
            build_claude_init_args(&config),
            vec![
                "--permission-mode".to_string(),
                "default".to_string(),
                "--allow-dangerously-skip-permissions".to_string(),
                "--disallowed-tools".to_string(),
                "AskUserQuestion".to_string(),
            ],
            "an unconfigured claude session is gated as `default` (never silently bypassed), \
             with runtime-bypass UNLOCKED but not activated, and AskUserQuestion denied \
             (temporary — no multi-question frontend renderer yet)"
        );
        assert_eq!(build_claude_mcp_config(&[]), None, "no servers → no --mcp-config");
    }

    /// MCP servers → `--mcp-config <json>` + `--strict-mcp-config` (the latter ONLY
    /// alongside --mcp-config). The JSON is claude's MAP shape keyed by server name,
    /// stdio carrying command/args/env.
    #[test]
    fn build_claude_init_args_mcp_emits_strict_and_map_json() {
        let config = SessionConfig {
            init: SessionInit {
                mcp_servers: vec![McpServerSpec {
                    name: "fs".into(),
                    transport: McpTransport::Stdio {
                        command: "/usr/bin/mcp-fs".into(),
                        args: vec!["--root".into(), "/tmp".into()],
                        env: vec![("TOKEN".into(), "abc".into())],
                    },
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let args = build_claude_init_args(&config);
        // --mcp-config <json> --strict-mcp-config, in that order, adjacent.
        let i = args
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("--mcp-config present");
        assert_eq!(
            args.get(i + 2).map(String::as_str),
            Some("--strict-mcp-config"),
            "--strict-mcp-config must immediately follow the --mcp-config value"
        );
        let json: serde_json::Value = serde_json::from_str(&args[i + 1]).expect("valid mcp-config json");
        assert_eq!(json["mcpServers"]["fs"]["command"], "/usr/bin/mcp-fs");
        assert_eq!(json["mcpServers"]["fs"]["args"][0], "--root");
        assert_eq!(json["mcpServers"]["fs"]["env"]["TOKEN"], "abc");
    }

    /// `--strict-mcp-config` must NEVER appear without `--mcp-config` (stripping the
    /// machine's ambient `~/.claude` servers when we inject none would silently
    /// disable a user's machine-level config).
    #[test]
    fn build_claude_init_args_no_strict_without_mcp() {
        let config = SessionConfig {
            model: Some("opus".into()),
            ..Default::default()
        };
        let args = build_claude_init_args(&config);
        assert!(
            !args.iter().any(|a| a == "--strict-mcp-config"),
            "no --strict-mcp-config without --mcp-config"
        );
    }

    /// preset_context → `--system-prompt`; model → `--model`; mode →
    /// `--permission-mode`; each omitted independently when its source is empty.
    #[test]
    fn build_claude_init_args_threads_preset_model_mode() {
        let config = SessionConfig {
            model: Some("global.anthropic.claude-opus-4-8".into()),
            mode: Some("plan".into()),
            init: SessionInit {
                preset_context: Some("[Assistant Rules] be precise".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let args = build_claude_init_args(&config);
        let pair = |flag: &str| -> Option<String> {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1).cloned())
        };
        assert_eq!(pair("--system-prompt").as_deref(), Some("[Assistant Rules] be precise"));
        assert_eq!(pair("--model").as_deref(), Some("global.anthropic.claude-opus-4-8"));
        assert_eq!(pair("--permission-mode").as_deref(), Some("plan"));

        // Whitespace-only / empty model & preset are omitted (not emitted as blank
        // flags), but `--permission-mode` is the SECURITY exception: a blank/missing
        // mode falls through to `default`, never to claude's bypass default. So the
        // only flag a fully-blank config emits is `["--permission-mode", "default"]`.
        let blank = SessionConfig {
            model: Some("".into()),
            mode: Some("   ".into()),
            init: SessionInit {
                preset_context: Some("  ".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let blank_args = build_claude_init_args(&blank);
        assert!(
            !blank_args.iter().any(|a| a == "--model" || a == "--system-prompt"),
            "blank model/preset emit no flags"
        );
        assert_eq!(
            blank_args,
            vec![
                "--permission-mode".to_string(),
                "default".to_string(),
                "--allow-dangerously-skip-permissions".to_string(),
                "--disallowed-tools".to_string(),
                "AskUserQuestion".to_string(),
            ],
            "a blank mode is gated as `default` (never silently bypassed); the unlock flag \
             is always present so a later in-band switch to bypass is accepted; \
             AskUserQuestion is denied (temporary)"
        );
    }

    /// claude-mode-gating: an UNRECOGNIZED `--permission-mode` value makes claude exit 1
    /// at spawn (LIVE-PROBED), surfacing as an opaque crash. `config.mode` is sourced
    /// from unconstrained storage (a persisted `current_mode_id`, an assistant default,
    /// a stale generic alias), so `build_claude_init_args` must validate it against
    /// claude's exact enum and fall back to the fail-CLOSED `default` — never pass an
    /// invalid value through to the flag. Mirrors the ACP path's
    /// `clear_invalid_desired_mode` (drop-if-not-in-catalog).
    #[test]
    fn build_claude_init_args_invalid_mode_falls_back_to_default() {
        let permission_mode = |mode: &str| -> Option<String> {
            let cfg = SessionConfig {
                mode: Some(mode.to_string()),
                ..Default::default()
            };
            let args = build_claude_init_args(&cfg);
            args.iter()
                .position(|a| a == "--permission-mode")
                .and_then(|i| args.get(i + 1).cloned())
        };
        // Every valid enum value passes through verbatim. This is claude's full
        // accepted set (SDK `PermissionMode` + CLI): a SUPERSET of the advertised
        // picker — `auto`/`dontAsk` are legal wire values (CLI-accepted, live-probed)
        // even though `auto` is not advertised, so a resumed session carrying either
        // must pass through, never crash.
        for valid in ["default", "acceptEdits", "bypassPermissions", "plan", "dontAsk", "auto"] {
            assert_eq!(
                permission_mode(valid).as_deref(),
                Some(valid),
                "valid mode {valid:?} must pass through unchanged"
            );
        }
        // Anything else (a stale alias, a codex-ism, free text, the CLI-only `manual`
        // alias we never emit) falls back to `default` instead of crashing the spawn.
        for invalid in ["yolo", "yoloNoSandbox", "manual", "acceptedits", "danger", "Plan"] {
            assert_eq!(
                permission_mode(invalid).as_deref(),
                Some("default"),
                "invalid mode {invalid:?} must fall back to `default` (not crash the spawn)"
            );
        }
    }

    /// http/sse MCP transports map to claude's `{type,url,headers}` entry shape.
    #[test]
    fn build_claude_mcp_config_http_carries_type_and_headers() {
        let json_str = build_claude_mcp_config(&[McpServerSpec {
            name: "api".into(),
            transport: McpTransport::Http {
                url: "https://example.com/mcp".into(),
                headers: vec![("Authorization".into(), "Bearer x".into())],
            },
        }])
        .expect("http server → some json");
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(json["mcpServers"]["api"]["type"], "http");
        assert_eq!(json["mcpServers"]["api"]["url"], "https://example.com/mcp");
        assert_eq!(json["mcpServers"]["api"]["headers"]["Authorization"], "Bearer x");
    }

    /// SESS-INIT-17 (audit): duplicate MCP server NAMES collapse by construction.
    /// `build_claude_mcp_config` builds claude's map shape keyed by `name`
    /// (`map.insert(name, …)`), so two specs sharing a name yield ONE entry, last
    /// spec wins — there is no pre-wire "reject duplicates" gate (the design never
    /// mandated one; the map collapse + `--strict-mcp-config` is the contract). This
    /// pins that dedup-by-map-collapse so a future refactor to a shape that could
    /// emit duplicate keys (e.g. a JSON array) trips RED.
    #[test]
    fn build_claude_mcp_config_duplicate_names_collapse_last_wins() {
        let json_str = build_claude_mcp_config(&[
            McpServerSpec {
                name: "fs".into(),
                transport: McpTransport::Stdio {
                    command: "/first".into(),
                    args: vec![],
                    env: vec![],
                },
            },
            McpServerSpec {
                name: "fs".into(), // same name → collapses
                transport: McpTransport::Stdio {
                    command: "/second".into(),
                    args: vec![],
                    env: vec![],
                },
            },
        ])
        .expect("two servers → some json");
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let servers = json["mcpServers"].as_object().expect("mcpServers is a map");
        assert_eq!(
            servers.len(),
            1,
            "duplicate server names collapse to ONE map entry (no duplicate keys on the wire), got {servers:?}"
        );
        assert_eq!(
            json["mcpServers"]["fs"]["command"], "/second",
            "the LATER spec wins on a name collision (map insert last-wins)"
        );
    }

    /// `prepend_args` keeps init flags BEFORE caller `extra_args` (a duplicate caller
    /// flag then wins by appearing later on the CLI).
    #[test]
    fn prepend_args_orders_init_before_caller() {
        let head = vec!["--model".to_string(), "opus".to_string()];
        let tail = vec!["--model".to_string(), "sonnet".to_string()];
        assert_eq!(
            prepend_args(&head, &tail),
            vec!["--model", "opus", "--model", "sonnet"],
            "init flags first, caller flags after (caller wins by position)"
        );
    }

    /// `to_legacy_spec` maps the two-id SessionSpec correctly: Fresh mints a
    /// valid-UUID claude id under the FRESH flag; Resume with a bound
    /// backend_session_id resumes THAT id verbatim; Resume with a lost binding
    /// rebinds a fresh valid-UUID claude session. The logical id (demux key) is
    /// preserved in every arm.
    #[test]
    fn to_legacy_spec_mints_uuid_and_preserves_logical_id() {
        // Fresh, non-UUID logical id → Fresh(<valid uuid>), logical id preserved.
        let (logical, claude_id, legacy) = ClaudeConnection::to_legacy_spec(&SessionSpec::Fresh {
            session_id: "conv_abc".into(),
        });
        assert_eq!(logical, "conv_abc", "logical demux key is preserved");
        assert!(uuid::Uuid::parse_str(&claude_id).is_ok(), "claude id is a valid UUID");
        match legacy {
            LegacySessionSpec::Fresh(id) => assert_eq!(id, claude_id, "Fresh spawns with the minted claude id"),
            other => panic!("Fresh logical → Fresh legacy, got {other:?}"),
        }

        // Resume with a bound backend id → Resume(that id) verbatim (claude already
        // echoed a valid UUID via BackendBound).
        let (logical, claude_id, legacy) = ClaudeConnection::to_legacy_spec(&SessionSpec::Resume {
            session_id: "conv_abc".into(),
            backend_session_id: Some("8cd37cd6-2e88-4c8d-847a-7b237ffa9710".into()),
        });
        assert_eq!(logical, "conv_abc");
        assert_eq!(claude_id, "8cd37cd6-2e88-4c8d-847a-7b237ffa9710");
        match legacy {
            LegacySessionSpec::Resume(id) => assert_eq!(id, "8cd37cd6-2e88-4c8d-847a-7b237ffa9710"),
            other => panic!("bound Resume → Resume legacy, got {other:?}"),
        }

        // Resume with a LOST backend id → rebind a FRESH valid-UUID claude session.
        let (_logical, claude_id, legacy) = ClaudeConnection::to_legacy_spec(&SessionSpec::Resume {
            session_id: "conv_abc".into(),
            backend_session_id: None,
        });
        assert!(
            uuid::Uuid::parse_str(&claude_id).is_ok(),
            "lost resume rebinds a valid UUID"
        );
        assert!(
            matches!(legacy, LegacySessionSpec::Fresh(ref id) if id == &claude_id),
            "lost backend session → Fresh rebind with the minted id, got {legacy:?}"
        );
    }

    /// §C5 wiring verification: drive a full claude turn through the new seam over a
    /// FakeAgentIo — dispatch(Send) delivers the prompt + bumps turn_gen, claude's
    /// REPLAY of our uuid (--replay-user-messages) surfaces PromptAccepted (B, the
    /// Native ack that replaced the old flush-ok synthesized emit), and the reader
    /// surfaces the fixture's events wrapped in SessionEnvelope, ending with Detached
    /// on EOF.
    #[tokio::test]
    async fn dispatch_send_drives_turn_and_emits_envelopes() {
        let fixture = concat!(
            // claude replays our user frame with the uuid we stamped (= client_msg_id
            // "m1") → the reader's sniff_replay_prompt_ack emits PromptAccepted{m1}.
            r#"{"type":"user","uuid":"m1","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"hi"}"#,
            "\n",
        )
        .as_bytes()
        .to_vec();
        let fake = FakeAgentIo::new(
            fixture,
            Some(crate::event::ExitStatusLite {
                code: Some(0),
                signal: None,
            }),
        );
        // The process exits after emitting its frames; pre-arm the exit gate so
        // the reader's wait_for_exit resolves once stdout EOFs (it only checks the
        // flag AFTER draining the fixture). Models "claude prints then exits".
        fake.release_exit();
        let backend = ClaudeSessionBackend::build_with_io("logical-1", Box::new(fake)).await;
        let mut events = backend.events();

        let receipt = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("hello".into())],
                metadata: CommandMeta {
                    client_msg_id: Some("m1".into()),
                    ..Default::default()
                },
            })
            .await
            .expect("dispatch accepted");
        assert!(receipt.accepted);
        assert_eq!(receipt.turn_gen, 1, "first Send bumps turn_gen to 1");
        assert_eq!(receipt.admission, Admission::Started);

        let mut saw_prompt_accepted = false;
        let mut saw_message_delta = false;
        let mut saw_turn_result = false;
        let mut saw_detached = false;
        for _ in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), events.next()).await {
                Ok(Some(env)) => {
                    assert_eq!(env.session_id, "logical-1", "every envelope demuxes by logical id");
                    match env.event {
                        SessionEvent::PromptAccepted { ref client_msg_id } => {
                            assert_eq!(client_msg_id, "m1");
                            saw_prompt_accepted = true;
                        }
                        SessionEvent::MessageDelta { .. } => saw_message_delta = true,
                        SessionEvent::TurnResult { .. } => saw_turn_result = true,
                        SessionEvent::Detached { .. } => {
                            saw_detached = true;
                            break;
                        }
                        _ => {}
                    }
                }
                _ => break,
            }
        }
        assert!(
            saw_prompt_accepted,
            "PromptAccepted delivered from claude's uuid replay (B)"
        );
        assert!(saw_message_delta, "fixture assistant text surfaced as MessageDelta");
        assert!(saw_turn_result, "fixture result surfaced as TurnResult");
        assert!(saw_detached, "EOF surfaced as Detached");
    }

    /// first-send-race-500 #2: a first `deliver_prompt` that fails because the
    /// just-spawned process is not ready yet (here a degenerate spawn with no stdin)
    /// must classify as the RETRYABLE `HandshakeTimeout` (→ BackendUnavailable → 502
    /// "agent starting, retry"), NOT a bare `Transport`→500. Keyed on `turn_gen == 0`
    /// (no prompt has landed yet = the agent may still be coming up).
    #[tokio::test]
    async fn first_send_failure_before_ready_is_retryable_handshake_timeout() {
        let backend = ClaudeSessionBackend::build_with_io("first-send", Box::new(FakeAgentIo::no_stdio())).await;
        let res = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("hi".into())],
                metadata: CommandMeta {
                    client_msg_id: Some("m1".into()),
                    ..Default::default()
                },
            })
            .await;
        assert!(
            matches!(&res, Err(BackendError::HandshakeTimeout(m)) if m.contains("claude still starting")),
            "a first-send failure before readiness must be retryable HandshakeTimeout, got {res:?}"
        );
    }

    /// first-send-race-500 #2 (negative half): once a send HAS succeeded
    /// (`turn_gen > 0`), a later delivery failure is an established process dropping a
    /// write = genuinely broken → it stays an honest `Transport` (terminal), never
    /// masked as a retryable startup race. MUTATION-PROVEN: make the wrap classify
    /// unconditionally as HandshakeTimeout and this assertion fails.
    #[tokio::test]
    async fn delivery_failure_after_first_success_stays_transport_not_retryable() {
        // First send succeeds over a real fake stdin (turn_gen → 1); then we drop the
        // stdin slot to force the SECOND send into the "stdin unavailable" arm.
        let backend =
            ClaudeSessionBackend::build_with_io("post-ready", Box::new(FakeAgentIo::never_exits(Vec::new()))).await;
        backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("one".into())],
                metadata: CommandMeta {
                    client_msg_id: Some("m1".into()),
                    ..Default::default()
                },
            })
            .await
            .expect("first send accepted (turn_gen → 1)");
        // Drop the live stdin so the next delivery fails like a broken pipe.
        *backend.stdin.lock().await = None;
        let res = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("two".into())],
                metadata: CommandMeta {
                    client_msg_id: Some("m2".into()),
                    ..Default::default()
                },
            })
            .await;
        assert!(
            matches!(&res, Err(BackendError::Transport(_))),
            "a delivery failure AFTER the first successful send must stay Transport (honest terminal), got {res:?}"
        );
    }

    /// Resume-hang startup guard: a `--resume` whose on-disk session is a broken husk
    /// hangs the claude process — it emits ZERO frames and never EOFs. The reader's
    /// STARTUP-ONLY zero-frame timeout must fire and surface a terminal `Detached` so
    /// the FSM folds Error{Crashed}, the UI unlocks, and the next get_or_build
    /// evicts+self-heals — instead of parking forever in `read`.
    /// MUTATION-PROVEN: drop the startup `timeout` wrap and this test hangs (the outer
    /// 5s guard fails) — a bare `read().await` never returns on a zero-frame hang.
    #[tokio::test]
    async fn zero_frame_hung_startup_times_out_to_terminal_detached() {
        // never_exits + a gated tail never released = empty prefix (zero frames),
        // stdout stays open (never EOFs), exit never fires → a true startup hang.
        let fake = FakeAgentIo::never_exits(Vec::new()).with_gated_tail(b"unused".to_vec());
        // Short budget so the guard fires fast instead of waiting the real 30s.
        // SAFETY: restored below; the assertion is about the TERMINAL, not the value.
        let saved = std::env::var("AIONUI_HANDSHAKE_TIMEOUT_SECS").ok();
        unsafe { std::env::set_var("AIONUI_HANDSHAKE_TIMEOUT_SECS", "1") };

        let backend = ClaudeSessionBackend::build_with_io("hung-1", Box::new(fake)).await;
        let mut events = backend.events();
        backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("hello".into())],
                metadata: CommandMeta {
                    client_msg_id: Some("m1".into()),
                    ..Default::default()
                },
            })
            .await
            .expect("dispatch accepted");

        let mut saw_detached = false;
        for _ in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), events.next()).await {
                Ok(Some(env)) => {
                    if let SessionEvent::Detached { exit, .. } = env.event {
                        // A hang exit is unknown (we never wait_for_exit) → None →
                        // reducer maps it to Error{Crashed} (the unlock terminal).
                        assert_eq!(exit, None, "a zero-frame hang reports unknown exit (None)");
                        saw_detached = true;
                        break;
                    }
                }
                _ => break,
            }
        }

        match saved {
            Some(v) => unsafe { std::env::set_var("AIONUI_HANDSHAKE_TIMEOUT_SECS", v) },
            None => unsafe { std::env::remove_var("AIONUI_HANDSHAKE_TIMEOUT_SECS") },
        }
        assert!(
            saw_detached,
            "a zero-frame hung startup must surface a terminal Detached (guard), not park forever"
        );
    }

    /// Owner-decision tripwire: once the process has produced its FIRST frame it is
    /// proven alive, so a subsequent mid-turn stall must NOT be timed out — a long
    /// turn that thinks/runs tools silently for longer than the budget is normal and
    /// must keep running, never be killed. Here the prefix emits one assistant frame
    /// (disarms the startup guard) then the gated tail is never released (a silent
    /// stall longer than the 1s budget). The reader must stay parked WITHOUT emitting
    /// a terminal — no premature Detached. MUTATION-PROVEN: make the read stay bounded
    /// after the first frame (drop the `if seen_frame` unbounded branch) and a
    /// spurious Detached appears → this assertion fails.
    #[tokio::test]
    async fn first_frame_disarms_startup_guard_long_silent_turn_not_killed() {
        let prefix = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
            "\n"
        )
        .as_bytes()
        .to_vec();
        // Prefix flows immediately (one frame → seen_frame latches); the gated tail is
        // NEVER released → the process then goes silent for longer than the budget.
        let fake = FakeAgentIo::never_exits(prefix).with_gated_tail(b"never-sent".to_vec());
        let saved = std::env::var("AIONUI_HANDSHAKE_TIMEOUT_SECS").ok();
        unsafe { std::env::set_var("AIONUI_HANDSHAKE_TIMEOUT_SECS", "1") };

        let backend = ClaudeSessionBackend::build_with_io("alive-1", Box::new(fake)).await;
        let mut events = backend.events();
        backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("hello".into())],
                metadata: CommandMeta {
                    client_msg_id: Some("m1".into()),
                    ..Default::default()
                },
            })
            .await
            .expect("dispatch accepted");

        // Drain a few events; we must see the assistant frame but NEVER a Detached,
        // even after waiting well past the 1s budget (the stall is not timed).
        let mut saw_message = false;
        let mut saw_terminal = false;
        for _ in 0..10 {
            // A timeout slice (no event) just means the turn is still silently
            // running — keep waiting past the budget; only an actual event matters.
            if let Ok(Some(env)) = tokio::time::timeout(std::time::Duration::from_millis(400), events.next()).await {
                match env.event {
                    SessionEvent::MessageDelta { .. } => saw_message = true,
                    SessionEvent::Detached { .. } | SessionEvent::TurnResult { .. } => {
                        saw_terminal = true;
                        break;
                    }
                    _ => {}
                }
            }
        }

        match saved {
            Some(v) => unsafe { std::env::set_var("AIONUI_HANDSHAKE_TIMEOUT_SECS", v) },
            None => unsafe { std::env::remove_var("AIONUI_HANDSHAKE_TIMEOUT_SECS") },
        }
        assert!(
            saw_message,
            "the first assistant frame must surface (proves the process is alive)"
        );
        assert!(
            !saw_terminal,
            "a long SILENT turn (alive, just slow) must NOT be timed out after the first frame"
        );
    }

    /// Windows pipe-EOF gap: after the first frame proves the process alive
    /// (`seen_frame` latched), the process EXITS but its stdout NEVER EOFs — modelling
    /// a surviving grandchild (detached MCP/tool descendant) that inherited the write
    /// handle and keeps the pipe's write end open, so `stdout.read()` never returns
    /// `Ok(0)`. The reader must NOT park forever: the exit-watch leg of the read race
    /// wins, and a terminal `Detached` fires carrying the captured exit status → the
    /// reducer folds Error{Crashed}/CleanNoResult → the UI unlocks instead of wedging
    /// at `pending` with no error.
    ///
    /// This is the mirror of `first_frame_disarms_startup_guard_long_silent_turn_not_killed`:
    /// there the process is ALIVE (never_exits) and must stay parked; here the process
    /// is GONE (release_exit) and must terminate. The two together pin the exact
    /// boundary — terminate on real exit, never on mere silence.
    /// MUTATION-PROVEN: revert the `seen_frame` branch to a bare `stdout.read().await`
    /// (drop the exit-watch select) and this test hangs (the process exited but the
    /// pipe never EOFs → no terminal → the 3s guard fails).
    #[tokio::test]
    async fn process_exit_without_eof_surfaces_terminal_detached() {
        let prefix = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"partial"}]}}"#,
            "\n"
        )
        .as_bytes()
        .to_vec();
        // Prefix flows immediately (one frame → seen_frame latches). The gated tail is
        // NEVER released → the writer parks holding the duplex open, so stdout NEVER
        // EOFs (the Windows inherited-handle case). But the process DOES exit
        // (release_exit), which is the orthogonal signal the reader must react to.
        let fake = FakeAgentIo::new(
            prefix,
            Some(crate::event::ExitStatusLite {
                code: Some(137), // SIGKILL-style exit, as a `taskkill`'d leaf would report
                signal: None,
            }),
        )
        .with_gated_tail(b"never-released".to_vec());
        fake.release_exit(); // the process is gone, even though stdout stays open

        let backend = ClaudeSessionBackend::build_with_io("win-eof-1", Box::new(fake)).await;
        let mut events = backend.events();
        backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("hello".into())],
                metadata: CommandMeta {
                    client_msg_id: Some("m1".into()),
                    ..Default::default()
                },
            })
            .await
            .expect("dispatch accepted");

        let mut saw_message = false;
        let mut detached_exit: Option<Option<crate::event::ExitStatusLite>> = None;
        for _ in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_secs(3), events.next()).await {
                Ok(Some(env)) => match env.event {
                    SessionEvent::MessageDelta { .. } => saw_message = true,
                    SessionEvent::Detached { exit, .. } => {
                        detached_exit = Some(exit);
                        break;
                    }
                    _ => {}
                },
                _ => break,
            }
        }

        assert!(
            saw_message,
            "the pre-exit assistant frame must surface (proves seen_frame)"
        );
        assert_eq!(
            detached_exit,
            Some(Some(crate::event::ExitStatusLite {
                code: Some(137),
                signal: None,
            })),
            "process exit without EOF must surface a terminal Detached reusing the captured exit status"
        );
    }

    /// G2 tripwire: when a backend process exits with allowlisted stderr (e.g. a
    /// usage-limit line), the terminal `Detached` carries the REDACTED summary so
    /// the conversation layer can tell the user *why* — and a non-allowlisted
    /// secret-bearing line is NEVER surfaced. The redaction happens at the backend
    /// boundary (`redact_exit_stderr`), so raw stderr never crosses into the event.
    #[tokio::test]
    async fn detached_carries_redacted_stderr_summary_on_crash() {
        // No stdout frames; the process just dies after writing stderr.
        let fake = FakeAgentIo::new(
            Vec::new(),
            Some(crate::event::ExitStatusLite {
                code: Some(1),
                signal: None,
            }),
        )
        .with_stderr(
            "DEBUG bootstrap: loaded ANTHROPIC_API_KEY=sk-ant-0123456789abcdef\n\
             ERROR codex_acp::thread: You've hit your usage limit, try again later",
        );
        fake.release_exit();
        let backend = ClaudeSessionBackend::build_with_io("logical-g2", Box::new(fake)).await;
        let mut events = backend.events();

        let mut redacted: Option<Option<String>> = None;
        for _ in 0..10 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), events.next()).await {
                Ok(Some(env)) => {
                    if let SessionEvent::Detached { redacted_summary, .. } = env.event {
                        redacted = Some(redacted_summary);
                        break;
                    }
                }
                _ => break,
            }
        }
        let summary = redacted
            .expect("a Detached must arrive")
            .expect("allowlisted stderr must yield a redacted summary");
        assert!(
            summary.contains("usage limit"),
            "the allowlisted reason surfaces; got {summary}"
        );
        assert!(
            !summary.contains("sk-ant"),
            "the secret on the non-allowlisted line must never leak; got {summary}"
        );
    }

    /// 009 R1a: a final `result` frame truncated mid-write (no trailing newline —
    /// e.g. the process was SIGKILLed/OOM'd while flushing it) must NOT be
    /// silently dropped. The reader's EOF tail-flush parses the trailing
    /// half-line as a final frame, so its TurnResult still surfaces BEFORE the
    /// Detached. Reverse control: without the flush, only the `\n`-terminated
    /// assistant frame would surface and the turn's result would vanish.
    #[tokio::test]
    async fn truncated_final_result_is_flushed_at_eof_not_lost() {
        let fixture = {
            let mut v = Vec::new();
            v.extend_from_slice(
                concat!(
                    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"the answer is 42"}]}}"#,
                    "\n",
                    // Final result frame WITH NO TRAILING NEWLINE — truncated write.
                    r#"{"type":"result","subtype":"success","is_error":false,"result":"42"}"#,
                )
                .as_bytes(),
            );
            v
        };
        // SIGKILL exit (signal 9) models the OOM/kill that truncated the write.
        let fake = FakeAgentIo::new(
            fixture,
            Some(crate::event::ExitStatusLite {
                code: None,
                signal: Some(9),
            }),
        );
        fake.release_exit();
        let backend = ClaudeSessionBackend::build_with_io("trunc-1", Box::new(fake)).await;
        let mut events = backend.events();
        let _ = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("q".into())],
                metadata: CommandMeta::default(),
            })
            .await
            .expect("dispatch accepted");

        let mut saw_turn_result = false;
        let mut turn_result_before_detached = false;
        for _ in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), events.next()).await {
                Ok(Some(env)) => match env.event {
                    SessionEvent::TurnResult { .. } => saw_turn_result = true,
                    SessionEvent::Detached { .. } => {
                        turn_result_before_detached = saw_turn_result;
                        break;
                    }
                    _ => {}
                },
                _ => break,
            }
        }
        assert!(
            saw_turn_result,
            "the truncated final result frame must be flushed at EOF, not silently dropped"
        );
        assert!(
            turn_result_before_detached,
            "the flushed TurnResult must arrive BEFORE the terminal Detached (drain-before-honor)"
        );
    }

    #[tokio::test]
    async fn unsupported_commands_are_rejected_by_capability() {
        // Reject matrix: every cap=false command MUST return the EXACT
        // CommandNotSupported{command} — never silently accept (Layer-2 rule).
        // SetMode/SetModel are NO LONGER here (G2 wired the in-band switch → cap=true,
        // dispatch accepts); their accept path is covered by set_mode/set_model tests.
        let io = Box::new(FakeAgentIo::never_exits(Vec::new()));
        let backend = ClaudeSessionBackend::build_with_io("s", io).await;
        let caps = backend.capabilities();
        // cap honesty: each rejected command is advertised false.
        assert!(!caps.supported_commands.rewind);
        assert!(!caps.supported_commands.list_checkpoints);
        assert!(!caps.supported_commands.answer_auth);
        assert!(!caps.supported_commands.steer);
        // G2: set_mode/set_model are now advertised TRUE (wired in-band).
        assert!(caps.supported_commands.set_mode);
        assert!(caps.supported_commands.set_model);
        assert!(!caps.supported_commands.cancel_tool);
        // Attachment caps: image + resource are now advertised TRUE (deliver_prompt
        // emits a native base64 image block / a Read-tool path ref); audio +
        // at_mention remain false (no working claude input path).
        assert!(caps.prompt_blocks.image, "image cap true (native base64 block)");
        assert!(caps.prompt_blocks.resource, "resource cap true (Read-tool path ref)");
        assert!(!caps.prompt_blocks.audio);
        assert!(!caps.prompt_blocks.at_mention);

        assert!(matches!(
            backend.dispatch(Command::Rewind { num_turns: 1 }).await,
            Err(BackendError::CommandNotSupported { command: "rewind" })
        ));
        assert!(matches!(
            backend.dispatch(Command::ListCheckpoints).await,
            Err(BackendError::CommandNotSupported {
                command: "list_checkpoints"
            })
        ));
        assert!(matches!(
            backend
                .dispatch(Command::AnswerAuth {
                    method_id: "x".into(),
                    credentials: serde_json::Value::Null
                })
                .await,
            Err(BackendError::CommandNotSupported { command: "answer_auth" })
        ));
        assert!(matches!(
            backend.dispatch(Command::Steer { content: Vec::new() }).await,
            Err(BackendError::CommandNotSupported { command: "steer" })
        ));
        assert!(matches!(
            backend
                .dispatch(Command::Cancel {
                    target: CancelTarget::Tool("t".into())
                })
                .await,
            Err(BackendError::CommandNotSupported { command: "cancel_tool" })
        ));
        // Un-advertised content blocks are still rejected before wire-write
        // (audio / at_mention), keyed on their content_block:<kind> name.
        assert!(matches!(
            backend
                .dispatch(Command::Send {
                    content: vec![ContentBlock::Audio {
                        data: vec![0],
                        media_type: "audio/wav".into()
                    }],
                    metadata: CommandMeta::default(),
                })
                .await,
            Err(BackendError::CommandNotSupported {
                command: "content_block:audio"
            })
        ));
        assert!(matches!(
            backend
                .dispatch(Command::Send {
                    content: vec![ContentBlock::AtMention { user_id: "u1".into() }],
                    metadata: CommandMeta::default(),
                })
                .await,
            Err(BackendError::CommandNotSupported {
                command: "content_block:at_mention"
            })
        ));
    }

    /// `Acknowledge` (user-ack of a done-unseen turn) is accepted as a pure no-op:
    /// claude has no "acknowledge" wire concept — it folds at the conversation
    /// fold-on-read layer, never the backend (§C1). It must NOT be rejected
    /// (cap_behavior excludes it from the gated set) and must NOT write any frame
    /// or open a turn — `NoTurn`, no stdin write. (The only claude dispatch arm
    /// without its own test before this; closes the claude dispatch-arm coverage.)
    #[tokio::test]
    async fn acknowledge_is_accepted_as_noturn_noop_no_wire() {
        let fake = FakeAgentIo::never_exits(Vec::new());
        let captured = fake.captured_stdin();
        let backend = ClaudeSessionBackend::build_with_io("s-ack", Box::new(fake)).await;

        let receipt = backend
            .dispatch(Command::Acknowledge { node_id: "n-1".into() })
            .await
            .expect("Acknowledge is always accepted (never CommandNotSupported)");
        assert!(receipt.accepted);
        assert_eq!(
            receipt.admission,
            Admission::NoTurn,
            "Acknowledge folds at read layer; it must not open a turn"
        );

        // Give any (erroneous) async write a chance to land, then assert stdin stayed empty.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let written = String::from_utf8_lossy(&captured.lock().await.clone()).to_string();
        assert!(
            written.trim().is_empty(),
            "Acknowledge must write NOTHING to the claude wire, got: {written:?}"
        );
    }

    /// §C5 HARD acceptance: claude parse ZERO-DIFF. The new ClaudeSessionBackend
    /// MUST surface exactly the SessionEvent sequence the legacy
    /// `ClaudeAdapter::parse_chunk` produces for the same bytes — the wrapping
    /// (envelope/turn_gen/reader) must not add, drop, reorder, or mutate any
    /// parsed event. (Both paths share the same parser, so this pins the WRAPPING
    /// invariant — the only place the new path could diverge.)
    #[tokio::test]
    async fn claude_parse_is_zero_diff_vs_legacy() {
        // A realistic F1-shape multi-frame turn (the shape claude --print emits
        // without --include-partial-messages): system noise, an assistant text +
        // tool_use, a user tool_result, and the terminal result.
        let frames = [
            r#"{"type":"system","subtype":"init","session_id":"s","tools":[]}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#,
        ];
        let bytes: Vec<u8> = format!("{}\n", frames.join("\n")).into_bytes();

        // (a) LEGACY ground truth: feed the bytes straight through parse_chunk.
        let legacy_events: Vec<SessionEvent> = {
            let mut parser = ClaudeAdapter::new();
            parser.parse_chunk(&bytes)
        };
        assert!(!legacy_events.is_empty(), "fixture must produce events");

        // (b) NEW path: drive the same bytes through ClaudeSessionBackend; collect
        // the parsed events (unwrapped from envelopes), EXCLUDING the wrapper-only
        // additions the new seam legitimately introduces (synthesized
        // PromptAccepted from dispatch, and the EOF Detached the reader appends).
        let fake = FakeAgentIo::new(
            bytes.clone(),
            Some(crate::event::ExitStatusLite {
                code: Some(0),
                signal: None,
            }),
        );
        fake.release_exit();
        let backend = ClaudeSessionBackend::build_with_io("logical-1", Box::new(fake)).await;
        let mut events = backend.events();
        // No PromptAccepted here: dispatch with client_msg_id:None (so the only
        // events are the parsed ones + the terminal Detached).
        backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("go".into())],
                metadata: CommandMeta::default(),
            })
            .await
            .expect("accepted");

        let mut new_events: Vec<SessionEvent> = Vec::new();
        for _ in 0..50 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), events.next()).await {
                Ok(Some(env)) => match env.event {
                    SessionEvent::Detached { .. } => break, // reader's EOF marker (wrapper-only)
                    // wrapper-only reader/dispatch additions (NOT from parse_chunk):
                    SessionEvent::PromptAccepted { .. }
                    | SessionEvent::BackendBound { .. }
                    | SessionEvent::SubagentUpdate { .. } => continue,
                    ev => new_events.push(ev),
                },
                _ => break,
            }
        }

        // ZERO-DIFF: the parsed event sequence is identical.
        assert_eq!(
            new_events, legacy_events,
            "ClaudeSessionBackend must surface the legacy parse sequence verbatim \
             (wrapping adds only the dispatch PromptAccepted + EOF Detached)"
        );
    }

    /// §C5 HARD acceptance over a REAL captured fixture (claude 2.1.169, a real
    /// single-tool subagent turn, 15 frames). Same zero-diff invariant against a
    /// production-shape byte stream — pins that real frame volume/ordering
    /// survives the wrapping unchanged.
    #[tokio::test]
    async fn claude_parse_zero_diff_over_real_fixture() {
        let bytes = include_str!("../../tests/fixtures/claude_2.1.169_single_tool_turn.ndjson")
            .as_bytes()
            .to_vec();

        let legacy_events: Vec<SessionEvent> = {
            let mut parser = ClaudeAdapter::new();
            parser.parse_chunk(&bytes)
        };
        assert!(legacy_events.len() >= 3, "real fixture must produce several events");

        let fake = FakeAgentIo::new(
            bytes.clone(),
            Some(crate::event::ExitStatusLite {
                code: Some(0),
                signal: None,
            }),
        );
        fake.release_exit();
        let backend = ClaudeSessionBackend::build_with_io("real-1", Box::new(fake)).await;
        let mut events = backend.events();
        backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("go".into())],
                metadata: CommandMeta::default(),
            })
            .await
            .expect("accepted");

        let mut new_events: Vec<SessionEvent> = Vec::new();
        for _ in 0..100 {
            match tokio::time::timeout(std::time::Duration::from_secs(3), events.next()).await {
                Ok(Some(env)) => match env.event {
                    SessionEvent::Detached { .. } => break,
                    // Reader-side WRAPPER additions (NOT from parse_chunk): synthesized
                    // PromptAccepted, B-CLAUDE-INIT Provisioning + BackendBound from
                    // the raw system/init frame, SubagentUpdate sniffed from the raw
                    // system/task_* frames, and ConfigChanged sniffed from the raw
                    // system/init|status permissionMode (sniff_mode — the real fixture's
                    // init carries permissionMode:bypassPermissions). The zero-diff
                    // contract is over the PARSED stream, so these reader-side sniffs are
                    // excluded.
                    SessionEvent::PromptAccepted { .. }
                    | SessionEvent::Provisioning { .. }
                    | SessionEvent::BackendBound { .. }
                    | SessionEvent::ConfigChanged { .. }
                    | SessionEvent::SubagentUpdate { .. } => continue,
                    ev => new_events.push(ev),
                },
                _ => break,
            }
        }

        assert_eq!(
            new_events, legacy_events,
            "real-fixture parse must be verbatim through the new seam (excl. reader-side PromptAccepted + Provisioning)"
        );

        // 009 H5 load-bearing: the zero-diff assert above only proves old==new — it
        // would PASS even if subagent attribution were dropped on BOTH legs (the
        // exact "froze unattributed as covered" trap the test-coverage audit §4
        // flagged). This fixture's subagent frames carry
        // parent_tool_use_id=toolu_bdrk_01AnD5Af6r9vYWvADBW8tCqt, so a correctly
        // attributing parser MUST surface it on a ToolCall/ToolResult. Revert the
        // adapter's top-level parent_tool_use_id read → every parent becomes None →
        // this fails.
        let attributed = new_events.iter().any(|e| {
            matches!(
                e,
                SessionEvent::ToolCall { parent_tool_use_id: Some(p), .. }
                    | SessionEvent::ToolResult { parent_tool_use_id: Some(p), .. }
                if p == "toolu_bdrk_01AnD5Af6r9vYWvADBW8tCqt"
            )
        });
        assert!(
            attributed,
            "H5: a subagent tool step must carry its frame parent_tool_use_id, got {new_events:?}"
        );
    }

    /// MAJOR-1 (codex-M2 mirror): AnswerPermission MUST write the keyed
    /// control_response to stdin AND broadcast PermissionResolved — not silently
    /// accept-and-drop (which wedges the can_use_tool turn forever). Feeds a
    /// can_use_tool control_request (so the reader registers it), answers it, and
    /// asserts both effects.
    #[tokio::test]
    async fn answer_permission_writes_control_response_and_resolves() {
        let fixture = concat!(
            r#"{"type":"control_request","request_id":"req-7","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"toolu-7","input":{"command":"ls"}}}"#,
            "\n",
        )
        .as_bytes()
        .to_vec();
        // never_exits: the persistent process stays alive so we can answer + read
        // back what we wrote on stdin.
        let fake = FakeAgentIo::never_exits(fixture);
        let captured = fake.captured_stdin();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;

        // Wait for the reader to surface Permission{request_id} (pending registered).
        let mut events = backend.events();
        let saw_perm = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::Permission { ref request_id, .. } if request_id == "req-7") {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(saw_perm, "can_use_tool surfaced as Permission{{request_id}}");

        let receipt = backend
            .dispatch(Command::AnswerPermission {
                request_id: "req-7".into(),
                decision: super::super::types::PermissionDecision::Approved,
                selected: None,
                answers: Vec::new(),
            })
            .await
            .expect("answer accepted");
        assert_eq!(receipt.admission, Admission::NoTurn);

        // (a) a control_response keyed to req-7 + echoing toolUseID hit stdin.
        let written = {
            let mut s = String::new();
            for _ in 0..40 {
                s = String::from_utf8_lossy(&captured.lock().await.clone()).to_string();
                if s.contains("control_response") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            s
        };
        assert!(
            written.contains(r#""type":"control_response""#),
            "wrote control_response, got: {written}"
        );
        assert!(
            written.contains(r#""request_id":"req-7""#),
            "echoes the request_id, got: {written}"
        );
        assert!(
            written.contains(r#""toolUseID":"toolu-7""#),
            "echoes the toolUseID, got: {written}"
        );
        assert!(
            written.contains(r#""behavior":"allow""#),
            "Approved → allow, got: {written}"
        );
        // DEPTH (anti-shallow-assertion): a plain-tool allow MUST carry updatedInput
        // (a record) — claude's stdio schema ZodErrors without it and the approved
        // tool never runs. This end-to-end test drives a Bash allow, so it must
        // assert the frame field, not just the {type,id,behavior} shell (the gap
        // that let the missing-updatedInput regression ship). Echoes the original
        // input {"command":"ls"}.
        assert!(
            written.contains(r#""updatedInput""#) && written.contains(r#""command":"ls""#),
            "plain-tool allow frame MUST carry updatedInput == original input (ZodError guard), got: {written}"
        );

        // (b) PermissionResolved broadcast (FSM leaves requires-action).
        let saw_resolved = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::PermissionResolved { ref request_id, .. } if request_id == "req-7")
                {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(saw_resolved, "AnswerPermission broadcasts PermissionResolved{{req-7}}");
    }

    /// REST-recovery source: `pending_permission_requests()` lists the OUTSTANDING
    /// permission (request_id + tool_name) after a `can_use_tool` arrives, and is
    /// EMPTY after `AnswerPermission` consumes it. This is the data
    /// `GET /confirmations` projects to rebuild a reloaded permission card; the
    /// answer-clears-it half proves the list never shows an already-answered card.
    #[tokio::test]
    async fn pending_permission_requests_lists_open_then_clears_on_answer() {
        let fixture = concat!(
            r#"{"type":"control_request","request_id":"req-9","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"toolu-9","input":{"command":"ls"}}}"#,
            "\n",
        )
        .as_bytes()
        .to_vec();
        let fake = FakeAgentIo::never_exits(fixture);
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;

        // Wait for the reader to register the pending permission.
        let mut events = backend.events();
        let saw = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::Permission { ref request_id, .. } if request_id == "req-9") {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(saw, "can_use_tool registered the pending permission");

        // The recovery view lists it: request_id + tool_name, NO raw input exposed.
        let pending = backend.pending_permission_requests();
        assert_eq!(pending.len(), 1, "one outstanding permission, got {pending:?}");
        assert_eq!(pending[0].request_id, "req-9");
        assert_eq!(pending[0].tool_name, "Bash");

        // Answering it removes it from the pending set → recovery lists nothing
        // (the card is no longer outstanding, so it must not re-surface on reload).
        backend
            .dispatch(Command::AnswerPermission {
                request_id: "req-9".into(),
                decision: super::super::types::PermissionDecision::Approved,
                selected: None,
                answers: Vec::new(),
            })
            .await
            .expect("answer accepted");
        assert!(
            backend.pending_permission_requests().is_empty(),
            "answered permission no longer outstanding"
        );
    }

    /// G-A regression: `dispatch(Cancel)` MUST write a `control_request{subtype:
    /// "interrupt"}` to the retained stdin — the WIRE-OUT oracle the old live test
    /// lacked (it only asserted the FSM folds to Idle, which the reducer does
    /// unconditionally, so a no-op Cancel stub passed). This pins "cancel actually
    /// interrupts the long-lived claude", not just "our side unlocked". (Equivalent of
    /// the deleted legacy `cancel_writes_interrupt_control_request_to_stdin`.)
    #[tokio::test]
    async fn cancel_writes_interrupt_control_request_to_stdin() {
        use super::super::types::CancelTarget;
        // never_exits: the persistent process stays alive so we can read back stdin.
        let fake = FakeAgentIo::never_exits(Vec::new());
        let captured = fake.captured_stdin();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;

        let receipt = backend
            .dispatch(Command::Cancel {
                target: CancelTarget::Turn,
            })
            .await
            .expect("cancel accepted");
        assert_eq!(receipt.admission, Admission::NoTurn);

        let written = {
            let mut s = String::new();
            for _ in 0..40 {
                s = String::from_utf8_lossy(&captured.lock().await.clone()).to_string();
                if s.contains("interrupt") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            s
        };
        assert!(
            written.contains(r#""type":"control_request""#),
            "cancel wrote a control_request to stdin (not a no-op stub), got: {written:?}"
        );
        assert!(
            written.contains(r#""subtype":"interrupt""#),
            "cancel's control_request is an interrupt, got: {written:?}"
        );
    }

    #[tokio::test]
    async fn answer_permission_unknown_request_is_rejected() {
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(FakeAgentIo::never_exits(Vec::new()))).await;
        let err = backend
            .dispatch(Command::AnswerPermission {
                request_id: "nope".into(),
                decision: super::super::types::PermissionDecision::Denied,
                selected: None,
                answers: Vec::new(),
            })
            .await
            .expect_err("no pending → reject (not silent-accept)");
        assert!(matches!(err, BackendError::Transport(m) if m.contains("no pending permission")));
    }

    /// race-audit conn-10: claude can RETRACT an outstanding `can_use_tool` via a
    /// `control_cancel_request` (e.g. a hook resolved it, or the turn was
    /// interrupted) BEFORE the user answers. The reader must drop the pending entry
    /// so a subsequently-arriving `AnswerPermission` sees None and is REJECTED —
    /// never builds a stale `control_response` for a request claude no longer
    /// awaits (which would desync the CLI). The retract must be TARGETED: a second,
    /// un-retracted permission stays answerable.
    ///
    /// Determinism: the reader is a single in-order task and the retract emits no
    /// SessionEvent, so a second permission (req-2) fed AFTER the req-1 cancel acts
    /// as a sequencing barrier — observing Permission{req-2} proves req-1's
    /// control_cancel_request was already consumed.
    #[tokio::test]
    async fn control_cancel_request_retracts_pending_so_answer_is_rejected() {
        let fixture = concat!(
            // 1) req-1 can_use_tool → Permission{req-1}, registers pending[req-1].
            r#"{"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"toolu-1","input":{"command":"ls"}}}"#,
            "\n",
            // 2) claude RETRACTS req-1 → reader removes pending[req-1] (no event).
            r#"{"type":"control_cancel_request","request_id":"req-1"}"#,
            "\n",
            // 3) req-2 can_use_tool → Permission{req-2}; observing this proves the
            //    in-order reader already processed the req-1 retract above.
            r#"{"type":"control_request","request_id":"req-2","request":{"subtype":"can_use_tool","tool_name":"Write","tool_use_id":"toolu-2","input":{"file":"x"}}}"#,
            "\n",
        )
        .as_bytes()
        .to_vec();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(FakeAgentIo::never_exits(fixture))).await;
        let mut events = backend.events();

        // Barrier: wait for Permission{req-2} (⇒ req-1's retract already consumed).
        let saw_req2 = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::Permission { ref request_id, .. } if request_id == "req-2") {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(
            saw_req2,
            "req-2 permission surfaced (sequencing barrier past the req-1 retract)"
        );

        // Answering the RETRACTED req-1 must be rejected (pending was dropped) —
        // NOT silently answered with a stale control_response.
        let err = backend
            .dispatch(Command::AnswerPermission {
                request_id: "req-1".into(),
                decision: super::super::types::PermissionDecision::Approved,
                selected: None,
                answers: Vec::new(),
            })
            .await
            .expect_err("retracted req-1 → no pending → reject");
        assert!(
            matches!(err, BackendError::Transport(m) if m.contains("no pending permission")),
            "retracted permission must reject, not build a stale control_response"
        );

        // The retract is TARGETED: req-2 (never retracted) is still answerable.
        let receipt = backend
            .dispatch(Command::AnswerPermission {
                request_id: "req-2".into(),
                decision: super::super::types::PermissionDecision::Approved,
                selected: None,
                answers: Vec::new(),
            })
            .await
            .expect("un-retracted req-2 still answerable (retract was not a blanket wipe)");
        assert_eq!(receipt.admission, Admission::NoTurn);
    }

    /// G2: SetMode / SetModel while IDLE (no turn in flight) write the in-band
    /// control_request to stdin IMMEDIATELY. Proves cap=true ↔ dispatch accepts + the
    /// real wire shape (probe-verified control_request{subtype:set_permission_mode|set_model}).
    ///
    /// Confirmation semantics (design §9.10.1): SetModel emits ConfigChanged
    /// OPTIMISTICALLY (its ack carries no model echo, Optimistic tier); SetMode does
    /// NOT (de-optimistic — confirmed by the inbound system/status, see
    /// `claude_advertises_fixed_modes_and_remembers_mode_from_status` +
    /// `sniff_mode_emits_config_changed_from_system_status`). Here we assert the wire
    /// frames + the SetModel optimistic ConfigChanged; SetMode's ConfigChanged is NOT
    /// expected at dispatch.
    #[tokio::test]
    async fn set_mode_and_model_write_in_band_control_request() {
        let fake = FakeAgentIo::never_exits(Vec::new());
        let captured = fake.captured_stdin();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;
        let mut events = backend.events();

        // SetMode (idle) → immediate control_request{set_permission_mode}, NO ConfigChanged.
        let receipt = backend
            .dispatch(Command::SetMode { mode: "plan".into() })
            .await
            .expect("SetMode accepted (cap=true)");
        assert_eq!(receipt.admission, Admission::NoTurn);

        // SetModel (idle) → control_request{set_model} + OPTIMISTIC ConfigChanged{model}.
        backend
            .dispatch(Command::SetModel { model: "sonnet".into() })
            .await
            .expect("SetModel accepted (cap=true)");
        let cfg = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::ConfigChanged { mode, model } = env.event {
                    return Some((mode, model));
                }
            }
            None
        })
        .await
        .expect("a ConfigChanged emitted");
        // The ONLY ConfigChanged at dispatch is SetModel's optimistic model emit —
        // SetMode emits none (de-optimistic), so the first ConfigChanged is model:sonnet.
        assert_eq!(
            cfg,
            Some((None, Some("sonnet".to_string()))),
            "SetModel → optimistic ConfigChanged{{model:sonnet}}; SetMode emits no ConfigChanged at dispatch"
        );

        let written = {
            let mut s = String::new();
            for _ in 0..40 {
                s = String::from_utf8_lossy(&captured.lock().await.clone()).to_string();
                if s.contains("set_permission_mode") && s.contains("set_model") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            s
        };
        assert!(
            written.contains(r#""type":"control_request""#),
            "in-band switch is a control_request, got: {written}"
        );
        assert!(
            written.contains(r#""subtype":"set_permission_mode""#) && written.contains(r#""mode":"plan""#),
            "set_permission_mode frame on the wire, got: {written}"
        );
        assert!(
            written.contains(r#""subtype":"set_model""#) && written.contains(r#""model":"sonnet""#),
            "set_model frame on the wire, got: {written}"
        );
    }

    /// #99: SetConfigOption{effort} writes the in-band
    /// `control_request{apply_flag_settings, settings:{effortLevel}}` (LIVE-PROBED
    /// 2.1.181 — NOT set_effort). A non-effort option id rejects (cap=false ↔ reject).
    #[tokio::test]
    async fn set_config_option_effort_writes_apply_flag_settings() {
        let fake = FakeAgentIo::never_exits(Vec::new());
        let captured = fake.captured_stdin();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;

        let receipt = backend
            .dispatch(Command::SetConfigOption {
                option_id: "effort".into(),
                value: "high".into(),
            })
            .await
            .expect("effort SetConfigOption accepted");
        assert_eq!(receipt.admission, Admission::NoTurn);

        let written = {
            let mut s = String::new();
            for _ in 0..40 {
                s = String::from_utf8_lossy(&captured.lock().await.clone()).to_string();
                if s.contains("apply_flag_settings") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            s
        };
        assert!(
            written.contains(r#""subtype":"apply_flag_settings""#) && written.contains(r#""effortLevel":"high""#),
            "effort → apply_flag_settings{{effortLevel}} on the wire, got: {written}"
        );

        // CP-1: claude does not echo effort back, so the backend must REMEMBER it →
        // capabilities().current_effort reflects the last-set level (this is the
        // genuinely-new state; model/mode are backend-reported, effort is not).
        assert_eq!(
            backend.capabilities().current_effort.as_deref(),
            Some("high"),
            "the backend remembers the set effort for current_effort"
        );

        // A non-effort generic option id is rejected (no claude wire for it).
        let err = backend
            .dispatch(Command::SetConfigOption {
                option_id: "verbosity".into(),
                value: "loud".into(),
            })
            .await
            .expect_err("unknown config option → CommandNotSupported");
        assert!(matches!(err, BackendError::CommandNotSupported { command } if command == "set_config_option"));
    }

    /// #1 effort catalog validation (ACP `clear_invalid_desired_*` ported to effort).
    /// Once the initialize control_response has advertised a model with a bounded
    /// `supportedEffortLevels` set, a `SetConfigOption{effort}` for a level OUTSIDE that
    /// set is REJECTED (BadRequest-style Transport error) instead of being written and
    /// poisoning `current_effort` — while a level INSIDE the set still applies. Before
    /// the catalog lands (empty), any level is permissive (matches the empty-catalog
    /// semantics of ACP `is_*_valid`, covered by the test above).
    #[tokio::test]
    async fn set_config_option_effort_validates_against_model_catalog() {
        // Catalog: one model advertising only low/medium/high (NO "max").
        let init_resp = r#"{"type":"control_response","response":{"subtype":"success","request_id":"ctl-1","response":{"models":[{"value":"default","displayName":"Default","supportedEffortLevels":["low","medium","high"]}]}}}"#;
        let fake = FakeAgentIo::never_exits(format!("{init_resp}\n").into_bytes());
        let captured = fake.captured_stdin();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;
        let _events = backend.events();
        // Wait for the catalog to land.
        for _ in 0..40 {
            if !backend.capabilities().available_models.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        // An UNSUPPORTED level ("max") is rejected — no wire, no current_effort poison.
        let err = backend
            .dispatch(Command::SetConfigOption {
                option_id: "effort".into(),
                value: "max".into(),
            })
            .await
            .expect_err("effort not in the model's catalog → rejected");
        assert!(
            matches!(err, BackendError::Transport(msg) if msg.contains("not supported")),
            "unsupported effort must be rejected as an error"
        );
        assert!(
            backend.capabilities().current_effort.is_none(),
            "a rejected effort must NOT poison current_effort"
        );

        // A SUPPORTED level ("high") still applies.
        backend
            .dispatch(Command::SetConfigOption {
                option_id: "effort".into(),
                value: "high".into(),
            })
            .await
            .expect("a catalog-valid effort is accepted");
        let written = {
            let mut s = String::new();
            for _ in 0..40 {
                s = String::from_utf8_lossy(&captured.lock().await.clone()).to_string();
                if s.contains("apply_flag_settings") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            s
        };
        assert!(
            written.contains(r#""effortLevel":"high""#),
            "a valid effort reaches the wire, got: {written}"
        );
        assert_eq!(backend.capabilities().current_effort.as_deref(), Some("high"));
    }

    /// `ultracode` is surfaced as an effort level for xhigh-capable models (mirroring the
    /// CLI's own picker entry) but dispatches the DEDICATED boolean flag
    /// `apply_flag_settings{settings:{ultracode:true}}` — NOT `effortLevel:"ultracode"`
    /// (which our own `effort_is_supported` gate would reject since it is absent from
    /// `supportedEffortLevels`). Wire LIVE-PROBED 2.1.206
    /// (samples/claude-cli/2.1.206/ultracode_wire.result.md).
    #[tokio::test]
    async fn set_config_option_ultracode_writes_boolean_flag() {
        // Catalog: a model advertising xhigh → fill_discovery injects "ultracode".
        let init_resp = r#"{"type":"control_response","response":{"subtype":"success","request_id":"ctl-1","response":{"models":[{"value":"default","displayName":"Default","supportedEffortLevels":["low","medium","high","xhigh"]}]}}}"#;
        let fake = FakeAgentIo::never_exits(format!("{init_resp}\n").into_bytes());
        let captured = fake.captured_stdin();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;
        let _events = backend.events();
        for _ in 0..40 {
            if !backend.capabilities().available_models.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        // The synthetic level is advertised (so the picker shows it AND the gate passes).
        assert!(
            backend
                .capabilities()
                .available_models
                .iter()
                .any(|m| m.reasoning_efforts.iter().any(|e| e == "ultracode")),
            "ultracode must be injected into an xhigh-capable model's efforts"
        );

        backend
            .dispatch(Command::SetConfigOption {
                option_id: "reasoning_effort".into(),
                value: "ultracode".into(),
            })
            .await
            .expect("ultracode SetConfigOption accepted");

        let written = {
            let mut s = String::new();
            for _ in 0..40 {
                s = String::from_utf8_lossy(&captured.lock().await.clone()).to_string();
                if s.contains("apply_flag_settings") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            s
        };
        assert!(
            written.contains(r#""subtype":"apply_flag_settings""#) && written.contains(r#""ultracode":true"#),
            "ultracode → apply_flag_settings{{ultracode:true}} on the wire, got: {written}"
        );
        assert!(
            !written.contains(r#""effortLevel":"ultracode""#),
            "ultracode must NOT ride the effortLevel field, got: {written}"
        );
        assert_eq!(
            backend.capabilities().current_effort.as_deref(),
            Some("ultracode"),
            "the backend remembers ultracode for the picker highlight"
        );
    }

    /// `ultracode` is injected ONLY for xhigh-capable models — a model that tops out at
    /// `high` must NOT gain the synthetic level (matches the CLI gate: ultracode requires
    /// xhigh). Guards against offering a level the model can never engage.
    #[tokio::test]
    async fn ultracode_not_injected_for_non_xhigh_model() {
        let init_resp = r#"{"type":"control_response","response":{"subtype":"success","request_id":"ctl-1","response":{"models":[{"value":"default","displayName":"Default","supportedEffortLevels":["low","medium","high"]}]}}}"#;
        let fake = FakeAgentIo::never_exits(format!("{init_resp}\n").into_bytes());
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;
        let _events = backend.events();
        for _ in 0..40 {
            if !backend.capabilities().available_models.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            backend
                .capabilities()
                .available_models
                .iter()
                .all(|m| m.reasoning_efforts.iter().all(|e| e != "ultracode")),
            "a non-xhigh model must not advertise ultracode"
        );
        // And the gate rejects it (not in this model's catalog).
        let err = backend
            .dispatch(Command::SetConfigOption {
                option_id: "reasoning_effort".into(),
                value: "ultracode".into(),
            })
            .await
            .expect_err("ultracode not offered by a non-xhigh model → rejected");
        assert!(matches!(err, BackendError::Transport(msg) if msg.contains("not supported")));
    }

    /// Mode read/advertise parity: claude advertises its permission modes in
    /// `available_modes` (so the picker has data), VERBATIM-EQUIVALENT to the legacy
    /// ACP bridge `buildAvailableModes` — same ids, same order (default, acceptEdits,
    /// plan, dontAsk, bypassPermissions). `auto` is omitted because the bridge gates it
    /// on `supportsAutoMode`, which the direct CLI never reports (see
    /// `claude_permission_modes`). current_mode is now REMEMBERED from claude's inbound
    /// `system/status{permissionMode}` (design §9.10.1 option A — de-optimistic), NOT
    /// optimistically at dispatch. This drives a system/status through the reader (as
    /// claude emits when a mode actually applies) and asserts current_mode reflects it.
    #[tokio::test]
    async fn claude_advertises_fixed_modes_and_remembers_mode_from_status() {
        // The fake emits a system/status{permissionMode:plan} (the real applied-mode
        // signal, shape from protocols/samples/claude-cli/2.1.187/_all_autonomous_mode.jsonl).
        let status = r#"{"type":"system","subtype":"status","permissionMode":"plan","session_id":"s"}"#;
        let fake = FakeAgentIo::never_exits(format!("{status}\n").into_bytes());
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;

        // The advertised modes carry the EXACT wire ids claude accepts, in the legacy
        // bridge's order. `auto` is gated out (see claude_permission_modes doc).
        let caps = backend.capabilities();
        let ids: Vec<&str> = caps.available_modes.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["default", "acceptEdits", "plan", "dontAsk", "bypassPermissions"],
            "claude advertises the legacy-equivalent permission-mode picker (picker data source)"
        );
        assert!(
            caps.available_modes
                .iter()
                .all(|m| !m.name.is_empty() && m.description.is_some()),
            "each mode carries display name + description"
        );

        // Subscribe drives the reader; it consumes the system/status → sniff_mode sets
        // current_mode_override. Poll until the merge lands.
        let _events = backend.events();
        let mut cur = backend.capabilities().current_mode;
        for _ in 0..40 {
            if cur.as_deref() == Some("plan") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            cur = backend.capabilities().current_mode;
        }
        assert_eq!(
            cur.as_deref(),
            Some("plan"),
            "current_mode reflects the inbound system/status applied mode (not an optimistic dispatch value)"
        );
    }

    /// G2: a SetModel issued WHILE A TURN IS IN FLIGHT is QUEUED (not written
    /// mid-turn, which would truncate the turn) and drained over stdin BEFORE the
    /// next prompt — so the switch applies to the next turn. Proves the queue +
    /// drain ordering: the control_request bytes precede the next user prompt bytes.
    #[tokio::test]
    async fn set_model_mid_turn_is_queued_and_drained_before_next_prompt() {
        let fake = FakeAgentIo::never_exits(Vec::new());
        let captured = fake.captured_stdin();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;

        // First Send → turn_in_flight=true (the reader never sees a terminal here,
        // never_exits + no fixture, so the flag stays set: models "mid-turn").
        backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("first".into())],
                metadata: CommandMeta::default(),
            })
            .await
            .expect("first Send accepted");

        // SetModel now → QUEUED (turn in flight), nothing new on the wire yet.
        backend
            .dispatch(Command::SetModel { model: "opus".into() })
            .await
            .expect("SetModel accepted (queued)");
        let before = String::from_utf8_lossy(&captured.lock().await.clone()).to_string();
        assert!(
            !before.contains("set_model"),
            "mid-turn SetModel must NOT write to the wire yet (queued), got: {before}"
        );

        // Next Send drains the queued control_request BEFORE writing the prompt.
        backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("second".into())],
                metadata: CommandMeta::default(),
            })
            .await
            .expect("second Send accepted");

        let written = {
            let mut s = String::new();
            for _ in 0..40 {
                s = String::from_utf8_lossy(&captured.lock().await.clone()).to_string();
                if s.contains("set_model") && s.contains("second") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            s
        };
        let set_model_at = written.find("set_model").expect("queued set_model drained to wire");
        let second_prompt_at = written.find("second").expect("second prompt on wire");
        assert!(
            set_model_at < second_prompt_at,
            "the queued set_model must be drained BEFORE the next prompt (else it truncates the turn), got: {written}"
        );
    }

    /// G2: repeated mid-turn switches of the SAME kind collapse to the latest
    /// (last-write-wins de-dup) — only the final model is drained, not every one.
    #[tokio::test]
    async fn mid_turn_same_kind_switches_dedup_to_latest() {
        let fake = FakeAgentIo::never_exits(Vec::new());
        let captured = fake.captured_stdin();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;
        backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("first".into())],
                metadata: CommandMeta::default(),
            })
            .await
            .expect("first Send");
        // Three SetModel mid-turn → only the last survives the de-dup.
        for m in ["sonnet", "haiku", "opus"] {
            backend
                .dispatch(Command::SetModel { model: m.into() })
                .await
                .expect("SetModel queued");
        }
        backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("second".into())],
                metadata: CommandMeta::default(),
            })
            .await
            .expect("second Send");
        let written = {
            let mut s = String::new();
            for _ in 0..40 {
                s = String::from_utf8_lossy(&captured.lock().await.clone()).to_string();
                if s.contains("opus") && s.contains("second") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            s
        };
        assert!(
            written.contains(r#""model":"opus""#),
            "latest model survives, got: {written}"
        );
        assert!(
            !written.contains(r#""model":"sonnet""#) && !written.contains(r#""model":"haiku""#),
            "earlier same-kind switches are de-duped away, got: {written}"
        );
    }

    /// 2A: the chosen AskUserQuestion option label (Command::AnswerPermission.selected)
    /// MUST ride into `updatedInput.answers:{question: <selected>}` — NOT the
    /// first-option degrade. Proves a user picking the 2nd option is answered correctly.
    #[test]
    fn build_control_response_uses_selected_label_over_first_option() {
        use super::super::types::PermissionDecision;
        let pending = PendingPerm {
            tool_use_id: "toolu-1".into(),
            tool_name: "AskUserQuestion".into(),
            input: serde_json::json!({
                "questions": [{
                    "question": "Pick one",
                    "options": [{"label": "Alpha"}, {"label": "Beta"}]
                }]
            }),
        };
        // User picked "Beta" (the SECOND option) — must be answered, not "Alpha".
        let resp = build_control_response("req-1", &pending, PermissionDecision::Approved, Some("Beta"), &[]);
        let answers = &resp["response"]["response"]["updatedInput"]["answers"];
        assert_eq!(
            answers["Pick one"], "Beta",
            "explicit selected label is the answer, got: {resp}"
        );
        assert_eq!(resp["response"]["response"]["behavior"], "allow");
        assert_eq!(resp["response"]["response"]["toolUseID"], "toolu-1");

        // No `selected` → degrade to the first option (a plain allow).
        let degraded = build_control_response("req-1", &pending, PermissionDecision::Approved, None, &[]);
        assert_eq!(
            degraded["response"]["response"]["updatedInput"]["answers"]["Pick one"], "Alpha",
            "None selected → first-option degrade"
        );

        // Denied ignores the label entirely → deny body.
        let denied = build_control_response("req-1", &pending, PermissionDecision::Denied, Some("Beta"), &[]);
        assert_eq!(denied["response"]["response"]["behavior"], "deny");
    }

    /// Task #83 (load-bearing): claude can ask MULTIPLE questions in one call and a
    /// question can be `multiSelect:true`. The full per-question `answers` set MUST
    /// cover EVERY question (keyed by question text), with a multi-select value
    /// emitted as a JSON ARRAY of labels — the live-captured 2.1.178 wire
    /// (`protocols/samples/claude-cli/2.1.178/ask_user_question_multi_array.ndjson`).
    /// Reverting `build_ask_user_question_answers` to the old `questions.first()`
    /// single-answer path makes this test fail (only the first question answered,
    /// no array) — the regression guard for the silent under-answer bug.
    #[test]
    fn build_control_response_answers_all_questions_with_multiselect_array() {
        use super::super::types::{PermissionDecision, QuestionAnswer};
        // Two questions: a single-select + a multiSelect — the exact shape claude
        // emits (see the array fixture's control_request input).
        let pending = PendingPerm {
            tool_use_id: "toolu-9".into(),
            tool_name: "AskUserQuestion".into(),
            input: serde_json::json!({
                "questions": [
                    { "question": "Which language?", "header": "Language",
                      "options": [{"label": "Rust"}, {"label": "Go"}, {"label": "TypeScript"}],
                      "multiSelect": false },
                    { "question": "Which features do you want?", "header": "Features",
                      "options": [{"label": "Auth"}, {"label": "Logging"}, {"label": "Metrics"}],
                      "multiSelect": true }
                ]
            }),
        };
        let answers = vec![
            QuestionAnswer {
                question: "Which language?".into(),
                labels: vec!["Rust".into()],
            },
            QuestionAnswer {
                question: "Which features do you want?".into(),
                labels: vec!["Auth".into(), "Logging".into()],
            },
        ];
        let resp = build_control_response("req-9", &pending, PermissionDecision::Approved, None, &answers);
        let updated = &resp["response"]["response"]["updatedInput"];
        let ans = &updated["answers"];

        // EVERY question is answered (both keys present) — not just the first.
        assert_eq!(
            ans["Which language?"], "Rust",
            "single-select → bare label, got: {resp}"
        );
        // multi-select → JSON ARRAY of labels (claude joins it with ", " itself).
        assert_eq!(
            ans["Which features do you want?"],
            serde_json::json!(["Auth", "Logging"]),
            "multi-select → array of labels, got: {resp}"
        );
        assert_eq!(
            ans.as_object().map(serde_json::Map::len),
            Some(2),
            "all questions answered (no silent under-answer), got: {resp}"
        );
        // The original input (questions) is preserved alongside the injected answers.
        assert!(updated["questions"].is_array(), "original input echoed");
        assert_eq!(resp["response"]["response"]["behavior"], "allow");
        assert_eq!(resp["response"]["response"]["toolUseID"], "toolu-9");
    }

    /// REGRESSION (the ZodError that killed every plain-tool approval): allowing a
    /// NON-AskUserQuestion tool (Bash/Write/Edit) MUST include `updatedInput` (a
    /// record) — claude's stdio control-response schema rejects an allow branch
    /// without it (`expected record, received undefined`), so the approved tool never
    /// runs. The plain-tool allow branch previously emitted only {behavior, toolUseID}.
    /// This was the coverage blind spot: the only permission test exercised
    /// AskUserQuestion (which always carries updatedInput), so the plain-tool path
    /// shipped untested. Echo the original input unchanged.
    #[test]
    fn build_control_response_plain_tool_allow_carries_updated_input() {
        use super::super::types::PermissionDecision;
        let pending = PendingPerm {
            tool_use_id: "toolu-bash".into(),
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": "ls" }),
        };
        let resp = build_control_response("req-bash", &pending, PermissionDecision::Approved, None, &[]);
        let body = &resp["response"]["response"];
        assert_eq!(body["behavior"], "allow");
        assert_eq!(body["toolUseID"], "toolu-bash");
        // updatedInput MUST be present (a record) and equal the original input —
        // never null/undefined (that is the exact ZodError trigger).
        assert!(
            body["updatedInput"].is_object(),
            "plain-tool allow MUST carry updatedInput as a record (ZodError guard), got: {resp}"
        );
        assert_eq!(
            body["updatedInput"]["command"], "ls",
            "original tool input echoed unchanged"
        );
    }

    /// Defensive: a non-object tool input still yields a valid `{}` record (never
    /// `undefined`), so the allow frame can't re-trigger the union failure.
    #[test]
    fn build_control_response_plain_tool_allow_non_object_input_falls_back_to_empty_record() {
        use super::super::types::PermissionDecision;
        let pending = PendingPerm {
            tool_use_id: "toolu-x".into(),
            tool_name: "Weird".into(),
            input: serde_json::json!("not-an-object"),
        };
        let resp = build_control_response("req-x", &pending, PermissionDecision::Approved, None, &[]);
        let updated = &resp["response"]["response"]["updatedInput"];
        assert!(
            updated.is_object() && updated.as_object().unwrap().is_empty(),
            "fallback {{}} record, got: {updated}"
        );
    }

    #[tokio::test]
    async fn dropping_backend_aborts_reader() {
        // MAJOR-3 (codex-M5 mirror): drop must abort the reader so a mid-turn /
        // hung-claude process is reaped (never_exits models the no-EOF case).
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(FakeAgentIo::never_exits(Vec::new()))).await;
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

    #[tokio::test]
    async fn b_claude_init_captures_current_model_and_emits_mcp_provisioning() {
        // B-CLAUDE-INIT: the system/init frame's `model` → capabilities().current_model
        // (config supplied none via build_with_io), and mcp_servers[] → Provisioning
        // events (connected→ToolsReady, failed→LoadFailed, needs-auth→Degraded).
        let init = r#"{"type":"system","subtype":"init","session_id":"s","model":"global.anthropic.claude-opus-4-8","tools":[],"mcp_servers":[{"name":"ok","status":"connected"},{"name":"bad","status":"failed"},{"name":"auth","status":"needs-auth"}]}"#;
        let bytes = format!("{init}\n").into_bytes();
        let fake = FakeAgentIo::never_exits(bytes);
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;
        let mut events = backend.events();

        // Collect the Provisioning events the reader emits from the init mcp_servers.
        let mut phases = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::Provisioning { phase } = env.event {
                    phases.push(phase);
                    if phases.len() == 3 {
                        return;
                    }
                }
            }
        })
        .await;
        assert_eq!(phases.len(), 3, "one Provisioning per mcp server, got {phases:?}");
        assert!(
            phases
                .iter()
                .any(|p| matches!(p, crate::event::ProvisioningPhase::ToolsReady)),
            "connected→ToolsReady"
        );
        assert!(
            phases
                .iter()
                .any(|p| matches!(p, crate::event::ProvisioningPhase::LoadFailed { .. })),
            "failed→LoadFailed"
        );
        assert!(
            phases
                .iter()
                .any(|p| matches!(p, crate::event::ProvisioningPhase::Degraded { .. })),
            "needs-auth→Degraded"
        );
        // current_model captured from init (config gave none).
        assert_eq!(
            backend.capabilities().current_model.as_deref(),
            Some("global.anthropic.claude-opus-4-8"),
            "init model → capabilities().current_model"
        );
    }

    #[tokio::test]
    async fn b_claude_init_does_not_override_config_model() {
        // config model is authoritative: when build_with_io seeds a model (it does
        // not — defaults None — so we test the inverse: when config HAS a model, the
        // init wire model must NOT overwrite it). build_with_io uses default config
        // (None), so here we assert the wire fills it; the config-wins path is
        // covered by the want_init_model gate (config.model.is_none()).
        let init = r#"{"type":"system","subtype":"init","session_id":"s","model":"wire-model","tools":[]}"#;
        let fake = FakeAgentIo::never_exits(format!("{init}\n").into_bytes());
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;
        let _events = backend.events();
        for _ in 0..40 {
            if backend.capabilities().current_model.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(backend.capabilities().current_model.as_deref(), Some("wire-model"));
    }

    /// #98/#101: the reader sniffs the `control_request{initialize}` RESPONSE for the
    /// selectable model list + slash commands and fills `capabilities()`. Wire shape
    /// pinned from the live 2.1.181 probe (fixture
    /// protocols/samples/claude-cli/2.1.181/control_initialize_response): the success
    /// payload nests the init response under `response.response`, models carry
    /// `value`/`displayName`/`supportedEffortLevels`, commands carry `name`/`description`.
    #[tokio::test]
    async fn control_initialize_response_fills_models_and_slash_commands() {
        let init_resp = r#"{"type":"control_response","response":{"subtype":"success","request_id":"ctl-1","response":{"models":[{"value":"default","displayName":"Default","description":"Use the default model","supportsEffort":true,"supportedEffortLevels":["low","medium","high","max"]},{"value":"opus","displayName":"global.anthropic.claude-opus-4-8","description":"Custom Opus model"}],"commands":[{"name":"deep-research","description":"Deep research harness","argumentHint":""},{"name":"verify","description":"Verify claims"}]}}}"#;
        let fake = FakeAgentIo::never_exits(format!("{init_resp}\n").into_bytes());
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;
        let _events = backend.events();
        // Poll until the catalog lands (the reader is async, like discovered_model).
        for _ in 0..40 {
            if !backend.capabilities().available_models.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let caps = backend.capabilities();
        // Models: value→id, displayName→name, supportedEffortLevels→reasoning_efforts.
        assert_eq!(caps.available_models.len(), 2, "two models parsed");
        assert_eq!(caps.available_models[0].id, "default");
        assert_eq!(caps.available_models[0].name, "Default");
        assert_eq!(
            caps.available_models[0].reasoning_efforts,
            vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "max".to_string()
            ],
            "supportedEffortLevels → reasoning_efforts (the #99 effort surface)"
        );
        assert_eq!(caps.available_models[1].id, "opus");
        assert_eq!(caps.available_models[1].name, "global.anthropic.claude-opus-4-8");
        assert!(
            caps.available_models[1].reasoning_efforts.is_empty(),
            "a model without supportedEffortLevels → empty efforts"
        );
        // Slash commands: name + description.
        assert_eq!(caps.slash_commands.len(), 2, "two slash commands parsed");
        assert_eq!(caps.slash_commands[0].name, "deep-research");
        assert_eq!(
            caps.slash_commands[0].description.as_deref(),
            Some("Deep research harness")
        );
        assert_eq!(caps.slash_commands[1].name, "verify");
    }

    /// The FIX (async catalog-arrival signal): when the `initialize` RESPONSE lands the
    /// reader must BROADCAST a `CatalogUpdated` so the conversation re-projects the
    /// picker — before this, the catalog silently filled `discovered_caps` with no
    /// upward signal and the frontend (which read an empty `config_options` on open)
    /// never re-fetched, leaving the model selector permanently disabled. Asserts the
    /// event carries the parsed models AND claude's fixed permission modes (the frontend
    /// replaces its whole snapshot on this frame, so the modes must ride along or the
    /// mode picker would be wiped).
    #[tokio::test]
    async fn control_initialize_response_broadcasts_catalog_updated() {
        use futures_util::StreamExt as _;
        let init_resp = r#"{"type":"control_response","response":{"subtype":"success","request_id":"ctl-1","response":{"models":[{"value":"default","displayName":"Default"},{"value":"opus","displayName":"global.anthropic.claude-opus-4-8"}],"commands":[{"name":"verify","description":"Verify claims"}]}}}"#;
        let fake = FakeAgentIo::never_exits(format!("{init_resp}\n").into_bytes());
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;
        // Subscribe BEFORE the reader drains the frame so the broadcast is observed.
        let mut events = backend.events();

        let mut catalog = None;
        for _ in 0..40 {
            if let Ok(Some(env)) = tokio::time::timeout(std::time::Duration::from_millis(200), events.next()).await
                && let SessionEvent::CatalogUpdated {
                    models,
                    modes,
                    slash_commands,
                } = env.event
            {
                catalog = Some((models, modes, slash_commands));
                break;
            }
        }
        let (models, modes, slash_commands) = catalog.expect("a CatalogUpdated must be broadcast on initialize");
        assert_eq!(models.len(), 2, "parsed models ride the event");
        assert_eq!(models[0].id, "default");
        assert_eq!(models[1].id, "opus");
        // claude's permission modes must ride along (whole-snapshot replace),
        // legacy-bridge order, `auto` gated out (see claude_permission_modes).
        let mode_ids: Vec<&str> = modes.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            mode_ids,
            vec!["default", "acceptEdits", "plan", "dontAsk", "bypassPermissions"],
            "the permission modes ride the catalog event so the mode picker survives the snapshot replace"
        );
        assert_eq!(slash_commands.len(), 1, "slash commands ride the event");
        assert_eq!(slash_commands[0].name, "verify");
    }

    /// A non-initialize success control_response (e.g. a set_model ack, which has no
    /// `models`/`commands`) must NOT clobber the catalog — the request_id-free sniff
    /// keys on the presence of `models`/`commands`, not on a correlation id.
    #[tokio::test]
    async fn non_initialize_control_response_does_not_touch_catalog() {
        // A set_model-style success with no models/commands, THEN an initialize reply.
        let other = r#"{"type":"control_response","response":{"subtype":"success","request_id":"ctl-7","response":{"ok":true}}}"#;
        let init = r#"{"type":"control_response","response":{"subtype":"success","request_id":"ctl-1","response":{"models":[{"value":"default","displayName":"Default"}]}}}"#;
        let fake = FakeAgentIo::never_exits(format!("{other}\n{init}\n").into_bytes());
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;
        let _events = backend.events();
        for _ in 0..40 {
            if !backend.capabilities().available_models.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        // The init reply still landed (the prior non-init success was a no-op, not a clobber).
        assert_eq!(backend.capabilities().available_models.len(), 1);
        assert_eq!(backend.capabilities().available_models[0].id, "default");
    }

    /// Bug-A (regression A, claude proactive=true): a TurnResult from a turn that was
    /// SUPERSEDED by a proactive resend must carry that turn's OWN (older) epoch — the
    /// epoch locked at its `system/init` — NOT the read-time `turn_gen` (which the
    /// resend already bumped). This is the reader-level mechanism that lets the
    /// reducer's cross-turn guard (result_epoch < since_epoch) drop the stale
    /// `is_error` result instead of surfacing it as a spurious Error bubble.
    ///
    /// Sequence (hermetic, deterministic — mirrors `_all_zerogap_cancel.jsonl` C):
    ///   Send#1 (turn_gen 0→1) → init#1 locks turn_open_epoch=1 → Send#2/resend
    ///   (turn_gen 1→2) → turn-1's late result is read AFTER the bump. Without the fix
    ///   it would be stamped 2 (== the resend turn's since_epoch → NOT dropped). The
    ///   fix stamps it 1 (the open-turn epoch) so it is older than the resend turn.
    #[tokio::test]
    async fn bug_a_late_result_keeps_superseded_turn_epoch_not_readtime() {
        // Two gated segments: [0]=turn-1's system/init, [1]=turn-1's late is_error result.
        let init1 = r#"{"type":"system","subtype":"init","session_id":"s"}"#;
        let late_result = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"s"}"#;
        let fake = FakeAgentIo::never_exits(Vec::new()).with_gated_segments(vec![
            format!("{init1}\n").into_bytes(),
            format!("{late_result}\n").into_bytes(),
        ]);
        let seg = fake.segment_releaser();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;
        let mut events = backend.events();

        // Send#1 → turn_gen 0→1 (the turn that will be superseded).
        backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("first".into())],
                metadata: CommandMeta::default(),
            })
            .await
            .expect("Send#1 accepted");
        // Release segment 0: turn-1's system/init → reader locks turn_open_epoch = 1.
        seg();

        // Wait until the init has been observed (turn_open_epoch is now locked to 1).
        // We can't read turn_open_epoch directly, so gate on a tiny settle then proceed;
        // the segment gate guarantees ordering (segment 1 is not released yet).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Send#2 (the proactive resend) → turn_gen 1→2, BEFORE the late result is read.
        backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("resend".into())],
                metadata: CommandMeta::default(),
            })
            .await
            .expect("Send#2 (resend) accepted");
        // Now release segment 1: turn-1's LATE result, read while turn_gen == 2.
        seg();

        // Collect the TurnResult envelope and assert its epoch is the SUPERSEDED turn's
        // locked epoch (1), not the read-time turn_gen (2).
        let tr_epoch = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::TurnResult { .. }) {
                    return Some(env.turn_gen);
                }
            }
            None
        })
        .await
        .expect("timed out waiting for the late TurnResult")
        .expect("a TurnResult envelope");
        assert_eq!(
            tr_epoch, 1,
            "the superseded turn's late result must carry its OWN turn-open epoch (1), \
             NOT the read-time turn_gen (2) the resend bumped to — else the reducer's \
             cross-turn guard cannot drop it (spurious Error bubble, bug-A)"
        );
    }

    /// B (regression A): claude's replay of OUR stamped uuid (--replay-user-messages)
    /// surfaces PromptAccepted{client_msg_id: uuid} — the Native ack that replaced the
    /// flush-ok synthesized emit. A claude-MINTED user frame (tool_result content, or
    /// the [Request interrupted] ghost) must NOT spuriously emit one for a top-level
    /// prompt id (tool_result is skipped; a ghost's own uuid simply never matches a
    /// pending client_msg_id downstream, but we also skip tool_result frames here).
    #[tokio::test]
    async fn replay_of_stamped_uuid_emits_prompt_accepted_minted_frames_do_not() {
        let our_replay =
            r#"{"type":"user","uuid":"cm-9","message":{"role":"user","content":[{"type":"text","text":"do it"}]}}"#;
        // A claude-minted continuation: a tool_result user frame (carries claude's own
        // uuid). Must NOT yield a PromptAccepted (skipped as a tool_result frame).
        let minted_tool_result = r#"{"type":"user","uuid":"claude-mint-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#;
        let fake = FakeAgentIo::never_exits(format!("{minted_tool_result}\n{our_replay}\n").into_bytes());
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(fake)).await;
        let mut events = backend.events();

        // Collect PromptAccepted ids until our replay's id arrives (or timeout). The
        // minted tool_result precedes it on the wire; if it wrongly emitted, we'd see
        // "claude-mint-1" FIRST.
        let mut accepted_ids: Vec<String> = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::PromptAccepted { client_msg_id } = env.event {
                    accepted_ids.push(client_msg_id.clone());
                    if client_msg_id == "cm-9" {
                        break;
                    }
                }
            }
        })
        .await;
        assert_eq!(
            accepted_ids,
            vec!["cm-9".to_string()],
            "ONLY our stamped-uuid replay emits PromptAccepted; the minted tool_result frame does not"
        );
    }

    /// F-4 default: with idle_ttl=None (the production default via build_with_io),
    /// the backend NEVER suspends — no idle timer, slot stays Active. Proves the
    /// opt-in invariant that protects the parse zero-diff acceptance.
    #[tokio::test]
    async fn f4_off_by_default_no_suspension() {
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(FakeAgentIo::never_exits(Vec::new()))).await;
        assert!(backend.idle_timer.is_none(), "no idle timer when idle_ttl is None");
        assert_eq!(backend.suspend.idle_ttl_ms(), None);
        assert!(backend.suspend.is_active().await, "slot Active");
        // Even after a wait, the slot is still Active (nothing can suspend it).
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert!(
            backend.suspend.is_active().await,
            "stays Active forever (production parity)"
        );
    }

    /// F-4 suspend→wake: a configured idle_ttl makes the idle timer suspend the
    /// idle process; the next dispatch(Send) wakes via the supplied spawner,
    /// routing to `--resume <logical_id>` (the resume recipe). FakeSpawner records
    /// the spawn then Errs (can't make a real process), so dispatch surfaces the
    /// wake error — which is the observable proof the resume path ran with the
    /// right args. (A live re-spawn is a real-binary concern; the hermetic proof
    /// is "the wake recipe routed `--resume <id>` through the injected spawner".)
    #[tokio::test]
    async fn f4_suspend_then_wake_routes_resume_through_spawner() {
        use crate::testing::FakeSpawner;
        let spawner = Arc::new(FakeSpawner::new());
        // ttl 40ms → idle_check_interval clamps to 1s; drive suspension directly to
        // avoid a 1s wait, then assert wake on dispatch.
        let backend = ClaudeSessionBackend::build_with_io_suspending(
            "logical-resume-1",
            Box::new(FakeAgentIo::never_exits(Vec::new())),
            spawner.clone(),
            40,
        )
        .await;
        assert!(backend.idle_timer.is_some(), "idle timer spawned when ttl is Some");
        assert!(backend.suspend.is_active().await, "starts Active");

        // Force a suspend (idle past ttl) without waiting on the 1s timer cadence.
        let suspended = backend
            .suspend
            .suspend_if_idle(aionui_common::now_ms() + 10_000, false)
            .await;
        assert!(suspended, "idle past ttl → suspended");
        assert!(!backend.suspend.is_active().await, "now Dormant");

        // The next Send must wake → route `--resume logical-resume-1` through the
        // spawner. FakeSpawner Errs, so dispatch returns that wake error.
        let err = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("wake up".into())],
                metadata: CommandMeta::default(),
            })
            .await
            .expect_err("FakeSpawner cannot make a real process → wake Errs");
        assert!(
            matches!(&err, BackendError::Transport(m) if m.contains("resume-spawn failed")),
            "dispatch surfaced the wake re-spawn error, got {err:?}"
        );
        assert_eq!(
            spawner.call_count(),
            1,
            "wake routed through the injected spawner exactly once"
        );
        let spec = spawner.last_command().await.expect("a spawn was recorded");
        assert!(
            spec.args.iter().any(|a| a == "--resume") && spec.args.iter().any(|a| a == "logical-resume-1"),
            "wake spawns with `--resume <logical_id>` (resume continuity), got args {:?}",
            spec.args
        );
        drop(backend); // idle timer + (Dormant) controller tear down cleanly
    }

    /// #103: `config.spawn_env` (the cc-switch provider env the app registry fills for
    /// backend == "claude") MUST reach the spawned process's `CommandSpec.env`. Before
    /// this fix the adapter hardcoded `env: Vec::new()`, so a cc-switch third-party
    /// relay user's claude process never saw `ANTHROPIC_BASE_URL`/`AUTH_TOKEN`.
    #[tokio::test]
    async fn spawn_env_is_injected_into_command_spec() {
        use crate::testing::FakeSpawner;
        let spawner = Arc::new(FakeSpawner::new());
        let conn = ClaudeConnection::new(spawner.clone());
        let config = SessionConfig {
            spawn_env: vec![
                aionui_common::EnvVar {
                    name: "ANTHROPIC_BASE_URL".into(),
                    value: "https://relay.example".into(),
                },
                aionui_common::EnvVar {
                    name: "ANTHROPIC_AUTH_TOKEN".into(),
                    value: "tok-123".into(),
                },
            ],
            ..Default::default()
        };
        // FakeSpawner RECORDS the CommandSpec then Errs (no real process), so
        // open_session surfaces a spawn error — but the spec we care about was already
        // captured. (Same hermetic pattern as f4_suspend_then_wake.)
        let _ = conn
            .open_session(
                SessionSpec::Fresh {
                    session_id: "11111111-1111-4111-8111-111111111111".into(),
                },
                config,
            )
            .await;
        let spec = spawner.last_command().await.expect("a spawn was recorded");
        let base = spec.env.iter().find(|e| e.name == "ANTHROPIC_BASE_URL");
        let tok = spec.env.iter().find(|e| e.name == "ANTHROPIC_AUTH_TOKEN");
        assert_eq!(base.map(|e| e.value.as_str()), Some("https://relay.example"));
        assert_eq!(tok.map(|e| e.value.as_str()), Some("tok-123"));
    }

    /// #103 parity: an empty `spawn_env` (no cc-switch config, or a non-claude backend
    /// the app never fills) yields an empty `CommandSpec.env` — byte-identical to the
    /// pre-#103 spawn (inherit the parent env only).
    #[tokio::test]
    async fn empty_spawn_env_yields_empty_command_env() {
        use crate::testing::FakeSpawner;
        let spawner = Arc::new(FakeSpawner::new());
        let conn = ClaudeConnection::new(spawner.clone());
        let _ = conn
            .open_session(
                SessionSpec::Fresh {
                    session_id: "22222222-2222-4222-8222-222222222222".into(),
                },
                SessionConfig::default(),
            )
            .await;
        let spec = spawner.last_command().await.expect("a spawn was recorded");
        assert!(
            spec.env.is_empty(),
            "no spawn_env → empty CommandSpec.env, got {:?}",
            spec.env
        );
    }

    /// F-4 #1-critical regression: a turn in flight (set by dispatch(Send)) must
    /// prevent the idle timer from suspending the process MID-TURN — otherwise the
    /// reader is aborted before it emits the terminal and the FSM strands in Running.
    /// dispatch(Send) sets turn_in_flight; suspend_if_idle(.., turn_active=true) must
    /// then refuse to close even though the slot is idle past the ttl.
    #[tokio::test]
    async fn f4_turn_in_flight_blocks_idle_suspend() {
        use crate::testing::FakeSpawner;
        // never_exits → the reader stays blocked (turn "in flight"); a real Send
        // sets turn_in_flight=true and the fixture never emits a terminal to clear it.
        let backend = ClaudeSessionBackend::build_with_io_suspending(
            "logical-live-1",
            Box::new(FakeAgentIo::never_exits(Vec::new())),
            Arc::new(FakeSpawner::new()),
            40,
        )
        .await;
        backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("long turn".into())],
                metadata: CommandMeta::default(),
            })
            .await
            .expect("send accepted (slot already Active)");
        assert!(
            backend.turn_in_flight.load(std::sync::atomic::Ordering::SeqCst),
            "dispatch(Send) marks the turn in flight"
        );
        // Idle WAY past the ttl, but turn_active=true → MUST NOT suspend.
        let suspended = backend
            .suspend
            .suspend_if_idle(aionui_common::now_ms() + 10_000, true)
            .await;
        assert!(!suspended, "a live turn is never suspended even when idle past ttl");
        assert!(
            backend.suspend.is_active().await,
            "process kept resident for the live turn"
        );
        drop(backend);
    }

    #[tokio::test]
    async fn sniff_task_emits_subagent_update_lifecycle() {
        // §6b b1: claude system/task_* frames → SubagentUpdate (keyed by task_id,
        // parent = tool_use_id, label = subagent_type/workflow_name). task_started →
        // Running; task_notification{status} → terminal. The reducer upserts these
        // into Running.subagents, which drives has_foreground_activity.
        let frames = [
            r#"{"type":"system","subtype":"task_started","task_id":"tk-1","tool_use_id":"toolu-9","subagent_type":"general-purpose"}"#,
            r#"{"type":"system","subtype":"task_notification","task_id":"tk-1","tool_use_id":"toolu-9","status":"completed"}"#,
        ];
        let bytes = format!("{}\n", frames.join("\n")).into_bytes();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(FakeAgentIo::never_exits(bytes))).await;
        let mut events = backend.events();

        let mut updates = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::SubagentUpdate {
                    r#ref,
                    status,
                    parent_ref,
                    label,
                } = env.event
                {
                    updates.push((r#ref, status, parent_ref, label));
                    if updates.len() == 2 {
                        return;
                    }
                }
            }
        })
        .await;

        assert_eq!(
            updates.len(),
            2,
            "task_started + task_notification → 2 SubagentUpdate, got {updates:?}"
        );
        // started → Running, keyed by task_id, parent = tool_use_id, label = subagent_type.
        assert_eq!(updates[0].0, "tk-1", "ref = task_id");
        assert_eq!(
            updates[0].1,
            crate::event::SubagentStatus::Running,
            "task_started → Running"
        );
        assert_eq!(updates[0].2.as_deref(), Some("toolu-9"), "parent_ref = tool_use_id");
        assert_eq!(
            updates[0].3.as_deref(),
            Some("general-purpose"),
            "label = subagent_type"
        );
        // notification completed → Completed, SAME ref (lifecycle upsert).
        assert_eq!(updates[1].0, "tk-1", "same ref across the lifecycle");
        assert_eq!(
            updates[1].1,
            crate::event::SubagentStatus::Completed,
            "status=completed → Completed"
        );
    }

    /// sniff_mode: claude's AUTHORITATIVE mode signal is `permissionMode` on a
    /// `system/status` frame — emitted for BOTH a user-driven set AND an autonomous
    /// change (plan-exit). The reader adopts it (normal→default) as current_mode AND
    /// emits ConfigChanged{mode} (design §9.10.1 option A; README #10). Wire shape from
    /// protocols/samples/claude-cli/2.1.187/_all_autonomous_mode.jsonl (autonomous
    /// plan-exit emitted exactly this system/status). MUTATION-PROVEN by the autonomous
    /// scenario: without sniff_mode the autonomous mode change is silently dropped.
    #[tokio::test]
    async fn sniff_mode_emits_config_changed_from_system_status() {
        // `normal` is claude's internal name for our `default` — covers the mapping too.
        let frame = r#"{"type":"system","subtype":"status","permissionMode":"normal","session_id":"s"}"#;
        let bytes = format!("{frame}\n").into_bytes();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(FakeAgentIo::never_exits(bytes))).await;
        let mut events = backend.events();

        let mut confirmed: Option<Option<String>> = None;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::ConfigChanged { mode, .. } = env.event {
                    confirmed = Some(mode);
                    return;
                }
            }
        })
        .await;
        assert_eq!(
            confirmed,
            Some(Some("default".to_string())),
            "system/status{{permissionMode:normal}} → ConfigChanged{{mode:default}} (normal→default)"
        );
        assert_eq!(
            backend.capabilities().current_mode.as_deref(),
            Some("default"),
            "the inbound applied mode becomes the authoritative current_mode"
        );
    }

    /// sniff_mode autonomous-exit + dedup: a system/status carrying a NEW mode emits
    /// ConfigChanged; a repeated status echoing the SAME mode does NOT (reducer-ignored,
    /// but keep the stream clean). Pins the "autonomous plan→bypass exit" path that was
    /// dropped before sniff_mode (the bug this fix closes).
    #[tokio::test]
    async fn sniff_mode_emits_on_autonomous_change_and_dedups_repeats() {
        // status[0] plan → status[1] bypassPermissions (autonomous exit) → status[2]
        // bypassPermissions again (echo; must NOT re-emit).
        let frames = concat!(
            r#"{"type":"system","subtype":"status","permissionMode":"plan","session_id":"s"}"#,
            "\n",
            r#"{"type":"system","subtype":"status","permissionMode":"bypassPermissions","session_id":"s"}"#,
            "\n",
            r#"{"type":"system","subtype":"status","permissionMode":"bypassPermissions","session_id":"s"}"#,
            "\n",
        );
        let backend =
            ClaudeSessionBackend::build_with_io("s", Box::new(FakeAgentIo::never_exits(frames.as_bytes().to_vec())))
                .await;
        let mut events = backend.events();

        let mut modes: Vec<String> = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(600), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::ConfigChanged { mode: Some(m), .. } = env.event {
                    modes.push(m);
                }
            }
        })
        .await;
        assert_eq!(
            modes,
            vec!["plan".to_string(), "bypassPermissions".to_string()],
            "two distinct modes emit (incl the autonomous plan→bypass exit); the repeat is deduped"
        );
    }

    #[tokio::test]
    async fn sniff_set_mode_response_error_clears_override_and_diagnoses() {
        // A rejected switch (e.g. bypass without the unlock flag, or as root) replies
        // error. The optimistic switch did NOT take → the reader CLEARS the override
        // (so the picker shows the actually-enforced mode, not the refused one) and
        // surfaces an AdapterSpecific{mode_switch_rejected} diagnostic.
        let frame = r#"{"type":"control_response","response":{"subtype":"error","request_id":"ctl-1","error":"Cannot set permission mode to bypassPermissions because the session was not launched with --dangerously-skip-permissions"}}"#;
        let bytes = format!("{frame}\n").into_bytes();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(FakeAgentIo::never_exits(bytes))).await;
        let mut events = backend.events();

        let mut diag: Option<String> = None;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::AdapterSpecific { tag, payload } = env.event
                    && tag == "mode_switch_rejected"
                {
                    diag = payload.get("error").and_then(|e| e.as_str()).map(str::to_string);
                    return;
                }
            }
        })
        .await;
        assert!(
            diag.is_some_and(|e| e.contains("permission mode")),
            "a permission-mode rejection surfaces an AdapterSpecific{{mode_switch_rejected}}"
        );
        assert_eq!(
            backend.capabilities().current_mode,
            None,
            "a rejected switch clears the optimistic override (no lying picker)"
        );
    }

    /// #99: a REJECTED `set_config_option(effort)` (claude returns a
    /// `control_response{subtype:"error"}` for a bad effort value) must surface a
    /// `Notice{Warning}` carrying the label + error, not be silently dropped. Routed
    /// strictly by the ctl-id registered in pending_set_config — a permission-mode
    /// error (or any other ctl-id) produces NO spurious effort Notice.
    #[tokio::test]
    async fn sniff_set_config_reject_surfaces_notice_not_silent() {
        // A gated tail: the error control_response for our effort set's ctl-id (ctl-9),
        // PLUS a permission-mode error for a DIFFERENT id (ctl-1) — the latter must not
        // produce an effort Notice (it has no pending_set_config entry).
        let tail = concat!(
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"ctl-1","error":"Cannot set permission mode to bypassPermissions"}}"#,
            "\n",
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"ctl-9","error":"unknown effort level: ultra"}}"#,
            "\n",
        )
        .as_bytes()
        .to_vec();
        let fake = FakeAgentIo::never_exits(Vec::new()).with_gated_tail(tail);
        let release = fake.stdout_releaser();
        let backend = ClaudeSessionBackend::build_with_io("s-effort-err", Box::new(fake)).await;
        // Register the in-flight effort set keyed on the id we minted (live path:
        // dispatch(SetConfigOption{effort}) does this).
        backend.set_pending_set_config_for_test("ctl-9", "effort\u{2192}ultra");

        let mut events = backend.events();
        release();

        let notice = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::Notice { level, message } = env.event {
                    return Some((level, message));
                }
            }
            None
        })
        .await
        .expect("must not hang")
        .expect("a rejected effort set must surface a Notice (not be silently dropped)");
        assert_eq!(notice.0, crate::event::NoticeLevel::Warning);
        assert!(
            notice.1.contains("effort\u{2192}ultra") && notice.1.contains("unknown effort level: ultra"),
            "the Notice carries the label + claude's error message, got: {}",
            notice.1
        );
        // The matching pending entry was claimed; the permission-mode error (ctl-1)
        // never had one, so it produced no effort Notice and left no leak.
        assert!(
            backend
                .pending_set_config
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "the pending_set_config entry is claimed (no leak)"
        );
    }

    /// set_model is OPTIMISTIC (design §9.10.1). LIVE-PROBED (2.1.187,
    /// protocols/samples/claude-cli/2.1.187/_all_set_model.jsonl): claude's set_model
    /// control_response is a BARE {subtype:"success"} with NO model echo (and a bogus
    /// id also returns success), so there is no wire signal to reconcile against — the
    /// reader must NOT emit a ConfigChanged from a set_model reply (that would require
    /// parsing a shape the wire never sends = inert + self-confirming). The ONLY
    /// ConfigChanged{model} comes from the dispatch(SetModel) optimistic emit; the real
    /// applied model is read back from the next turn's system/init. This pins that the
    /// reader stays silent on a bare set_model ack (the prior inferred-shape reconcile
    /// + its two self-confirming tests were removed).
    #[tokio::test]
    async fn bare_set_model_success_ack_produces_no_reader_side_config_changed() {
        // The real wire: a bare success ack with no nested response body.
        let frame = r#"{"type":"control_response","response":{"subtype":"success","request_id":"ctl-1"}}"#;
        let bytes = format!("{frame}\n").into_bytes();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(FakeAgentIo::never_exits(bytes))).await;
        let mut events = backend.events();

        let mut saw_config_changed = false;
        let _ = tokio::time::timeout(std::time::Duration::from_millis(400), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::ConfigChanged { .. }) {
                    saw_config_changed = true;
                    return;
                }
            }
        })
        .await;
        assert!(
            !saw_config_changed,
            "a bare set_model success ack must NOT trigger a reader-side ConfigChanged \
             (set_model is Optimistic — only dispatch emits it; the reader has no wire to reconcile)"
        );
    }

    #[tokio::test]
    async fn sniff_session_info_get_context_usage_maps_to_session_info() {
        // G: claude's get_context_usage reply (keyed by ctl-qsi-usage-N) →
        // SessionInfo{context_usage:{used,max,categories}}. Shape pinned from
        // samples/claude-cli/2.1.186/get_context_usage_response.json.
        let frame = format!(
            r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{QSI_USAGE_PREFIX}3","response":{{"totalTokens":3025,"maxTokens":200000,"categories":[{{"name":"System prompt","tokens":1460}},{{"name":"Skills","tokens":1529}}]}}}}}}"#
        );
        let bytes = format!("{frame}\n").into_bytes();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(FakeAgentIo::never_exits(bytes))).await;
        let mut events = backend.events();

        let mut got: Option<crate::event::ContextUsage> = None;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::SessionInfo {
                    context_usage: Some(u), ..
                } = env.event
                {
                    got = Some(u);
                    return;
                }
            }
        })
        .await;
        let u = got.expect("get_context_usage → SessionInfo{context_usage}");
        assert_eq!(u.used, 3025);
        assert_eq!(u.max, 200000);
        assert_eq!(u.categories.len(), 2);
        assert_eq!(u.categories[0].name, "System prompt");
        assert_eq!(u.categories[0].tokens, 1460);
    }

    #[tokio::test]
    async fn sniff_session_info_get_session_cost_maps_to_session_info() {
        // G: claude's get_session_cost reply (keyed by ctl-qsi-cost-N) →
        // SessionInfo{cost_text} (a preformatted report; we do not parse it).
        let frame = format!(
            r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{QSI_COST_PREFIX}5","response":{{"text":"Total cost: $0.1180"}}}}}}"#
        );
        let bytes = format!("{frame}\n").into_bytes();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(FakeAgentIo::never_exits(bytes))).await;
        let mut events = backend.events();

        let mut got: Option<String> = None;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::SessionInfo { cost_text: Some(t), .. } = env.event {
                    got = Some(t);
                    return;
                }
            }
        })
        .await;
        assert_eq!(got.as_deref(), Some("Total cost: $0.1180"));
    }

    #[tokio::test]
    async fn sniff_task_emits_rich_subagent_detail_from_workflow_progress() {
        // 009 R6b / H1: a task_progress frame's workflow_progress[] yields a rich
        // SubagentDetail per workflow_agent — keyed by agentId (the per-agent id,
        // distinct from the container task_id), parent_ref = task_id, carrying
        // model/tokens/toolCalls/loop-state/lastToolName for the per-agent panel.
        // (Real shape from workflow_multiagent_3parallel_1fail.ndjson 'done' frame.)
        let frame = r#"{"type":"system","subtype":"task_progress","task_id":"wanv3yy20","tool_use_id":"toolu-1","workflow_progress":[{"type":"workflow_phase","index":1,"title":"Run"},{"type":"workflow_agent","index":1,"label":"run:C","agentId":"agent-C","state":"done","model":"opus","tokens":10107,"toolCalls":4,"lastToolName":"StructuredOutput"}]}"#;
        let bytes = format!("{frame}\n").into_bytes();
        let backend = ClaudeSessionBackend::build_with_io("s", Box::new(FakeAgentIo::never_exits(bytes))).await;
        let mut events = backend.events();

        let mut detail = None;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::SubagentDetail { .. } = &env.event {
                    detail = Some(env.event);
                    return;
                }
            }
        })
        .await;

        let SessionEvent::SubagentDetail {
            r#ref,
            parent_ref,
            label,
            loop_state,
            model,
            tokens,
            tool_calls,
            last_tool_name,
        } = detail.expect("a workflow_agent must yield a SubagentDetail")
        else {
            unreachable!()
        };
        assert_eq!(
            r#ref, "agent-C",
            "ref = agentId (per-agent id, NOT the container task_id)"
        );
        assert_eq!(
            parent_ref.as_deref(),
            Some("wanv3yy20"),
            "parent_ref = container task_id (1:N)"
        );
        assert_eq!(label.as_deref(), Some("run:C"));
        assert_eq!(loop_state, Some(crate::state::WorkflowLoopState::Done));
        assert_eq!(model.as_deref(), Some("opus"));
        assert_eq!(tokens, Some(10107));
        assert_eq!(tool_calls, Some(4));
        assert_eq!(last_tool_name.as_deref(), Some("StructuredOutput"));
    }

    /// H1 anti-collapse (audit): replay the REAL multi-agent workflow fixture
    /// (`workflow_multiagent_3parallel_1fail.ndjson`, 6 parallel Task subagents,
    /// one of which fails) and assert the N distinct task_ids surface as N DISTINCT
    /// roster refs with INDEPENDENT terminal statuses — they must NOT collapse to a
    /// single entry, and the one failure must not be smeared onto the others.
    /// The failure signal is on the top-level SubagentUpdate stream (task_id
    /// `bgw0rnxcj` → status:failed → Errored), NOT on the workflow_progress
    /// SubagentDetail stream (those carry no failure). Pins keyed-by-ref upsert
    /// (reducer + orchestrator both key on r#ref).
    #[tokio::test]
    async fn multiagent_fixture_emits_distinct_subagents_one_errored() {
        use crate::event::SubagentStatus;
        use std::collections::HashMap;

        let bytes =
            include_str!("../../tests/fixtures/claude_2.1.176_workflow_multiagent_3parallel_1fail.ndjson").as_bytes();
        let backend =
            ClaudeSessionBackend::build_with_io("s", Box::new(FakeAgentIo::never_exits(bytes.to_vec()))).await;
        let mut events = backend.events();

        // Collect the LAST status seen per task ref (last-write-wins, mirroring the
        // reducer's upsert). Drain until the stream goes quiet.
        let mut last_status: HashMap<String, SubagentStatus> = HashMap::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Ok(Some(env)) = tokio::time::timeout(std::time::Duration::from_millis(300), events.next()).await {
                if let SessionEvent::SubagentUpdate { r#ref, status, .. } = env.event {
                    last_status.insert(r#ref, status);
                }
            }
        })
        .await;

        // N distinct refs did NOT collapse (the fixture has 6 parallel tasks).
        assert!(
            last_status.len() >= 3,
            "≥3 distinct subagent refs must survive (no collapse to one row), got {} refs: {:?}",
            last_status.len(),
            last_status.keys().collect::<Vec<_>>()
        );
        // Exactly one is Errored, and it is the specific failed task — the failure
        // is NOT smeared onto the others.
        let errored: Vec<&String> = last_status
            .iter()
            .filter(|(_, s)| matches!(s, SubagentStatus::Errored))
            .map(|(r, _)| r)
            .collect();
        assert_eq!(
            errored.len(),
            1,
            "exactly one subagent failed (independent statuses), got errored={errored:?}"
        );
        assert_eq!(errored[0], "bgw0rnxcj", "the failed ref is the fixture's failed task");
        // At least two others reached Completed independently (not dragged to Errored).
        let completed = last_status
            .values()
            .filter(|s| matches!(s, SubagentStatus::Completed))
            .count();
        assert!(
            completed >= 2,
            "≥2 sibling subagents complete independently of the one failure, got {completed} completed"
        );
    }

    #[tokio::test]
    async fn sniff_task_maps_terminal_statuses() {
        use crate::event::SubagentStatus;
        for (wire, expected) in [
            ("completed", SubagentStatus::Completed),
            ("failed", SubagentStatus::Errored),
            ("stopped", SubagentStatus::Interrupted),
        ] {
            let frame = format!(r#"{{"type":"system","subtype":"task_notification","task_id":"t","status":"{wire}"}}"#);
            let backend = ClaudeSessionBackend::build_with_io(
                "s",
                Box::new(FakeAgentIo::never_exits(format!("{frame}\n").into_bytes())),
            )
            .await;
            let mut events = backend.events();
            let got = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while let Some(env) = events.next().await {
                    if let SessionEvent::SubagentUpdate { status, .. } = env.event {
                        return Some(status);
                    }
                }
                None
            })
            .await
            .ok()
            .flatten();
            assert_eq!(got, Some(expected), "task_notification status={wire} → {expected:?}");
        }
    }
}
