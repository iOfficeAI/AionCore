//! 007 §C5 (codex variant): `CodexConnection` / `CodexSessionBackend` over
//! `codex app-server --stdio` JSON-RPC. This is the REAL point of feature 007 —
//! the first non-claude backend, proving the seam is genuinely transport-
//! agnostic (a fundamentally different wire: bidirectional JSON-RPC with
//! server-initiated reverse-RPC, vs claude's one-way `--print` pipe).
//!
//! Two freeze-blockers (§C5 A1/A2/A3) are handled BY CONSTRUCTION here:
//!  - A1 (ThreadItem closed-enum panic): we NEVER deserialize into codex's
//!    closed 16-variant `ThreadItem`. We parse `item` as `serde_json::Value`
//!    and match on the `type` string with a fallthrough → `AdapterSpecific`.
//!    An unknown future `type` is data, not a panic.
//!  - A2/A3 (reverse-RPC deadlock): the reader loop WRITES A REAL JSON-RPC
//!    RESPONSE back to stdin for blocking server requests so the channel never
//!    deadlocks / hangs the turn. Infra requests (`account/chatgptAuthTokens/
//!    refresh`, `attestation/generate`) get an immediate -32601 error (we hold no
//!    ChatGPT tokens — this deployment runs codex on Bedrock — so the honest reply
//!    is "unsupported", which unblocks); tool/file approvals are NOT auto-answered
//!    — they surface as `Permission` and the conversation's `AnswerPermission`
//!    writes the keyed accept/decline response.
//!
//! The reader-task parse helpers (`reader_task`, `map_*`, `handle_reverse_rpc`,
//! `emit`) are now production-reachable via `open_session`'s live spawn (R4) and
//! independently contract-tested via the `build_with_io` seam.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aionui_process::Spawner;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use super::suspend::{ProcHandle, SuspendController, spawn_idle_timer};
use super::types::{
    Admission, BackendError, CancelTarget, Command, CommandReceipt, ContentBlock, PendingPermissionView,
    SessionEnvelope, SessionSpec,
};
use super::{BackendConnection, SessionBackend, SessionConfig};
use crate::adapter::AgentIo;
use crate::capability::{BlockSet, Capabilities, CapabilityTier, CommandSet, PromptAcceptedSource, SignalSet};
use crate::event::{CancelReason, ProvisioningPhase, SessionEvent, StopReason, SubagentStatus, TurnOutcome};
use futures_util::stream::{BoxStream, StreamExt};

/// Connection-level factory for codex. Holds the injected `Spawner`. Unlike
/// claude (1:1), codex's app-server CAN multiplex threads on one process — but
/// P1 opens one process per logical session (multiplexing is a later refinement;
/// the seam already supports it via the threadId→session_id demux).
pub struct CodexConnection {
    /// Injected spawner (S14 — never raw-spawn) used by `open_session` to launch
    /// `codex app-server`.
    spawner: Arc<dyn Spawner>,
}

impl CodexConnection {
    pub fn new(spawner: Arc<dyn Spawner>) -> Self {
        Self { spawner }
    }
}

#[async_trait::async_trait]
impl BackendConnection for CodexConnection {
    async fn open_session(
        &self,
        spec: SessionSpec,
        config: SessionConfig,
    ) -> Result<Arc<dyn SessionBackend>, BackendError> {
        // Live spawn: `codex app-server --stdio` via the INJECTED Spawner (S14,
        // never raw-spawn). The PARSE/reverse-RPC/dispatch contract (the real 007
        // risk) is fully hermetic-tested via build_with_io; this path adds the
        // process + JSON-RPC handshake (mirrors ClaudeConnection::open_session).
        let logical_id = match &spec {
            SessionSpec::Fresh { session_id } => session_id.clone(),
            SessionSpec::Resume { session_id, .. } => session_id.clone(),
            SessionSpec::Fork { session_id, .. } => session_id.clone(),
        };
        let mut args = vec!["app-server".to_string()];
        args.extend(config.extra_args.iter().cloned());
        let cmd = aionui_common::CommandSpec {
            // Orchestration-resolved bundled CLI (packaged app) or bare "codex"
            // (dev → PATH). See SessionConfig.cli_program.
            command: config.cli_program.clone().unwrap_or_else(|| "codex".into()),
            args,
            // #103 (parity with claude_conn): forward the orchestration-filled
            // spawn env (per-agent overrides + AIONUI_* conversation runtime
            // context). Empty = inherit the parent env only, as before.
            env: config.spawn_env.clone(),
            cwd: config.cwd.clone(),
        };
        let proc = self
            .spawner
            .spawn(cmd, &[], "aionui-session")
            .await
            .map_err(|e| BackendError::from_spawn("codex spawn failed", e))?;
        let io: Box<dyn AgentIo> = Box::new(crate::adapter::ManagedProcessIo::new(proc));
        // F-4 wake recipe: a Dormant→dispatch wake re-spawns `codex app-server` and
        // replays the resume handshake against the bound threadId. Capture the
        // spawner + config so it is logically continuous (§4.1). idle_ttl=None
        // (default) → never suspends → identical to pre-F-4.
        let wake = CodexWakeRecipe {
            spawner: Some(self.spawner.clone()),
            config: config.clone(),
        };
        let mut backend = CodexSessionBackend::spawn_with_wake(logical_id, io, wake, config.idle_ttl_ms).await;
        // Report a codex whose version differs from the release AionUi verified.
        // codex runs from the user's own install (nothing is bundled), the same
        // situation agy has always been in. Placed here rather than at the two
        // return sites so the reconcile path cannot skip it.
        backend.spawn_version_check();
        // Seed the current model (M1): the backend tracks it from the start so the
        // model selector's current value and a subsequent SetModel are consistent.
        // OPTIMISTIC seed: the model is not actually bound at thread/start
        // (codex-model-gating). `reconcile_codex_model` clears this back to None if the
        // requested model turns out NOT to be in the discovered catalog, so a stale
        // picker default never poisons every turn. (Feature 012 removed the old
        // collaborationMode dependency: SetMode now uses the permissions channel and
        // needs no current_model — this seed is purely for the model axis.)
        if let Some(model) = &config.model {
            *backend.current_model.lock().await = Some(model.clone());
        }
        // GAP-D: also seed the immutable capabilities SNAPSHOT's current_model /
        // current_mode from config (parity with claude_conn, which does this at
        // spawn). Without this `capabilities().current_model` stays None even with a
        // known config.model. (The live `current_model` Mutex above is for building
        // collaborationMode; the snapshot is what the conversation reads to render
        // the model/mode selector. SetModel/SetMode updates flow via ConfigChanged,
        // not by mutating this open-time snapshot — §5.5.)
        backend.capabilities.current_model = config.model.clone();
        // Present the current mode in the SAME value space as the catalog (frontend-facing
        // legacy bare token). `config.mode` can arrive in ANY accepted vocabulary — the
        // #608 canonical id `agent-full-access` (what `normalize_requested_mode` yields
        // from a persisted full-access mode on Resume), a legacy alias, or an older
        // persisted colon id (`:danger-full-access`) — so it must go through the full
        // inbound→outbound round trip (`mode_to_catalog_value`), not the outbound leg
        // alone: `profile_id_to_legacy_value` translates only colon ids and would pass
        // `agent-full-access` through verbatim, a value the catalog never contains
        // ([read-only, auto, full-access]) → the picker highlights nothing and the
        // compact pill renders an empty label after "权限 ·".
        //
        // P9 (fresh-session parity): a fresh thread carries no requested tier
        // (`config.mode` None) and `thread/start` launches on codex's workspace-write
        // default, so seed the current mode to that tier's value (`:workspace` → `auto`) —
        // exactly what the legacy `@zed-industries/codex-acp` path advertised as
        // `currentModeId` on a fresh `session/new` (live-verified: `"auto"`, never empty),
        // so the picker shows a highlighted default instead of a blank. This is a faithful
        // replication of the thread's real launch tier, not a masking default.
        //
        // The fresh-default is gated to `SessionSpec::Fresh`: on Resume codex restores the
        // thread's own tier and surfaces it via `thread/settings/updated`, so falsely
        // seeding `auto` for a resumed thread that was actually on another tier would
        // mis-highlight until the notification lands (and Resume's fresh currentModeId is
        // not live-verified). Resume therefore keeps only the normalized persisted value.
        let normalized_config_mode = config.mode.as_deref().map(codex_perm::mode_to_catalog_value);
        backend.capabilities.current_mode = match &spec {
            SessionSpec::Fresh { .. } => {
                normalized_config_mode.or_else(|| Some(codex_perm::profile_id_to_legacy_value(":workspace")))
            }
            // Fork inherits the forked thread's own tier exactly like Resume
            // (codex restores it and surfaces `thread/settings/updated`), so no
            // fresh-default seed either.
            SessionSpec::Resume { .. } | SessionSpec::Fork { .. } => normalized_config_mode,
        };

        // JSON-RPC handshake over the retained stdin (the reader task is already
        // draining stdout). REAL codex 0.137.0 wire (verified against the
        // aion-probe transcripts in protocols/verification fixtures):
        //   initialize{clientInfo} → thread/start{approvalPolicy,sandbox,cwd}
        //   (Fresh; NO model — applied later via a validated SetModel, see
        //   reconcile_codex_model) | thread/resume{threadId}. The threadId comes back BOTH in the
        // thread/* RESULT and the `thread/started` NOTIFICATION; the reader binds it
        // from the latter (two-id, §4.1). For Resume we already hold it, so
        // run_handshake pre-seeds the binding so the first `turn/start` has a
        // threadId without waiting on the wire. Same wire frames as a wake re-attach
        // (run_handshake is shared with wake_handle).
        let handshake_mode = match &spec {
            SessionSpec::Fresh { .. } => HandshakeMode::Fresh,
            // lost backend session → start fresh under the same logical id (§4.1)
            SessionSpec::Resume { backend_session_id, .. } => match backend_session_id.as_deref() {
                Some(tid) => HandshakeMode::Resume(tid),
                None => HandshakeMode::Fresh,
            },
            SessionSpec::Fork {
                parent_backend_session_id,
                at_turn_id,
                ..
            } => HandshakeMode::Fork {
                parent_thread_id: parent_backend_session_id,
                last_turn_id: at_turn_id.as_deref(),
            },
        };
        backend.run_handshake(handshake_mode).await?;

        // codex-model-gating: `thread/start` intentionally did NOT bind `config.model`
        // (see `thread_start_params`), nor a permission tier (`thread/start` carries no
        // `permissions` field, U1) — the thread launched on codex's own default model +
        // default permission profile. Now apply the requested model+mode the ACP way:
        // wait for `model/list` / `permissionProfile/list` to fill the catalogs, then
        // dispatch VALIDATED `SetModel`/`SetMode` (each dropped if not in its catalog, so
        // a stale picker default never poisons every turn). Detached + off the open hot
        // path; only a Fresh session with a requested model and/or mode does any work
        // (Resume keeps codex's rollout-restored model + permission tier; no requested
        // config → nothing to reconcile). The two are SEQUENCED (model first) only to keep
        // the two writes deterministic — SetMode no longer depends on current_model
        // (feature 012置换: permissions channel), but sequencing keeps the wire order stable.
        if matches!(spec, SessionSpec::Fresh { .. }) && (config.model.is_some() || config.mode.is_some()) {
            let backend = Arc::new(backend);
            spawn_codex_reconcile(backend.clone(), config.model.clone(), config.mode.clone());
            return Ok(backend);
        }

        Ok(Arc::new(backend))
    }

    async fn close_session(&self, _session_id: &str) -> Result<(), BackendError> {
        // No connection-level per-session state to release: `open_session` returns
        // a self-owned `CodexSessionBackend` (it is not registered in any map on
        // `self`), and the conversation layer holds that `Arc<dyn SessionBackend>`.
        // Graceful close therefore happens when the conversation drops its handle —
        // `CodexSessionBackend::drop` aborts the reader, which releases the
        // `AgentIo` clone so the persistent `codex app-server` is reaped
        // (kill_on_drop). There is no codex `thread/close` RPC to send. Idempotent.
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        codex_capabilities()
    }
}

/// How long a post-handshake reconcile waits for a `*/list` response to fill its
/// catalog before giving up. 100 × 50ms = 5s — the same bound `spawn_catalog_writeback`
/// uses to wait for models (codex answers modes before models). If the catalog never
/// arrives we do NOT apply the requested value (we cannot validate it), leaving the
/// thread on codex's launch default rather than risk binding a bad model/mode.
const CODEX_RECONCILE_POLLS: u32 = 100;

/// codex-model/mode-gating self-heal (the ACP `clear_invalid_desired_*` +
/// `reconcile_session` port). `thread/start` launched the thread on codex's OWN default
/// model+mode (it deliberately did not embed `config.model`, and codex has no
/// `thread/start` mode param at all). This detached task applies the requested model
/// then mode, each validated against its discovered catalog. The two are SEQUENCED —
/// model MUST settle first because `SetMode` builds a `collaborationMode` around the
/// tracked `current_model`; running them concurrently could fire `SetMode` while
/// `current_model` is still the (possibly-invalid) optimistic seed or already cleared.
fn spawn_codex_reconcile(backend: Arc<CodexSessionBackend>, model: Option<String>, mode: Option<String>) {
    tokio::spawn(async move {
        if let Some(model) = model {
            reconcile_codex_model(&backend, model).await;
        }
        if let Some(mode) = mode {
            reconcile_codex_mode(&backend, mode).await;
        }
    });
}

/// Wait for a codex `*/list` catalog to populate `discovered`, returning the id list.
/// Empty vec = never populated within the poll bound (cannot validate).
async fn await_codex_catalog(
    backend: &CodexSessionBackend,
    extract: impl Fn(&Discovered) -> Vec<String>,
) -> Vec<String> {
    for _ in 0..CODEX_RECONCILE_POLLS {
        {
            let disc = backend.discovered.lock().unwrap_or_else(|e| e.into_inner());
            let ids = extract(&disc);
            if !ids.is_empty() {
                return ids;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Vec::new()
}

/// Apply `requested` model the ACP way: wait for `model/list` to fill the catalog, then
///   - if `requested` IS in the catalog → dispatch a `SetModel` (validated apply;
///     success converges via `thread/settings/updated`);
///   - if `requested` is NOT in the catalog → DROP it (WARN) and clear the optimistic
///     open-time `current_model` seed, so a stale frontend default (e.g. `gpt-5.5` the
///     local codex lacks) never binds and poisons every turn with an opaque
///     UNKNOWN_UPSTREAM_ERROR. Exact ACP contract (`session/new` carried no model; the
///     model was applied only after `clear_invalid_desired_model` validated it against
///     the session/new catalog), adapted to codex's `model/list`-after-`thread/start`
///     ordering.
async fn reconcile_codex_model(backend: &CodexSessionBackend, requested: String) {
    let catalog = await_codex_catalog(backend, |d| d.models.iter().map(|m| m.id.clone()).collect()).await;

    if catalog.is_empty() {
        // Never learned the catalog → cannot validate. Leave codex on its launch
        // default (the safe choice) rather than bind a possibly-invalid model.
        tracing::warn!(
            requested_model = %requested,
            "codex model reconcile: model/list never populated; leaving thread on codex default \
             (requested model NOT applied — cannot validate)"
        );
        return;
    }

    if !catalog.contains(&requested) {
        // DROP the invalid desire (ACP `clear_invalid_desired_model`). Clear the
        // optimistic open-time seed so SetMode can't later build a collaborationMode
        // around a model codex rejected, and the UI intent doesn't outlive reality.
        tracing::warn!(
            requested_model = %requested,
            catalog = ?catalog,
            "codex model reconcile: requested model not in catalog; dropping it \
             (thread stays on codex default)"
        );
        *backend.current_model.lock().await = None;
        return;
    }

    // Valid → apply via the normal SetModel wire (validated apply). Success converges to
    // the UI via the `thread/settings/updated` → ConfigChanged notif; a rejection
    // surfaces as a Notice{Warning} (pending_set path).
    tracing::info!(
        model = %requested,
        "codex model reconcile: applying requested model (validated against catalog)"
    );
    if let Err(e) = backend.dispatch(Command::SetModel { model: requested }).await {
        tracing::error!(error = %e, "codex model reconcile: SetModel dispatch failed");
    }
}

/// Apply `requested` mode the ACP way, symmetric to [`reconcile_codex_model`]. For codex
/// the mode axis IS the permission axis: a thread always launches on codex's default
/// permission profile (`thread/start` carries NO `permissions` field, U1), so a persisted
/// non-default tier was never applied at open before this. This closes that gap AND
/// validates against the DISCOVERED catalog, exactly as legacy ACP `set_mode` gated on
/// `is_mode_valid` (advertised `availableModes`): wait for `permissionProfile/list` to
/// fill the catalog (colon-prefixed profile ids), normalize a legacy persisted value onto
/// its colon id, then apply a `SetMode` if the value IS in the catalog, or DROP it (WARN,
/// leaving codex's default) if it is NOT. Empty/never-populated catalog → do NOT apply
/// (cannot validate). Unlike the old collaborationMode path this needs no settled
/// `current_model`.
async fn reconcile_codex_mode(backend: &CodexSessionBackend, requested: String) {
    // The discovered catalog's `ModeInfo.id` is now the FRONTEND-facing legacy bare token
    // (`auto` for the workspace tier); the wire/validation axis speaks colon ids. Normalize
    // each catalog id back to its colon profile id so both sides of the `contains` check
    // below are colon-shaped (a custom profile is already colon → normalize is a no-op).
    let catalog: Vec<String> = await_codex_catalog(backend, |d| {
        d.modes
            .iter()
            .map(|m| codex_perm::normalize_to_profile_id(&m.id))
            .collect()
    })
    .await;

    if catalog.is_empty() {
        tracing::warn!(
            requested_mode = %requested,
            "codex mode reconcile: permissionProfile/list never populated; leaving thread on codex default \
             (requested mode NOT applied — cannot validate)"
        );
        return;
    }

    // Normalize the persisted/legacy value onto its colon-prefixed profile id BEFORE
    // validating: a stored legacy `yolo`/`full-access` must resolve to `:danger-full-access`
    // (a discovered id) rather than miss the catalog; a already-colon discovered id passes
    // through. Then validate against the LIVE catalog — a colon id no longer advertised
    // (a removed custom profile) is dropped, mirroring legacy `is_mode_valid`.
    let normalized = codex_perm::normalize_to_profile_id(&requested);
    if !catalog.contains(&normalized) {
        tracing::warn!(
            requested_mode = %requested,
            normalized_mode = %normalized,
            catalog = ?catalog,
            "codex mode reconcile: requested mode not in catalog; dropping it \
             (thread stays on codex default)"
        );
        return;
    }

    tracing::info!(
        mode = %normalized,
        "codex mode reconcile: applying requested mode (validated against catalog)"
    );
    if let Err(e) = backend.dispatch(Command::SetMode { mode: normalized }).await {
        // The permissions channel needs no current_model, so a dispatch error here is a
        // genuine transport/reject — surface it (still WARN: the thread stays on codex's
        // default, which is safe, so this is not a turn-blocking contract violation).
        tracing::warn!(error = %e, "codex mode reconcile: SetMode dispatch failed; mode not applied");
    }
}

/// A pure params object that knows how to wrap itself in a JSON-RPC request frame.
/// Extracted so the handshake wire shapes are unit-testable WITHOUT a live process
/// (open_session's spawn path needs a real codex binary).
struct HandshakeParams(Value);

impl HandshakeParams {
    fn into_frame(self, id: u64, method: &str) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": self.0 })
    }
}

/// Prefix tagging a `Permission.request_id` that came from an MCP
/// `mcpServer/elicitation/request` (vs a command/file `*/requestApproval`). The
/// two reverse-RPCs need DIFFERENT response bodies — elicitation wants
/// `{action, content}`, approval wants `{decision}` — so `dispatch(AnswerPermission)`
/// branches on this prefix to pick the wire shape. The reducer ref-counts on
/// `kind` only and never inspects the request_id, so this is a transparent
/// dispatch-side discriminator (no new shared state). The conversation layer
/// echoes the request_id back verbatim, so the prefix survives the round-trip.
const ELICIT_PREFIX: &str = "elicit:";

/// `initialize` params (REAL codex 0.137.0). ⚠️ `capabilities.experimentalApi:true`
/// is REQUIRED, not optional: the experimental methods we advertise + use
/// (thread/settings/update for SetMode/SetModel, thread/turns/list for
/// ListCheckpoints) are `#[experimental]` on the codex server (common.rs:532/603)
/// and rejected with `invalid_request` unless this is set
/// (message_processor.rs:826-830; codex's own test
/// `thread_settings_update_requires_experimental_api_capability`). The field is
/// NESTED under `capabilities` (InitializeParams schema — a top-level
/// `experimentalApi` is silently ignored), serialized camelCase.
fn initialize_params() -> HandshakeParams {
    HandshakeParams(json!({
        "clientInfo": { "name": "aionui-session", "version": "0.1.0" },
        "capabilities": { "experimentalApi": true, "requestAttestation": false }
    }))
}

/// `thread/start` params (Fresh / lost-Resume). approvalPolicy/sandbox are valid
/// AskForApproval/SandboxMode enum values; cwd threaded from config.
///
/// ⚠️ MODEL IS DELIBERATELY NOT EMBEDDED HERE (codex-model-gating regression fix).
/// `thread/start` binds the model for the WHOLE thread, and `turn/start` carries NO
/// model — so a stale/invalid `config.model` (e.g. a frontend picker default like
/// `gpt-5.5` the local codex doesn't have) bound here makes EVERY turn fail with an
/// opaque UNKNOWN_UPSTREAM_ERROR (live-repro: a fresh codex conv's first reply
/// fails; an old conv resumes via thread/resume which sends no model → works). The
/// catalog (`model/list`) is UNKNOWN at this instant — it is fired AFTER thread/start
/// in `run_handshake` — so we CANNOT validate the model here. Instead we launch on
/// codex's OWN default model (always valid locally) and apply `config.model` later
/// via a VALIDATED `SetModel`, dropping it if it is not in the discovered catalog.
/// This is the faithful port of the ACP path, which likewise launched model-less
/// (`session/new` carried no model) and applied the model only after
/// `clear_invalid_desired_model` validated it against the session/new catalog.
///
/// Wave 0c: the session-init surface is injected here. codex reads MCP servers
/// from its CONFIG (NOT a per-thread param), so they go into `config.mcp_servers`
/// — a map keyed by name (verified live, 0.139.0: thread/start accepts it AND
/// launches the servers). The preset/system prompt goes into `baseInstructions`.
/// Both are omitted when empty so the pre-0c handshake is byte-identical.
fn thread_start_params(config: &SessionConfig) -> HandshakeParams {
    // G1-A: data-drive the sandbox from SessionConfig (None ⇒ workspace-write,
    // byte-identical to the pre-G1-A handshake). A yolo agent resolves to
    // "danger-full-access" at the orchestration boundary (app registry), restoring
    // the legacy codex_sandbox mapping without writing ~/.codex/config.toml.
    let sandbox = config.sandbox_mode.as_deref().unwrap_or("workspace-write");
    // approvalPolicy is data-driven (sibling of sandbox): None ⇒ "on-request"
    // (byte-identical to the pre-data-driven handshake); a yolo / full-access agent
    // resolves to "never" at the orchestration boundary (app registry).
    let approval = config.approval_policy.as_deref().unwrap_or("on-request");
    let mut params = json!({
        "approvalPolicy": approval,
        "sandbox": sandbox,
    });
    if let Some(cwd) = &config.cwd {
        params["cwd"] = json!(cwd);
    }
    // NB: `config.model` is intentionally NOT written here — see the doc comment
    // above. It is applied post-discovery by `reconcile_codex_model` (validated).
    if !config.init.mcp_servers.is_empty() {
        params["config"] = json!({ "mcp_servers": build_codex_mcp_servers(&config.init.mcp_servers) });
    }
    if let Some(preset) = &config.init.preset_context {
        params["baseInstructions"] = json!(preset);
    }
    HandshakeParams(params)
}

/// `thread/resume` params = the full `thread/start` override surface + the
/// threadId. LIVE-confirmed (0.144.1, `samples/codex-cli/0.144.1/_probe_resume_mcp.py`
/// → `_all_resume_mcp.jsonl`): a bare `thread/resume {threadId}` does NOT restore
/// the thread/start overrides from the rollout — the MCP inventory comes back
/// EMPTY (`mcpServerStatus/list`) and approvalPolicy resets to the default
/// (`on-request`; the thread was started with `never`). Re-sending the surface is
/// consumed: `config.mcp_servers` relaunches the servers (startupStatus
/// starting→ready + inventory restored) and `approvalPolicy` echoes back applied.
/// `ThreadResumeParams` (schema-full 0.137.0) accepts the same field names as
/// thread/start (`approvalPolicy`/`sandbox`/`cwd`/`config`/`baseInstructions`);
/// sandbox/baseInstructions had no direct oracle in the probe — re-sending the
/// start-time values is at worst a no-op, while dropping them is a confirmed loss
/// on the probed axes.
fn thread_resume_params(config: &SessionConfig, thread_id: &str) -> HandshakeParams {
    let HandshakeParams(mut params) = thread_start_params(config);
    params["threadId"] = json!(thread_id);
    HandshakeParams(params)
}

/// `thread/fork` params: the same override surface as resume (fork creates a NEW
/// thread, which needs its MCP servers / approvalPolicy just like a resumed one),
/// plus the PARENT threadId as the fork source and an optional `lastTurnId` —
/// copy history only through that turn (inclusive) and drop later turns
/// (app-server ThreadForkParams, exported from codex 0.145.0). Omitting
/// `lastTurnId` forks at HEAD.
fn thread_fork_params(config: &SessionConfig, parent_thread_id: &str, last_turn_id: Option<&str>) -> HandshakeParams {
    let HandshakeParams(mut params) = thread_start_params(config);
    params["threadId"] = json!(parent_thread_id);
    if let Some(turn_id) = last_turn_id {
        params["lastTurnId"] = json!(turn_id);
    }
    HandshakeParams(params)
}

/// How `run_handshake` opens the thread: shared by `open_session` (all three
/// modes) and `wake_handle` (Resume against the bound threadId, or Fresh when
/// no binding survived — a wake NEVER replays a fork: by the time a session can
/// idle-suspend its fork has completed and its own thread is bound).
enum HandshakeMode<'a> {
    Fresh,
    Resume(&'a str),
    Fork {
        parent_thread_id: &'a str,
        last_turn_id: Option<&'a str>,
    },
}

/// Serialize neutral [`McpServerSpec`]s into codex's `config.mcp_servers` MAP
/// (keyed by name), the shape codex's config loader expects (verified live +
/// against `codex mcp add` TOML output). DISTINCT from the ACP wire: codex stdio
/// `env` is a MAP `{KEY:VAL}` (not an array of `{name,value}`), and an HTTP server
/// carries `{url, bearer_token_env_var}` (codex resolves the token from the env
/// var; there is no inline-headers field). Pure `serde_json`, no codex SDK.
fn build_codex_mcp_servers(servers: &[crate::backend::McpServerSpec]) -> Value {
    use crate::backend::McpTransport;
    let mut map = serde_json::Map::new();
    for s in servers {
        let entry = match &s.transport {
            McpTransport::Stdio { command, args, env } => {
                let env_map: serde_json::Map<String, Value> = env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
                json!({ "command": command, "args": args, "env": Value::Object(env_map) })
            }
            // codex streamable-http MCP: url + (optional) bearer token env var. The
            // neutral spec carries headers; codex takes a bearer_token_env_var, so we
            // pass the url and let codex's own auth/oauth path handle credentials
            // (inline arbitrary headers are not a codex config field).
            McpTransport::Http { url, .. } | McpTransport::Sse { url, .. } => {
                json!({ "url": url })
            }
        };
        map.insert(s.name.clone(), entry);
    }
    Value::Object(map)
}

/// codex's declared capabilities (§C5.5). tier=Hook (app-server is a parsed
/// JSON-RPC but not the full claude stream); supports the rich command set +
/// answer_auth (auth_methods non-empty → mid-session re-auth path) + rewind (G3:
/// thread/rollback down + Rewound{to_turn} up).
pub fn codex_capabilities() -> Capabilities {
    Capabilities {
        tier: CapabilityTier::Hook,
        emits: SignalSet {
            heartbeat: true,
            tool_lifecycle: true,
            terminal_result: true,
        },
        supported_commands: CommandSet {
            steer: true,
            // codex has NO tool-scoped cancel on the wire (only turn/interrupt =
            // whole-turn); dispatch(Cancel{Tool}) returns CommandNotSupported. The
            // cap MUST advertise false so a Layer-1 consumer never surfaces a
            // cancel-tool affordance the backend always rejects (matches the
            // authoritative §C5.5 stub `cancel_tool: false /* P1+ */`).
            cancel_tool: false,
            answer_permission: true,
            answer_auth: true,
            acknowledge: true,
            set_mode: true,
            set_model: true,
            // G3: rewind = true. codex's wire (thread/rollback) rewinds; the seam
            // now wires the full T17 model — down: thread/rollback{numTurns}; up:
            // Rewound{to_turn} receipt (the orchestrator rehydrates to it, the
            // conversation forks from it, parent block stream append-only). dispatch
            // idle-gates it (mid-turn rollback is rejected). cap=true ↔ dispatch
            // accepts (the cap-behavior invariant holds).
            rewind: true,
            list_checkpoints: true, // thread/turns/list
            // codex has a usage notification (thread/tokenUsage/updated) but no
            // on-demand cumulative context/cost QUERY wire → false.
            query_session_info: false,
        },
        prompt_blocks: BlockSet {
            text: true,
            image: true,
            audio: false,
            // resource = true: a ResourceLink is delivered by reference as a codex
            // `UserInput::Mention { name, path }` (turn.rs:266–297) — codex's native
            // @file mention, not a base64 body. The file must be reachable from the
            // codex spawn cwd / sandbox roots (same constraint as claude's Read-tool
            // path-ref). `accepts_files()` derives from this bit.
            resource: true,
            at_mention: false,
        },
        prompt_accepted: PromptAcceptedSource::Native, // turn/started is a real wire ack
        available_models: Vec::new(),
        available_modes: Vec::new(),
        current_model: None,
        current_mode: None,
        current_effort: None,
        auth_methods: vec!["chatgptAuthTokens".into(), "refresh".into()],
        // 009 R2: codex advertises steer, but the conv layer does not route Steer
        // today (B5), so there is no proactive next-turn input path → false. (Keying
        // can_queue off steer here would be the MX-QUEUE-3 dead button.) Flips true
        // only when B5 wires Steer routing.
        accepts_proactive_input: false,
        // #101: codex's app-server has no slash-command discovery wire (112 methods
        // audited, none lists commands — samples/codex-cli/0.137.0/schema-full/
        // ClientRequest.json). The legacy codex-acp bridge instead advertised a
        // STATIC 6-command table and translated each to a native op at prompt time
        // (zed-industries/codex-acp v0.14.0 thread.rs:2894 builtin_commands /
        // :3252 handle_prompt). We replicate that table here; dispatch(Send)
        // performs the same slash→native-op translation.
        slash_commands: builtin_slash_commands(),
    }
}

/// The codex-acp bridge's static slash-command table, verbatim
/// (zed-industries/codex-acp v0.14.0 src/thread.rs:2894-2924 `builtin_commands`;
/// captured live: samples/codex-acp/0.14.0/freshmode.jsonl). codex itself has no
/// command-discovery wire, so this is the authoritative catalog for the direct-CLI
/// path — each entry is translated to its native app-server op in dispatch(Send)
/// (`route_slash_command`).
fn builtin_slash_commands() -> Vec<crate::capability::SlashCommandInfo> {
    use crate::capability::SlashCommandInfo;
    vec![
        SlashCommandInfo {
            name: "review".into(),
            description: Some("Review my current changes and find issues".into()),
        },
        SlashCommandInfo {
            name: "review-branch".into(),
            description: Some("Review the code changes against a specific branch".into()),
        },
        SlashCommandInfo {
            name: "review-commit".into(),
            description: Some("Review the code changes introduced by a commit".into()),
        },
        SlashCommandInfo {
            name: "init".into(),
            description: Some("create an AGENTS.md file with instructions for Codex".into()),
        },
        SlashCommandInfo {
            name: "compact".into(),
            description: Some("summarize conversation to prevent hitting the context limit".into()),
        },
        SlashCommandInfo {
            name: "logout".into(),
            description: Some("logout of Codex".into()),
        },
    ]
}

/// Per-session codex handle. `&self`-concurrent (stdin write behind a Mutex).
pub struct CodexSessionBackend {
    session_id: String,
    capabilities: Capabilities,
    /// JSON-RPC request id counter (outbound client requests).
    rpc_id: AtomicU64,
    /// Live turn epoch (set on dispatch(Send), read by the reader to stamp).
    turn_gen: Arc<AtomicU64>,
    /// stdin shared with the reader task: dispatch writes client requests; the
    /// reader writes auto-responses to infra reverse-RPCs (A2/A3 deadlock guard).
    /// Both go through the same async Mutex, so writes are serialized.
    stdin: Arc<Mutex<Option<aionui_process::BoxedStdin>>>,
    event_tx: broadcast::Sender<SessionEnvelope>,
    /// F-4 self-suspend controller, owning the live `{reader, io}` pair. The reader
    /// is the long-lived JSON-RPC reader: codex's app-server is PERSISTENT (stdout
    /// never EOFs), so it would block forever on `next_line()`, pinning its
    /// `Arc<dyn AgentIo>` clone alive → the child is never reaped. The controller
    /// aborts the reader on suspend AND on Drop (`abort_on_drop`, M5), releasing the
    /// clone so the subprocess is reaped. When idle_ttl=None (default) the slot
    /// stays Active for life — identical to the pre-F-4 behavior.
    suspend: Arc<SuspendController>,
    /// Per-backend idle timer (Some only when idle_ttl is set). Aborted on Drop.
    idle_timer: Option<tokio::task::JoinHandle<()>>,
    /// Everything needed to re-spawn (`thread/resume`) the codex app-server on wake.
    wake: CodexWakeRecipe,
    /// Shared reader-task inputs, cloned into the open-time reader AND every
    /// post-wake reader so they drain into the same event_tx/turn_gen/bindings.
    reader_state: CodexReaderState,
    /// F-4 turn-active flag (shared with the reader via `reader_state`): set on
    /// dispatch(Send), cleared by the reader at the terminal. The idle timer reads
    /// it so a streaming turn is never suspended mid-flight.
    turn_in_flight: Arc<std::sync::atomic::AtomicBool>,
    /// Logical session_id ← backend threadId binding (filled on thread/started,
    /// or pre-seeded on Resume). All `turn/*` + `thread/*` client requests need
    /// it. Two-id (§4.1): the backend threadId never escapes upward.
    thread_binding: Arc<Mutex<Option<String>>>,
    /// The rpc id of the in-flight `thread/resume` (Resume handshakes only). The
    /// reader claims the response: an ERROR means the pre-seeded binding points at
    /// a thread this codex cannot restore ("no rollout found for thread id …",
    /// verified: samples/codex-cli/0.144.1/dead_resume.jsonl) — the binding is
    /// cleared and `resume_poison` set. A success just drops the correlation
    /// (the follow-up `thread/started` re-confirms the binding).
    pending_resume: Arc<Mutex<Option<u64>>>,
    /// Set when codex REJECTED the `thread/resume` (dead resume anchor). Carries
    /// the codex error message; `bound_thread_within` fails FAST with it
    /// (`BackendError::SessionNotFound`) instead of polling a binding that will
    /// never arrive — the send-path then classifies it as a dead-session error
    /// and the conversation's recovery (anchor clear + auto-replay) takes over.
    /// Reset at the start of every handshake (a re-spawn is a fresh chance).
    resume_poison: Arc<Mutex<Option<String>>>,
    /// The rpc id of the in-flight `thread/fork` (Fork handshakes only,
    /// `pending_resume`'s sibling). The reader claims the response: an ERROR
    /// (parent rollout gone, parent turn in flight, …) poisons the bound-thread
    /// wait — a Fork handshake pre-seeds NO binding, so without the poison the
    /// first Send would poll forever for a `thread/started` that never comes.
    /// A success needs no action (`thread/started` for the NEW thread binds it).
    pending_fork: Arc<Mutex<Option<u64>>>,
    /// The id of the in-flight turn (codex `turn/started.turn.id`), needed by
    /// `turn/interrupt{turnId}` and `turn/steer{expectedTurnId}` (optimistic
    /// concurrency token). Set on `turn/started`, cleared on terminal.
    active_turn_id: Arc<Mutex<Option<String>>>,
    /// The wire id of a pending `account/chatgptAuthTokens/refresh` reverse-RPC
    /// (R6/R15): set by the reader when it surfaces `Permission{Auth}`, consumed
    /// by `dispatch(AnswerAuth)` which writes the keyed RESPONSE carrying the
    /// supplied tokens. UNLIKE infra reverse-RPCs this is NOT auto-answered — a
    /// human/credential source must satisfy it (mid-session re-auth, §6b b3).
    pending_auth_id: Arc<Mutex<Option<Value>>>,
    /// The current model id (M1): codex's `collaborationMode` for SetMode REQUIRES
    /// `settings.model`, so the backend must know it. Seeded from config at open,
    /// updated by `dispatch(SetModel)` + the `thread/settings/updated` notif. None
    /// until known → SetMode rejects (can't build a valid collaborationMode).
    current_model: Arc<Mutex<Option<String>>>,
    /// REST-recovery (`GET /confirmations`) source: the currently-open (unanswered)
    /// tool/file/elicitation approval requests, keyed by the SAME request_id the
    /// backend surfaced on `SessionEvent::Permission` (so the recovered card's
    /// id==call_id matches the live frame and de-dups). The value is a safe title
    /// label derived from the reverse-RPC method (NOT the command body — TIO-13).
    /// Lifecycle: the reader inserts on each `*/requestApproval` (+ elicitation)
    /// reverse-RPC, removes on `serverRequest/resolved` (codex retracted/answered it)
    /// and `dispatch(AnswerPermission)` (we answered it). `std::sync::Mutex` because
    /// the sync `pending_permission_requests()` trait method reads it without await —
    /// mirrors claude's `pending_perms`. Behind an Arc so the reader (cloned into
    /// every post-wake reader via `reader_state`) shares the one registry.
    pending_tool_approvals: Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// GAP-A: rpc-id → [`PendingSend`] correlation for in-flight `turn/start`
    /// (and review/compact/logout) requests. codex IS a bidirectional JSON-RPC
    /// client: `turn/start` gets a synchronous response
    /// `{turn:{id,status:inProgress}}` keyed by the request id — that response IS
    /// the "prompt accepted" receipt (NOT the `turn/started` notification).
    /// dispatch(Send) inserts; the reader claims the matching response: a result
    /// emits `PromptAccepted{client_msg_id}` so the conversation's pending queue
    /// drains (Addendum 3); an ERROR response terminates the turn (turn-flavored)
    /// or surfaces a Notice (NoTurn) — NEVER a silent drop (a dropped rejection
    /// left the turn hanging Running forever, ELECTRON-3Q0).
    pending_sends: Arc<Mutex<HashMap<u64, PendingSend>>>,
    /// B-CODEX-MODEL-LIST (§9.10 discovery): rpc ids of the `model/list` +
    /// `collaborationMode/list` calls `open_session` issues at handshake, mapped to
    /// which list they fill. The reader claims the matching responses and writes
    /// `discovered`. (We do NOT block the first Send on these — fill is lazy; if a
    /// Send races ahead the UI just sees the switcher populate a beat later.)
    pending_discovery: Arc<Mutex<HashMap<u64, DiscoveryKind>>>,
    /// Live-discovered models/modes (B-CODEX-MODEL-LIST). `capabilities()` merges
    /// these into the returned snapshot. Behind an Arc so the reader can fill it
    /// after `open_session` returns (the static `codex_capabilities()` cannot carry
    /// per-session discovery).
    discovered: Arc<std::sync::Mutex<Discovered>>,
    /// rpc_id → `"mode→<v>"` / `"model→<v>"` label for in-flight
    /// `thread/settings/update` SetMode/SetModel requests. The reader claims the
    /// response: a JSON-RPC ERROR (e.g. an invalid model/mode rejected by codex) is
    /// surfaced as a `Notice{Warning}` + error log instead of being silently dropped
    /// (a failed set the user would never see). A SUCCESS does NOTHING here — codex
    /// converges via the separate `thread/settings/updated` notification (handled in
    /// `map_notification` → ConfigChanged, live-verified), so emitting here too would
    /// duplicate the ConfigChanged. The codex analogue of acp_conn's `pending_set`.
    pending_set: Arc<Mutex<HashMap<u64, String>>>,
}

/// One in-flight prompt-carrying client request (GAP-A correlation entry).
/// `client_msg_id` is what a success response drains via `PromptAccepted`
/// (None when the caller supplied no correlation id — nothing to drain, but the
/// error path below still applies). `opens_turn` decides what a JSON-RPC ERROR
/// response means: a turn-flavored request (turn/start, review/start,
/// thread/compact/start) was REJECTED, so the turn that dispatch admitted must
/// be terminated with an is_error terminal (the FSM already went Running via
/// the lowered TurnStarted); a NoTurn request (/logout → account/logout) never
/// opened a turn, so the rejection surfaces as a Notice instead.
#[derive(Clone)]
struct PendingSend {
    client_msg_id: Option<String>,
    opens_turn: bool,
}

/// Which pending response a claimed rpc id maps to. Models/Modes fill the
/// per-session `discovered` cache (capabilities() merges them); Checkpoints maps
/// to a `CheckpointList` event (O2 up-leg); Rewind maps to a `Rewound{to_turn}`
/// receipt (G3 up-leg — the post-rollback history-end the orchestrator rehydrates
/// to / the conversation forks from, T17). All four are query/command responses
/// the reader claims by rpc id; none touches the FSM.
#[derive(Clone, Copy)]
enum DiscoveryKind {
    Models,
    /// codex's mode axis: filled from `permissionProfile/list` and mapped to the fixed
    /// permission-tier enum (feature 012). codex sends no `collaborationMode/list`.
    Permissions,
    Checkpoints,
    Rewind,
}

/// Per-session handshake-discovered capability lists (B-CODEX-MODEL-LIST).
#[derive(Default, Clone)]
struct Discovered {
    models: Vec<crate::capability::ModelInfo>,
    /// For codex this holds the fixed permission-tier mode enum mapped from
    /// `permissionProfile/list` (feature 012), NOT collaborationMode.
    modes: Vec<crate::capability::ModeInfo>,
}

/// What `CodexSessionBackend::wake_handle` needs to re-spawn the codex app-server
/// after an idle suspend and replay the resume handshake. `inert()` (no spawner)
/// is used for test-built backends, which never suspend, so it is never consulted.
struct CodexWakeRecipe {
    spawner: Option<Arc<dyn Spawner>>,
    config: SessionConfig,
}

impl CodexWakeRecipe {
    /// A recipe that cannot wake (no spawner). Used by `spawn`/`build_with_io`
    /// where suspension is never enabled.
    #[cfg(any(test, feature = "test-support"))]
    fn inert() -> Self {
        Self {
            spawner: None,
            config: SessionConfig::default(),
        }
    }
}

/// Shared reader inputs — held by the backend, cloned into the open-time reader
/// and every post-wake reader, so they all drain into the same broadcast/atomics
/// and bindings (the threadId binding survives a suspend, so a wake re-attaches).
#[derive(Clone)]
struct CodexReaderState {
    session_id: String,
    turn_gen: Arc<AtomicU64>,
    event_tx: broadcast::Sender<SessionEnvelope>,
    thread_binding: Arc<Mutex<Option<String>>>,
    active_turn_id: Arc<Mutex<Option<String>>>,
    pending_auth_id: Arc<Mutex<Option<Value>>>,
    pending_tool_approvals: Arc<std::sync::Mutex<HashMap<String, String>>>,
    pending_sends: Arc<Mutex<HashMap<u64, PendingSend>>>,
    pending_discovery: Arc<Mutex<HashMap<u64, DiscoveryKind>>>,
    pending_set: Arc<Mutex<HashMap<u64, String>>>,
    pending_resume: Arc<Mutex<Option<u64>>>,
    resume_poison: Arc<Mutex<Option<String>>>,
    pending_fork: Arc<Mutex<Option<u64>>>,
    discovered: Arc<std::sync::Mutex<Discovered>>,
    stdin: Arc<Mutex<Option<aionui_process::BoxedStdin>>>,
    /// F-4 turn-active flag: set on dispatch(Send), cleared by the reader at a turn
    /// terminal (TurnResult / Detached). The idle timer reads it so a streaming turn
    /// is never suspended mid-flight.
    turn_in_flight: Arc<std::sync::atomic::AtomicBool>,
}

/// Spawn a codex JSON-RPC reader over `stdout`/`io` using the shared state. Used
/// both at open (`spawn`) and on every idle-wake (`wake_handle`).
fn start_codex_reader(
    state: &CodexReaderState,
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
            state.thread_binding,
            state.active_turn_id,
            state.pending_auth_id,
            state.pending_tool_approvals,
            state.pending_sends,
            state.pending_discovery,
            state.pending_set,
            state.pending_resume,
            state.resume_poison,
            state.pending_fork,
            state.discovered,
            state.stdin,
            state.turn_in_flight,
        )
        .await;
    })
}

/// The idle-check cadence for a ttl: poll at ~ttl/4 (bounded 1s..=30s). Only
/// consulted when idle_ttl is Some (else no timer is spawned).
fn idle_check_interval_ms(idle_ttl_ms: Option<i64>) -> u64 {
    match idle_ttl_ms {
        Some(ttl) => ((ttl / 4).clamp(1_000, 30_000)) as u64,
        None => 30_000,
    }
}

impl CodexSessionBackend {
    /// Tell the user once per conversation when the installed codex is not the
    /// release AionUi verified.
    ///
    /// Fire-and-forget: the probe spawns `codex --version` and a failure only
    /// costs the drift claim, never the session.
    fn spawn_version_check(&self) {
        let Some(spawner) = self.wake.spawner.clone() else {
            tracing::debug!(session_id = %self.session_id, "codex version check skipped: no spawner on the wake recipe");
            return;
        };
        let session_id = self.session_id.clone();
        let program = self
            .wake
            .config
            .cli_program
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("codex"));
        let event_tx = self.event_tx.clone();
        let turn_gen = Arc::clone(&self.turn_gen);
        tokio::spawn(async move {
            let Some((level, message, localized)) =
                crate::backend::cli_version::session_drift_notice(&spawner, "codex", &program, &session_id).await
            else {
                return;
            };
            // Retry until subscribed: a broadcast send with no receiver is
            // discarded, and this notice has no second chance.
            crate::backend::cli_version::broadcast_notice(
                &event_tx,
                SessionEnvelope {
                    session_id: session_id.clone(),
                    turn_gen: turn_gen.load(std::sync::atomic::Ordering::SeqCst),
                    event: SessionEvent::Notice {
                        level,
                        message,
                        localized: Some(localized),
                        supersedes_key: None,
                    },
                },
                "codex",
            )
            .await;
        });
    }

    /// Test-support seam: build over an injected `AgentIo` replaying a codex
    /// JSON-RPC fixture WITHOUT spawning a real app-server — proves the
    /// parse/reverse-RPC/dispatch contract end-to-end.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn build_with_io(session_id: impl Into<String>, io: Box<dyn AgentIo>) -> Self {
        Self::spawn(session_id.into(), io).await
    }

    /// Test-support seam: build a SUSPENDABLE backend with a caller-supplied
    /// `Spawner` (to observe the wake re-spawn) + an `idle_ttl_ms`. Lets a test
    /// drive the suspend→wake path: the idle slot suspends, and the next dispatch
    /// wakes via the supplied spawner (asserting the `thread/resume` recipe).
    #[cfg(any(test, feature = "test-support"))]
    pub async fn build_with_io_suspending(
        session_id: impl Into<String>,
        io: Box<dyn AgentIo>,
        spawner: Arc<dyn Spawner>,
        idle_ttl_ms: i64,
    ) -> Self {
        let wake = CodexWakeRecipe {
            spawner: Some(spawner),
            config: SessionConfig::default(),
        };
        Self::spawn_with_wake(session_id.into(), io, wake, Some(idle_ttl_ms)).await
    }

    /// Test-support seam: pre-bind the backend threadId (the resume anchor the
    /// live path binds from `thread/started`). Lets a hermetic wake test drive the
    /// suspend→wake path with a known resume anchor.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn seed_thread_binding_for_test(&self, thread_id: impl Into<String>) {
        *self.thread_binding.lock().await = Some(thread_id.into());
    }

    /// Test-support seam: mark a turn in flight WITHOUT a bound active_turn_id —
    /// the cancel-before-fold window (dispatch(Send) ran, but the reader has not yet
    /// bound the turn id from the async turn/started). Lets a test drive the
    /// pending-interrupt path in dispatch(Cancel).
    #[cfg(any(test, feature = "test-support"))]
    pub fn mark_turn_in_flight_for_test(&self) {
        self.turn_in_flight.store(true, Ordering::SeqCst);
    }

    /// Test-support seam: bind the active turn id (simulating the reader applying a
    /// late turn/started). Paired with `mark_turn_in_flight_for_test` to exercise the
    /// pending-interrupt poll resolving mid-wait.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn bind_active_turn_for_test(&self, turn_id: impl Into<String>) {
        *self.active_turn_id.lock().await = Some(turn_id.into());
    }

    /// Test-support seam: register a pending `model/list` discovery id so a test
    /// can drive the model/list RESPONSE through the reader (open_session does this
    /// after the handshake; `build_with_io` skips the handshake). Lets a test prove
    /// the async-discovery → `capabilities()` merge without a real app-server.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn register_model_discovery_for_test(&self, rpc_id: u64) {
        self.pending_discovery
            .lock()
            .await
            .insert(rpc_id, DiscoveryKind::Models);
    }

    /// Test-support seam: register a pending `thread/settings/update`
    /// (SetMode/SetModel) rpc id + label so a hermetic fixture can replay an error
    /// response and assert the reader surfaces a `Notice` (not a silent drop). On the
    /// live path `dispatch(SetMode/SetModel)` registers it.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_pending_set_for_test(&self, rpc_id: u64, label: impl Into<String>) {
        self.pending_set.lock().await.insert(rpc_id, label.into());
    }

    /// Test-support seam: register a pending `thread/resume` rpc id so a hermetic
    /// fixture can replay its ERROR response and assert the reader clears the
    /// (seeded) binding + poisons the bound-thread wait. On the live path
    /// `run_handshake` registers it.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn register_pending_resume_for_test(&self, rpc_id: u64) {
        *self.pending_resume.lock().await = Some(rpc_id);
    }

    /// Test-only convenience: spawn an inert (never-suspending, no-spawner)
    /// backend. Production opens via `open_session` → `spawn_with_wake` with a real
    /// wake recipe; only the `build_with_io` test seam uses this.
    #[cfg(any(test, feature = "test-support"))]
    async fn spawn(session_id: String, io: Box<dyn AgentIo>) -> Self {
        Self::spawn_with_wake(session_id, io, CodexWakeRecipe::inert(), None).await
    }

    /// Spawn + (optionally) enable F-4 idle self-suspend. `wake` carries what a
    /// Dormant→dispatch wake needs (spawner + config); `idle_ttl_ms` None = never
    /// suspend (the `spawn` default), Some = run the idle timer.
    async fn spawn_with_wake(
        session_id: String,
        io: Box<dyn AgentIo>,
        wake: CodexWakeRecipe,
        idle_ttl_ms: Option<i64>,
    ) -> Self {
        let io: Arc<dyn AgentIo> = Arc::from(io);
        let turn_gen = Arc::new(AtomicU64::new(0));
        let thread_binding = Arc::new(Mutex::new(None));
        let active_turn_id = Arc::new(Mutex::new(None));
        let pending_auth_id = Arc::new(Mutex::new(None));
        let pending_tool_approvals = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let current_model = Arc::new(Mutex::new(None));
        let pending_sends = Arc::new(Mutex::new(HashMap::new()));
        let pending_discovery = Arc::new(Mutex::new(HashMap::new()));
        let pending_set = Arc::new(Mutex::new(HashMap::new()));
        let pending_resume = Arc::new(Mutex::new(None));
        let resume_poison = Arc::new(Mutex::new(None));
        let pending_fork = Arc::new(Mutex::new(None));
        let discovered = Arc::new(std::sync::Mutex::new(Discovered::default()));
        let turn_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (event_tx, _) = broadcast::channel(1024);

        let (stdin, stdout) = match io.take_stdio().await {
            Some((stdin, stdout)) => (Some(stdin), Some(stdout)),
            None => (None, None),
        };
        let stdin = Arc::new(Mutex::new(stdin));

        let reader_state = CodexReaderState {
            session_id: session_id.clone(),
            turn_gen: turn_gen.clone(),
            event_tx: event_tx.clone(),
            thread_binding: thread_binding.clone(),
            active_turn_id: active_turn_id.clone(),
            pending_auth_id: pending_auth_id.clone(),
            pending_tool_approvals: pending_tool_approvals.clone(),
            pending_sends: pending_sends.clone(),
            pending_discovery: pending_discovery.clone(),
            pending_set: pending_set.clone(),
            pending_resume: pending_resume.clone(),
            resume_poison: resume_poison.clone(),
            pending_fork: pending_fork.clone(),
            discovered: discovered.clone(),
            stdin: stdin.clone(),
            turn_in_flight: turn_in_flight.clone(),
        };
        let reader = start_codex_reader(&reader_state, stdout, io.clone());

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
            capabilities: codex_capabilities(),
            rpc_id: AtomicU64::new(0),
            turn_gen,
            stdin,
            event_tx,
            suspend,
            idle_timer,
            wake,
            reader_state,
            turn_in_flight,
            thread_binding,
            active_turn_id,
            pending_auth_id,
            pending_tool_approvals,
            current_model,
            pending_sends,
            pending_discovery,
            pending_set,
            pending_resume,
            resume_poison,
            pending_fork,
            discovered,
        }
    }

    /// Write one JSON-RPC frame (request or response) to stdin as a single line.
    async fn write_frame(&self, frame: Value) -> Result<(), BackendError> {
        let mut guard = self.stdin.lock().await;
        let stdin = guard
            .as_mut()
            .ok_or_else(|| BackendError::Transport("codex stdin unavailable".into()))?;
        let mut line = serde_json::to_vec(&frame).map_err(|e| BackendError::Transport(e.to_string()))?;
        line.push(b'\n');
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(&line)
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        stdin
            .flush()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        Ok(())
    }

    fn next_rpc_id(&self) -> u64 {
        self.rpc_id.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Resolve the bound backend threadId, waiting briefly for the async
    /// `thread/started` notification (Fresh sessions bind it on the wire; Resume
    /// pre-seeds it in open_session). Every `turn/*` + `thread/*` client request
    /// needs it. Polls up to ~2s before giving up (the handshake is sub-100ms in
    /// practice — see the captured transcripts).
    async fn bound_thread(&self) -> Result<String, BackendError> {
        // bug-hunt codex-500: the bound-thread window must cover a COLD start, not just
        // a warm dev machine. The old 40×50ms=2s was a magic constant that passed every
        // live test on a fast box with an already-trusted ~/.codex (where thread/started
        // arrives in <2s), but a fresh/untrusted project slows codex init past 2s →
        // timeout → opaque 500. Align to the agent-handshake budget the ACP lane uses
        // (~15s); env-overridable for genuinely slow environments. On timeout return the
        // RETRYABLE HandshakeTimeout (not Transport→500): the agent is still starting.
        self.bound_thread_within(super::handshake_budget()).await
    }

    /// Inner: poll for the thread binding within `budget` (the public `bound_thread`
    /// passes `handshake_budget()`; tests pass a tiny budget to exercise the timeout
    /// branch deterministically without a global env override / a 30s wait).
    async fn bound_thread_within(&self, budget: std::time::Duration) -> Result<String, BackendError> {
        let polls = (budget.as_millis() / 50).max(1) as u64;
        for _ in 0..polls {
            // Dead resume anchor (ELECTRON-3Q0): codex rejected the thread/resume,
            // so the binding this poll waits for will NEVER arrive. Fail fast with
            // the codex message as a SessionNotFound — the send-path maps it to the
            // dead-session error class, which clears the persisted anchor and lets
            // the conversation's auto-replay reopen Fresh.
            if let Some(poison) = self.resume_poison.lock().await.clone() {
                return Err(BackendError::SessionNotFound(poison));
            }
            if let Some(tid) = self.thread_binding.lock().await.clone() {
                return Ok(tid);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Err(BackendError::HandshakeTimeout(format!(
            "codex threadId not bound (thread/started not received within {budget:?})"
        )))
    }

    /// Replay the JSON-RPC handshake over the (already-connected) stdin: an
    /// `initialize`, then `thread/start` (Fresh / lost-Resume) or `thread/resume`
    /// (Resume — pre-seeds the threadId binding), then the model/list +
    /// collaborationMode/list discovery calls (registered in `pending_discovery`
    /// so the reader fills `discovered`). Shared by `open_session` (the initial
    /// open) and `wake_handle` (an idle-wake re-attach), so the wire shape lives in
    /// one place.
    async fn run_handshake(&self, mode: HandshakeMode<'_>) -> Result<(), BackendError> {
        // A handshake is a fresh chance: any prior resume rejection belonged to
        // the previous process/attempt.
        *self.resume_poison.lock().await = None;
        self.write_frame(initialize_params().into_frame(self.next_rpc_id(), "initialize"))
            .await?;
        match mode {
            HandshakeMode::Resume(tid) => {
                *self.thread_binding.lock().await = Some(tid.to_string());
                // Resume re-sends the full thread/start override surface — a bare
                // {threadId} resume silently drops the user's MCP servers and
                // resets approvalPolicy to its default (LIVE 0.144.1, see
                // `thread_resume_params`).
                // Register the rpc id so the reader can claim the response: an
                // ERROR ("no rollout found for thread id …", verified:
                // samples/codex-cli/0.144.1/dead_resume.jsonl) means the
                // pre-seeded binding is poisoned and must be cleared — leaving it
                // in place made every turn/start hit the dead threadId and hang
                // (ELECTRON-3Q0).
                let resume_id = self.next_rpc_id();
                *self.pending_resume.lock().await = Some(resume_id);
                self.write_frame(thread_resume_params(&self.wake.config, tid).into_frame(resume_id, "thread/resume"))
                    .await?;
            }
            HandshakeMode::Fork {
                parent_thread_id,
                last_turn_id,
            } => {
                // Deliberately NO thread_binding pre-seed: the parent threadId is
                // only the fork SOURCE. codex answers with `thread/started` for
                // the NEW thread, and the reader's existing handler binds it —
                // on this session's own connection that binding is exactly the
                // one we want (the parent conversation has its own connection
                // and is never touched). Register the rpc id so the reader can
                // claim an ERROR response (parent rollout gone, in-flight turn,
                // …) and poison the bound-thread wait — otherwise the first
                // Send would poll for a binding that will never arrive.
                let fork_id = self.next_rpc_id();
                *self.pending_fork.lock().await = Some(fork_id);
                self.write_frame(
                    thread_fork_params(&self.wake.config, parent_thread_id, last_turn_id)
                        .into_frame(fork_id, "thread/fork"),
                )
                .await?;
            }
            HandshakeMode::Fresh => {
                self.write_frame(thread_start_params(&self.wake.config).into_frame(self.next_rpc_id(), "thread/start"))
                    .await?;
            }
        }
        // Discovery (B-CODEX-MODEL-LIST): fire-and-forget; the reader claims the
        // responses by rpc id and fills `discovered`.
        let model_list_id = self.next_rpc_id();
        self.pending_discovery
            .lock()
            .await
            .insert(model_list_id, DiscoveryKind::Models);
        self.write_frame(json!({
            "jsonrpc": "2.0", "id": model_list_id, "method": "model/list",
            "params": { "includeHidden": false }
        }))
        .await?;
        // feature 012: codex's mode selector IS its permission axis, so we discover
        // `permissionProfile/list` (mapped to the fixed mode enum in `fill_discovery`)
        // and do NOT send `collaborationMode/list` (plan/default has no UI entry,
        // matching legacy ACP). The reader claims the response and fills `disc.modes`.
        // Backends without this list (older codex) simply never respond → modes stay
        // empty → no frontend mode selector.
        let perm_list_id = self.next_rpc_id();
        self.pending_discovery
            .lock()
            .await
            .insert(perm_list_id, DiscoveryKind::Permissions);
        self.write_frame(json!({
            "jsonrpc": "2.0", "id": perm_list_id, "method": "permissionProfile/list", "params": {}
        }))
        .await?;
        Ok(())
    }

    /// Wake from Dormant: re-spawn `codex app-server`, re-take its stdio, swap the
    /// fresh stdin into the retained slot, start a new reader on the SAME
    /// event_tx/turn_gen/bindings, and replay the resume handshake against the
    /// bound threadId (the resume anchor that survived the suspend) — so the FSM
    /// and subscribers never notice. Only reached when idle_ttl is set AND the slot
    /// was suspended (a test backend has no spawner → `inert()` → never enabled).
    async fn wake_handle(&self) -> Result<ProcHandle, BackendError> {
        let spawner = self
            .wake
            .spawner
            .as_ref()
            .ok_or_else(|| BackendError::Transport("codex wake: no spawner (suspension not enabled)".into()))?;
        let mut args = vec!["app-server".to_string()];
        args.extend(self.wake.config.extra_args.iter().cloned());
        let cmd = aionui_common::CommandSpec {
            // Same bundled-CLI resolution + spawn env as the initial spawn
            // (R16 continuity).
            command: self.wake.config.cli_program.clone().unwrap_or_else(|| "codex".into()),
            args,
            env: self.wake.config.spawn_env.clone(),
            cwd: self.wake.config.cwd.clone(),
        };
        let proc = spawner
            .spawn(cmd, &[], "aionui-session")
            .await
            .map_err(|e| BackendError::from_spawn("codex resume-spawn failed", e))?;
        let io: Arc<dyn AgentIo> = Arc::from(Box::new(crate::adapter::ManagedProcessIo::new(proc)) as Box<dyn AgentIo>);
        let (stdin, stdout) = match io.take_stdio().await {
            Some((stdin, stdout)) => (Some(stdin), Some(stdout)),
            None => (None, None),
        };
        *self.stdin.lock().await = stdin;
        // The pre-suspend turn id is dead — a fresh process has no active turn yet.
        // Clearing it prevents a steer/interrupt right after wake from targeting a
        // stale turn id (the reader re-binds active_turn_id on the next turn/started).
        *self.active_turn_id.lock().await = None;
        let reader = start_codex_reader(&self.reader_state, stdout, io.clone());
        // Replay the handshake against the bound threadId (resume re-attach). On a
        // handshake failure, abort the just-started reader so its AgentIo clone
        // releases and the freshly-spawned child is reaped (kill_on_drop) — else it
        // leaks (the controller never takes ownership of a failed wake's handle).
        // A wake NEVER replays a fork: by the time a session can idle-suspend,
        // its fork completed and `thread_binding` holds the NEW thread — resume
        // against it (or Fresh if the binding was lost, the pre-fork behavior).
        let resume_tid = self.thread_binding.lock().await.clone();
        let mode = match resume_tid.as_deref() {
            Some(tid) => HandshakeMode::Resume(tid),
            None => HandshakeMode::Fresh,
        };
        if let Err(e) = self.run_handshake(mode).await {
            reader.abort();
            return Err(e);
        }
        Ok(ProcHandle::new(reader, io))
    }
}

/// The long-lived JSON-RPC reader: each line is a server notification, a
/// response to one of our requests, or a server-initiated request (reverse-RPC).
/// Notifications → SessionEvent (demuxed by threadId→logical id). Reverse-RPC →
/// AUTO-RESPONDED (A2/A3: never deadlock) and, where user-facing, surfaced as
/// Permission.
#[allow(clippy::too_many_arguments)]
async fn reader_task(
    session_id: String,
    stdout: Option<aionui_process::BoxedStdout>,
    io: Arc<dyn AgentIo>,
    turn_gen: Arc<AtomicU64>,
    event_tx: broadcast::Sender<SessionEnvelope>,
    thread_binding: Arc<Mutex<Option<String>>>,
    active_turn_id: Arc<Mutex<Option<String>>>,
    pending_auth_id: Arc<Mutex<Option<Value>>>,
    pending_tool_approvals: Arc<std::sync::Mutex<HashMap<String, String>>>,
    pending_sends: Arc<Mutex<HashMap<u64, PendingSend>>>,
    pending_discovery: Arc<Mutex<HashMap<u64, DiscoveryKind>>>,
    pending_set: Arc<Mutex<HashMap<u64, String>>>,
    pending_resume: Arc<Mutex<Option<u64>>>,
    resume_poison: Arc<Mutex<Option<String>>>,
    pending_fork: Arc<Mutex<Option<u64>>>,
    discovered: Arc<std::sync::Mutex<Discovered>>,
    stdin: Arc<Mutex<Option<aionui_process::BoxedStdin>>>,
    turn_in_flight: Arc<std::sync::atomic::AtomicBool>,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};

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

    // R8: has the CURRENT turn already produced its single TurnResult? Set by the
    // authoritative `turn/completed`; the trailing `status→idle` is then absorbed.
    // Reset on `turn/started` so the NEXT turn can terminate once.
    let mut terminated = false;
    // R8/M3: codex sends `status→idle` BEFORE `turn/completed`. We DEFER on idle
    // (set this) and let the authoritative completed produce the rich terminal. If
    // the turn somehow ends with idle but no completed (defensive), this is flushed
    // as a clean terminal at EOF so the FSM never hangs Running.
    let mut idle_pending = false;
    // Deferred `status→systemError` (mirrors idle_pending): the status carries no
    // detail, so we wait for the rich follow-up (error{willRetry:false} or
    // turn/completed — live capture 0.145.0 shows both arrive within ms) instead
    // of synthesizing the opaque terminal immediately. The deadline bounds the
    // wait so an unfollowed systemError still terminates the turn.
    let mut system_error_pending = false;
    let mut system_error_deadline: Option<tokio::time::Instant> = None;

    let mut lines = BufReader::new(stdout).lines();
    // Unbounded mid-turn read (AGENTS.md §"出了问题必须查到根因": NO mid-turn
    // watchdog/timeout — it masks the real cause AND false-kills a healthy long turn,
    // e.g. codex can legitimately go ~55s silent between a finished tool and the
    // agentMessage. A wedged turn is ended by user Cancel, per the no-auto-timeout
    // design; startup binding is the only thing bounded, via bound_thread_within).
    // A REAL fatal signal (error{willRetry:false}) still synthesizes a terminal below.
    // The ONLY bounded read is the SYSTEM_ERROR_GRACE below, armed strictly AFTER
    // codex has already declared the thread fatally errored (status→systemError) —
    // it cannot false-kill a healthy turn.
    loop {
        let next = match system_error_deadline {
            Some(deadline) if system_error_pending && !terminated => {
                match tokio::time::timeout_at(deadline, lines.next_line()).await {
                    Ok(read) => read,
                    Err(_elapsed) => {
                        // Grace expired: no rich follow-up arrived after systemError
                        // (never observed live — defensive bound). Fall back to the
                        // opaque terminal so the FSM leaves Running.
                        terminated = true;
                        system_error_pending = false;
                        system_error_deadline = None;
                        *active_turn_id.lock().await = None;
                        turn_in_flight.store(false, Ordering::SeqCst);
                        emit(
                            &event_tx,
                            &session_id,
                            turn_gen.load(Ordering::SeqCst),
                            synth_error_terminal("codex reported a system error".into()),
                        );
                        continue;
                    }
                }
            }
            _ => lines.next_line().await,
        };
        if !system_error_pending || terminated {
            system_error_deadline = None;
        }
        match next {
            Ok(Some(line)) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                // Opt-in raw wire tap for turn-boundary forensics (never on by
                // default — carries tool input/output; enable with
                // `--log-level "info,codex_wire=debug"`).
                tracing::debug!(target: "codex_wire", session_id = %session_id, frame = %line, "codex <- frame");
                let Ok(frame): Result<Value, _> = serde_json::from_str(line) else {
                    // unparseable line → opaque, never panic
                    emit(
                        &event_tx,
                        &session_id,
                        turn_gen.load(Ordering::SeqCst),
                        SessionEvent::AdapterSpecific {
                            tag: "codex_unparseable".into(),
                            payload: json!({ "raw": line }),
                        },
                    );
                    continue;
                };

                // A server-initiated REQUEST has BOTH `method` and `id`
                // (reverse-RPC). A notification has `method` but no `id`. A
                // response to our request has `id` + (`result`|`error`), no method.
                let method = frame.get("method").and_then(Value::as_str);
                let has_id = frame.get("id").is_some();
                match (method, has_id) {
                    (Some(m), true) => {
                        // reverse-RPC (ServerRequest): infra → auto-reject to prevent
                        // deadlock (A2/A3); auth-refresh + approvals → surface as
                        // Permission (NOT auto-answered — a human/credential answers).
                        handle_reverse_rpc(
                            m,
                            &frame,
                            &session_id,
                            &turn_gen,
                            &event_tx,
                            &pending_auth_id,
                            &pending_tool_approvals,
                            &stdin,
                        )
                        .await;
                    }
                    (Some(m), false) => {
                        // server notification → SessionEvent(s)
                        let cur = turn_gen.load(Ordering::SeqCst);
                        let params = frame.get("params").unwrap_or(&Value::Null);
                        if m == "thread/started" {
                            // bind threadId (backend transport key, kept private).
                            if let Some(tid) = params.get("thread").and_then(|t| t.get("id")).and_then(Value::as_str) {
                                *thread_binding.lock().await = Some(tid.to_string());
                                // Addendum 9: lower the binding downstream so the
                                // conversation persists backend_session_id (the
                                // resume/rewind anchor). This covers fresh + fork +
                                // resume re-attach (all surface a thread/started).
                                emit(
                                    &event_tx,
                                    &session_id,
                                    cur,
                                    SessionEvent::BackendBound {
                                        backend_session_id: Some(tid.to_string()),
                                    },
                                );
                            }
                        }
                        if m == "turn/started" {
                            terminated = false; // a new turn can terminate once (R8 reset)
                            idle_pending = false; // and a fresh turn has no deferred idle
                            system_error_pending = false; // nor a deferred systemError
                            system_error_deadline = None;
                            // Capture the active turn id (optimistic token needed by
                            // turn/interrupt{turnId} + turn/steer{expectedTurnId}).
                            if let Some(tid) = params.get("turn").and_then(|t| t.get("id")).and_then(Value::as_str) {
                                *active_turn_id.lock().await = Some(tid.to_string());
                                // Fork anchoring: lower codex's OWN turn id so the
                                // conversation layer stamps it onto this turn's
                                // message rows (`messages.backend_turn_id`) — the
                                // lookup key for `thread/fork`'s `lastTurnId`.
                                emit(
                                    &event_tx,
                                    &session_id,
                                    cur,
                                    SessionEvent::BackendTurnBound {
                                        backend_turn_id: tid.to_string(),
                                    },
                                );
                            }
                        }
                        // R8 dual-terminal reconcile: codex sends status→idle FIRST
                        // (deferred), then the authoritative turn/completed produces
                        // the rich terminal (M3). Exactly ONE TurnResult per turn.
                        if m == "turn/completed" || m == "thread/status/changed" {
                            let was_pending = system_error_pending;
                            if let Some(ev) = reconcile_terminal(
                                m,
                                params,
                                &mut terminated,
                                &mut idle_pending,
                                &mut system_error_pending,
                            ) {
                                // Turn ended → clear the active turn id (a stale token
                                // would make a later steer/interrupt target a dead turn).
                                *active_turn_id.lock().await = None;
                                // F-4: turn terminal → clear the turn-active flag so the
                                // idle timer may suspend the now-idle process.
                                turn_in_flight.store(false, Ordering::SeqCst);
                                emit(&event_tx, &session_id, cur, ev);
                            } else if system_error_pending && !was_pending {
                                // systemError was just deferred: arm the bounded grace
                                // for the rich follow-up (error{willRetry:false} /
                                // turn/completed). Intervening frames do NOT extend it.
                                system_error_deadline = Some(tokio::time::Instant::now() + SYSTEM_ERROR_GRACE);
                            }
                            continue;
                        }
                        // serverRequest/resolved: codex's confirmation that a
                        // ServerRequest (approval or auth-refresh) was answered →
                        // PermissionResolved so the reducer decrements the matching
                        // counter (R9/R15). We can't know Tool-vs-Auth from this
                        // bookkeeping notif alone, so resolve against the pending
                        // auth id if it matches, else default Tool (the common case).
                        if m == "serverRequest/resolved" {
                            let req_id = params.get("request_id").or_else(|| params.get("requestId")).cloned();
                            let kind = {
                                let pending = pending_auth_id.lock().await;
                                match (pending.as_ref(), req_id.as_ref()) {
                                    (Some(p), Some(r)) if p == r => crate::event::PermissionKind::Auth,
                                    _ => crate::event::PermissionKind::Tool,
                                }
                            };
                            if matches!(kind, crate::event::PermissionKind::Auth) {
                                *pending_auth_id.lock().await = None;
                            }
                            let resolved_id = req_id.map(|v| v.to_string()).unwrap_or_default();
                            // Drop the recovered-card entry: codex resolved this approval
                            // (answered elsewhere or retracted), so it is no longer a
                            // pending confirmation for REST recovery. The registry keys
                            // by the surfaced request_id (raw for tool/file approvals,
                            // ELICIT_PREFIX-tagged for elicitation), matching the id shape
                            // stored on the requestApproval emit below.
                            remove_pending_tool_approval(&pending_tool_approvals, &resolved_id);
                            emit(
                                &event_tx,
                                &session_id,
                                cur,
                                SessionEvent::PermissionResolved {
                                    request_id: resolved_id,
                                    kind,
                                },
                            );
                            continue;
                        }
                        // FATAL error terminal (#codex-no-terminal): a codex
                        // `error{willRetry:false}` is the turn's terminal cause, but
                        // codex does NOT reliably follow it with `turn/completed`
                        // (and may instead go silent). Previously we emitted nothing
                        // here and bet on a completed that might never come → the FSM
                        // hung Running forever → permanent UI spinner. Now we synthesize
                        // an is_error terminal so the turn ends; if a real
                        // `turn/completed` DOES arrive later, `terminated`/I10 absorb it
                        // (no double terminal). `willRetry:true` is a transient retry →
                        // still falls through to map_notification → Heartbeat (NOT a
                        // terminal). See protocols/design/aioncore-codex-turn-no-terminal-hang-prompt.md.
                        if m == "error"
                            && params.get("willRetry").and_then(Value::as_bool) != Some(true)
                            && turn_in_flight.load(Ordering::SeqCst)
                        {
                            if !terminated {
                                terminated = true;
                                system_error_pending = false; // the fatal error IS the rich follow-up
                                system_error_deadline = None;
                                *active_turn_id.lock().await = None;
                                turn_in_flight.store(false, Ordering::SeqCst);
                                let message = params
                                    .get("error")
                                    .and_then(|e| e.get("message").and_then(Value::as_str).or_else(|| e.as_str()))
                                    .unwrap_or("codex reported a fatal error")
                                    .to_string();
                                tracing::warn!(
                                    conversation_id = %session_id,
                                    turn_gen = cur,
                                    "codex error{{willRetry:false}} → synthesizing is_error terminal (no turn/completed guaranteed)"
                                );
                                emit(&event_tx, &session_id, cur, synth_error_terminal(message));
                            }
                            continue;
                        }
                        for ev in map_notification(m, params) {
                            emit(&event_tx, &session_id, cur, scope_notice_to_turn(ev, cur));
                        }
                    }
                    _ => {
                        // A response to one of OUR client requests (id + result/error,
                        // no method). GAP-A: claim the `turn/start` response — it is
                        // codex's synchronous "prompt accepted" receipt (carries
                        // {turn:{id,status:inProgress}}). If its rpc id matches a
                        // pending Send, a result emits PromptAccepted{client_msg_id}
                        // so the conversation's pending queue drains (Addendum 3); an
                        // ERROR terminates the turn / surfaces a Notice (below) —
                        // never a silent drop. Other responses (settings/rollback/etc)
                        // flow via notifications; diagnostic only.
                        if let Some(rid) = frame.get("id").and_then(Value::as_u64) {
                            let error_message = frame.get("error").map(|e| {
                                e.get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("request rejected (no error message)")
                                    .to_string()
                            });
                            // (ELECTRON-3Q0 fix A) Claim the thread/resume response.
                            // An ERROR ("no rollout found for thread id …", verified:
                            // samples/codex-cli/0.144.1/dead_resume.jsonl) means the
                            // pre-seeded binding points at a thread this codex cannot
                            // restore: clear it and poison the bound-thread wait so a
                            // Send fails fast with the real cause instead of writing
                            // turn/start at a dead threadId (or timing out opaquely).
                            let is_resume = {
                                let mut pending = pending_resume.lock().await;
                                match *pending {
                                    Some(prid) if prid == rid => {
                                        *pending = None;
                                        true
                                    }
                                    _ => false,
                                }
                            };
                            if is_resume && let Some(msg) = error_message.as_deref() {
                                tracing::warn!(
                                    conversation_id = %session_id,
                                    error = %msg,
                                    "codex thread/resume rejected — clearing poisoned thread binding (dead resume anchor)"
                                );
                                *thread_binding.lock().await = None;
                                *resume_poison.lock().await = Some(format!("codex thread/resume failed: {msg}"));
                                continue;
                            }
                            // Claim the thread/fork response (pending_resume's
                            // sibling). A Fork handshake pre-seeds NO binding, so
                            // an ERROR (parent rollout gone, …) must poison the
                            // bound-thread wait or the first Send polls forever.
                            // NEVER fall back to a fresh/HEAD thread here — the
                            // user asked for the parent's context, and silently
                            // dropping it is worse than failing (§G). A success
                            // needs nothing: `thread/started` for the NEW thread
                            // binds it via the notification handler above.
                            let is_fork = {
                                let mut pending = pending_fork.lock().await;
                                match *pending {
                                    Some(pfid) if pfid == rid => {
                                        *pending = None;
                                        true
                                    }
                                    _ => false,
                                }
                            };
                            if is_fork && let Some(msg) = error_message.as_deref() {
                                tracing::warn!(
                                    conversation_id = %session_id,
                                    error = %msg,
                                    "codex thread/fork rejected — poisoning bound-thread wait (no silent fallback)"
                                );
                                *resume_poison.lock().await = Some(format!("codex thread/fork failed: {msg}"));
                                continue;
                            }
                            let pending_send = pending_sends.lock().await.remove(&rid);
                            if let Some(send) = pending_send {
                                if frame.get("result").is_some() {
                                    if let Some(client_msg_id) = send.client_msg_id {
                                        emit(
                                            &event_tx,
                                            &session_id,
                                            turn_gen.load(Ordering::SeqCst),
                                            SessionEvent::PromptAccepted { client_msg_id },
                                        );
                                    }
                                } else if let Some(msg) = error_message.as_deref() {
                                    // (ELECTRON-3Q0 fix B) codex REJECTED the request.
                                    // Previously the correlation was dropped without
                                    // emitting → the admitted turn hung Running forever
                                    // (permanently locked conversation). Turn-flavored →
                                    // synthesize the is_error terminal (same `terminated`
                                    // discipline as the fatal-error arm; a late real
                                    // terminal is absorbed by I10). NoTurn (/logout) →
                                    // no turn to end; surface a Notice instead.
                                    if send.opens_turn {
                                        if !terminated {
                                            terminated = true;
                                            *active_turn_id.lock().await = None;
                                            turn_in_flight.store(false, Ordering::SeqCst);
                                            tracing::warn!(
                                                conversation_id = %session_id,
                                                error = %msg,
                                                "codex rejected the turn request — synthesizing is_error terminal"
                                            );
                                            emit(
                                                &event_tx,
                                                &session_id,
                                                turn_gen.load(Ordering::SeqCst),
                                                synth_error_terminal(format!("codex rejected the turn request: {msg}")),
                                            );
                                        }
                                    } else {
                                        emit(
                                            &event_tx,
                                            &session_id,
                                            turn_gen.load(Ordering::SeqCst),
                                            SessionEvent::Notice {
                                                level: crate::event::NoticeLevel::Warning,
                                                message: format!("Codex logout failed: {msg}"),
                                                localized: None,
                                                supersedes_key: None,
                                            },
                                        );
                                    }
                                }
                            }
                            // B-CODEX-MODEL-LIST / O2: claim a discovery response.
                            // model/list + collaborationMode/list fill the
                            // `discovered` cache (capabilities() merges them);
                            // thread/turns/list (Checkpoints) is mapped to a
                            // CheckpointList event instead (O2 up-leg — a query
                            // result, not a capability). Lazy; a later page
                            // (next_cursor) is ignored — first page bounds the N2
                            // unbounded-catalog risk (we don't chase the cursor).
                            let disc_kind = pending_discovery.lock().await.remove(&rid);
                            if let Some(kind) = disc_kind
                                && let Some(result) = frame.get("result")
                            {
                                match kind {
                                    DiscoveryKind::Models | DiscoveryKind::Permissions => {
                                        fill_discovery(kind, result, &discovered);
                                        // Signal the async catalog arrival so the conversation
                                        // re-projects the model/mode picker (the ACP
                                        // `emit_snapshot_events` analogue). model/list and
                                        // permissionProfile/list are SEPARATE responses; emit a
                                        // full snapshot of whatever `discovered` holds now, so the
                                        // first arrival already lights the picker and later ones
                                        // refine it. Without this the frontend, which read an empty
                                        // `config_options` on open, never re-fetches and the
                                        // selectors stay disabled. (codex's modes come from
                                        // permissionProfile/list — the fixed permission-tier enum.)
                                        let (models, modes) = {
                                            let disc = discovered.lock().unwrap_or_else(|e| e.into_inner());
                                            (disc.models.clone(), disc.modes.clone())
                                        };
                                        emit(
                                            &event_tx,
                                            &session_id,
                                            turn_gen.load(Ordering::SeqCst),
                                            SessionEvent::CatalogUpdated {
                                                models,
                                                modes,
                                                // The static bridge-parity command table (codex has
                                                // no discovery wire) — carried on the catalog event
                                                // so the agent_metadata writeback + the frontend
                                                // AvailableCommands push see it (ELECTRON-3PX).
                                                slash_commands: builtin_slash_commands(),
                                            },
                                        );
                                    }
                                    DiscoveryKind::Checkpoints => {
                                        emit(
                                            &event_tx,
                                            &session_id,
                                            turn_gen.load(Ordering::SeqCst),
                                            SessionEvent::CheckpointList {
                                                entries: map_turns_to_checkpoints(result),
                                            },
                                        );
                                    }
                                    DiscoveryKind::Rewind => {
                                        // G3 up-leg: thread/rollback response → Rewound
                                        // {to_turn}. to_turn = the post-rollback history-
                                        // end turn count (result.thread.turns.len(), the
                                        // turns codex re-sends populated only on rollback/
                                        // resume/fork/read). The orchestrator rehydrates to
                                        // it / the conversation forks from it (T17); the
                                        // reducer ignores it (no FSM phase change). Without
                                        // this receipt the rollback silently mutated codex
                                        // history with no upward signal (GAP-B).
                                        emit(
                                            &event_tx,
                                            &session_id,
                                            turn_gen.load(Ordering::SeqCst),
                                            SessionEvent::Rewound {
                                                to_turn: rollback_to_turn(result),
                                            },
                                        );
                                    }
                                }
                            }

                            // SetMode/SetModel: claim the `thread/settings/update`
                            // response (dispatch registered rpc_id → "mode→<v>"/"model→<v>").
                            // A JSON-RPC ERROR (codex rejected the model/mode) is surfaced
                            // as a Notice{Warning} + error log so a FAILED set is visible
                            // instead of being silently dropped (it used to be claimed by no
                            // one). A SUCCESS does NOTHING here: codex converges via the
                            // separate `thread/settings/updated` notification (→ ConfigChanged,
                            // live-verified) — emitting a second ConfigChanged would duplicate.
                            // On SUCCESS this does nothing: codex converges via the
                            // separate thread/settings/updated notification (→ ConfigChanged),
                            // so the claim only matters when the response carries an error.
                            if let Some(label) = pending_set.lock().await.remove(&rid)
                                && let Some(err) = frame.get("error")
                            {
                                let message = err
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("set rejected")
                                    .to_string();
                                tracing::error!(
                                    conversation_id = %session_id,
                                    set = %label,
                                    "codex thread/settings/update (SetMode/SetModel/effort) rejected by agent: {message}"
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
                            }
                        }
                    }
                }
            }
            Ok(None) => break, // EOF
            Err(_) => break,
        }
    }

    // F-4: the reader loop ended (process exited / stdout EOF) → the turn (if any)
    // is terminal. Clear the turn-active flag so the idle timer is unblocked.
    turn_in_flight.store(false, std::sync::atomic::Ordering::SeqCst);

    // Deferred-systemError flush: the stream ended (EOF) before the rich follow-up
    // arrived. Emit the opaque error terminal so the turn still ends as an error —
    // this keeps the pre-defer behavior for the process-death path.
    if system_error_pending && !terminated {
        terminated = true;
        *active_turn_id.lock().await = None;
        emit(
            &event_tx,
            &session_id,
            turn_gen.load(Ordering::SeqCst),
            synth_error_terminal("codex reported a system error".into()),
        );
    }

    // M3 defensive flush: the stream ended with a deferred `status→idle` but NO
    // authoritative `turn/completed` ever arrived (not observed in real codex, but
    // the §C5 R8 contract allows "one may be missing"). Emit a clean terminal so a
    // turn that reached idle isn't left hanging Running.
    if idle_pending && !terminated {
        *active_turn_id.lock().await = None;
        emit(
            &event_tx,
            &session_id,
            turn_gen.load(Ordering::SeqCst),
            synth_clean_terminal(),
        );
    }

    // Addendum 9: the backend session is gone (process exited / stdout EOF). Lower
    // BackendBound{None} so the conversation knows the live binding is dead (the
    // turn won't continue on this process). We do NOT clear `thread_binding` itself
    // — the threadId is still the resume anchor (conversation persisted it; a later
    // Resume re-attaches via thread/resume). This only signals "not live now".
    let was_bound = thread_binding.lock().await.is_some();
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

/// feature 012 — codex permission-profile ↔ fixed-mode-enum mapping (SSOT).
///
/// codex's permission tiers are DISCOVERED, not a fixed AionUi enum — mirroring the
/// legacy ACP mechanism (`manager/acp/session.rs`: `availableModes[]` advertised by the
/// agent, `is_mode_valid` validates against that live list, AionCore defines no values).
/// codex advertises them via `permissionProfile/list`, each identified by a COLON-
/// PREFIXED id (`:workspace` / `:danger-full-access` / `:read-only`, plus any user
/// `[permissions.<id>]` custom profile — the bare form is rejected on the wire,
/// live-verified 0.139.0). We surface those ids VERBATIM as the mode catalog; codex has
/// no separately-exposed collaborationMode selector, so its mode axis IS the permission
/// axis.
///
/// This module owns the codex mode-value translation, in BOTH directions, so the colon
/// wire id stays an internal/wire detail and the value the FRONTEND sees is byte-identical
/// to what the legacy `@zed-industries/codex-acp` path advertised (live-verified: the
/// bridge's `availableModes[].id` were the BARE tokens `read-only` / `auto` / `full-access`,
/// never colon-prefixed):
///   - inbound  (`normalize_to_profile_id`): a persisted/legacy value → colon profile id,
///     the codex analogue of legacy ACP `mode_normalize::normalize_requested_mode`.
///   - outbound (`profile_id_to_legacy_value`): a discovered colon id → the legacy bare
///     token the frontend keys its i18n / picker off, so all 12 locales auto-adapt with no
///     frontend change. It is the exact inverse of `normalize_to_profile_id` over the three
///     built-in tiers; a custom `[permissions.<id>]` profile (which legacy could not
///     express — the bridge hardcoded only the three tiers) has no bare equivalent, so it
///     flows through colon-and-all in BOTH directions (round-trip preserved).
mod codex_perm {
    /// Normalize a persisted/legacy mode value into a colon-prefixed permission-profile
    /// id, so an upgrading user's stored value (or a legacy alias) maps onto a real codex
    /// profile with no fallback to a value codex would reject. This is the codex analogue
    /// of legacy ACP `mode_normalize.rs` (alias → native id) + `codex_sandbox.rs` (2-tier
    /// bucketing):
    ///   - already colon-prefixed (`:workspace`, a discovered/custom id) → passed through
    ///     verbatim (the discovery path already speaks colon ids)
    ///   - `agent-full-access` (canonical) / `full-access` / `yolo` / `yoloNoSandbox` → `:danger-full-access`
    ///   - `read-only`                               → `:read-only`
    ///   - `default` / `auto` / `autoEdit` / else    → `:workspace`
    ///
    /// The catch-all lands on `:workspace` (the safe workspace-write tier), never a value
    /// codex would reject. Validation against the DISCOVERED catalog (a custom id that no
    /// longer exists) happens at the call site, exactly as legacy `is_mode_valid` did.
    pub(super) fn normalize_to_profile_id(mode: &str) -> String {
        let trimmed = mode.trim();
        if let Some(rest) = trimmed.strip_prefix(':') {
            // Already a profile id (discovery / custom / re-persisted colon value):
            // pass through verbatim, unless it is empty (`":"`) which is nonsense.
            if !rest.is_empty() {
                return trimmed.to_owned();
            }
        }
        match trimmed {
            // `agent-full-access` is the #608 canonical codex full-access id (migration 021 +
            // `normalize_requested_mode`); legacy `full-access` / `yolo` / `yoloNoSandbox` stay
            // recognized for pre-021 persisted data. All map onto the danger-full-access profile.
            "agent-full-access" | "full-access" | "yolo" | "yoloNoSandbox" => ":danger-full-access".to_owned(),
            "read-only" => ":read-only".to_owned(),
            _ => ":workspace".to_owned(),
        }
    }

    /// Map a colon-prefixed permission-profile id back to the legacy bare mode token the
    /// FRONTEND expects, so the direct-CLI path presents the SAME value legacy ACP did
    /// (`read-only` / `auto` / `full-access`) — the picker's i18n keys off this value, and
    /// `agentMode.json` only carries the bare keys, so this is what makes all 12 locales
    /// render "Read Only / Default / Full Access" instead of an English fallback on a colon
    /// id that misses every key.
    ///
    /// The three built-in tiers are the EXACT inverse of `normalize_to_profile_id`
    /// (`:workspace` ↔ the workspace-write tier legacy advertised as `auto`), so a value
    /// round-trips losslessly: outbound colon → bare here, inbound bare → colon there.
    /// A custom `[permissions.<id>]` profile has NO legacy bare form (the bridge hardcoded
    /// only the three tiers), so it MUST flow through colon-and-all — stripping its colon to
    /// `<id>` would send the frontend a value that `normalize_to_profile_id` cannot recover
    /// (it would bucket the unknown bare token into the `:workspace` catch-all → wrong tier
    /// applied). Passing the colon through keeps the round-trip intact (the frontend renders
    /// its `name` via `defaultValue`, and echoes the colon id back unchanged).
    pub(super) fn profile_id_to_legacy_value(profile_id: &str) -> String {
        match profile_id {
            ":read-only" => "read-only".to_owned(),
            ":workspace" => "auto".to_owned(),
            ":danger-full-access" => "full-access".to_owned(),
            other => other.to_owned(),
        }
    }

    /// Present a requested/persisted mode value in the CATALOG vocabulary (legacy bare
    /// token), whichever accepted vocabulary it arrives in: the inbound leg
    /// (`normalize_to_profile_id`) buckets canonical/legacy/colon values onto a colon
    /// profile id, and the outbound leg (`profile_id_to_legacy_value`) maps that id onto
    /// the bare token the catalog rows carry. Composing the two is what makes the
    /// `capabilities.current_mode` seed land on a value the picker can highlight —
    /// notably `agent-full-access` (the #608 canonical id `normalize_requested_mode`
    /// emits for a resumed full-access conversation, which the outbound leg alone would
    /// pass through as a token the catalog never contains) → `:danger-full-access` →
    /// `full-access`. A custom colon profile id round-trips verbatim (both legs pass it
    /// through), and an unknown bare token lands on the workspace tier's `auto` — the
    /// same bucketing the SetMode APPLY path uses, so the displayed tier cannot drift
    /// from the tier that would be applied.
    pub(super) fn mode_to_catalog_value(mode: &str) -> String {
        profile_id_to_legacy_value(&normalize_to_profile_id(mode))
    }

    /// Friendly display `(name, description)` for a built-in codex permission profile.
    ///
    /// codex's `permissionProfile/list` returns the built-in profiles with `description:
    /// null` and NO display name (live-verified 0.139.0 — the wire is just
    /// `{"id":":read-only","description":null}`), so a verbatim pass-through would surface
    /// bare colon ids (`:workspace`) with no tooltip in the picker. The legacy ACP path did
    /// NOT do that: the `@zed-industries/codex-acp` bridge enriched each profile into an ACP
    /// `SessionMode{id,name,description}` with a human label and a full sentence before
    /// advertising it. This table reproduces that display layer so the direct-CLI path
    /// matches the old UX — the strings are copied VERBATIM from the bridge binary
    /// (`codex-acp` 0.14.0). It is a codex-specific DISPLAY adaptation (the analogue of
    /// `codex_sandbox.rs`'s param adaptation), NOT a value-set definition: the id set still
    /// comes from codex, and a custom `[permissions.<id>]` profile (unknown here) keeps
    /// falling back to whatever codex sends.
    ///
    /// Id note: legacy advertised the workspace tier as `auto` displayed "Default"; the
    /// direct app-server id for the same tier is `:workspace`. Semantics are identical
    /// (workspace-write, approval for network / out-of-workspace edits), so we keep the
    /// legacy "Default" label + sentence.
    pub(super) fn builtin_profile_display(profile_id: &str) -> Option<(&'static str, &'static str)> {
        match profile_id {
            ":read-only" => Some((
                "Read Only",
                "Codex can read files in the current workspace. Approval is required to edit files or access the internet.",
            )),
            ":workspace" => Some((
                "Default",
                "Codex can read and edit files in the current workspace, and run commands. Approval is required to access the internet or edit other files. (Identical to Agent mode)",
            )),
            ":danger-full-access" => Some((
                "Full Access",
                "Codex can edit files outside this workspace and access the internet without asking for approval. Exercise caution when using.",
            )),
            _ => None,
        }
    }
}

/// B-CODEX-MODEL-LIST: map a `model/list` / `collaborationMode/list` response
/// `result` into the `discovered` cache.
///
/// WIRE SHAPE — calibrated to the REAL capture
/// `protocols/samples/codex-cli/0.137.0/appserver-methods/catalog.jsonl` (id:5
/// model/list, id:7 collaborationMode/list), NOT to a hand-written assumption
/// (README discipline #9 / dimension 25 — the prior `result.models[]` /
/// `result.modes[]` keys were a self-confirming guess that never matched the wire
/// → empty lists → config-options empty → frontend fell back to a hardcoded model
/// name → Bedrock 404):
/// - BOTH lists are under `result.data[]` (NOT `models`/`modes`); a `nextCursor`
///   rides alongside (first page only — we do not chase it, N2 bound). We try
///   `data` first then fall back to `models`/`modes` so a cross-version rename in
///   either direction degrades gracefully rather than silently emptying.
/// - model item: `{id, displayName, description, supportedReasoningEfforts}` where
///   `supportedReasoningEfforts` is an array of OBJECTS `{reasoningEffort, description}`
///   (the old code read bare strings → every object dropped → empty efforts). We
///   accept both: an object → its `reasoningEffort`, a bare string → itself.
/// - mode item: `{name:"Plan", mode:"plan", model?, reasoning_effort?}`. The id MUST
///   be the lowercase `mode` token, because `dispatch(SetMode)` sends
///   `collaborationMode.mode` = that token (codex rejects the display `name`); `name`
///   is the human label. Falls back to `name` only if `mode` is absent.
///
/// A genuinely empty list after a successful response means the wire shape drifted
/// again — `warn!` so it is diagnosable (it must never silently degrade to empty
/// like the original bug did).
fn fill_discovery(kind: DiscoveryKind, result: &Value, discovered: &Arc<std::sync::Mutex<Discovered>>) {
    use crate::capability::{ModeInfo, ModelInfo};
    // The real wire wraps both lists in `data`; `models`/`modes` is the legacy/guessed
    // key kept only as a cross-version fallback.
    let list = |primary: &str, legacy: &str| -> Option<Vec<Value>> {
        result
            .get(primary)
            .or_else(|| result.get(legacy))
            .and_then(Value::as_array)
            .cloned()
    };
    match kind {
        DiscoveryKind::Models => {
            let arr = list("data", "models");
            let present = arr.is_some();
            let models = arr
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| {
                            let id = m.get("id").and_then(Value::as_str)?.to_string();
                            Some(ModelInfo {
                                id,
                                name: m.get("displayName").and_then(Value::as_str).unwrap_or("").to_string(),
                                description: m.get("description").and_then(Value::as_str).map(str::to_string),
                                reasoning_efforts: m
                                    .get("supportedReasoningEfforts")
                                    .and_then(Value::as_array)
                                    .map(|e| {
                                        e.iter()
                                            // real wire: object {reasoningEffort, description};
                                            // legacy/guess: bare string. Accept either.
                                            .filter_map(|v| {
                                                v.get("reasoningEffort")
                                                    .and_then(Value::as_str)
                                                    .or_else(|| v.as_str())
                                                    .map(str::to_string)
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if present && models.is_empty() {
                tracing::warn!("codex model/list parsed to empty (wire shape may have drifted from result.data[])");
            }
            discovered.lock().unwrap_or_else(|e| e.into_inner()).models = models;
        }
        DiscoveryKind::Permissions => {
            // codex's mode axis IS the permission axis. This is the DISCOVERY half of the
            // legacy-ACP mechanism (`session.rs::apply_advertised_modes`): every profile
            // codex advertises via `permissionProfile/list` is surfaced VERBATIM as a mode
            // — colon-prefixed id and all (`:workspace` / `:danger-full-access` /
            // `:read-only`, plus any user `[permissions.<id>]` custom profile). We no
            // longer translate to a fixed AionUi enum or drop custom profiles: codex
            // defines the value set, AionCore only transports it (parity with legacy ACP,
            // where `availableModes[]` came straight off the wire). `disc.modes` is the
            // SAME cache slot `reconcile_codex_mode` validates against and the capabilities
            // snapshot exposes; `collaborationMode/list` is not sent (plan/default has no
            // UI entry, matching legacy ACP).
            let arr = list("data", "permissions");
            let present = arr.is_some();
            let modes = arr
                .map(|arr| {
                    arr.iter()
                        .filter_map(|p| {
                            // Wire id retains the leading colon (`:workspace`) — that is
                            // what SetMode sends back and what the reader matches. Skip only
                            // a malformed entry with no id.
                            let profile_id = p.get("id").and_then(Value::as_str)?;
                            // Display layer (matches legacy ACP): codex's built-in profiles
                            // arrive with no name and `description:null`, so prefer codex's
                            // own fields when present (a custom `[permissions.<id>]` may carry
                            // them), then the built-in friendly table (verbatim bridge copy),
                            // then the bare id as a last resort.
                            let builtin = codex_perm::builtin_profile_display(profile_id);
                            let name = p
                                .get("name")
                                .or_else(|| p.get("displayName"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .or_else(|| builtin.map(|(n, _)| n.to_string()))
                                .unwrap_or_else(|| profile_id.to_string());
                            let description = p
                                .get("description")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .or_else(|| builtin.map(|(_, d)| d.to_string()));
                            // The catalog's `id` is the value the FRONTEND sees and keys its
                            // i18n / picker off. Present the legacy bare token
                            // (`:workspace`→`auto`) so all 12 locales auto-adapt exactly as
                            // they did on the legacy ACP path; a custom profile keeps its
                            // colon (no bare equivalent). SetMode's `normalize_to_profile_id`
                            // is the inverse on the return trip, and `reconcile_codex_mode`
                            // re-normalizes before validating against the colon wire catalog.
                            Some(ModeInfo {
                                id: codex_perm::profile_id_to_legacy_value(profile_id),
                                name,
                                description,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if present && modes.is_empty() {
                tracing::warn!("codex permissionProfile/list parsed to empty (no profile with an id)");
            }
            discovered.lock().unwrap_or_else(|e| e.into_inner()).modes = modes;
        }
        // Checkpoints → CheckpointList event, Rewind → Rewound event: both mapped at
        // the call site, not a cache fill — fill_discovery is never called for them.
        DiscoveryKind::Checkpoints | DiscoveryKind::Rewind => {}
    }
}

/// O2 up-leg: map a `thread/turns/list` response `result` into the
/// `CheckpointList` entries. codex `ThreadTurnsListResponse{data: Vec<Turn>, ...}`
/// (source-verified thread.rs:1204-1214); each `Turn{id, status, completed_at, ..}`
/// (thread_data.rs:152-174). We surface `Turn.id` as the checkpoint id and the
/// turn `status` as the label (the user-facing "which point"); codex turns have no
/// `turn_gen` (that is our adapter-owned epoch), so `turn_gen` is None. First page
/// only — `next_cursor` is not chased (bounds the N2 unbounded-history risk).
fn map_turns_to_checkpoints(result: &Value) -> Vec<crate::event::CheckpointEntry> {
    result
        .get("data")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let id = t.get("id").and_then(Value::as_str)?.to_string();
                    let label = t.get("status").and_then(Value::as_str).map(str::to_string);
                    Some(crate::event::CheckpointEntry {
                        id,
                        label,
                        turn_gen: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// G3 up-leg: derive `Rewound.to_turn` from a `thread/rollback` response. The key
/// path is `result.thread.turns[]` — CONFIRMED against the real wire (live capture
/// `protocols/samples/codex-cli/0.139.0/_all_rollback_plan.jsonl`: the success
/// result is `{thread:{...,turns:[...]}, model, ...}`). `to_turn` = the post-rollback
/// turn count.
///
/// ⚠️ LIVE-OBSERVED CAVEAT (codex 0.139.0): after a valid `numTurns:1` rollback the
/// returned `thread.turns` was an EMPTY array, so this yields `to_turn = 0` even when
/// history survived. `to_turn` is a **consumer/display signal only** — the reducer's
/// `Rewound` arm is a no-op (reducer.rs:388, never reads the value), so an inaccurate
/// count is cosmetic (UI "rewound to N"), not a state/FSM bug. A count-based
/// history-end is unreliable on this codex version; treat `to_turn` as best-effort.
/// The flat `{turns}` fallback is kept for cross-version tolerance (the real wire
/// nests under `thread`). Missing/empty → 0.
fn rollback_to_turn(result: &Value) -> u64 {
    result
        .get("thread")
        .and_then(|t| t.get("turns"))
        .or_else(|| result.get("turns"))
        .and_then(Value::as_array)
        .map(|a| a.len() as u64)
        .unwrap_or(0)
}

/// Remove a resolved/answered approval from the recovery registry. `serverRequest/
/// resolved` carries the RAW wire id, but an elicitation was stored under the
/// `ELICIT_PREFIX`-tagged key — so try the raw id first, then the prefixed form, so
/// a resolved notification clears either entry shape. `dispatch(AnswerPermission)`
/// passes the exact stored key (raw or prefixed), which the first lookup catches.
fn remove_pending_tool_approval(pending: &Arc<std::sync::Mutex<HashMap<String, String>>>, request_id: &str) {
    let mut map = pending.lock().unwrap_or_else(|e| e.into_inner());
    if map.remove(request_id).is_none() {
        map.remove(&format!("{ELICIT_PREFIX}{request_id}"));
    }
}

/// Reverse-RPC handler (A2/A3). The blocking ServerRequest MUST eventually get a
/// JSON-RPC RESPONSE (same `id`) or the channel deadlocks and the turn hangs.
/// THREE classes:
///  - Pure infra (`attestation/generate`): we cannot satisfy it and no human can
///    either → auto-reject with -32601 NOW so the turn never deadlocks.
///  - Mid-session auth refresh (`account/chatgptAuthTokens/refresh`, R6/R15): a
///    human/credential source CAN satisfy it → surface `Permission{Auth}`, stash
///    the wire id in `pending_auth_id`, and let `dispatch(AnswerAuth)` write the
///    keyed response with the supplied tokens. NOT auto-answered.
///  - Tool/file approvals (`*/requestApproval`): a human decides → `Permission`
///    (Tool); `dispatch(AnswerPermission)` writes the keyed accept/decline.
#[allow(clippy::too_many_arguments)]
async fn handle_reverse_rpc(
    method: &str,
    frame: &Value,
    session_id: &str,
    turn_gen: &Arc<AtomicU64>,
    event_tx: &broadcast::Sender<SessionEnvelope>,
    pending_auth_id: &Arc<Mutex<Option<Value>>>,
    pending_tool_approvals: &Arc<std::sync::Mutex<HashMap<String, String>>>,
    stdin: &Arc<Mutex<Option<aionui_process::BoxedStdin>>>,
) {
    let cur = turn_gen.load(Ordering::SeqCst);
    let id = frame.get("id").cloned().unwrap_or(Value::Null);
    match method {
        // Mid-session re-auth (R6/R15): the server hit a 401 mid-turn and is asking
        // the client for fresh ChatGPT tokens. A human/credential source answers
        // this → surface Permission{Auth} (sets waiting_on_auth) and remember the
        // wire id so dispatch(AnswerAuth) can write the keyed response. We do NOT
        // auto-answer: that is the whole point of the mid-session-auth path.
        "account/chatgptAuthTokens/refresh" => {
            *pending_auth_id.lock().await = Some(id.clone());
            emit(
                event_tx,
                session_id,
                cur,
                SessionEvent::Permission {
                    request_id: id.to_string(),
                    kind: crate::event::PermissionKind::Auth,
                    // G3 auto-approval is ACP-only (acp_conn parses MCP context); a
                    // codex auth refresh carries no team-MCP server to allowlist.
                    metadata: None,
                    // AskUserQuestion projection is claude-direct only.
                    tool_name: None,
                    input: None,
                },
            );
        }
        // Pure infra (A2/A3): no human can satisfy attestation either, so reply
        // with a JSON-RPC -32601 NOW to UNBLOCK the channel. If codex genuinely
        // needed it the turn surfaces as a failure (TurnResult.is_error) — strictly
        // better than a deadlock. A diagnostic records the auto-answer.
        "attestation/generate" => {
            write_reverse_error(stdin, &id, -32601, "client cannot supply codex attestation").await;
            emit(
                event_tx,
                session_id,
                cur,
                SessionEvent::AdapterSpecific {
                    tag: "codex_reverse_rpc_auto_answered".into(),
                    payload: json!({ "method": method, "id": id }),
                },
            );
        }
        // Command/file approval requests → user-facing Permission (Tool). These two
        // take a `{decision: accept|decline}` response (CommandExecution/FileChange
        // RequestApprovalResponse, schema-verified), which is EXACTLY what
        // dispatch(AnswerPermission) writes. The wire `id` is the request_id the
        // conversation answers; a human decides (NOT auto-answered here).
        //
        // ⚠️ We deliberately do NOT surface `item/permissions/requestApproval` here
        // (M2): its response is `{permissions: GrantedPermissionProfile, scope}`,
        // NOT `{decision}` — answering it with our generic decision body would be
        // rejected. Until a permission-grant command exists, it falls through to the
        // clean -32601 reject below (unblocks the channel; the escalation just
        // can't be granted — strictly better than a malformed answer or a deadlock).
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            let request_id = id.to_string();
            // Register for REST recovery (`GET /confirmations`): a tool/file approval
            // raised before the client subscribed (or after a page reload) must be
            // rebuildable, else the turn hangs waiting for an answer that can never be
            // given. Keyed by the SAME request_id we surface, so a duplicate live+
            // recovered pair de-dups; the value is a safe title (the approval class,
            // NOT the command body — TIO-13). Cleared on serverRequest/resolved or
            // dispatch(AnswerPermission).
            let title = if method == "item/fileChange/requestApproval" {
                "FileChange"
            } else {
                "CommandExecution"
            };
            pending_tool_approvals
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(request_id.clone(), title.to_string());
            emit(
                event_tx,
                session_id,
                cur,
                SessionEvent::Permission {
                    request_id,
                    kind: crate::event::PermissionKind::Tool,
                    // G3 auto-approval is ACP-only (acp_conn). The native codex
                    // app-server command/file approval is not a team-MCP path.
                    metadata: None,
                    // The approval class as the card title, and the request `params`
                    // as an UNPARSED passthrough so the card can show the approver
                    // what they are approving (AionUi issue #3779 — approving blind).
                    // No shape is asserted: the frontend reads `command` when present
                    // and falls back to the title otherwise, so an unprobed params
                    // shape degrades to the previous title-only card.
                    tool_name: Some(title.to_string()),
                    input: frame.get("params").cloned(),
                },
            );
        }
        // MCP elicitation (LIVE-confirmed 0.139.0, missing-wire-probe): codex bridges
        // both an MCP tool-call APPROVAL and a real MCP server form `elicitation/create`
        // to this ONE reverse-RPC, distinguished by `mode` + `_meta.codex_approval_kind`
        // + whether `requestedSchema.properties` is empty. BOTH take a
        // `{action: "accept"|"decline", content: {...}}` response (NOT `{decision}`),
        // so we tag the surfaced request_id with the `ELICIT_PREFIX` and
        // dispatch(AnswerPermission) writes the right body shape. A human decides
        // (NOT auto-answered) → Permission{Tool} (waiting_on_approval). The reducer
        // ref-counts on `kind` only (never the request_id string), so the prefix is
        // a transparent dispatch-side discriminator. `serverRequest/resolved` already
        // emits the matching PermissionResolved{Tool} on answer.
        "mcpServer/elicitation/request" => {
            let request_id = format!("{ELICIT_PREFIX}{id}");
            // Carry the elicitation context so the conversation can render a form /
            // approval prompt: the message, the requested schema, and the mode.
            let input = json!({
                "message": frame.get("params").and_then(|p| p.get("message")),
                "requestedSchema": frame.get("params").and_then(|p| p.get("requestedSchema")),
                "mode": frame.get("params").and_then(|p| p.get("mode")),
                "serverName": frame.get("params").and_then(|p| p.get("serverName")),
            });
            // Register for REST recovery, keyed by the ELICIT_PREFIX-tagged request_id
            // (the same id we surface + dispatch(AnswerPermission) answers). Title is
            // the safe approval class, not the elicitation message body (TIO-13).
            pending_tool_approvals
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(request_id.clone(), "Elicitation".to_string());
            emit(
                event_tx,
                session_id,
                cur,
                SessionEvent::Permission {
                    request_id,
                    kind: crate::event::PermissionKind::Tool,
                    metadata: None,
                    tool_name: None,
                    input: Some(input),
                },
            );
        }
        // Any other reverse-RPC → unblock with a -32601 error (never let an unknown
        // blocking request hang the turn) + opaque diagnostic. This matches the
        // ACP-audit finding: an unhandled reverse method should be clean-rejected,
        // not silently dropped (which deadlocks a blocking request).
        //
        // KNOWN deferred case: `item/tool/requestUserInput` (codex's native
        // ask-the-user tool, the AskUserQuestion analog) currently falls here and is
        // rejected, so the user never sees the question and the tool gets the empty
        // fallback. Wiring it (surface Permission{questions} + answer
        // {answers:{<id>:{answers}}} via ELICIT_PREFIX, claude AskUserQuestion is the
        // template) is a reachable FOLLOW-UP — gap-reaudit confirmed the schema is fully
        // defined. NOT wired yet because we could not capture a LIVE requestUserInput
        // frame: it is mode-gated (`available_modes`, spec_plan.rs:729) and codex
        // refused to invoke it in default OR plan mode in this env ("unavailable in the
        // current mode"). Per the no-parser-for-an-unprobed-shape discipline, deferred
        // until the trigger/mode is found and a real frame is captured.
        _ => {
            write_reverse_error(stdin, &id, -32601, "method not handled by aionui-session").await;
            emit(
                event_tx,
                session_id,
                cur,
                SessionEvent::AdapterSpecific {
                    tag: "codex_reverse_rpc".into(),
                    payload: json!({ "method": method, "id": id }),
                },
            );
        }
    }
}

/// Write a JSON-RPC ERROR response (`{jsonrpc, id, error{code,message}}`) to the
/// shared stdin. Used by the reader to auto-reject blocking infra reverse-RPCs
/// (A2/A3) so the JSON-RPC channel never deadlocks. Best-effort: a closed stdin
/// means the process is gone and the turn is ending anyway.
async fn write_reverse_error(
    stdin: &Arc<Mutex<Option<aionui_process::BoxedStdin>>>,
    id: &Value,
    code: i64,
    message: &str,
) {
    if id.is_null() {
        return; // a notification, not a request — nothing to answer
    }
    let frame = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    let mut guard = stdin.lock().await;
    let Some(w) = guard.as_mut() else { return };
    use tokio::io::AsyncWriteExt;
    if let Ok(mut line) = serde_json::to_vec(&frame) {
        line.push(b'\n');
        let _ = w.write_all(&line).await;
        let _ = w.flush().await;
    }
}

/// Map a codex server notification → canonical SessionEvent(s). The A1 fix lives
/// here: `item` payloads are matched on the `type` STRING (never deserialized
/// into the closed 16-variant ThreadItem enum), so an unknown future variant is
/// data (→ AdapterSpecific), not a panic.
fn map_notification(method: &str, params: &Value) -> Vec<SessionEvent> {
    match method {
        "turn/started" => vec![], // optimistic; the orchestrator already lowered TurnStarted
        "item/agentMessage/delta" => {
            let item_id = params.get("itemId").and_then(Value::as_str).unwrap_or("").to_string();
            let text = params.get("delta").and_then(Value::as_str).unwrap_or("").to_string();
            vec![SessionEvent::MessageDelta { item_id, text }]
        }
        "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
            let item_id = params.get("itemId").and_then(Value::as_str).unwrap_or("").to_string();
            let text = params.get("delta").and_then(Value::as_str).unwrap_or("").to_string();
            vec![SessionEvent::ThoughtDelta { item_id, text }]
        }
        "item/started" | "item/completed" => map_item(params, method == "item/completed"),
        // Live tool-output stream (codex `item/commandExecution/outputDelta`): the
        // incremental stdout of a RUNNING command, keyed by the owning tool's itemId.
        // PLAINTEXT (NOT base64 — the turn-scoped item stream; verified live 0.139.0).
        // → ToolOutputDelta (display liveness; the full output still rides the
        // completed item's aggregatedOutput → ToolResult).
        "item/commandExecution/outputDelta" => {
            let item_id = params.get("itemId").and_then(Value::as_str).unwrap_or("").to_string();
            let text = params.get("delta").and_then(Value::as_str).unwrap_or("").to_string();
            vec![SessionEvent::ToolOutputDelta { item_id, text }]
        }
        // Live cumulative turn diff (codex `turn/diff/updated`): the FULL git-style
        // unified diff of all file edits in the turn so far (full-replace snapshot,
        // re-sent as edits land; verified live 0.139.0). → TurnDiffUpdated (display
        // liveness; the per-file authoritative diff rides the completed fileChange
        // item → ToolResult FilePath).
        "turn/diff/updated" => {
            let diff = params.get("diff").and_then(Value::as_str).unwrap_or("").to_string();
            vec![SessionEvent::TurnDiffUpdated { diff }]
        }
        "thread/tokenUsage/updated" => map_usage(params),
        // LC-8a: codex to-do plan snapshot. `TurnPlanUpdatedNotification{plan:[{step,
        // status}], explanation?}` (schema-verified, codex 0.137.0) → SessionEvent::Plan.
        // step→content; camelCase `inProgress`→InProgress; codex has no per-step priority.
        "turn/plan/updated" => map_plan(params),
        // R8 dual-terminal reconcile: codex signals turn end via BOTH
        // `turn/completed` (carries the rich Turn{status,error}) AND
        // `thread/status/changed → idle`. They can arrive in EITHER order (or one
        // may be missing). We must produce EXACTLY ONE TurnResult per turn. The
        // reconcile lives in the reader loop via a single per-turn `terminated`
        // flag (NOT turnId dedup — codex does not guarantee matching turnIds across
        // the two signals); map_notification just classifies — see reconcile_terminal.
        "turn/completed" | "thread/status/changed" => {
            // handled by reconcile_terminal in the caller (needs per-turn state)
            vec![]
        }
        // thread/settings/updated → ConfigChanged (frozen C6 §6): the
        // non-optimistic confirmation that a SetMode/SetModel applied. The
        // conversation updates its mode/model selector on THIS, not on the
        // sent-assume-done dispatch return.
        //
        // feature 012: for codex the mode axis IS the permission axis. The confirmation
        // carries `threadSettings.activePermissionProfile.id` = the colon-prefixed profile
        // id (LIVE 0.139.0: `:workspace`/`:read-only`/`:danger-full-access`, plus any
        // custom `[permissions.<id>]`), which we surface VERBATIM as the current mode —
        // the SAME colon id `permissionProfile/list` advertised, so the selector matches a
        // discovered entry (legacy-ACP parity: `current_mode_update.currentModeId` was the
        // advertised id, untranslated). `activePermissionProfile` is `null` when the tier
        // was set via the raw sandboxPolicy channel (not our path) — then we carry no mode
        // (leaving the last-known selection). We deliberately do NOT read
        // `collaborationMode.mode` (plan/default): codex has no separately-exposed
        // collaboration selector in AionUi, and that field would clobber the permission mode.
        "thread/settings/updated" => {
            let settings = params
                .get("threadSettings")
                .or_else(|| params.get("thread_settings"))
                .unwrap_or(&Value::Null);
            let model = settings.get("model").and_then(Value::as_str).map(str::to_string);
            let mode = settings
                .get("activePermissionProfile")
                .and_then(|p| p.get("id"))
                .and_then(Value::as_str)
                // Map the colon wire id back to the legacy bare token the catalog/frontend
                // uses (`:workspace`→`auto`), so the picker highlights the matching entry;
                // a custom profile keeps its colon (matches its verbatim catalog entry).
                .map(codex_perm::profile_id_to_legacy_value);
            vec![SessionEvent::ConfigChanged { mode, model }]
        }
        // (No `item/userMessage/delta` arm: codex never emits that method — the user
        // echo arrives as an `item/*` with item.type=="userMessage", handled by the
        // item path. A dead arm here was a guessed method, removed per the protocol
        // audit; an unknown method now falls through to the AdapterSpecific catch-all.)
        // MCP server startup → Provisioning, mapped per status (parity with claude's
        // sniff_init mcp_servers[] → ToolsReady/LoadFailed/Degraded). The wire method
        // is `mcpServer/startupStatus/updated` (LIVE-confirmed 0.139.0: starting→failed
        // observed) with status ∈ {starting,ready,failed,cancelled}; `error` carries
        // the failure reason. Previously this arm matched the WRONG prefix
        // `mcpServerStatus` (that is only the OUTBOUND `mcpServerStatus/list` request
        // we send — never an inbound notification) and hardcoded ToolsWaiting, so a
        // real startup notification fell through to AdapterSpecific and produced NO
        // Provisioning, and a failed/cancelled server could never surface as
        // Degraded/LoadFailed. Fixed to the real method + a 4-way status map.
        "mcpServer/startupStatus/updated" => {
            let reason = params
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_default();
            let phase = match params.get("status").and_then(Value::as_str).unwrap_or("") {
                "starting" => ProvisioningPhase::ToolsWaiting,
                "ready" => ProvisioningPhase::ToolsReady,
                "failed" => ProvisioningPhase::LoadFailed { reason },
                "cancelled" => ProvisioningPhase::Degraded { reason },
                // Unknown future status → conservative non-terminal (never panic).
                _ => ProvisioningPhase::ToolsWaiting,
            };
            vec![SessionEvent::Provisioning { phase }]
        }
        // MCP OAuth completion: success=false means the server is up but unauthorized
        // → Degraded (mirrors claude's needs-auth → Degraded). success=true carries no
        // FSM signal here (a subsequent startupStatus→ready covers readiness).
        "mcpServer/oauthLogin/completed" => {
            if params.get("success").and_then(Value::as_bool) == Some(false) {
                let reason = params
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_default();
                vec![SessionEvent::Provisioning {
                    phase: ProvisioningPhase::Degraded { reason },
                }]
            } else {
                vec![]
            }
        }
        // codex `error` notification: `{error: TurnError, threadId, turnId, willRetry}`
        // (schema-verified 0.137.0). willRetry=true = a TRANSIENT retry (codex hit an
        // error mid-turn and is retrying → a liveness signal, mirrors claude's
        // system/api_retry → Heartbeat). willRetry=false (FATAL) is handled in the
        // reader loop BEFORE this fn (synthesizes an is_error terminal so the turn
        // ends even if codex never sends turn/completed) — it never reaches this arm.
        // The `vec![]` is a defensive fallthrough only.
        "error" => {
            if params.get("willRetry").and_then(Value::as_bool) == Some(true) {
                // A retry keeps the turn alive (Heartbeat), but staying silent
                // made an upstream stall indistinguishable from a hung app:
                // codex says "Reconnecting... 1/5" with a reason, and the user
                // saw only a spinner that never resolved (live 0.147.0, whose
                // response stream disconnected repeatedly). Surface what codex
                // said so a stall is explained while it is happening.
                let mut out = vec![SessionEvent::Heartbeat];
                if let Some(message) = retry_notice_message(params) {
                    // One card for the whole stall: attempt 2/5 replaces 1/5 in
                    // place, so the user watches it count up instead of
                    // collecting five near-identical cards (codex emits each
                    // attempt more than once, so appending produced ten).
                    // Deliberately NOT keyed on codex's own `turnId`: a single
                    // prompt can make codex retry in several rounds, each with a
                    // fresh turnId, and that showed up as two cards both reading
                    // "5/5". The reader scopes this key to OUR turn instead.
                    // The card is localizable: the CLI's own line rides as a
                    // parameter, and the wrapper adds the running totals the
                    // relay fills in (how many attempts this prompt has cost,
                    // how long it has been waiting) — codex restarts its own
                    // "1/5" counter every round, so its text alone understates
                    // a long stall.
                    let mut localized = crate::event::LocalizedText::new(CODE_CODEX_RETRYING);
                    localized.params.insert("detail".into(), Value::String(message.clone()));
                    out.push(SessionEvent::Notice {
                        level: crate::event::NoticeLevel::Warning,
                        message,
                        localized: Some(localized),
                        supersedes_key: Some(RETRY_NOTICE_KEY.to_string()),
                    });
                }
                out
            } else {
                vec![]
            }
        }
        // Out-of-turn advisories → Notice (was dropped to AdapterSpecific, never seen).
        // warning/guardianWarning carry `{message}`; deprecationNotice/configWarning
        // carry `{summary, details?}`. guardian/warning/config = Warning; deprecation =
        // Info (advisory, non-urgent). Shapes schema-verified (ServerNotification.json).
        "warning" | "guardianWarning" => {
            let message = params.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            vec![SessionEvent::Notice {
                level: crate::event::NoticeLevel::Warning,
                message,
                localized: None,
                supersedes_key: None,
            }]
        }
        "configWarning" => {
            let message = notice_message(params);
            vec![SessionEvent::Notice {
                level: crate::event::NoticeLevel::Warning,
                message,
                localized: None,
                supersedes_key: None,
            }]
        }
        "deprecationNotice" => {
            let message = notice_message(params);
            vec![SessionEvent::Notice {
                level: crate::event::NoticeLevel::Info,
                message,
                localized: None,
                supersedes_key: None,
            }]
        }
        // `hook/*` provisioning is a separate concern (not MCP startup) — kept as a
        // coarse ToolsWaiting signal pending its own wire investigation.
        m if m.starts_with("hook/") => {
            vec![SessionEvent::Provisioning {
                phase: ProvisioningPhase::ToolsWaiting,
            }]
        }
        // Unknown notification → opaque (never panic, never drop silently).
        _ => vec![SessionEvent::AdapterSpecific {
            tag: format!("codex_notif:{method}"),
            payload: params.clone(),
        }],
    }
}

/// Build a [`SessionEvent::Notice`] message from a codex `deprecationNotice` /
/// `configWarning` params object: `{summary, details?, ...}`. Joins summary +
/// details (when present) so the user sees the actionable guidance, not just the
/// headline. Falls back to `message` then empty string.
/// The user-facing text for a RETRYING codex error, or `None` when the frame
/// carries nothing worth showing.
///
/// `error.message` is the headline ("Reconnecting... 1/5"); `additionalDetails`
/// carries the cause ("We're currently experiencing high demand"), which is the
/// part that tells the user this is not their fault and not a hung app.
/// Merge key for the retry card, scoped to our turn by [`scope_notice_to_turn`]
/// before it leaves the reader.
const RETRY_NOTICE_KEY: &str = "codex-retry";

/// i18n code for the retry card. Its body wraps the CLI's own line with the
/// running totals the stream relay fills in.
const CODE_CODEX_RETRYING: &str = "CODEX_RETRYING";

/// Qualify a superseding notice with the turn generation that produced it.
///
/// The key decides which card a notice replaces, so its scope decides what the
/// user sees. Session-wide would let a stall in turn 7 rewrite a card sitting
/// next to turn 1; codex's own `turnId` is too narrow — one prompt can make
/// codex retry in several rounds, and keying on its id showed two cards both
/// reading "5/5", which looks like a duplicate. Our turn generation is the
/// scope that matches what the user did: one prompt, one card.
fn scope_notice_to_turn(event: SessionEvent, turn_gen: u64) -> SessionEvent {
    match event {
        SessionEvent::Notice {
            level,
            message,
            localized,
            supersedes_key: Some(key),
        } => SessionEvent::Notice {
            level,
            message,
            localized,
            supersedes_key: Some(format!("{key}:turn-{turn_gen}")),
        },
        other => other,
    }
}

fn retry_notice_message(params: &Value) -> Option<String> {
    let error = params.get("error")?;
    let headline = error.get("message").and_then(Value::as_str).unwrap_or("").trim();
    let details = error
        .get("additionalDetails")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    match (headline.is_empty(), details.is_empty()) {
        (true, true) => None,
        (false, false) => Some(format!("{headline} — {details}")),
        (false, true) => Some(headline.to_owned()),
        (true, false) => Some(details.to_owned()),
    }
}

fn notice_message(params: &Value) -> String {
    let summary = params
        .get("summary")
        .or_else(|| params.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match params.get("details").and_then(Value::as_str) {
        Some(d) if !d.is_empty() => format!("{summary} — {d}"),
        _ => summary.to_string(),
    }
}

/// Map codex's 7-state `CollabAgentStatus` (schema-full/ServerNotification.json
/// `CollabAgentStatus`) onto our 6-state [`SubagentStatus`]. `notFound` has no
/// roster meaning of its own (the agent is gone) → fold to `Shutdown` so the
/// entry settles terminal and prunes at the boundary (we never invent a 7th
/// state, §9.12 "codex 7 minus NotFound"). Unknown future strings → `Running`
/// (active, non-terminal) so a new codex status never wedges as terminal.
fn map_collab_status(s: &str) -> SubagentStatus {
    match s {
        "pendingInit" => SubagentStatus::PendingInit,
        "running" => SubagentStatus::Running,
        "interrupted" => SubagentStatus::Interrupted,
        "completed" => SubagentStatus::Completed,
        "errored" => SubagentStatus::Errored,
        "shutdown" | "notFound" => SubagentStatus::Shutdown,
        _ => SubagentStatus::Running,
    }
}

/// A1 CORE: match the item's `type` STRING with a fallthrough. The closed codex
/// ThreadItem enum is NEVER constructed in our code, so an unknown future `type`
/// becomes `AdapterSpecific` instead of a deserialization panic.
fn map_item(params: &Value, completed: bool) -> Vec<SessionEvent> {
    let item = params.get("item").unwrap_or(&Value::Null);
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    let id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let mut out: Vec<SessionEvent> = Vec::new();

    // GAP-E (C5.3 frozen item-brackets, §9.2): emit the ADDITIVE partial-lifecycle
    // bracket (Tier-0; reducer no-op) around the content event(s), for the
    // bracketable item types the (P2) TurnFinalizer needs. We do NOT bracket the
    // server's userMessage echo (not a model item) nor the collabAgent item (a
    // subagent lifecycle carried by SubagentUpdate, not a message/tool item).
    let bracketed = matches!(
        item_type,
        "agentMessage"
            | "reasoning"
            | "commandExecution"
            | "mcpToolCall"
            | "dynamicToolCall"
            | "fileChange"
            | "webSearch"
            | "imageGeneration"
    );
    if bracketed && !completed {
        out.push(SessionEvent::ItemStarted {
            item_id: id.clone(),
            kind: item_kind_for(item_type),
        });
    }

    match item_type {
        "agentMessage" => {
            // A STREAMED agentMessage starts with `text:""` and arrives via
            // item/agentMessage/delta — for it the bracket is the whole signal.
            // But a PRE-FILLED agentMessage exists: the `review/start` verdict
            // (item id `review_rollout_assistant`) carries its full `text` on
            // item/started and NO deltas ever follow (live-verified:
            // samples/codex-cli/0.144.1/review_start_uncommitted.jsonl). Emit the
            // STARTED edge's initial text as a MessageDelta so a delta-less
            // message is not silently dropped; started.text + subsequent deltas
            // compose the same final text under both shapes. Only the started
            // edge — the completed frame repeats the same text (double-emit).
            if !completed
                && let Some(text) = item.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                out.push(SessionEvent::MessageDelta {
                    item_id: id.clone(),
                    text: text.to_string(),
                });
            }
        }
        "commandExecution" | "mcpToolCall" | "dynamicToolCall" | "fileChange" | "webSearch" | "imageGeneration" => {
            if completed {
                // 009 R7/H3: a codex tool is failed when status==failed OR a command
                // exited non-zero — carry it so a failed tool is not shown as success.
                let is_error = item.get("status").and_then(Value::as_str) == Some("failed")
                    || item.get("exitCode").and_then(Value::as_i64).is_some_and(|c| c != 0);
                // 009 R8: carry the tool OUTPUT. `aggregatedOutput` (command stdout)
                // is fixture-confirmed (commandExecution completed). codex writes any
                // generated file to disk (never inlines bytes); a `fileChange` item
                // carries the produced-file PATHs + per-file unified diff in
                // `changes[]` (LIVE-confirmed 0.139.0, missing-wire-probe: each entry
                // is `{path, kind:{type:"update"|...}, diff}`). We map each change to a
                // `FilePath` so the conversation TurnFinalizer renders a FileDiff card
                // (turn_finalizer `tool_result_display`) instead of dropping the path.
                let mut content = Vec::new();
                if item_type == "fileChange"
                    && let Some(changes) = item.get("changes").and_then(Value::as_array)
                {
                    for ch in changes {
                        let Some(path) = ch.get("path").and_then(Value::as_str) else {
                            continue;
                        };
                        // codex `kind.type` ∈ {add, update, delete, ...}; the unified
                        // hunk (when present) rides `diff`. We carry it as `new_text`
                        // so the FileDiff card has a body; `old_text` stays None
                        // (codex sends a single combined hunk, not before/after pair).
                        let diff = ch.get("diff").and_then(Value::as_str).map(str::to_string);
                        content.push(crate::event::ToolResultContent::FilePath {
                            path: path.to_string(),
                            mime: None,
                            old_text: None,
                            new_text: diff,
                        });
                    }
                }
                // imageGeneration writes the produced image to disk and reports its
                // path in `savedPath` (source-verified: v2/item.rs:372-380
                // ImageGeneration{result:String(base64), saved_path:Option<AbsolutePathBuf>}
                // → wire key `savedPath`). Previously DROPPED — we only read
                // aggregatedOutput (a commandExecution-only field imageGeneration lacks),
                // so the image card was empty. Carry the path as FilePath (NOT the
                // base64 `result` as Text — that would dump megabytes of bytes).
                if item_type == "imageGeneration"
                    && let Some(path) = item.get("savedPath").and_then(Value::as_str)
                {
                    content.push(crate::event::ToolResultContent::FilePath {
                        path: path.to_string(),
                        mime: None,
                        old_text: None,
                        new_text: None,
                    });
                }
                if let Some(text) = item.get("aggregatedOutput").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    content.push(crate::event::ToolResultContent::Text(text.to_string()));
                }
                // MCP + dynamic-tool OUTPUT (previously DROPPED — only aggregatedOutput,
                // a commandExecution-only field, was read, so every MCP/dynamic tool
                // rendered as an empty card = silent data loss). Source-verified shapes
                // (openai/codex app-server-protocol v2/item.rs:299/313, mcp.rs:125):
                //   mcpToolCall completed → result:{content:[mcp Content blocks],
                //     structuredContent?, _meta?} + error:{message} on failure.
                //   dynamicToolCall completed → contentItems:[{type:inputText,text}|
                //     {type:inputImage,imageUrl}].
                content.extend(parse_codex_mcp_result(item.get("result")));
                content.extend(parse_codex_dynamic_content_items(item.get("contentItems")));
                if let Some(msg) = item
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    // a failed mcpToolCall carries its cause in error.message (no
                    // aggregatedOutput) — surface it so the red card has a reason.
                    content.push(crate::event::ToolResultContent::Text(msg.to_string()));
                }
                out.push(SessionEvent::ToolResult {
                    tool_use_id: id.clone(),
                    is_error,
                    content,
                    // 009 H5: codex inline tool item — main agent (collab-agent
                    // subagents arrive on the separate collabAgentToolCall plane).
                    parent_tool_use_id: None,
                });
            } else {
                out.push(SessionEvent::ToolCall {
                    tool_use_id: id.clone(),
                    name: item_type.to_string(),
                    subagent: crate::event::SubagentKind::Inline,
                    // Gap #4 / H2: carry the codex tool ARGUMENTS. On the started
                    // (non-completed) item the invocation fields (command/cwd/
                    // commandActions for commandExecution; arguments for mcp/dynamic
                    // tool calls) are present while the output fields are still null,
                    // so the whole item Value is the faithful argument carrier.
                    // TIO-13: never logged at info.
                    input: item.clone(),
                    // 009 H5: codex inline tool — main agent.
                    parent_tool_use_id: None,
                });
            }
        }
        "collabAgentToolCall" => {
            // §6b b1: a collab/spawned subagent → SubagentUpdate, keyed by the CHILD
            // thread id (codex `agentId`, state.rs:80). codex carries the live child
            // roster in `agentsStates: { threadId -> { status, message } }` and the
            // spawning parent in `senderThreadId`. Emit ONE update per known child so
            // the roster reflects every collab agent with its REAL lifecycle status
            // (the 7-state `CollabAgentStatus`, mapped to our 6-state SubagentStatus)
            // — not a coarse completed-bool, and with the spawn edge (`parent_ref`)
            // so multi-level collab renders (reducer.rs:382-392).
            let parent_ref = item
                .get("senderThreadId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            // `model` is the agent's model id; empty on the spawn-in-flight frame.
            let label = item
                .get("model")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .map(str::to_string);
            match item.get("agentsStates").and_then(Value::as_object) {
                Some(states) if !states.is_empty() => {
                    for (thread_id, st) in states {
                        let status = st
                            .get("status")
                            .and_then(Value::as_str)
                            .map(map_collab_status)
                            // agentsStates entry without a status → fall back to the
                            // bool so it is never stuck mid-lifecycle.
                            .unwrap_or(if completed {
                                SubagentStatus::Completed
                            } else {
                                SubagentStatus::Running
                            });
                        out.push(SessionEvent::SubagentUpdate {
                            r#ref: thread_id.clone(),
                            label: label.clone(),
                            status,
                            parent_ref: parent_ref.clone(),
                            // codex collab threads declare no container kind, and no
                            // sample proves a codex turn emits a post-completion
                            // terminal result — so they must not hold a turn open.
                            kind: None,
                        });
                    }
                }
                // No child thread reported yet (the spawn-in-flight frame carries an
                // empty `agentsStates`): fall back to the tool-call id so the action
                // still surfaces, with a bool-derived status. The spawn tool's own
                // completion is terminal, so this fallback entry settles + prunes at
                // the turn boundary; the next frame carries the real child threadId.
                _ => {
                    out.push(SessionEvent::SubagentUpdate {
                        r#ref: id.clone(),
                        label,
                        status: if completed {
                            SubagentStatus::Completed
                        } else {
                            SubagentStatus::Running
                        },
                        parent_ref,
                        // Same as the agentsStates branch: no declared kind.
                        kind: None,
                    });
                }
            }
        }
        "reasoning" => {} // streamed via reasoning/*Delta; bracket is the signal
        // The server's echo of the user's own input — NOT a model output. Drop so
        // the conversation doesn't duplicate the prompt it already rendered.
        "userMessage" => {}
        // ⭐ A1: ANY unknown item type (incl. a future codex variant) → opaque,
        // NEVER a panic. This is the freeze-blocker fix made structural.
        other => out.push(SessionEvent::AdapterSpecific {
            tag: format!("codex_item:{other}"),
            payload: item.clone(),
        }),
    }

    if bracketed && completed {
        out.push(SessionEvent::ItemCompleted {
            item_id: id,
            truncation: None,
        });
    }
    out
}

/// Map a codex item `type` → the canonical `ItemKind` for the partial-lifecycle
/// bracket (GAP-E).
fn item_kind_for(item_type: &str) -> crate::event::ItemKind {
    use crate::event::ItemKind;
    match item_type {
        "agentMessage" => ItemKind::Text,
        "reasoning" => ItemKind::Thinking,
        "imageGeneration" => ItemKind::Image,
        _ => ItemKind::Tool,
    }
}

/// thread/tokenUsage/updated → UsageDelta. codex gives BOTH total (cumulative)
/// and last (per-turn); we use `.last` directly (G6: native per-turn, no
/// subtraction, no double-count on reconnect — measured 2026-06-10).
///
/// `.last` is also the right figure for the usage indicator's CURRENT CONTEXT
/// OCCUPANCY, which is why it feeds `used`. Live two-turn probe (0.145.0,
/// window 258400): turn 2 reported `last{inputTokens:11024, cachedInputTokens:
/// 11006, outputTokens:6, totalTokens:11030}` while `total.totalTokens` had
/// already reached 22043. `last.inputTokens` INCLUDES the cached part, so
/// `last.totalTokens` == the request carrying the whole history + its reply —
/// the occupancy. `total.*` accumulates across turns and would exceed the window
/// on a long session; it must never feed `used`.
///
/// `modelContextWindow` is the indicator's denominator and is nullable in the
/// schema (verified: `codex app-server generate-json-schema` → ThreadTokenUsage).
fn map_usage(params: &Value) -> Vec<SessionEvent> {
    let usage = params.get("tokenUsage").unwrap_or(&Value::Null);
    let last = usage.get("last").unwrap_or(&Value::Null);
    let g = |k: &str| last.get(k).and_then(Value::as_u64).unwrap_or(0);
    vec![SessionEvent::UsageDelta {
        input_tokens: g("inputTokens"),
        output_tokens: g("outputTokens"),
        total_tokens: g("totalTokens"),
        cost_usd: None,
        // DELIBERATELY NOT REPORTED, despite `tokenUsage.modelContextWindow` being
        // the only context-window field in the whole codex protocol.
        //
        // Live-probed on 0.145.0 through a custom `model_provider`: `gpt-5.6-sol`,
        // `gpt-5.5` and `gpt-5.6-luna` ALL report 258_400, and an explicit
        // `model_context_window` (in `config.toml` AND via `-c`) is ignored. A
        // figure that never varies by model and cannot be configured says nothing
        // about the model in use, and a wrong denominator is worse than none: the
        // renderer has a designed "context window size unknown" state that shows a
        // plain token count instead of inventing a percentage.
        //
        // The likely explanation is that codex falls back to a stand-in for models
        // it does not recognise, i.e. any custom provider — an official/Bedrock
        // setup may well report a real window. That is UNVERIFIED: this machine has
        // no official codex auth (no `~/.codex/auth.json`) to test against. Suppress
        // unconditionally rather than encode a guess; revisit with a capture from a
        // first-party provider, and gate on that evidence rather than on a
        // hardcoded constant.
        context_window: None,
        // Per-turn detail line. codex reports every field natively on `last`
        // (verified: TokenUsageBreakdown in the generated schema), including the
        // reasoning tokens claude only exposes per-call.
        breakdown: crate::event::UsageBreakdown {
            cached_read_tokens: g("cachedInputTokens"),
            cached_write_tokens: g("cacheWriteInputTokens"),
            thought_tokens: g("reasoningOutputTokens"),
        },
    }]
}

#[cfg(test)]
mod usage_window_tests {
    use super::*;

    /// Verbatim `tokenUsage` from a live two-turn probe (codex-cli 0.145.0,
    /// `gpt-5.6-sol`): turn 2, window 258400. `used` must stay on `last`
    /// (11030 = the request carrying the whole history + its reply), NOT on the
    /// cumulative `total` (22043) which would blow past any window on a long
    /// session. The reported `modelContextWindow` is NOT forwarded — see
    /// `map_usage` for why codex's only window figure is unusable.
    #[test]
    fn live_token_usage_maps_last_as_occupancy_and_drops_the_window() {
        let params: Value = serde_json::from_str(
            r#"{"threadId":"th1","turnId":"t1","tokenUsage":{
                 "modelContextWindow":258400,
                 "last":{"totalTokens":11030,"inputTokens":11024,"cachedInputTokens":11006,
                         "cacheWriteInputTokens":16,"outputTokens":6,"reasoningOutputTokens":0},
                 "total":{"totalTokens":22043,"inputTokens":22032,"cachedInputTokens":11006,
                          "cacheWriteInputTokens":11022,"outputTokens":11,"reasoningOutputTokens":0}}}"#,
        )
        .unwrap();
        let events = map_usage(&params);
        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::UsageDelta {
                    input_tokens: 11024,
                    output_tokens: 6,
                    total_tokens: 11030,
                    // The frame carried `modelContextWindow: 258400`; it is
                    // deliberately not forwarded — see `map_usage`.
                    context_window: None,
                    cost_usd: None,
                    ..
                }]
            ),
            "expected last-based occupancy and no window, got {events:?}"
        );
    }

    /// codex reports the whole detail line natively on `last`, including the
    /// reasoning tokens claude only exposes per-call. Values from the live two-turn
    /// probe (turn 2).
    #[test]
    fn breakdown_comes_straight_off_last() {
        let params: Value = serde_json::from_str(
            r#"{"tokenUsage":{"modelContextWindow":258400,
                 "last":{"totalTokens":11030,"inputTokens":11024,"cachedInputTokens":11006,
                         "cacheWriteInputTokens":16,"outputTokens":6,"reasoningOutputTokens":242}}}"#,
        )
        .unwrap();
        let b = match map_usage(&params).into_iter().next() {
            Some(SessionEvent::UsageDelta { breakdown, .. }) => breakdown,
            other => panic!("expected UsageDelta, got {other:?}"),
        };
        assert_eq!(b.cached_read_tokens, 11_006);
        assert_eq!(b.cached_write_tokens, 16);
        assert_eq!(b.thought_tokens, 242, "reasoningOutputTokens is the thinking count");
    }

    /// `modelContextWindow` is `int|null` in the schema — a null (or absent) field
    /// must degrade to `None`, never to 0, which would render a 0-sized bar.
    #[test]
    fn null_or_absent_context_window_is_none() {
        for raw in [
            r#"{"tokenUsage":{"modelContextWindow":null,"last":{"totalTokens":5,"inputTokens":4,"outputTokens":1}}}"#,
            r#"{"tokenUsage":{"last":{"totalTokens":5,"inputTokens":4,"outputTokens":1}}}"#,
        ] {
            let params: Value = serde_json::from_str(raw).unwrap();
            assert!(
                matches!(
                    map_usage(&params).as_slice(),
                    [SessionEvent::UsageDelta {
                        context_window: None,
                        total_tokens: 5,
                        ..
                    }]
                ),
                "expected no window for {raw}"
            );
        }
    }
}

/// LC-8a: codex `turn/plan/updated` params → `SessionEvent::Plan`. `plan` is an
/// array of `{step, status}` (TurnPlanStep); codex carries no per-step priority, so
/// `priority: None`. `explanation` is codex-only (Option). Anti-panic: filter_map
/// over the array, never deserialize a closed enum (A1 doctrine).
fn map_plan(params: &Value) -> Vec<SessionEvent> {
    let entries: Vec<crate::event::PlanEntry> = params
        .get("plan")
        .and_then(Value::as_array)
        .map(|steps| {
            steps
                .iter()
                .filter_map(|s| {
                    let content = s.get("step").and_then(Value::as_str)?.to_string();
                    let status = map_plan_status(s.get("status").and_then(Value::as_str).unwrap_or(""));
                    Some(crate::event::PlanEntry {
                        content,
                        status,
                        priority: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let explanation = params.get("explanation").and_then(Value::as_str).map(str::to_string);
    vec![SessionEvent::Plan { entries, explanation }]
}

/// Normalize a plan-step status string → canonical `PlanStatus` (I8). Accepts BOTH
/// codex camelCase (`inProgress`) and ACP snake_case (`in_progress`); unknown →
/// `Pending` (never panic). Shared intent with the ACP plan parse.
fn map_plan_status(s: &str) -> crate::event::PlanStatus {
    use crate::event::PlanStatus;
    match s {
        "inProgress" | "in_progress" => PlanStatus::InProgress,
        "completed" => PlanStatus::Completed,
        _ => PlanStatus::Pending,
    }
}

/// R8 dual-terminal reconcile. codex ends a turn via BOTH `turn/completed` (rich
/// Turn{status,error}) AND `thread/status/changed → idle`, in EITHER order (or
/// one may be absent). We must produce EXACTLY ONE `TurnResult` per turn.
///
/// codex does NOT guarantee the two signals carry matching (or any) turnId, so
/// matching on turnId is unreliable. Instead we track a single `terminated`
/// flag for the CURRENT turn (the reader processes one turn's frames before the
/// next turn's `turn/started`/deltas, so "current turn" is unambiguous in-stream):
/// - whichever terminal arrives FIRST closes the turn (sets `terminated`);
/// - the SECOND (the other terminal source) is absorbed (`terminated` already set);
/// - a fresh non-terminal turn signal (a new turn/started, or any item event)
///   RESETS `terminated` so the NEXT turn can terminate once. (Reset is driven by
///   the reader on `turn/started`; here we only flip on terminals.)
///
/// ⚠️ ORDERING (verified against ALL 6 real 0.137.0 transcripts): codex ALWAYS
/// sends `thread/status/changed→idle` BEFORE the authoritative `turn/completed`
/// (which carries status interrupted/failed + httpStatusCode). So `turn/completed`
/// MUST win even though it arrives SECOND — otherwise a failed turn is silently
/// reported `is_error:false` (→ Idle, not Error) and an interrupted turn loses its
/// Cancelled outcome. We therefore DEFER on `idle` (set `idle_pending`, emit
/// nothing) and let `turn/completed` produce the rich terminal. The defensive
/// "idle but no completed ever" case (not observed in real codex, but the §C5 R8
/// contract says "one may be missing") is flushed as a clean terminal at EOF (see
/// the reader's `flush_pending_terminal`), so the FSM never hangs Running.
/// `idle_pending` resets per turn on `turn/started`. Returns Some only for the
/// authoritative `turn/completed`; `idle` returns None (deferred).
///
/// `status→systemError` is deferred the same way (`system_error_pending`): the
/// status carries no detail (schema: SystemErrorThreadStatus has only `type`),
/// while the follow-ups — `error{willRetry:false}` and `turn/completed{failed}`
/// — carry the real cause and arrive within ms (live capture 0.145.0, bedrock
/// stream failure). Emitting here won the terminal race and replaced the rich
/// message with an opaque "codex reported a system error". The reader bounds
/// the deferral with SYSTEM_ERROR_GRACE and flushes at EOF, so the FSM still
/// never hangs Running if the follow-up is missing.
fn reconcile_terminal(
    method: &str,
    params: &Value,
    terminated: &mut bool,
    idle_pending: &mut bool,
    system_error_pending: &mut bool,
) -> Option<SessionEvent> {
    match method {
        "turn/completed" => {
            if *terminated {
                return None; // already closed this turn
            }
            *terminated = true;
            *idle_pending = false; // the authoritative terminal supersedes the deferred idle
            *system_error_pending = false; // …and resolves a deferred systemError with the rich cause
            Some(map_turn_completed(params))
        }
        "thread/status/changed" => {
            let status = params
                .get("status")
                .and_then(|s| s.get("type").or(Some(s)))
                .and_then(Value::as_str)
                .unwrap_or("");
            match status {
                "idle" => {
                    if *terminated {
                        return None; // turn/completed already produced the terminal
                    }
                    // DEFER: do NOT emit a generic terminal here — wait for the
                    // authoritative turn/completed (carries interrupted/failed status).
                    *idle_pending = true;
                    None
                }
                // systemError is a FATAL session fault, but the status itself carries
                // NO detail (schema-verified: SystemErrorThreadStatus has only `type`,
                // codex 0.145.0 generate-json-schema v2/ThreadStatusChangedNotification.json).
                // The rich cause rides the follow-ups (live capture 0.145.0, bedrock
                // stream failure: systemError → error{willRetry:false} same ms →
                // turn/completed{failed, turn.error.message} +5ms) — synthesizing here
                // used to WIN the terminal race and the opaque "codex reported a system
                // error" masked the real cause (e.g. "failed to load AWS credentials").
                // So DEFER like idle: set pending, emit nothing; the fatal error branch
                // or turn/completed produces the rich terminal. The reader bounds the
                // wait with SYSTEM_ERROR_GRACE (and flushes at EOF) so a hypothetical
                // unfollowed systemError still cannot hang the FSM Running.
                "systemError" => {
                    if *terminated {
                        return None;
                    }
                    *system_error_pending = true;
                    None
                }
                // active / other → advisory (no terminal).
                _ => None,
            }
        }
        _ => None,
    }
}

/// A clean fallback terminal (`EndTurn`, `is_error:false`) — used only at EOF when
/// a turn produced `idle` but NO `turn/completed` ever arrived (defensive; codex
/// always sends completed). Reducer routes `is_error:false` → Idle so the FSM
/// never hangs Running.
fn synth_clean_terminal() -> SessionEvent {
    SessionEvent::TurnResult {
        is_error: false,
        api_error_status: None,
        result_text: String::new(),
        epoch: 0,
        outcome: TurnOutcome::default(),
    }
}

/// A synthetic ERROR terminal (`is_error:true`, `Failed`) used when codex leaves a
/// turn with NO authoritative `turn/completed` — either a fatal `error{willRetry:false}`
/// that codex does not follow with a completed, or a liveness-watchdog timeout (codex
/// went silent mid-turn). Reducer routes `is_error:true` → Error so the FSM leaves
/// Running and the composer unlocks instead of spinning forever. `epoch:0` is restamped
/// by the orchestrator from the live turn_gen; a later real `turn/completed` for the
/// same turn is idempotently absorbed by the reducer's terminal-absorbing law (I10).
/// Grace window for a deferred `systemError`: how long the reader waits for the
/// rich follow-up (`error{willRetry:false}` / `turn/completed`) before falling
/// back to the opaque synthesized terminal. Live capture (0.145.0, bedrock
/// stream failure) shows the follow-ups arrive within milliseconds — this bound
/// only exists so a hypothetical unfollowed systemError cannot hang the FSM.
const SYSTEM_ERROR_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

fn synth_error_terminal(message: String) -> SessionEvent {
    SessionEvent::TurnResult {
        is_error: true,
        api_error_status: None,
        result_text: message,
        epoch: 0,
        outcome: TurnOutcome::Failed,
    }
}

/// turn/completed → TurnResult, mapping turn.status → outcome (§C2/O3):
/// Parse a completed `mcpToolCall` item's `result.content[]` (MCP/rmcp Content
/// blocks: `{type:"text",text}` / `{type:"image",data:<base64>,mimeType}`) into
/// `ToolResultContent`. Previously DROPPED entirely (only aggregatedOutput was read).
/// `structuredContent` (when present) is appended as pretty JSON text so a
/// structured-only tool still shows its payload. None/absent → empty.
fn parse_codex_mcp_result(result: Option<&Value>) -> Vec<crate::event::ToolResultContent> {
    use crate::event::ToolResultContent;
    use base64::Engine as _;
    let Some(result) = result else { return Vec::new() };
    let mut out = Vec::new();
    if let Some(arr) = result.get("content").and_then(Value::as_array) {
        for el in arr {
            match el.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = el.get("text").and_then(Value::as_str) {
                        out.push(ToolResultContent::Text(t.to_string()));
                    }
                }
                Some("image") => {
                    // MCP image block = {type:image, data:<base64>, mimeType}.
                    let media_type = el
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("image/png")
                        .to_string();
                    if let Some(bytes) = el
                        .get("data")
                        .and_then(Value::as_str)
                        .and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok())
                    {
                        out.push(ToolResultContent::Image {
                            media_type,
                            data: bytes,
                        });
                    }
                }
                _ => {} // resource / audio / unknown — skipped (no neutral mapping yet)
            }
        }
    }
    if let Some(sc) = result.get("structuredContent").filter(|v| !v.is_null())
        && let Ok(s) = serde_json::to_string_pretty(sc)
    {
        out.push(ToolResultContent::Text(s));
    }
    out
}

/// Parse a completed `dynamicToolCall` item's `contentItems[]`
/// (`{type:"inputText",text}` / `{type:"inputImage",imageUrl}`). Previously DROPPED.
/// An `inputImage` is a URL (often a data: or remote URL) — carried as a text
/// reference (we can't assume it is decodable raw bytes). None/absent → empty.
fn parse_codex_dynamic_content_items(items: Option<&Value>) -> Vec<crate::event::ToolResultContent> {
    use crate::event::ToolResultContent;
    let Some(arr) = items.and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|el| match el.get("type").and_then(Value::as_str) {
            Some("inputText") => el
                .get("text")
                .and_then(Value::as_str)
                .map(|t| ToolResultContent::Text(t.to_string())),
            Some("inputImage") => el
                .get("imageUrl")
                .and_then(Value::as_str)
                .map(|u| ToolResultContent::Text(format!("[image: {u}]"))),
            _ => None,
        })
        .collect()
}

/// Completed→EndTurn (or Truncated if a stop reason says so), Interrupted→
/// Cancelled, Failed→Failed. `is_error` (the reducer's routing bit) is true only
/// for Failed.
fn map_turn_completed(params: &Value) -> SessionEvent {
    let turn = params.get("turn").unwrap_or(&Value::Null);
    let status = turn.get("status").and_then(Value::as_str).unwrap_or("completed");
    let (is_error, outcome) = match status {
        "interrupted" => (
            false,
            TurnOutcome::Cancelled {
                reason: CancelReason::UserCancel,
            },
        ),
        "failed" => (true, TurnOutcome::Failed),
        // "completed" (or any other treated as clean) → EndTurn.
        //
        // NOTE (protocol audit): the codex `Turn` struct has NO `stopReason` field
        // (source-verified: v2/thread_data.rs Turn + TurnStatus enum; grep stopReason
        // across v2/ = 0 hits). A prior maxTokens/maxTurns branch reading turn.stopReason
        // was dead code (the key never exists) → removed. codex surfaces truncation as
        // a FAILED turn carrying turn.error (e.g. contextWindowExceeded), already routed
        // via the "failed" arm above (is_error:true → TurnOutcome::Failed), not as a
        // clean Truncated outcome. If a truncation BADGE is wanted later, classify
        // turn.error.codexErrorInfo (contextWindowExceeded/usageLimitExceeded) — but
        // that is an additive enhancement, not the dead stopReason read.
        _ => (
            false,
            TurnOutcome::Completed {
                stop_reason: StopReason::EndTurn,
            },
        ),
    };
    let error = turn.get("error");
    let result_text = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // C-3 (correctness): httpStatusCode is NOT top-level on turn.error — it is
    // NESTED inside the externally-tagged `CodexErrorInfo` variant object on
    // `error.codexErrorInfo`, e.g. {"httpConnectionFailed":{"httpStatusCode":500}}
    // (verified-from-source v2/shared.rs:64-112). Reading it at the top level made
    // api_error_status ALWAYS None on real failed turns. Walk into codexErrorInfo's
    // single variant object; the variants without a status (ServerOverloaded,
    // Unauthorized, …) yield None, which is correct.
    let api_error_status = error
        .and_then(|e| e.get("codexErrorInfo"))
        .and_then(Value::as_object)
        .and_then(|m| m.values().next()) // the single externally-tagged variant payload
        .and_then(|v| v.get("httpStatusCode"))
        .and_then(Value::as_u64)
        // Fallback: tolerate a top-level httpStatusCode too (defensive — some
        // shapes/older versions may flatten it), so we never regress to None when
        // the status IS present somewhere.
        .or_else(|| error.and_then(|e| e.get("httpStatusCode")).and_then(Value::as_u64))
        // LIVE-found fallback: a real failed turn often carries NO structured
        // httpStatusCode — the variant collapses to `codexErrorInfo:"other"` (a
        // bare string) and the status lives ONLY in the message, e.g.
        // "unexpected status 404 Not Found: The model '…' does not exist".
        // (Observed live, codex 0.139.0, bad-model turn.) Parse it out so a real
        // provider HTTP error still surfaces its status, mirroring how
        // send_error folds status-as-text. Structured paths above win when present.
        .or_else(|| extract_http_status_from_message(&result_text))
        .map(|n| n as u16);
    SessionEvent::TurnResult {
        is_error,
        api_error_status,
        result_text,
        epoch: 0, // orchestrator restamps from the envelope turn_gen (§5.4)
        outcome,
    }
}

/// Extract an HTTP status from a codex error message of the form
/// `"unexpected status <NNN> <reason>: …"` (the shape a real provider HTTP error
/// takes when codex collapses `codexErrorInfo` to `"other"` and carries the status
/// only in text — LIVE-observed 0.139.0). Anchored on the literal `"status "`
/// prefix and a 3-digit 1xx–5xx code so it does NOT mis-fire on arbitrary numbers
/// in the message (model ids, request ids, token counts). Returns `None` when the
/// shape is absent — the structured paths remain authoritative.
fn extract_http_status_from_message(message: &str) -> Option<u64> {
    let lower = message.to_ascii_lowercase();
    let after = lower.split("status ").nth(1)?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    let code: u64 = digits.parse().ok()?;
    (100..=599).contains(&code).then_some(code)
}

#[async_trait::async_trait]
impl SessionBackend for CodexSessionBackend {
    /// Force-kill path (`UserCancelTimeout`): delegate to the suspend
    /// controller's unconditional teardown (abort reader → group-kill the codex
    /// CLI process tree), so the process dies even while an orchestrator still
    /// holds an `Arc` to this backend.
    async fn terminate(&self) {
        self.suspend.terminate().await;
    }

    async fn dispatch(&self, command: Command) -> Result<CommandReceipt, BackendError> {
        match command {
            Command::Send { content, metadata } => {
                // §C6 Layer-2: reject any block kind codex does not advertise
                // (prompt_blocks: text + image) BEFORE wire-write — never silently
                // drop it ("adapter authoritatively rejects → CommandNotSupported, never a silent drop"). An
                // audio/resource/at_mention block is rejected, keyed on its
                // `content_block:<kind>` name.
                let blocks = self.capabilities().prompt_blocks;
                if let Some(bad) = content.iter().find(|b| !blocks.allows(b)) {
                    return Err(BackendError::CommandNotSupported {
                        command: crate::capability::block_kind_name(bad),
                    });
                }
                // Bridge-parity slash routing (ELECTRON-3PX): the 6 advertised
                // commands (builtin_slash_commands) translate to native ops the way
                // the codex-acp bridge did in-process (v0.14.0 thread.rs:3252
                // handle_prompt) — codex does NOT interpret slash text itself.
                let route = route_slash_command(&content);

                // /logout → account/logout. No turn lifecycle follows (bridge:
                // auth.logout() then auth_required), so it is handled before the
                // flight-check and returns NoTurn: no turn_gen bump, no
                // turn_in_flight. The response still drains the pending queue via
                // pending_sends → PromptAccepted; a Notice tells the user what
                // happened (there is no other visible output).
                if matches!(route, Some(SlashRoute::Logout)) {
                    self.suspend
                        .ensure_awake(aionui_common::now_ms(), || self.wake_handle())
                        .await?;
                    let id = self.next_rpc_id();
                    // NoTurn: an error response surfaces as a Notice (there is no
                    // turn to terminate); a result drains the pending queue.
                    self.pending_sends.lock().await.insert(
                        id,
                        PendingSend {
                            client_msg_id: metadata.client_msg_id,
                            opens_turn: false,
                        },
                    );
                    // params is `null` per schema (AccountLogoutParams: {"type":"null"},
                    // samples/codex-cli/0.137.0/schema-full/ClientRequest.json).
                    let frame = json!({
                        "jsonrpc": "2.0", "id": id, "method": "account/logout", "params": Value::Null
                    });
                    self.write_frame(frame).await?;
                    emit(
                        &self.event_tx,
                        &self.session_id,
                        self.turn_gen.load(Ordering::SeqCst),
                        SessionEvent::Notice {
                            level: crate::event::NoticeLevel::Warning,
                            message: "Logged out of Codex. New turns will require re-authentication.".into(),
                            localized: None,
                            supersedes_key: None,
                        },
                    );
                    return Ok(CommandReceipt {
                        accepted: true,
                        admission: Admission::NoTurn,
                        turn_gen: self.turn_gen.load(Ordering::SeqCst),
                    });
                }
                // 009 R1c: a flight-period Send (a turn is already active) must NOT
                // open a second turn_gen. codex's app-server merges an overlapping
                // input into the live turn under a SINGLE turnId (verified:
                // concurrent_turn_start_merge.jsonl) — issuing a second `turn/start`
                // + fetch_add would phantom-split one wire turn across two turn_gen
                // buckets downstream (GROUP BY turn_gen). Mirror the Cancel arm's
                // active_turn_id probe: accept as NoTurn, no frame, no fetch_add.
                // (Slash commands during a live turn fold in as plain text — same
                // merge semantics as any other flight-period input.)
                if self.active_turn_id.lock().await.is_some() {
                    return Ok(CommandReceipt {
                        accepted: true,
                        admission: Admission::NoTurn,
                        turn_gen: self.turn_gen.load(Ordering::SeqCst),
                    });
                }
                // F-4: ensure the app-server is awake before the wire write. When
                // idle_ttl=None (default) the slot is always Active → one
                // uncontended lock, no re-spawn (pre-F-4 parity). When suspended,
                // this re-spawns + replays the resume handshake first.
                self.suspend
                    .ensure_awake(aionui_common::now_ms(), || self.wake_handle())
                    .await?;
                // F-4: mark the turn in flight so the idle timer won't suspend the
                // app-server mid-turn (the reader clears it at the terminal).
                self.turn_in_flight.store(true, Ordering::SeqCst);
                // REAL codex 0.137.0 turn-driver: `turn/start{threadId, input}`
                // (verified against the aion-probe transcripts). Needs the bound
                // threadId (waits briefly for the async thread/started; fails FAST
                // when the resume was rejected — see `resume_poison`).
                let tid = match self.bound_thread().await {
                    Ok(tid) => tid,
                    Err(e) => {
                        // The turn never reached the wire — undo the in-flight mark
                        // so a failed dispatch doesn't pin the idle timer awake.
                        self.turn_in_flight.store(false, Ordering::SeqCst);
                        return Err(e);
                    }
                };
                let id = self.next_rpc_id();
                // GAP-A: register the correlation so the reader can emit
                // PromptAccepted when codex's synchronous turn/start RESPONSE lands
                // (that response is the "accepted" receipt; the conversation drains
                // its pending queue on it). Registered even without a client_msg_id:
                // an ERROR response must terminate the admitted turn regardless
                // (ELECTRON-3Q0 — a dropped rejection hung the turn forever).
                // review/start's response is the same turn object and
                // thread/compact/start's is `{}` — both drain identically (verified
                // live: samples/codex-cli/0.144.1/review_start_uncommitted.jsonl +
                // thread_compact.jsonl).
                self.pending_sends.lock().await.insert(
                    id,
                    PendingSend {
                        client_msg_id: metadata.client_msg_id,
                        opens_turn: true,
                    },
                );
                // All three turn-flavored routes run a REAL wire turn on the thread
                // (turn/started → items → turn/completed; verified live 0.144.1, files
                // above), so the existing reader/FSM lifecycle applies unchanged.
                let (method, params) = match route {
                    None => ("turn/start", json!({ "threadId": tid, "input": build_input(&content) })),
                    // /init → the bridge's canned AGENTS.md prompt as a normal turn
                    // (bridge: Op::UserInput{INIT_COMMAND_PROMPT}).
                    Some(SlashRoute::Init) => (
                        "turn/start",
                        json!({ "threadId": tid, "input": [{ "type": "text", "text": CODEX_INIT_PROMPT }] }),
                    ),
                    // /compact → thread/compact/start{threadId} (bridge: Op::Compact).
                    Some(SlashRoute::Compact) => ("thread/compact/start", json!({ "threadId": tid })),
                    // /review* → review/start{threadId, target} (bridge: Op::Review;
                    // delivery omitted = inline on this thread, the bridge's behavior).
                    Some(SlashRoute::Review(target)) => ("review/start", json!({ "threadId": tid, "target": target })),
                    Some(SlashRoute::Logout) => unreachable!("handled above"),
                };
                let frame = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params
                });
                self.write_frame(frame).await?;
                let cur_gen = self.turn_gen.fetch_add(1, Ordering::SeqCst) + 1;
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
                // REAL codex: `turn/interrupt{threadId, turnId}` (hard cancel).
                // `turnId` is REQUIRED (non-Option on the wire). bound_thread first
                // (establishes the handshake completed + lets the reader bind the
                // active turn from turn/started).
                let tid = self.bound_thread().await?;
                // cancel-before-fold race (token-burn half): a cancel can arrive in the
                // window between dispatch(Send) (which set turn_in_flight + wrote
                // turn/start) and the reader binding `active_turn_id` from the async
                // turn/started notification. If we no-op'd here, codex would keep
                // running the turn (burning tokens) even though the user cancelled. So
                // when a turn IS in flight but the id is not bound yet, briefly poll for
                // the reader to bind it, then interrupt. A genuinely idle session
                // (no turn_in_flight) still no-ops without writing a frame codex would
                // reject. The orchestrator's lowered Cancel already folded the FSM to
                // Idle (§004 S14); this only stops the backend's wasted work.
                let mut active = self.active_turn_id.lock().await.clone();
                if active.is_none() && self.turn_in_flight.load(Ordering::SeqCst) {
                    for _ in 0..50 {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        active = self.active_turn_id.lock().await.clone();
                        if active.is_some() || !self.turn_in_flight.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                }
                let Some(turn_id) = active else {
                    return Ok(CommandReceipt {
                        accepted: true,
                        admission: Admission::NoTurn,
                        turn_gen: self.turn_gen.load(Ordering::SeqCst),
                    });
                };
                let id = self.next_rpc_id();
                let frame = json!({
                    "jsonrpc": "2.0", "id": id, "method": "turn/interrupt",
                    "params": { "threadId": tid, "turnId": turn_id }
                });
                self.write_frame(frame).await?;
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            Command::Steer { content } => {
                // REAL codex: `turn/steer{threadId, expectedTurnId, input}` — a SOFT
                // injection (queued to the active turn's input, NOT a hard cancel;
                // contrast turn/interrupt). The optimistic `expectedTurnId` is the
                // gated-steering wire: codex rejects (activeTurnNotSteerable) if the
                // turn already ended. NoTurn admission (no new turn_gen — folds into
                // the live turn, b-side FSM never sees Steer).
                let tid = self.bound_thread().await?;
                let Some(turn_id) = self.active_turn_id.lock().await.clone() else {
                    // No active turn to steer into.
                    return Err(BackendError::Transport("no active turn to steer".into()));
                };
                let id = self.next_rpc_id();
                let frame = json!({
                    "jsonrpc": "2.0", "id": id, "method": "turn/steer",
                    "params": { "threadId": tid, "expectedTurnId": turn_id, "input": build_input(&content) }
                });
                self.write_frame(frame).await?;
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            Command::SetMode { mode } => {
                // F-4: SetMode is a between-turn config write that can arrive while
                // the session is idle-suspended → wake first so it writes to a live
                // process (no-op when Active).
                self.suspend
                    .ensure_awake(aionui_common::now_ms(), || self.wake_handle())
                    .await?;
                // for codex the mode axis IS the permission axis. The mode value is a
                // DISCOVERED colon-prefixed profile id (`permissionProfile/list`), applied
                // via `thread/settings/update{threadId, permissions:":workspace"}` — NOT the
                // old collaborationMode object. U1 (live 0.139.0) froze the wire: the id
                // MUST retain its leading colon (a bare id is rejected with
                // "default_permissions requires a [permissions] table"), and `permissions`
                // is mutually exclusive with `sandboxPolicy`. Unlike the old
                // collaborationMode path this needs NO current_model. Applies to the NEXT
                // turn; confirmed via the thread/settings/updated notif.
                //
                // `normalize_to_profile_id` is the codex analogue of legacy ACP
                // `normalize_requested_mode`: a discovered colon id flows through verbatim,
                // while an upgrading user's legacy bare value (`full-access`/`yolo`/… from
                // an older persisted selection) rewrites onto its colon-id equivalent —
                // never producing a value codex would reject. Validation against the LIVE
                // catalog (a stale custom id) is the reader's job on the response (a reject
                // surfaces as a Notice), exactly as legacy `set_mode` relied on the backend.
                let profile_id = codex_perm::normalize_to_profile_id(&mode);
                let tid = self.bound_thread().await?;
                let id = self.next_rpc_id();
                // Register the rpc id so the reader claims the response: a JSON-RPC
                // error (codex rejected the profile) surfaces as a Notice instead of
                // being dropped (success converges via thread/settings/updated).
                self.pending_set
                    .lock()
                    .await
                    .insert(id, format!("mode\u{2192}{profile_id}"));
                let frame = json!({
                    "jsonrpc": "2.0", "id": id, "method": "thread/settings/update",
                    "params": {
                        "threadId": tid,
                        "permissions": profile_id
                    }
                });
                self.write_frame(frame).await?;
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
                // codex `thread/settings/update{threadId, model}` (verified frame:
                // {"threadId":..,"model":"gpt-5.5"}). Applies to subsequent turns.
                // Track it so a subsequent SetMode can build collaborationMode (M1).
                let tid = self.bound_thread().await?;
                *self.current_model.lock().await = Some(model.clone());
                let id = self.next_rpc_id();
                // Register the rpc id so the reader claims the response: a JSON-RPC
                // error (codex rejected the model) surfaces as a Notice instead of
                // being dropped (success converges via thread/settings/updated).
                self.pending_set
                    .lock()
                    .await
                    .insert(id, format!("model\u{2192}{model}"));
                let frame = json!({
                    "jsonrpc": "2.0", "id": id, "method": "thread/settings/update",
                    "params": { "threadId": tid, "model": model }
                });
                self.write_frame(frame).await?;
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            // codex has no AskUserQuestion (its MCP elicitation degrades to a
            // Permission with ELICIT_PREFIX); Ask is never raised here.
            Command::AnswerAsk { .. } => Err(BackendError::CommandNotSupported { command: "answer_ask" }),
            Command::AnswerAuth { method_id, credentials } => {
                // Mid-session re-auth (R6/R15): the server raised
                // `account/chatgptAuthTokens/refresh` (a blocking ServerRequest the
                // reader surfaced as Permission{Auth} + stashed the wire id). We
                // answer by writing the keyed JSON-RPC RESPONSE carrying the supplied
                // tokens — `{access_token, chatgpt_account_id, chatgpt_plan_type?}`
                // per ChatgptAuthTokensRefreshResponse — which UNBLOCKS the turn.
                // The b-side waiting_on_auth -1 happens on serverRequest/resolved.
                let Some(req_id) = self.pending_auth_id.lock().await.take() else {
                    return Err(BackendError::Transport("no pending auth refresh to answer".into()));
                };
                let _ = method_id; // codex's refresh has ONE response shape; method_id is advisory
                // RESPONSE KEY SHAPE = camelCase (accessToken/chatgptAccountId/chatgptPlanType).
                // SCHEMA-CONFIRMED (protocol audit, 0.139.0): ChatgptAuthTokensRefreshResponse
                // EXISTS in the generated schema, required [accessToken, chatgptAccountId],
                // optional chatgptPlanType — exactly the camelCase shape written here. (The
                // earlier "no schema, unverified best-guess" note was stale; the wire is
                // correct. A snake_case "fix" would be WRONG — do not revert.)
                // We accept either case from the CALLER (credentials) and normalize.
                let access_token = credentials
                    .get("access_token")
                    .or_else(|| credentials.get("accessToken"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let account_id = credentials
                    .get("chatgpt_account_id")
                    .or_else(|| credentials.get("chatgptAccountId"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let mut result = json!({ "accessToken": access_token, "chatgptAccountId": account_id });
                if let Some(plan) = credentials
                    .get("chatgpt_plan_type")
                    .or_else(|| credentials.get("chatgptPlanType"))
                {
                    result["chatgptPlanType"] = plan.clone();
                }
                let frame = json!({ "jsonrpc": "2.0", "id": req_id, "result": result });
                self.write_frame(frame).await?;
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            Command::ListCheckpoints => {
                // F-4: a between-turn query → wake a suspended session first.
                self.suspend
                    .ensure_awake(aionui_common::now_ms(), || self.wake_handle())
                    .await?;
                // codex `thread/turns/list{threadId}` — register the rpc id so the
                // reader maps the response's `data: Vec<Turn>` to a
                // `SessionEvent::CheckpointList{entries}` (O2 up-leg). First page
                // only (we do not chase `next_cursor` — bounds the N2 risk).
                let tid = self.bound_thread().await?;
                let id = self.next_rpc_id();
                self.pending_discovery
                    .lock()
                    .await
                    .insert(id, DiscoveryKind::Checkpoints);
                let frame = json!({
                    "jsonrpc": "2.0", "id": id, "method": "thread/turns/list",
                    "params": { "threadId": tid }
                });
                self.write_frame(frame).await?;
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            Command::Rewind { num_turns } => {
                // G3 (T17 model): write codex `thread/rollback{threadId, numTurns}`
                // (down) and register the rpc id so the reader maps the response to a
                // `Rewound{to_turn}` receipt (up). Per the FROZEN seam contract T17,
                // rewind is conversation-managed as a FORK (parent block stream is
                // append-only, NOT truncated); session's job is exactly: mutate the
                // backend history + emit the Rewound receipt the orchestrator
                // rehydrates to and the conversation forks from. The earlier
                // half-wiring wrote the rollback but emitted NO receipt (GAP-B) — that
                // gap is what this closes.
                //
                // Idle-gated: a rollback mid-turn would race the in-flight turn's
                // history. `active_turn_id` is the wire-truth proxy for Running (the
                // reader sets it on turn/started, clears on turn/completed), so it
                // catches a live turn regardless of who started it; reject a mid-turn
                // rewind so it never silently corrupts a running turn.
                if self.active_turn_id.lock().await.is_some() {
                    return Err(BackendError::Transport(
                        "cannot rewind while a turn is in flight".into(),
                    ));
                }
                if num_turns == 0 {
                    return Err(BackendError::Transport("rewind num_turns must be >= 1".into()));
                }
                self.suspend
                    .ensure_awake(aionui_common::now_ms(), || self.wake_handle())
                    .await?;
                let tid = self.bound_thread().await?;
                let id = self.next_rpc_id();
                self.pending_discovery.lock().await.insert(id, DiscoveryKind::Rewind);
                let frame = json!({
                    "jsonrpc": "2.0", "id": id, "method": "thread/rollback",
                    "params": { "threadId": tid, "numTurns": num_turns }
                });
                self.write_frame(frame).await?;
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            Command::AnswerPermission {
                request_id,
                decision,
                selected: _, // codex approval is accept/decline; no pick-one label
                answers: _,  // codex has no AskUserQuestion; per-question answers N/A
            } => {
                let approved = matches!(
                    decision,
                    super::types::PermissionDecision::Approved | super::types::PermissionDecision::AllowAlways
                );
                // codex CommandExecution/FileChangeApprovalDecision: accept (one-time)
                // / acceptForSession (allow-always, no re-prompt) / decline. Map
                // AllowAlways → acceptForSession so "don't ask again" persists for the
                // session (was collapsed to a one-time accept → codex re-prompted every
                // matching command). Schema: CommandExecutionRequestApprovalResponse.
                let decision_str = match decision {
                    super::types::PermissionDecision::AllowAlways => "acceptForSession",
                    super::types::PermissionDecision::Approved => "accept",
                    super::types::PermissionDecision::Denied => "decline",
                };
                // An elicitation request (ELICIT_PREFIX) needs `{action, content}`;
                // a command/file approval needs `{decision}`. Both keyed by the same
                // wire id we surfaced as Permission.request_id (prefix stripped for
                // elicitation). The reducer never read the request_id, so the prefix
                // is purely a dispatch-side wire-shape selector.
                let frame = if let Some(raw_id) = request_id.strip_prefix(ELICIT_PREFIX) {
                    let id: Value = serde_json::from_str(raw_id).unwrap_or(Value::String(raw_id.to_string()));
                    // Elicitation has only accept/decline (no per-session variant);
                    // accept executes the tool / submits the form; decline cancels.
                    // We have no form values to fill (the conversation layer would
                    // supply them when a real form UI exists), so content is empty.
                    json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "action": if approved { "accept" } else { "decline" }, "content": {} }
                    })
                } else {
                    let id: Value = serde_json::from_str(&request_id).unwrap_or(Value::String(request_id.clone()));
                    json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "decision": decision_str }
                    })
                };
                self.write_frame(frame).await?;
                // We answered it → drop from the REST-recovery registry so a
                // subsequent GET /confirmations no longer resurfaces a card the user
                // already resolved. `request_id` is the exact stored key (raw for
                // tool/file, ELICIT_PREFIX-tagged for elicitation).
                remove_pending_tool_approval(&self.pending_tool_approvals, &request_id);
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            Command::Acknowledge { .. } => {
                // User-ack of a completed turn (done-unseen → seen). NO wire frame —
                // codex has no "acknowledge" concept; this folds at the
                // conversation/fold-on-read layer, never the backend. Accept as a
                // no-op so the conversation can record the ack locally (§C1).
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            // codex's reasoning effort IS a first-class `thread/settings/update` field:
            // `ThreadSettingsUpdateParams.effort` ("Override the reasoning effort for
            // subsequent turns" → ReasoningEffort enum), verified in the generated
            // schema (samples/codex-cli/0.137.0/schema-full/ClientRequest.json,
            // ThreadSettingsUpdateParams). So the effort option routes through the exact
            // same wire SetModel/SetMode use — just with `{effort}` in params. The value
            // reaching here is one of the model's advertised `supportedReasoningEfforts`
            // (parsed verbatim from the catalog), so it is passed through unvalidated
            // exactly like `model`; codex rejects an out-of-catalog value with a JSON-RPC
            // error, which the reader surfaces as a Notice (validation is the reader's job
            // on the response, matching SetModel/SetMode). Any OTHER config option has no
            // codex wire and still rejects.
            Command::SetConfigOption { option_id, value }
                if matches!(option_id.as_str(), "effort" | "reasoning_effort" | "thought_level") =>
            {
                // F-4: between-turn config write → wake a suspended session first.
                self.suspend
                    .ensure_awake(aionui_common::now_ms(), || self.wake_handle())
                    .await?;
                let tid = self.bound_thread().await?;
                let id = self.next_rpc_id();
                // Register the rpc id so the reader claims the response: a JSON-RPC error
                // (codex rejected the effort) surfaces as a Notice instead of being
                // dropped (success converges via thread/settings/updated).
                self.pending_set
                    .lock()
                    .await
                    .insert(id, format!("effort\u{2192}{value}"));
                let frame = json!({
                    "jsonrpc": "2.0", "id": id, "method": "thread/settings/update",
                    "params": { "threadId": tid, "effort": value }
                });
                self.write_frame(frame).await?;
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: self.turn_gen.load(Ordering::SeqCst),
                })
            }
            Command::SetConfigOption { .. } => Err(BackendError::CommandNotSupported {
                command: "set_config_option",
            }),
            // codex streams per-turn usage (thread/tokenUsage/updated) but has no
            // on-demand cumulative context/cost QUERY wire → reject (cap=false).
            Command::QuerySessionInfo { .. } => Err(BackendError::CommandNotSupported {
                command: "query_session_info",
            }),
        }
    }

    fn events(&self) -> BoxStream<'static, SessionEnvelope> {
        let rx = self.event_tx.subscribe();
        // Replay the current thread binding to every NEW subscriber. codex
        // answers `thread/start` with `thread/started` in single-digit ms
        // (local bookkeeping, no LLM call), so on a fresh open the live
        // BackendBound routinely lands BEFORE the orchestration layer's
        // subscription and is lost — the conversation then never persists its
        // resume anchor (and the fork API refuses with FORK_PARENT_UNBOUND).
        // A synthetic BackendBound is idempotent downstream (same-value
        // set_session_id + same-value DB write), so replaying to an already-
        // caught-up subscriber is harmless. try_lock: the binding mutex is
        // held only for point writes/reads; on contention we just skip the
        // preface (the caller is racing the live event, which it will get).
        let preface: Vec<SessionEnvelope> = self
            .thread_binding
            .try_lock()
            .ok()
            .and_then(|guard| guard.clone())
            .map(|tid| SessionEnvelope {
                session_id: self.session_id.clone(),
                turn_gen: self.turn_gen.load(Ordering::SeqCst),
                event: SessionEvent::BackendBound {
                    backend_session_id: Some(tid),
                },
            })
            .into_iter()
            .collect();
        let live = futures_util::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(env) => return Some((env, rx)),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
        futures_util::stream::iter(preface).chain(live).boxed()
    }

    fn capabilities(&self) -> Capabilities {
        // B-CODEX-MODEL-LIST: merge the handshake-discovered models/modes into the
        // snapshot (the static base has them empty; the reader fills `discovered`
        // from model/list + permissionProfile/list responses — the latter mapped to the
        // fixed permission-tier mode enum, feature 012). Read-only sync lock.
        let mut caps = self.capabilities.clone();
        let disc = self.discovered.lock().unwrap_or_else(|e| e.into_inner());
        if !disc.models.is_empty() {
            caps.available_models = disc.models.clone();
        }
        if !disc.modes.is_empty() {
            caps.available_modes = disc.modes.clone();
        }
        caps
    }

    /// REST-recovery (`GET /confirmations`) source: the transient registry of
    /// currently-open (unanswered) codex approval requests — command/file
    /// approvals (`*/requestApproval`) and MCP elicitations. The reader inserts on
    /// each such reverse-RPC, and removes on `serverRequest/resolved` (codex
    /// retracted/answered) and `dispatch(AnswerPermission)` (we answered). Without
    /// this override (the default empty `Vec`), a codex tool/file approval raised
    /// before the client subscribed — or lost on a page reload — could never be
    /// rebuilt, and the turn hung forever waiting for an answer. The recovered
    /// card's id==call_id==request_id, matching the live `Permission` frame so a
    /// duplicate live+recovered pair de-dups. codex approvals carry no question
    /// payload (AskUserQuestion is claude-only), so `questions` is always `None`;
    /// the raw command body is NOT exposed (TIO-13) — only the approval-class title.
    fn pending_permission_requests(&self) -> Vec<PendingPermissionView> {
        self.pending_tool_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(request_id, title)| PendingPermissionView {
                request_id: request_id.clone(),
                tool_name: title.clone(),
                questions: None,
            })
            .collect()
    }
}

/// Map our multimodal `ContentBlock`s → codex `turn/start.input` items. codex
/// `UserInput` (source-verified turn.rs:266–297) carries Text, Image, LocalImage,
/// Skill and Mention. We map text, image (`image:true`) and file attachments
/// (`resource:true` → `Mention`). Audio/at_mention are not advertised in
/// codex_capabilities, so dispatch (§C6, `BlockSet::allows`) rejects them before
/// reaching here — the `_ => None` arm is a defensive belt-and-suspenders.
/// The canned `/init` prompt, verbatim from the codex-acp bridge
/// (zed-industries/codex-acp v0.14.0 src/prompt_for_init_command.md, wired at
/// thread.rs:3255-3264 as `Op::UserInput{INIT_COMMAND_PROMPT}`).
const CODEX_INIT_PROMPT: &str = include_str!("./codex_init_prompt.md");

/// Where a Send's slash command routes (bridge parity, ELECTRON-3PX). codex has
/// no slash-command wire of its own — the codex-acp bridge intercepted the 6
/// advertised names in `handle_prompt` (v0.14.0 thread.rs:3252-3320) and mapped
/// each to a native op; we replicate that mapping onto app-server JSON-RPC.
#[derive(Debug, PartialEq)]
enum SlashRoute {
    /// `/init` → the canned AGENTS.md prompt as a normal `turn/start`.
    Init,
    /// `/compact` → `thread/compact/start{threadId}`.
    Compact,
    /// `/review [instructions]` / `/review-branch <b>` / `/review-commit <sha>`
    /// → `review/start{threadId, target}`; payload = the wire `ReviewTarget`
    /// (schema: samples/codex-cli/0.137.0/schema-full/ClientRequest.json).
    Review(Value),
    /// `/logout` → `account/logout` (no turn follows).
    Logout,
}

/// Parse a leading slash command NAME from raw text. Sole grammar owner, shared
/// by codex's `route_slash_command` and the team recognition predicate so the two
/// never drift. Rules (byte-identical to the historical inline logic in
/// `route_slash_command`): the text must START with `/` (no leading whitespace
/// tolerated), the name runs to the first Unicode whitespace, and must be
/// non-empty. Returns the name without the leading `/`.
pub fn slash_command_name(text: &str) -> Option<&str> {
    let stripped = text.strip_prefix('/')?;
    let name_end = stripped
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(idx, _)| idx)
        .unwrap_or(stripped.len());
    let name = &stripped[..name_end];
    if name.is_empty() {
        return None;
    }
    Some(name)
}

/// Parse a leading slash command from the first Text block. Faithful port of the
/// bridge's `extract_slash_command` (thread.rs:4195): the text must START with
/// `/`, the name runs to the first whitespace, `rest` is the trimmed remainder.
/// An unknown name (or `/review-branch`//`/review-commit` with no argument, per
/// the bridge's `if !rest.is_empty()` guards) returns `None` → the text goes to
/// codex verbatim as a plain turn, exactly like the bridge's `_ =>` arm.
///
/// The NAME segment is parsed by the shared [`slash_command_name`] so this table
/// and the team recognition predicate always agree on the grammar.
fn route_slash_command(content: &[ContentBlock]) -> Option<SlashRoute> {
    let Some(ContentBlock::Text(text)) = content.first() else {
        return None;
    };
    let name = slash_command_name(text)?;
    // `rest` is everything after the name (name_end is the byte length of the
    // name because the name never contains a multi-byte prefix split), trimmed.
    let rest = text[1 + name.len()..].trim_start();
    match name {
        "compact" => Some(SlashRoute::Compact),
        "init" => Some(SlashRoute::Init),
        "review" => {
            let instructions = rest.trim();
            let target = if instructions.is_empty() {
                json!({ "type": "uncommittedChanges" })
            } else {
                json!({ "type": "custom", "instructions": instructions })
            };
            Some(SlashRoute::Review(target))
        }
        "review-branch" if !rest.is_empty() => Some(SlashRoute::Review(
            json!({ "type": "baseBranch", "branch": rest.trim() }),
        )),
        "review-commit" if !rest.is_empty() => {
            Some(SlashRoute::Review(json!({ "type": "commit", "sha": rest.trim() })))
        }
        "logout" => Some(SlashRoute::Logout),
        _ => None,
    }
}

fn build_input(content: &[ContentBlock]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(json!({ "type": "text", "text": t })),
            ContentBlock::Image { data, media_type } => {
                // codex `UserInput::Image { url }` (source-verified turn.rs:143-146,
                // wire `{type:"image", url}`). We pass a data: URL of the bytes.
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(data);
                Some(json!({ "type": "image", "url": format!("data:{media_type};base64,{b64}") }))
            }
            ContentBlock::ResourceLink { uri, .. } => {
                // Deliver a file BY REFERENCE as a TEXT element (the model spawns its
                // own read tool), NOT as codex `UserInput::Mention`.
                //
                // ROOT CAUSE (source-verified, openai/codex models.rs:1718
                // `from_user_input` → :1762): `Mention` contributes ZERO content to the
                // model prompt — it is consumed ONLY by plugins/mentions.rs for App/
                // Plugin (`plugin://`/connector/registered-app) references; a plain
                // filesystem path matches neither, resolves to nothing, AND leaves the
                // turn loop's follow-up state unresolved so codex never emits
                // turn/completed → the FSM hangs Running forever (the "no-terminal"
                // prod bug). Plain Text turns terminate cleanly. So a file attachment
                // must ride Text (mirrors the claude adapter's `[Attached file: <uri>]`).
                // Strip a `file://` scheme so the path reads cleanly. Pinned by
                // protocols/samples/codex-cli/0.139.0/_probe_hang_isolate.py (mention =
                // no terminal; text = terminal).
                let path = uri.strip_prefix("file://").unwrap_or(uri);
                Some(json!({ "type": "text", "text": format!("[Attached file: {path}]") }))
            }
            // Audio / at_mention: not in codex prompt_blocks → drop.
            _ => None,
        })
        .collect()
}

impl CodexSessionBackend {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl Drop for CodexSessionBackend {
    /// M5: abort the live reader (via the controller's mirrored AbortHandle, no
    /// await) so its `Arc<dyn AgentIo>` clone is released and the persistent codex
    /// subprocess is reaped (kill_on_drop). Without this the reader blocks forever
    /// on `next_line()` (codex stdout never EOFs), pinning the `ManagedProcess`
    /// alive → orphaned child. Also stop the idle timer if one was running.
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
    use crate::event::PermissionKind;
    use crate::testing::FakeAgentIo;
    use futures_util::StreamExt;

    /// A retrying error must reach the user, not just tick the heartbeat.
    ///
    /// Frame captured live from codex 0.147.0, whose response stream kept
    /// disconnecting: the turn stayed "in progress" forever with no assistant
    /// output, and the only clue — codex's own "Reconnecting… / high demand"
    /// message — was swallowed, so a stalled upstream was indistinguishable
    /// from a hung app.
    #[tokio::test]
    async fn a_retrying_error_surfaces_the_reason_alongside_the_heartbeat() {
        let events = drive_codex(&[
            r#"{"method":"error","params":{"error":{"message":"Reconnecting... 1/5","codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":null}},"additionalDetails":"We're currently experiencing high demand, which may cause temporary errors."},"willRetry":true,"threadId":"t1","turnId":"u1"},"emittedAtMs":1}"#,
        ])
        .await;

        assert!(
            events.iter().any(|e| matches!(e, SessionEvent::Heartbeat)),
            "a retry is still a liveness signal, got {events:?}"
        );
        let notice = events
            .iter()
            .find_map(|e| match e {
                SessionEvent::Notice { message, .. } => Some(message.clone()),
                _ => None,
            })
            .expect("the retry reason must reach the user");
        assert!(notice.contains("Reconnecting"), "keeps codex's headline: {notice}");
        assert!(notice.contains("high demand"), "keeps the cause: {notice}");
    }

    /// Successive attempts must REPLACE the previous card, not stack up: codex
    /// emits each of its five attempts more than once, so appending produced
    /// ten near-identical warnings for a single stall.
    #[tokio::test]
    async fn successive_retry_attempts_share_one_superseding_key() {
        let events = drive_codex(&[
            r#"{"method":"error","params":{"error":{"message":"Reconnecting... 1/5","additionalDetails":"high demand"},"willRetry":true,"threadId":"th1","turnId":"u1"},"emittedAtMs":1}"#,
            r#"{"method":"error","params":{"error":{"message":"Reconnecting... 2/5","additionalDetails":"high demand"},"willRetry":true,"threadId":"th1","turnId":"u1"},"emittedAtMs":2}"#,
        ])
        .await;

        let keys: Vec<Option<String>> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Notice { supersedes_key, .. } => Some(supersedes_key.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(keys.len(), 2, "both attempts are reported, got {events:?}");
        assert_eq!(
            keys[0], keys[1],
            "the same turn's attempts must share a key so the card updates"
        );
        assert!(
            keys[0].as_deref().is_some_and(|k| k.starts_with("codex-retry:turn-")),
            "the key is scoped to our turn, got {keys:?}"
        );
    }

    /// One prompt can make codex retry in more than one round, each round
    /// carrying a fresh `turnId`. Live 0.147.0 did exactly that and produced two
    /// cards both ending at "5/5", which reads as a bug. Rounds within one of
    /// OUR turns are one stall, so they share one card.
    #[tokio::test]
    async fn retry_rounds_within_one_prompt_share_one_card() {
        let events = drive_codex(&[
            r#"{"method":"error","params":{"error":{"message":"Reconnecting... 5/5"},"willRetry":true,"threadId":"th1","turnId":"u1"},"emittedAtMs":1}"#,
            r#"{"method":"error","params":{"error":{"message":"Reconnecting... 1/5"},"willRetry":true,"threadId":"th1","turnId":"u2"},"emittedAtMs":2}"#,
        ])
        .await;
        let keys: Vec<Option<String>> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Notice { supersedes_key, .. } => Some(supersedes_key.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(keys.len(), 2);
        assert_eq!(
            keys[0], keys[1],
            "a second retry round is the same stall, not a new card"
        );
    }

    /// The turn generation is what separates cards: a stall in a later turn must
    /// not rewrite a card sitting next to an earlier one.
    #[test]
    fn notices_from_different_turns_get_different_keys() {
        let notice = || SessionEvent::Notice {
            level: crate::event::NoticeLevel::Warning,
            message: "Reconnecting... 1/5".into(),
            localized: None,
            supersedes_key: Some(RETRY_NOTICE_KEY.to_string()),
        };
        let key_of = |event: SessionEvent| match event {
            SessionEvent::Notice { supersedes_key, .. } => supersedes_key,
            _ => None,
        };

        assert_ne!(
            key_of(scope_notice_to_turn(notice(), 1)),
            key_of(scope_notice_to_turn(notice(), 2))
        );
    }

    /// The retry card is localizable, with the CLI's own line as a parameter:
    /// the stream relay adds the running totals around it, because a stalled
    /// prompt can be replayed against a fresh codex process and a per-process
    /// counter would restart mid-card.
    #[tokio::test]
    async fn a_retry_card_carries_the_cli_line_as_an_i18n_parameter() {
        let events = drive_codex(&[
            r#"{"method":"error","params":{"error":{"message":"Reconnecting... 1/5","additionalDetails":"high demand"},"willRetry":true,"threadId":"th1","turnId":"u1"},"emittedAtMs":1}"#,
        ])
        .await;

        let localized = events
            .iter()
            .find_map(|e| match e {
                SessionEvent::Notice { localized, .. } => localized.clone(),
                _ => None,
            })
            .expect("the retry notice is localizable, got {events:?}");
        assert_eq!(localized.code, CODE_CODEX_RETRYING);
        assert_eq!(localized.params["detail"], "Reconnecting... 1/5 — high demand");
    }

    /// A notice with no key is untouched — scoping must not turn a plain notice
    /// into one that silently replaces something.
    #[test]
    fn a_notice_without_a_key_stays_without_one() {
        let plain = SessionEvent::Notice {
            level: crate::event::NoticeLevel::Info,
            message: "advisory".into(),
            localized: None,
            supersedes_key: None,
        };
        assert!(matches!(
            scope_notice_to_turn(plain, 3),
            SessionEvent::Notice {
                supersedes_key: None,
                ..
            }
        ));
    }

    /// A retry frame with nothing to say must not produce an empty card.
    #[tokio::test]
    async fn a_retry_without_text_only_heartbeats() {
        let events =
            drive_codex(&[r#"{"method":"error","params":{"error":{},"willRetry":true},"emittedAtMs":1}"#]).await;
        assert!(events.iter().any(|e| matches!(e, SessionEvent::Heartbeat)));
        assert!(
            !events.iter().any(|e| matches!(e, SessionEvent::Notice { .. })),
            "an empty error must not render a blank notice, got {events:?}"
        );
    }

    /// Build a FakeAgentIo replaying codex JSON-RPC lines, then collect the
    /// SessionEvents the backend surfaces (excluding the EOF Detached).
    async fn drive_codex(lines: &[&str]) -> Vec<SessionEvent> {
        let bytes = format!("{}\n", lines.join("\n")).into_bytes();
        let fake = FakeAgentIo::new(
            bytes,
            Some(crate::event::ExitStatusLite {
                code: Some(0),
                signal: None,
            }),
        );
        fake.release_exit();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let mut events = backend.events();
        let mut out = Vec::new();
        for _ in 0..100 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), events.next()).await {
                Ok(Some(env)) => {
                    assert_eq!(env.session_id, "codex-1", "demux by logical id");
                    match env.event {
                        SessionEvent::Detached { .. } => break,
                        ev => out.push(ev),
                    }
                }
                _ => break,
            }
        }
        out
    }

    // ===== build_input multimodal lowering =====

    #[test]
    fn build_input_maps_resource_link_to_text_reference_not_mention() {
        // A file attachment (ResourceLink) lowers to a TEXT element `[Attached file:
        // <path>]`, NOT codex `UserInput::Mention`. Source-verified root cause: a
        // Mention contributes nothing to the model prompt (models.rs:1762, only
        // App/Plugin refs are consumed) and leaves the turn loop unterminated → the
        // no-terminal hang. A text reference lets the model spawn its own read tool
        // (mirrors the claude adapter) and terminates cleanly. NO `mention` item must
        // ever be emitted for a file.
        let items = build_input(&[
            ContentBlock::Text("read this".into()),
            ContentBlock::ResourceLink {
                uri: "file:///tmp/sub/report.pdf".into(),
                mime_type: Some("application/pdf".into()),
            },
        ]);
        assert_eq!(items.len(), 2, "text + file-ref both lowered");
        assert_eq!(items[0]["type"], "text");
        assert_eq!(items[1]["type"], "text", "file rides Text, NOT mention");
        assert_eq!(
            items[1]["text"], "[Attached file: /tmp/sub/report.pdf]",
            "file:// scheme stripped, delivered as a text reference"
        );
        assert!(
            !items.iter().any(|i| i["type"] == "mention"),
            "no `mention` item is ever emitted for a file (it hangs the turn)"
        );
    }

    #[test]
    fn build_input_file_ref_keeps_bare_path_without_scheme() {
        // A path without a URL scheme passes through verbatim in the text reference.
        let items = build_input(&[ContentBlock::ResourceLink {
            uri: "/abs/notes.txt".into(),
            mime_type: None,
        }]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "text");
        assert_eq!(items[0]["text"], "[Attached file: /abs/notes.txt]");
    }

    // ===== ⭐ A1: ThreadItem closed-enum panic guard (freeze-blocker) =====

    /// PROPERTY (§F.3 input field-value boundary for the codex map_item entry,
    /// generalizes the A1 anti-panic guard below): for ANY `item/*` params shape —
    /// arbitrary `type` (known / unknown / absent), arbitrary / missing `id`,
    /// arbitrary payload — `map_item`:
    ///   1. NEVER panics (the real codex ThreadItem is a closed 16-variant enum that
    ///      would panic on an unknown type; we parse the type STRING with a
    ///      fallthrough, so malformed/future input is data, not a crash);
    ///   2. an UNKNOWN type → exactly an `AdapterSpecific{tag:"codex_item:<type>"}`
    ///      (never silently dropped, never a closed-variant construction).
    ///
    /// `map_item` is a pure free fn, so this needs no backend/fixture — it sweeps the
    /// type/field value space directly. claude tool_use has the sibling proptest
    /// (`prop_parse_assistant_never_emits_blank_name_toolcall`).
    #[test]
    fn prop_map_item_never_panics_unknown_type_is_adapter_specific() {
        use proptest::prelude::*;
        let known = prop_oneof![
            Just("agentMessage"),
            Just("reasoning"),
            Just("commandExecution"),
            Just("userMessage"),
            Just("collabAgent"),
        ]
        .prop_map(|s| s.to_string());
        // unknown type: any ident-ish string NOT in the known set.
        let unknown = "[a-zA-Z][a-zA-Z0-9]{0,12}".prop_filter("must be unknown", |s| {
            !matches!(
                s.as_str(),
                "agentMessage" | "reasoning" | "commandExecution" | "mcpToolCall" | "dynamicToolCall" | "fileChange"
            )
        });
        let type_strat = prop_oneof![Just(None), known.prop_map(Some), unknown.prop_map(Some)];
        let id_strat = prop_oneof![Just(None), "[a-z0-9-]{0,6}".prop_map(Some)];

        proptest!(|(ty in type_strat, id in id_strat, completed in any::<bool>())| {
            let mut item = serde_json::json!({"payload": {"k": 1}});
            if let Some(t) = &ty { item["type"] = serde_json::Value::String(t.clone()); }
            if let Some(i) = &id { item["id"] = serde_json::Value::String(i.clone()); }
            let params = serde_json::json!({"item": item, "threadId":"th", "turnId":"t"});

            let events = map_item(&params, completed); // (1) must not panic

            // (2) an unknown type (present, non-empty, not a known kind) ⟹ AdapterSpecific
            //     tagged codex_item:<type>. (Known kinds / absent type take other arms.)
            if let Some(t) = &ty {
                let is_known = matches!(
                    t.as_str(),
                    "agentMessage" | "reasoning" | "commandExecution" | "mcpToolCall"
                        | "dynamicToolCall" | "fileChange" | "userMessage" | "collabAgent"
                        | "webSearch" | "imageGeneration"
                );
                if !t.is_empty() && !is_known {
                    prop_assert!(
                        events.iter().any(|e| matches!(
                            e,
                            SessionEvent::AdapterSpecific { tag, .. } if tag == &format!("codex_item:{t}")
                        )),
                        "unknown item type {t:?} must surface as AdapterSpecific, got {events:?}"
                    );
                }
            }
        });
    }

    #[tokio::test]
    async fn a1_unknown_item_type_does_not_panic_falls_to_adapter_specific() {
        // A FUTURE codex ThreadItem.type our code has never seen. The real codex
        // ThreadItem is a CLOSED 16-variant enum that would PANIC on this during
        // deserialization. We parse `item.type` as a string with a fallthrough,
        // so it becomes AdapterSpecific — NEVER a panic. This is the §C5 A1 fix.
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th1"}}}"#,
            r#"{"jsonrpc":"2.0","method":"item/started","params":{"item":{"type":"aiGeneratedTimeMachine","id":"x9","payload":{"future":true}},"threadId":"th1","turnId":"t1"}}"#,
        ])
        .await;
        // The unknown item surfaced as AdapterSpecific, tagged, payload preserved.
        let found = events.iter().any(
            |e| matches!(e, SessionEvent::AdapterSpecific { tag, .. } if tag == "codex_item:aiGeneratedTimeMachine"),
        );
        assert!(
            found,
            "unknown ThreadItem.type must fall to AdapterSpecific (no panic), got {events:?}"
        );
    }

    // ===== Addendum 9: BackendBound (backend_session_id → conversation) =====

    #[tokio::test]
    async fn thread_started_lowers_backend_bound_with_thread_id() {
        // Addendum 9: on thread/started the adapter binds the threadId AND lowers
        // BackendBound{Some(threadId)} so the conversation can persist it as the
        // resume anchor. (The threadId still NEVER appears in any other envelope —
        // SessionEnvelope.session_id stays the logical id; BackendBound is the one
        // explicit channel.)
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-resume-anchor"}}}"#,
        ])
        .await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::BackendBound { backend_session_id: Some(tid) } if tid == "th-resume-anchor"
            )),
            "thread/started lowers BackendBound{{Some(threadId)}}, got {events:?}"
        );
    }

    #[tokio::test]
    async fn eof_lowers_backend_bound_none_when_session_was_bound() {
        // Addendum 9: backend gone (EOF) → BackendBound{None} so the conversation
        // clears its stale anchor (resuming a dead thread would fail).
        let events =
            drive_codex(&[r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-1"}}}"#]).await; // drive_codex EOFs after the scripted lines → reader runs the lost-binding path
        // Both a Some (on started) then a None (on EOF) must appear, in order.
        let bounds: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::BackendBound { backend_session_id } => Some(backend_session_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            bounds,
            vec![Some("th-1".to_string()), None],
            "BackendBound Some(on started) then None(on EOF/lost), got {bounds:?}"
        );
    }

    // ===== ⭐ A2/A3: reverse-RPC hang guard (freeze-blocker) =====

    #[tokio::test]
    async fn a2_a3_reverse_rpc_is_handled_not_hung() {
        // Blocking ServerRequests (method + id). If unhandled, the JSON-RPC
        // channel deadlocks and the turn hangs forever. We handle them by class:
        // pure infra (attestation) → auto-answered diagnostic; mid-session auth
        // refresh → Permission{Auth} (NOT auto-answered — a human answers). The
        // reader keeps draining either way (it never blocks on a response). We
        // assert all three are observed promptly — proving no hang.
        let events = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            drive_codex(&[
                r#"{"jsonrpc":"2.0","id":1,"method":"account/chatgptAuthTokens/refresh","params":{}}"#,
                r#"{"jsonrpc":"2.0","id":2,"method":"attestation/generate","params":{"nonce":"abc"}}"#,
                r#"{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"after the reverse-rpc"}}"#,
            ]),
        )
        .await
        .expect("must NOT hang on blocking reverse-RPC");

        // attestation → auto-answered diagnostic (pure infra, unblocked on the wire).
        let auto_answered = events
            .iter()
            .filter(
                |e| matches!(e, SessionEvent::AdapterSpecific { tag, .. } if tag == "codex_reverse_rpc_auto_answered"),
            )
            .count();
        assert_eq!(auto_answered, 1, "attestation auto-answered (pure infra)");
        // auth refresh → Permission{Auth} (mid-session re-auth; human-answered).
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::Permission {
                    kind: PermissionKind::Auth,
                    ..
                }
            )),
            "auth-token refresh surfaces as Permission(Auth), got {events:?}"
        );
        // Crucially, the reader CONTINUED past the blocking requests to deliver the
        // following notification — proving it never blocked.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::MessageDelta { text, .. } if text == "after the reverse-rpc")),
            "reader continued past reverse-RPC to deliver the next notification"
        );
    }

    #[tokio::test]
    async fn approval_reverse_rpc_surfaces_as_permission() {
        // commandExecution/fileChange approval ServerRequests (response = {decision},
        // which is what AnswerPermission writes) → user-facing Permission (Tool),
        // carrying the approval class as `tool_name` and the wire `params` verbatim
        // as `input` (unparsed passthrough) so the permission card can show the
        // approver what they are approving (AionUi issue #3779).
        for (m, title) in [
            ("item/commandExecution/requestApproval", "CommandExecution"),
            ("item/fileChange/requestApproval", "FileChange"),
        ] {
            let events = drive_codex(&[&format!(
                r#"{{"jsonrpc":"2.0","id":7,"method":"{m}","params":{{"command":"rm -rf /"}}}}"#
            )])
            .await;
            let perm = events
                .iter()
                .find_map(|e| match e {
                    SessionEvent::Permission {
                        kind: PermissionKind::Tool,
                        tool_name,
                        input,
                        ..
                    } => Some((tool_name.clone(), input.clone())),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{m} surfaces as Permission(Tool), got {events:?}"));
            assert_eq!(perm.0.as_deref(), Some(title), "{m} carries the approval class");
            let input = perm.1.expect("params ride as input for the card");
            assert_eq!(input["command"], "rm -rf /", "{m} passes the wire params through");
        }
    }

    /// REST-recovery parity with claude: a codex tool approval is LISTED by
    /// `pending_permission_requests()` while open, and the list is EMPTY after
    /// `dispatch(AnswerPermission)` consumes it. Without the registry the recovery
    /// read returned empty and a reloaded `waiting_confirmation` codex turn hung
    /// forever (the id needed to answer lived only in the missed live frame).
    #[tokio::test]
    async fn codex_pending_tool_approval_lists_open_then_clears_on_answer() {
        // Gate the reverse-RPC so it arrives AFTER we subscribe; keep the process
        // alive (never_exits) so the registry is not torn down by an EOF.
        let fake = FakeAgentIo::never_exits(Vec::new()).with_gated_tail(
            concat!(
                r#"{"jsonrpc":"2.0","id":7,"method":"item/commandExecution/requestApproval","params":{"command":"rm -rf /"}}"#,
                "\n",
            )
            .as_bytes()
            .to_vec(),
        );
        let releaser = fake.stdout_releaser();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let mut events = backend.events();
        releaser();

        // Wait for the live Permission frame so the reader has processed the insert.
        let request_id = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::Permission {
                    request_id,
                    kind: PermissionKind::Tool,
                    ..
                } = env.event
                {
                    return Some(request_id);
                }
            }
            None
        })
        .await
        .expect("timed out waiting for Permission")
        .expect("a Tool Permission frame");

        // OPEN: recovery lists exactly this pending approval, keyed by request_id.
        let open = backend.pending_permission_requests();
        assert_eq!(open.len(), 1, "one open approval recovered, got {open:?}");
        assert_eq!(open[0].request_id, request_id, "recovered id == live request_id");
        assert_eq!(open[0].tool_name, "CommandExecution");
        assert!(open[0].questions.is_none(), "codex approvals carry no question payload");

        // ANSWER: dispatch(AnswerPermission) → the registry entry is dropped.
        backend
            .dispatch(Command::AnswerPermission {
                request_id: request_id.clone(),
                decision: crate::PermissionDecision::Approved,
                selected: None,
                answers: Vec::new(),
            })
            .await
            .expect("AnswerPermission accepted");
        assert!(
            backend.pending_permission_requests().is_empty(),
            "recovery list EMPTY after the approval is answered"
        );
    }

    #[tokio::test]
    async fn command_execution_output_delta_maps_to_tool_output_delta() {
        // codex item/commandExecution/outputDelta → ToolOutputDelta (plaintext delta
        // keyed by itemId; verified live 0.139.0). The full output still rides the
        // completed item's aggregatedOutput → ToolResult.
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"item/commandExecution/outputDelta","params":{"threadId":"th1","turnId":"t1","itemId":"call_0","delta":"line-2\n"}}"#,
        ])
        .await;
        let got = events.iter().find_map(|e| match e {
            SessionEvent::ToolOutputDelta { item_id, text } => Some((item_id.clone(), text.clone())),
            _ => None,
        });
        assert_eq!(got, Some(("call_0".into(), "line-2\n".into())));
    }

    #[tokio::test]
    async fn diagnostic_notifications_map_to_notice_or_heartbeat() {
        use crate::event::NoticeLevel;
        // warning / guardianWarning → Notice{Warning, message}
        for m in ["warning", "guardianWarning"] {
            let events = drive_codex(&[&format!(
                r#"{{"jsonrpc":"2.0","method":"{m}","params":{{"message":"disk almost full"}}}}"#
            )])
            .await;
            assert!(
                events.iter().any(|e| matches!(
                    e,
                    SessionEvent::Notice { level: NoticeLevel::Warning, message, .. } if message == "disk almost full"
                )),
                "{m} → Notice(Warning), got {events:?}"
            );
        }
        // deprecationNotice → Notice{Info, summary — details}
        let dep = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"deprecationNotice","params":{"summary":"--foo is deprecated","details":"use --bar"}}"#,
        ])
        .await;
        assert!(
            dep.iter().any(|e| matches!(
                e,
                SessionEvent::Notice { level: NoticeLevel::Info, message, .. }
                    if message == "--foo is deprecated — use --bar"
            )),
            "deprecationNotice → Notice(Info) with joined details, got {dep:?}"
        );
        // configWarning → Notice{Warning, summary}
        let cfg = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"configWarning","params":{"summary":"unknown key X","path":"~/.codex/config.toml"}}"#,
        ])
        .await;
        assert!(
            cfg.iter()
                .any(|e| matches!(e, SessionEvent::Notice { level: NoticeLevel::Warning, message, .. } if message == "unknown key X")),
            "configWarning → Notice(Warning), got {cfg:?}"
        );
        // error{willRetry:true} → Heartbeat (transient retry, not a duplicate terminal)
        let retry = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"error","params":{"threadId":"th1","turnId":"t1","willRetry":true,"error":{"message":"503"}}}"#,
        ])
        .await;
        assert!(
            retry.iter().any(|e| matches!(e, SessionEvent::Heartbeat)),
            "error{{willRetry:true}} → Heartbeat, got {retry:?}"
        );
        // error{willRetry:false} with NO turn in-flight (drive_codex dispatches no
        // Send) → still no Notice/Heartbeat, and no synthetic terminal either (the
        // fatal-terminal arm is gated on turn_in_flight; a fatal error outside a turn
        // must not fold the FSM). The IN-FLIGHT fatal-terminal behavior is covered by
        // `codex_fatal_error_in_flight_synthesizes_terminal` below.
        let fatal = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"error","params":{"threadId":"th1","turnId":"t1","willRetry":false,"error":{"message":"boom"}}}"#,
        ])
        .await;
        assert!(
            !fatal
                .iter()
                .any(|e| matches!(e, SessionEvent::Notice { .. } | SessionEvent::Heartbeat)),
            "error{{willRetry:false}} (no turn in-flight) emits no Notice/Heartbeat, got {fatal:?}"
        );
    }

    /// #codex-no-terminal (fatal error): when a turn IS in flight and codex sends
    /// `error{willRetry:false}` (fatal, no guaranteed turn/completed), the reader
    /// synthesizes an is_error TurnResult so the FSM leaves Running instead of hanging
    /// forever. A later turn/completed for the same turn is absorbed (terminated guard).
    #[tokio::test]
    async fn codex_fatal_error_in_flight_synthesizes_terminal() {
        // gated_tail so the fatal error flows AFTER we mark the turn in-flight + subscribe.
        let fake = FakeAgentIo::never_exits(Vec::new()).with_gated_tail(
            concat!(
                r#"{"jsonrpc":"2.0","method":"turn/started","params":{"turn":{"id":"t1"}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","method":"error","params":{"threadId":"th1","turnId":"t1","willRetry":false,"error":{"message":"boom fatal"}}}"#,
                "\n",
            )
            .as_bytes()
            .to_vec(),
        );
        let releaser = fake.stdout_releaser();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        backend.mark_turn_in_flight_for_test();
        let mut events = backend.events();
        releaser();

        let tr = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::TurnResult {
                    is_error, result_text, ..
                } = env.event
                {
                    return Some((is_error, result_text));
                }
            }
            None
        })
        .await
        .expect("timed out")
        .expect("a TurnResult");
        assert!(tr.0, "fatal error in flight → is_error terminal");
        assert!(
            tr.1.contains("boom fatal"),
            "the fatal error message rides result_text, got {:?}",
            tr.1
        );
    }

    /// thread/status/changed → systemError with NO follow-up before EOF: the
    /// deferred systemError is flushed as an is_error terminal when the stream
    /// ends, so the FSM never hangs Running (pre-defer behavior preserved for
    /// the process-death path).
    #[tokio::test]
    async fn codex_system_error_status_synthesizes_terminal() {
        let sys_err = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"turn/started","params":{"turn":{"id":"t1"}}}"#,
            r#"{"jsonrpc":"2.0","method":"thread/status/changed","params":{"threadId":"th1","status":{"type":"systemError"}}}"#,
        ])
        .await;
        assert!(
            sys_err
                .iter()
                .any(|e| matches!(e, SessionEvent::TurnResult { is_error: true, .. })),
            "thread/status/changed→systemError must synthesize an is_error terminal (not hang), got {sys_err:?}"
        );
    }

    /// systemError carries NO detail (schema: SystemErrorThreadStatus has only
    /// `type`; codex 0.145.0 generate-json-schema v2/ThreadStatusChangedNotification.json)
    /// — the rich cause rides the `turn/completed` that follows it (live capture
    /// 0.145.0, bedrock stream failure: systemError → error{willRetry:false} same
    /// ms → turn/completed{failed, turn.error.message} +5ms). The deferred
    /// systemError must let turn/completed produce the terminal so result_text
    /// keeps the real cause instead of the opaque "codex reported a system error".
    #[tokio::test]
    async fn codex_system_error_then_turn_completed_preserves_rich_error() {
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"turn/started","params":{"turn":{"id":"t1"}}}"#,
            r#"{"jsonrpc":"2.0","method":"thread/status/changed","params":{"threadId":"th1","status":{"type":"systemError"}}}"#,
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"th1","turn":{"id":"t1","status":"failed","error":{"message":"stream disconnected before completion: failed to load AWS credentials: an error occurred while loading credentials","codexErrorInfo":"other"}}}}"#,
        ])
        .await;
        let terminals: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::TurnResult {
                    is_error, result_text, ..
                } => Some((*is_error, result_text.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(terminals.len(), 1, "exactly one terminal, got {terminals:?}");
        assert!(terminals[0].0, "failed turn → is_error terminal");
        assert!(
            terminals[0].1.contains("failed to load AWS credentials"),
            "terminal must keep the rich turn/completed error, got {:?}",
            terminals[0].1
        );
    }

    /// Same live-captured sequence, cut before turn/completed: the fatal
    /// `error{{willRetry:false}}` that follows systemError also carries the rich
    /// cause and must win over the opaque synthesized text.
    #[tokio::test]
    async fn codex_system_error_then_fatal_error_preserves_rich_error() {
        let fake = FakeAgentIo::never_exits(Vec::new()).with_gated_tail(
            concat!(
                r#"{"jsonrpc":"2.0","method":"turn/started","params":{"turn":{"id":"t1"}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","method":"thread/status/changed","params":{"threadId":"th1","status":{"type":"systemError"}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","method":"error","params":{"threadId":"th1","turnId":"t1","willRetry":false,"error":{"message":"stream disconnected before completion: failed to load AWS credentials: an error occurred while loading credentials","codexErrorInfo":"other"}}}"#,
                "\n",
            )
            .as_bytes()
            .to_vec(),
        );
        let releaser = fake.stdout_releaser();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        backend.mark_turn_in_flight_for_test();
        let mut events = backend.events();
        releaser();

        let tr = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::TurnResult {
                    is_error, result_text, ..
                } = env.event
                {
                    return Some((is_error, result_text));
                }
            }
            None
        })
        .await
        .expect("timed out")
        .expect("a TurnResult");
        assert!(tr.0, "fatal after systemError → is_error terminal");
        assert!(
            tr.1.contains("failed to load AWS credentials"),
            "terminal must keep the rich fatal-error message, got {:?}",
            tr.1
        );
    }

    /// Defensive fallback: systemError with NO follow-up on a STILL-OPEN stream
    /// (not observed live — capture shows error+turn/completed follow within ms)
    /// must still terminate the turn after the bounded grace instead of hanging
    /// Running forever. NOT a mid-turn watchdog: it only arms after codex has
    /// already declared the thread fatally errored. Gated SEGMENTS (second one
    /// never released) keep stdout open so the terminal can ONLY come from the
    /// grace timer — a gated tail would EOF and exercise the flush path instead.
    #[tokio::test]
    async fn codex_system_error_with_no_followup_times_out_with_generic_terminal() {
        let fake = FakeAgentIo::never_exits(Vec::new()).with_gated_segments(vec![
            concat!(
                r#"{"jsonrpc":"2.0","method":"turn/started","params":{"turn":{"id":"t1"}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","method":"thread/status/changed","params":{"threadId":"th1","status":{"type":"systemError"}}}"#,
                "\n",
            )
            .as_bytes()
            .to_vec(),
            // never released — keeps the stream open past the grace deadline
            b"{}\n".to_vec(),
        ]);
        let releaser = fake.segment_releaser();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        backend.mark_turn_in_flight_for_test();
        let mut events = backend.events();
        releaser();

        let tr = tokio::time::timeout(SYSTEM_ERROR_GRACE + std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::TurnResult {
                    is_error, result_text, ..
                } = env.event
                {
                    return Some((is_error, result_text));
                }
            }
            None
        })
        .await
        .expect("timed out waiting for the grace fallback")
        .expect("a TurnResult");
        assert!(tr.0, "unfollowed systemError → is_error terminal");
        assert!(
            tr.1.contains("codex reported a system error"),
            "fallback keeps the generic text, got {:?}",
            tr.1
        );
    }

    #[tokio::test]
    async fn turn_diff_updated_maps_to_turn_diff_updated() {
        // codex turn/diff/updated → TurnDiffUpdated (full cumulative unified diff;
        // verified live 0.139.0).
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"turn/diff/updated","params":{"threadId":"th1","turnId":"t1","diff":"diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n"}}"#,
        ])
        .await;
        let got = events.iter().find_map(|e| match e {
            SessionEvent::TurnDiffUpdated { diff } => Some(diff.clone()),
            _ => None,
        });
        assert!(got.is_some_and(|d| d.contains("diff --git")), "got {events:?}");
    }

    #[tokio::test]
    async fn elicitation_request_surfaces_as_permission_with_elicit_prefix() {
        // mcpServer/elicitation/request (LIVE-confirmed 0.139.0) → Permission(Tool),
        // request_id tagged with ELICIT_PREFIX so dispatch writes {action,content}
        // (not {decision}). The elicitation context (message/schema/mode) rides input.
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","id":5,"method":"mcpServer/elicitation/request","params":{"serverName":"elicitprobe","mode":"form","message":"Pick a color","requestedSchema":{"type":"object","properties":{"color":{"type":"string"}}}}}"#,
        ])
        .await;
        let perm = events
            .iter()
            .find_map(|e| match e {
                SessionEvent::Permission {
                    request_id,
                    kind: PermissionKind::Tool,
                    input,
                    ..
                } => Some((request_id.clone(), input.clone())),
                _ => None,
            })
            .expect("elicitation surfaces as Permission(Tool)");
        assert!(
            perm.0.starts_with(ELICIT_PREFIX),
            "request_id is tagged with the elicit prefix (so dispatch writes {{action,content}}), got {}",
            perm.0
        );
        let input = perm.1.expect("elicitation carries its context as input");
        assert_eq!(input["message"], "Pick a color");
        assert_eq!(input["mode"], "form");
        assert!(input["requestedSchema"]["properties"].get("color").is_some());
    }

    #[tokio::test]
    async fn command_request_approval_surfaces_as_permission_tool() {
        // item/commandExecution/requestApproval (the codex command/file approval the
        // live tests can only exercise non-deterministically — codex raises it at
        // model discretion) → Permission{Tool} carrying the reverse-RPC wire id as
        // request_id, NOT elicit-prefixed (so dispatch answers with `{decision}`, not
        // `{action,content}`). This pins the approval-surface mechanism deterministically
        // so the tolerant live approve/deny tests are integration smoke, not the only
        // coverage of the round-trip.
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","id":11,"method":"item/commandExecution/requestApproval","params":{"command":"echo hi","cwd":"/tmp"}}"#,
        ])
        .await;
        let (request_id, kind) = events
            .iter()
            .find_map(|e| match e {
                SessionEvent::Permission { request_id, kind, .. } => Some((request_id.clone(), *kind)),
                _ => None,
            })
            .expect("commandExecution/requestApproval surfaces as Permission");
        assert_eq!(kind, PermissionKind::Tool, "command approval is a Tool permission");
        assert_eq!(request_id, "11", "request_id is the reverse-RPC wire id (un-prefixed)");
        assert!(
            !request_id.starts_with(ELICIT_PREFIX),
            "a command approval must NOT be elicit-prefixed (it answers with {{decision}}), got {request_id}"
        );
    }

    #[tokio::test]
    async fn dispatch_answer_permission_command_writes_decision_body() {
        // The other half of the command-approval round-trip: answering a plain
        // (un-prefixed) approval id writes `{result:{decision:"accept"|"decline"|
        // "acceptForSession"}}` keyed by the wire id — the body codex's command/file
        // requestApproval expects. (The elicit path's {action,content} body is covered
        // by dispatch_answer_permission_elicit_writes_action_content_not_decision.)
        for (decision, expected) in [
            (super::super::types::PermissionDecision::Approved, "accept"),
            (super::super::types::PermissionDecision::Denied, "decline"),
            (super::super::types::PermissionDecision::AllowAlways, "acceptForSession"),
        ] {
            let fake = fake_with_binding("th-1", None);
            let captured = fake.captured_stdin();
            let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
            backend
                .dispatch(Command::AnswerPermission {
                    request_id: "11".into(),
                    decision,
                    selected: None,
                    answers: vec![],
                })
                .await
                .expect("accepted");
            let written = captured_str(&captured).await;
            assert!(
                written.contains(&format!(r#""decision":"{expected}""#)),
                "command answer writes {{decision:{expected}}}, got: {written}"
            );
            assert!(
                !written.contains(r#""action""#),
                "command answer must NOT use the elicit {{action}} body, got: {written}"
            );
            assert!(written.contains(r#""id":11"#), "keyed by the wire id, got: {written}");
        }
    }

    #[tokio::test]
    async fn m2_permissions_escalation_approval_is_not_surfaced_as_decision_permission() {
        // M2: item/permissions/requestApproval needs a {permissions, scope} response,
        // NOT the {decision} body AnswerPermission writes. Surfacing it as
        // Permission(Tool) would let the conversation answer it with a malformed body
        // codex rejects. It MUST fall through to the clean -32601 reject (unblocks the
        // channel) and surface a diagnostic — NOT a Permission.
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","id":9,"method":"item/permissions/requestApproval","params":{"reason":"escalate"}}"#,
        ])
        .await;
        assert!(
            !events.iter().any(|e| matches!(e, SessionEvent::Permission { .. })),
            "permissions/requestApproval must NOT surface as a {{decision}}-answerable Permission, got {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::AdapterSpecific { tag, .. } if tag == "codex_reverse_rpc")),
            "it falls through to the clean-reject diagnostic, got {events:?}"
        );
    }

    #[tokio::test]
    async fn m5_dropping_backend_aborts_reader_task() {
        // M5: dropping a CodexSessionBackend MUST abort the reader task (codex's
        // persistent stdout never EOFs, so the reader would block forever, pinning
        // the subprocess alive). We use never_exits so the reader truly blocks on
        // next_line; grab the JoinHandle's abort-handle, drop the backend, and assert
        // the task is finished (aborted).
        let fake = FakeAgentIo::never_exits(Vec::new());
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let handle = backend
            .suspend
            .current_abort_handle()
            .expect("live reader has an abort handle");
        assert!(
            !handle.is_finished(),
            "reader is live (blocked on next_line) before drop"
        );
        drop(backend);
        // Give the runtime a tick to process the abort.
        for _ in 0..40 {
            if handle.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(handle.is_finished(), "dropping the backend aborts the reader task (M5)");
    }

    #[tokio::test]
    async fn infra_reverse_rpc_writes_real_error_response_to_stdin() {
        // A2/A3 is now a REAL deadlock guard, not just a diagnostic: a blocking
        // PURE-INFRA ServerRequest (attestation — no human can satisfy it) gets an
        // actual JSON-RPC ERROR response written back to stdin (keyed by the same
        // id), so codex's blocking call returns instead of hanging the turn. The
        // fake captures the bytes we wrote. (Auth refresh is handled differently —
        // it surfaces as Permission{Auth}; see dispatch_answer_auth.)
        let bytes = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","id":42,"method":"attestation/generate","params":{"nonce":"x"}}"#
        )
        .into_bytes();
        let fake = FakeAgentIo::never_exits(bytes); // stay alive so we can read what we wrote
        let captured = fake.captured_stdin();
        let _backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""id":42"#),
            "response keyed to the request id, got: {written}"
        );
        assert!(
            written.contains(r#""error""#),
            "wrote a JSON-RPC error (not a hang), got: {written}"
        );
        assert!(
            written.contains("-32601"),
            "the unsupported-method code, got: {written}"
        );
    }

    // ===== notification → SessionEvent mapping (transport-agnostic fold) =====

    #[test]
    fn prefilled_agent_message_emits_text_streamed_one_does_not() {
        // The review/start verdict is a PRE-FILLED agentMessage: full `text` on
        // item/started, no deltas ever (live: samples/codex-cli/0.144.1/
        // review_start_uncommitted.jsonl, id `review_rollout_assistant`). Its text
        // must surface as a MessageDelta or the review verdict is silently lost.
        let prefilled: Value = serde_json::from_str(
            r#"{"item":{"type":"agentMessage","id":"review_rollout_assistant","text":"The change reverses the operator."},"threadId":"th1","turnId":"t1"}"#,
        )
        .unwrap();
        let events = map_item(&prefilled, false);
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::MessageDelta { text, .. } if text == "The change reverses the operator."
            )),
            "pre-filled agentMessage text must not be dropped, got: {events:?}"
        );

        // A STREAMED agentMessage starts empty (text carried by later deltas) —
        // no MessageDelta from the started edge (double-emit guard).
        let streamed: Value = serde_json::from_str(
            r#"{"item":{"type":"agentMessage","id":"m1","text":""},"threadId":"th1","turnId":"t1"}"#,
        )
        .unwrap();
        let events = map_item(&streamed, false);
        assert!(
            !events.iter().any(|e| matches!(e, SessionEvent::MessageDelta { .. })),
            "empty started text must not emit, got: {events:?}"
        );

        // The COMPLETED frame repeats the same text — must not re-emit it.
        let completed: Value = serde_json::from_str(
            r#"{"item":{"type":"agentMessage","id":"review_rollout_assistant","text":"The change reverses the operator."},"threadId":"th1","turnId":"t1"}"#,
        )
        .unwrap();
        let events = map_item(&completed, true);
        assert!(
            !events.iter().any(|e| matches!(e, SessionEvent::MessageDelta { .. })),
            "completed frame must not double-emit, got: {events:?}"
        );
    }

    #[tokio::test]
    async fn full_codex_turn_maps_to_canonical_events() {
        // A realistic codex turn: thread/started → turn/started → item deltas →
        // tokenUsage → turn/completed. Asserts the wire maps onto the SAME
        // canonical vocabulary claude uses (the §C transport-agnostic proof).
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th1"}}}"#,
            r#"{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"th1","turn":{"id":"t1"}}}"#,
            r#"{"jsonrpc":"2.0","method":"item/started","params":{"item":{"type":"agentMessage","id":"m1","text":""},"threadId":"th1","turnId":"t1"}}"#,
            r#"{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":"th1","turnId":"t1","itemId":"m1","delta":"hello"}}"#,
            r#"{"jsonrpc":"2.0","method":"thread/tokenUsage/updated","params":{"threadId":"th1","turnId":"t1","tokenUsage":{"last":{"inputTokens":10,"outputTokens":5,"totalTokens":15}}}}"#,
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"th1","turn":{"id":"t1","status":"completed"}}}"#,
        ])
        .await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::MessageDelta { text, .. } if text == "hello")),
            "agentMessage/delta → MessageDelta"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::UsageDelta {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    ..
                }
            )),
            "tokenUsage.last → UsageDelta (G6: native per-turn, no subtraction)"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::UsageDelta {
                    context_window: None,
                    ..
                }
            )),
            "codex never reports a window: its only figure is an unusable stand-in"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::TurnResult {
                    is_error: false,
                    outcome: TurnOutcome::Completed { .. },
                    ..
                }
            )),
            "turn/completed status:completed maps to TurnResult is_error:false Completed"
        );
        // GAP-E: item/started → ItemStarted bracket (the partial-lifecycle signal).
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::ItemStarted { item_id, kind: crate::event::ItemKind::Text } if item_id == "m1"
            )),
            "item/started(agentMessage) → ItemStarted{{m1, Text}}, got {events:?}"
        );
    }

    #[tokio::test]
    async fn gap_e_tool_item_emits_started_and_completed_brackets() {
        // GAP-E: a tool item gets ItemStarted (on started) + ToolCall, then
        // ToolResult + ItemCompleted (on completed) — the C5.3 frozen brackets
        // around the content events.
        // Bug-hunt #1: drive a real command body so the ToolCall assertion can pin the
        // LOAD-BEARING fields (name + input args), not just the id. map_item carries
        // them (codex_conn.rs ~1649, Gap #4/H2) and the conversation finalizer threads
        // them to the FileDiff/tool card — a regression dropping name or input was
        // unpinned by any codex test (the old assertion used `..`).
        let started = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"item/started","params":{"item":{"type":"commandExecution","id":"c1","command":"echo hi","cwd":"/tmp"},"threadId":"th1","turnId":"t1"}}"#,
        ])
        .await;
        assert!(
            started.iter().any(
                |e| matches!(e, SessionEvent::ItemStarted { item_id, kind: crate::event::ItemKind::Tool } if item_id == "c1")
            ),
            "tool item/started → ItemStarted{{Tool}}, got {started:?}"
        );
        let tool_call = started
            .iter()
            .find_map(|e| match e {
                SessionEvent::ToolCall {
                    tool_use_id,
                    name,
                    input,
                    ..
                } if tool_use_id == "c1" => Some((name.clone(), input.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("a ToolCall content event for c1, got {started:?}"));
        assert_eq!(
            tool_call.0, "commandExecution",
            "#1: ToolCall.name pinned (was unpinned)"
        );
        assert_eq!(
            tool_call.1.get("command").and_then(serde_json::Value::as_str),
            Some("echo hi"),
            "#1: ToolCall.input carries the command body (was unpinned → a producer regression went green), got {:?}",
            tool_call.1
        );
        let completed = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"commandExecution","id":"c1"},"threadId":"th1","turnId":"t1"}}"#,
        ])
        .await;
        assert!(
            completed
                .iter()
                .any(|e| matches!(e, SessionEvent::ItemCompleted { item_id, .. } if item_id == "c1")),
            "tool item/completed → ItemCompleted, got {completed:?}"
        );
        assert!(
            completed
                .iter()
                .any(|e| matches!(e, SessionEvent::ToolResult { tool_use_id, .. } if tool_use_id == "c1")),
            "and the ToolResult content event, got {completed:?}"
        );
    }

    #[tokio::test]
    async fn completed_tool_item_carries_is_error_on_failure() {
        // 009 R7/H3 (codex·ACP symmetry — codex leg): a completed tool item is
        // FAILED when status=="failed" OR a command exited non-zero (exitCode!=0).
        // The ToolResult MUST carry is_error so a failed tool is not rendered as a
        // success. codex had the parse code (map_item) but no failing-tool test.

        // (a) status:"failed" → is_error:true, output carried.
        let failed = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"mcpToolCall","id":"tf1","status":"failed","aggregatedOutput":"tool blew up"},"threadId":"th1","turnId":"t1"}}"#,
        ])
        .await;
        assert!(
            failed.iter().any(|e| matches!(e,
                SessionEvent::ToolResult { tool_use_id, is_error: true, content, .. }
                    if tool_use_id == "tf1"
                    && content.iter().any(|c| matches!(c, crate::event::ToolResultContent::Text(t) if t.contains("blew up"))))),
            "status:failed → ToolResult{{is_error:true}} with output, got {failed:?}"
        );

        // (b) non-zero exitCode → is_error:true (a command that ran but failed).
        let nonzero = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"commandExecution","id":"tf2","exitCode":1,"aggregatedOutput":"command not found"},"threadId":"th1","turnId":"t1"}}"#,
        ])
        .await;
        assert!(
            nonzero.iter().any(
                |e| matches!(e, SessionEvent::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "tf2")
            ),
            "non-zero exitCode → ToolResult{{is_error:true}}, got {nonzero:?}"
        );

        // (c) Control: status absent + exitCode 0 → success (is_error:false), so the
        // failure cases above are genuine signals, not constants.
        let ok = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"commandExecution","id":"ts1","exitCode":0,"aggregatedOutput":"done"},"threadId":"th1","turnId":"t1"}}"#,
        ])
        .await;
        assert!(
            ok.iter().any(
                |e| matches!(e, SessionEvent::ToolResult { tool_use_id, is_error: false, .. } if tool_use_id == "ts1")
            ),
            "exitCode 0 → ToolResult{{is_error:false}}, got {ok:?}"
        );
    }

    /// Protocol-audit fix (HIGH): MCP + dynamic-tool OUTPUT must reach ToolResult.
    /// Previously only `aggregatedOutput` (a commandExecution-only field) was read,
    /// so every mcpToolCall/dynamicToolCall rendered as an EMPTY card = silent data
    /// loss. Source-verified shapes (openai/codex v2/item.rs:299/313, mcp.rs:125):
    /// mcpToolCall result:{content:[{type:text,text}|{type:image,data,mimeType}],
    /// structuredContent} + error:{message}; dynamicToolCall contentItems:[{type:
    /// inputText,text}|{type:inputImage,imageUrl}].
    #[tokio::test]
    async fn mcp_and_dynamic_tool_output_is_carried_not_dropped() {
        // (a) mcpToolCall with text + structuredContent in result.
        let mcp = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"mcpToolCall","id":"m1","status":"completed","result":{"content":[{"type":"text","text":"weather: sunny"}],"structuredContent":{"temp":21}}},"threadId":"th1","turnId":"t1"}}"#,
        ])
        .await;
        let mcp_content = mcp.iter().find_map(|e| match e {
            SessionEvent::ToolResult {
                tool_use_id, content, ..
            } if tool_use_id == "m1" => Some(content.clone()),
            _ => None,
        });
        let mcp_content = mcp_content.expect("mcpToolCall completed → ToolResult");
        assert!(
            mcp_content
                .iter()
                .any(|c| matches!(c, crate::event::ToolResultContent::Text(t) if t.contains("weather: sunny"))),
            "MCP result.content text must reach ToolResult (was dropped), got {mcp_content:?}"
        );
        assert!(
            mcp_content
                .iter()
                .any(|c| matches!(c, crate::event::ToolResultContent::Text(t) if t.contains("21"))),
            "MCP structuredContent must be carried, got {mcp_content:?}"
        );

        // (b) a FAILED mcpToolCall with error.message (no aggregatedOutput) → the
        // cause reaches the red card.
        let mcp_err = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"mcpToolCall","id":"m2","status":"failed","error":{"message":"upstream 500"}},"threadId":"th1","turnId":"t1"}}"#,
        ])
        .await;
        assert!(
            mcp_err.iter().any(|e| matches!(e,
                SessionEvent::ToolResult { tool_use_id, is_error: true, content, .. }
                    if tool_use_id == "m2"
                    && content.iter().any(|c| matches!(c, crate::event::ToolResultContent::Text(t) if t.contains("upstream 500"))))),
            "failed mcpToolCall error.message must reach ToolResult, got {mcp_err:?}"
        );

        // (c) dynamicToolCall contentItems (inputText) → carried.
        let dyn_call = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"dynamicToolCall","id":"d1","status":"completed","contentItems":[{"type":"inputText","text":"dynamic answer"}]},"threadId":"th1","turnId":"t1"}}"#,
        ])
        .await;
        assert!(
            dyn_call.iter().any(|e| matches!(e,
                SessionEvent::ToolResult { tool_use_id, content, .. }
                    if tool_use_id == "d1"
                    && content.iter().any(|c| matches!(c, crate::event::ToolResultContent::Text(t) if t.contains("dynamic answer"))))),
            "dynamicToolCall contentItems text must reach ToolResult (was dropped), got {dyn_call:?}"
        );
    }

    #[tokio::test]
    async fn mcp_startup_status_maps_to_provisioning_phases() {
        // codex·ACP symmetry (target: claude b_claude_init_captures..._mcp_provisioning):
        // the SERVER→CLIENT notification `mcpServer/startupStatus/updated` must map per
        // status to the matching ProvisioningPhase, carrying `error` as the reason on
        // failure/cancel. Regression guard for the dead-arm bug: codex used to match
        // the WRONG prefix `mcpServerStatus` (an outbound request) and hardcode
        // ToolsWaiting → a real startup notification produced NO Provisioning.
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"mcpServer/startupStatus/updated","params":{"name":"fs","status":"starting"}}"#,
            r#"{"jsonrpc":"2.0","method":"mcpServer/startupStatus/updated","params":{"name":"fs","status":"ready"}}"#,
            r#"{"jsonrpc":"2.0","method":"mcpServer/startupStatus/updated","params":{"name":"bad","status":"failed","error":"spawn ENOENT"}}"#,
            r#"{"jsonrpc":"2.0","method":"mcpServer/startupStatus/updated","params":{"name":"oauth","status":"cancelled","error":"user aborted"}}"#,
        ])
        .await;
        let phases: Vec<&ProvisioningPhase> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Provisioning { phase } => Some(phase),
                _ => None,
            })
            .collect();
        assert_eq!(
            phases.len(),
            4,
            "one Provisioning per startup notification, got {phases:?}"
        );
        assert!(
            matches!(phases[0], ProvisioningPhase::ToolsWaiting),
            "starting→ToolsWaiting"
        );
        assert!(matches!(phases[1], ProvisioningPhase::ToolsReady), "ready→ToolsReady");
        assert!(
            matches!(phases[2], ProvisioningPhase::LoadFailed { reason } if reason == "spawn ENOENT"),
            "failed→LoadFailed{{reason}} carrying the error text, got {:?}",
            phases[2]
        );
        assert!(
            matches!(phases[3], ProvisioningPhase::Degraded { reason } if reason == "user aborted"),
            "cancelled→Degraded{{reason}}, got {:?}",
            phases[3]
        );
    }

    #[tokio::test]
    async fn mcp_oauth_login_failure_maps_to_degraded() {
        // success:false (login failed) → Degraded (server up but unauthorized; mirrors
        // claude needs-auth → Degraded). success:true carries no FSM signal here.
        let failed = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"mcpServer/oauthLogin/completed","params":{"name":"gh","success":false,"error":"token expired"}}"#,
        ])
        .await;
        assert!(
            failed.iter().any(|e| matches!(e,
                SessionEvent::Provisioning { phase: ProvisioningPhase::Degraded { reason } } if reason == "token expired")),
            "oauth success:false → Degraded{{reason}}, got {failed:?}"
        );
        let ok = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"mcpServer/oauthLogin/completed","params":{"name":"gh","success":true}}"#,
        ])
        .await;
        assert!(
            !ok.iter().any(|e| matches!(e, SessionEvent::Provisioning { .. })),
            "oauth success:true emits no Provisioning, got {ok:?}"
        );
    }

    #[tokio::test]
    async fn interrupted_turn_maps_to_cancelled_outcome() {
        // ⚠️ REAL ORDERING (M3): codex sends `status→idle` BEFORE
        // `turn/completed{status:interrupted}`. The interrupted outcome MUST
        // survive — the deferred idle must NOT absorb it into a clean EndTurn.
        // turn.status:interrupted → TurnResult{is_error:false, Cancelled} (cancel
        // is NOT an error; §C2/O3 + the cancel≠error invariant).
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"thread/status/changed","params":{"threadId":"th1","status":{"type":"idle"}}}"#,
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"th1","turn":{"id":"t1","status":"interrupted"}}}"#,
        ])
        .await;
        assert_eq!(
            count_turn_results(&events),
            1,
            "exactly one terminal (the deferred idle is absorbed by completed), got {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::TurnResult {
                    is_error: false,
                    outcome: TurnOutcome::Cancelled { .. },
                    ..
                }
            )),
            "interrupted → Cancelled outcome (NOT a clean EndTurn from the leading idle), is_error:false, got {events:?}"
        );
    }

    #[tokio::test]
    async fn failed_turn_maps_to_error_routing() {
        // ⚠️ REAL ORDERING (M3): `status→idle` arrives BEFORE
        // `turn/completed{status:failed}`. The error bits MUST survive — if the
        // leading idle won, a failed turn would be reported is_error:false and the
        // reducer would route it to Idle instead of Error{Backend} (silent failure).
        // turn.status:failed → TurnResult{is_error:true} + Failed + error/status.
        // ⚠️ C-3: the error uses the REAL codex wire shape — httpStatusCode is
        // NESTED inside the externally-tagged codexErrorInfo variant
        // ({"httpConnectionFailed":{"httpStatusCode":503}}), NOT a flat top-level
        // field. (The old fixture used the flat shape and masked the wrong-path bug.)
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"thread/status/changed","params":{"threadId":"th1","status":{"type":"idle"}}}"#,
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"th1","turn":{"id":"t1","status":"failed","error":{"message":"server overloaded","codexErrorInfo":{"httpConnectionFailed":{"httpStatusCode":503}}}}}}"#,
        ])
        .await;
        assert_eq!(count_turn_results(&events), 1, "exactly one terminal, got {events:?}");
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::TurnResult {
                    is_error: true,
                    api_error_status: Some(503),
                    outcome: TurnOutcome::Failed,
                    ..
                }
            )),
            "failed → is_error:true + api_error_status:503 (from NESTED codexErrorInfo) + Failed, got {events:?}"
        );
    }

    #[tokio::test]
    async fn failed_turn_without_http_status_is_none_not_panic() {
        // C-3 boundary: a CodexErrorInfo variant WITHOUT httpStatusCode → api_error_status
        // None, is_error:true. SHAPE CALIBRATED to schema (was a guessed object): the
        // schema (codex-cli/0.137.0/schema-full/ServerNotification.json CodexErrorInfo)
        // defines `serverOverloaded` as a BARE STRING enum member (the string oneOf arm:
        // contextWindowExceeded/usageLimitExceeded/serverOverloaded/…), NOT an object —
        // only httpConnectionFailed/responseStream*/responseTooManyFailedAttempts are
        // object-shaped (those carry httpStatusCode). So the real wire is
        // `codexErrorInfo:"serverOverloaded"`. The prior `{"serverOverloaded":{}}` object
        // wrapper contradicted the schema (contracts README #9).
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"th1","turn":{"id":"t1","status":"failed","error":{"message":"overloaded","codexErrorInfo":"serverOverloaded"}}}}"#,
        ])
        .await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::TurnResult {
                    is_error: true,
                    api_error_status: None,
                    outcome: TurnOutcome::Failed,
                    ..
                }
            )),
            "status-less error variant → is_error:true, api_error_status:None, got {events:?}"
        );
    }

    /// LIVE-found shape (codex 0.139.0 bad-model turn): a real provider HTTP error
    /// collapses `codexErrorInfo` to the bare string `"other"` (no structured
    /// httpStatusCode), and the status lives ONLY in the message
    /// (`"unexpected status 404 Not Found: …"`). The message-text fallback must lift
    /// it so a real provider error still carries its status. Pins the live shape the
    /// `codex_live_bad_model_folds_to_error_terminal` test surfaced.
    #[tokio::test]
    async fn failed_turn_lifts_http_status_from_message_when_codex_error_info_is_other() {
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"th1","turn":{"id":"t1","status":"failed","error":{"message":"unexpected status 404 Not Found: The model 'x' does not exist, request id: req_abc123","codexErrorInfo":"other"}}}}"#,
        ])
        .await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::TurnResult {
                    is_error: true,
                    api_error_status: Some(404),
                    outcome: TurnOutcome::Failed,
                    ..
                }
            )),
            "codexErrorInfo:\"other\" + status-in-message → api_error_status lifted from text (404), got {events:?}"
        );
    }

    /// The message-status extractor must NOT mis-fire on arbitrary numbers (model
    /// ids, request ids, token counts) — only on the anchored `status <NNN>` shape.
    #[test]
    fn extract_http_status_from_message_is_anchored_not_greedy() {
        assert_eq!(
            extract_http_status_from_message("unexpected status 404 Not Found: req_999"),
            Some(404)
        );
        assert_eq!(
            extract_http_status_from_message("HTTP status 503 from upstream"),
            Some(503)
        );
        // No "status N" shape → None (not a stray number match).
        assert_eq!(
            extract_http_status_from_message("the model gpt-5.5 returned 12345 tokens"),
            None
        );
        assert_eq!(extract_http_status_from_message("request id req_404abc failed"), None);
        // Out-of-range "status" number → None (not a real HTTP code).
        assert_eq!(extract_http_status_from_message("status 9999 weird"), None);
    }

    #[tokio::test]
    async fn collab_agent_maps_to_subagent_update() {
        // A codex collab/spawned agent item → SubagentUpdate (§6b b1) — the same
        // canonical subagent channel claude Task/Workflow uses. The spawn-in-flight
        // frame carries an EMPTY agentsStates → fall back to the tool-call id so the
        // action still surfaces (Running, no parent yet).
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"item/started","params":{"item":{"type":"collabAgentToolCall","id":"agent-9","model":"o3"},"threadId":"th1","turnId":"t1"}}"#,
        ])
        .await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::SubagentUpdate { r#ref, status: SubagentStatus::Running, .. } if r#ref == "agent-9"
            )),
            "collabAgentToolCall (empty agentsStates) → SubagentUpdate keyed by item.id, got {events:?}"
        );
    }

    #[tokio::test]
    async fn collab_agent_keys_on_child_thread_with_real_status_and_parent() {
        // D1: the REAL collab wire shape (collab_spawn_full.jsonl): the completed
        // frame carries the spawned child in `agentsStates: {childThreadId ->
        // {status}}` + the spawning parent in `senderThreadId`. The roster entry MUST
        // be keyed by the CHILD threadId (codex agentId, state.rs:80), carry the
        // child's REAL lifecycle status (pendingInit, NOT a coarse completed-bool),
        // and the spawn edge (parent_ref = senderThreadId).
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"collabAgentToolCall","id":"call_2","tool":"spawnAgent","status":"completed","senderThreadId":"019eabe9-parent","receiverThreadIds":["019eabea-child"],"model":"openai.gpt-5.5","agentsStates":{"019eabea-child":{"status":"pendingInit","message":null}}},"threadId":"th1","turnId":"t1"}}"#,
        ])
        .await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::SubagentUpdate {
                    r#ref,
                    status: SubagentStatus::PendingInit,
                    parent_ref: Some(p),
                    label: Some(l),
                    kind: None,
                } if r#ref == "019eabea-child" && p == "019eabe9-parent" && l == "openai.gpt-5.5"
            )),
            "real collab frame → SubagentUpdate keyed by CHILD thread, status from agentsStates, parent edge from senderThreadId, got {events:?}"
        );
        // The tool-call id (call_2) must NOT leak as a roster ref when a child thread
        // is known (else a phantom entry never prunes).
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::SubagentUpdate { r#ref, .. } if r#ref == "call_2")),
            "with a known child thread, the tool-call id must not also surface as a roster entry, got {events:?}"
        );
    }

    #[test]
    fn collab_status_maps_seven_to_six() {
        // The 7-state CollabAgentStatus → our 6-state SubagentStatus; notFound folds
        // to Shutdown (terminal, prunes), unknown → Running (active, never wedges).
        assert!(matches!(map_collab_status("pendingInit"), SubagentStatus::PendingInit));
        assert!(matches!(map_collab_status("running"), SubagentStatus::Running));
        assert!(matches!(map_collab_status("interrupted"), SubagentStatus::Interrupted));
        assert!(matches!(map_collab_status("completed"), SubagentStatus::Completed));
        assert!(matches!(map_collab_status("errored"), SubagentStatus::Errored));
        assert!(matches!(map_collab_status("shutdown"), SubagentStatus::Shutdown));
        assert!(
            matches!(map_collab_status("notFound"), SubagentStatus::Shutdown),
            "notFound has no 7th state → Shutdown (terminal)"
        );
        assert!(
            matches!(map_collab_status("someFutureCodexStatus"), SubagentStatus::Running),
            "unknown status → Running (active), never a wedged terminal"
        );
    }

    /// A FakeAgentIo that emits a `thread/started{thread.id=tid}` (so the reader
    /// binds the threadId every `turn/*` dispatch needs) optionally followed by a
    /// `turn/started{turn.id=turnid}` (so the active-turn token for steer/interrupt
    /// is set), then EOFs cleanly. Clone `captured_stdin()` BEFORE build to assert
    /// on the dispatched frames.
    fn fake_with_binding(tid: &str, turn_id: Option<&str>) -> FakeAgentIo {
        let mut lines = vec![format!(
            r#"{{"jsonrpc":"2.0","method":"thread/started","params":{{"thread":{{"id":"{tid}"}}}}}}"#
        )];
        if let Some(t) = turn_id {
            lines.push(format!(
                r#"{{"jsonrpc":"2.0","method":"turn/started","params":{{"threadId":"{tid}","turn":{{"id":"{t}"}}}}}}"#
            ));
        }
        let bytes = format!("{}\n", lines.join("\n")).into_bytes();
        let fake = FakeAgentIo::new(
            bytes,
            Some(crate::event::ExitStatusLite {
                code: Some(0),
                signal: None,
            }),
        );
        fake.release_exit();
        fake
    }

    /// Drain the captured-stdin buffer to a String after dispatch settles. Polls
    /// briefly because the duplex→capture copy is on a background task.
    async fn captured_str(captured: &Arc<tokio::sync::Mutex<Vec<u8>>>) -> String {
        for _ in 0..40 {
            let s = String::from_utf8_lossy(&captured.lock().await.clone()).to_string();
            if !s.trim().is_empty() {
                // settle a touch more so the whole frame is flushed
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                return String::from_utf8_lossy(&captured.lock().await.clone()).to_string();
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        String::new()
    }

    #[tokio::test]
    async fn dispatch_answer_permission_elicit_writes_action_content_not_decision() {
        // An elicit-prefixed request_id → dispatch writes the MCP elicitation body
        // `{action, content}` (NOT `{decision}`), keyed by the stripped wire id.
        // A plain (un-prefixed) request_id still writes `{decision}`.
        let fake = fake_with_binding("th-1", None);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        backend
            .dispatch(Command::AnswerPermission {
                request_id: format!("{ELICIT_PREFIX}5"),
                decision: super::super::types::PermissionDecision::Approved,
                selected: None,
                answers: vec![],
            })
            .await
            .expect("accepted");
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""action":"accept""#),
            "elicit answer writes {{action}}, got: {written}"
        );
        assert!(
            written.contains(r#""content":{}"#),
            "elicit answer writes a content body, got: {written}"
        );
        assert!(
            !written.contains(r#""decision""#),
            "elicit answer must NOT use the approval {{decision}} body, got: {written}"
        );
        assert!(
            written.contains(r#""id":5"#),
            "keyed by the stripped wire id (numeric), got: {written}"
        );
    }

    #[tokio::test]
    async fn dispatch_send_writes_turn_start_with_thread_id() {
        // dispatch(Send) bumps turn_gen + returns Started, writing the REAL codex
        // wire `turn/start{threadId, input:[{type:text,text}]}` (verified against
        // the aion-probe transcripts — NOT the fictional `sendUserTurn`). Requires
        // the bound threadId, which the fake supplies via thread/started.
        let fake = fake_with_binding("th-77", None);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let receipt = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("do it".into())],
                metadata: super::super::types::CommandMeta::default(),
            })
            .await
            .expect("accepted");
        assert_eq!(receipt.turn_gen, 1);
        assert_eq!(receipt.admission, Admission::Started);
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""method":"turn/start""#),
            "wrote turn/start, got: {written}"
        );
        assert!(
            written.contains(r#""threadId":"th-77""#),
            "carries the bound threadId, got: {written}"
        );
        assert!(
            written.contains("do it"),
            "frame carries the prompt text, got: {written}"
        );
        assert!(
            !written.contains("sendUserTurn"),
            "must NOT use the fictional sendUserTurn method"
        );
    }

    /// Bridge-parity slash routing (ELECTRON-3PX): the parser is a faithful port
    /// of codex-acp v0.14.0 `extract_slash_command` + `handle_prompt` match arms
    /// (thread.rs:4195 / :3252) — including the "unknown or arg-less
    /// review-branch/commit falls through as plain text" behavior.
    #[test]
    fn route_slash_command_maps_bridge_table() {
        let text = |s: &str| vec![ContentBlock::Text(s.into())];
        assert_eq!(route_slash_command(&text("/compact")), Some(SlashRoute::Compact));
        assert_eq!(route_slash_command(&text("/init")), Some(SlashRoute::Init));
        assert_eq!(route_slash_command(&text("/logout")), Some(SlashRoute::Logout));
        assert_eq!(
            route_slash_command(&text("/review")),
            Some(SlashRoute::Review(json!({ "type": "uncommittedChanges" })))
        );
        assert_eq!(
            route_slash_command(&text("/review focus on error handling")),
            Some(SlashRoute::Review(
                json!({ "type": "custom", "instructions": "focus on error handling" })
            ))
        );
        assert_eq!(
            route_slash_command(&text("/review-branch main")),
            Some(SlashRoute::Review(json!({ "type": "baseBranch", "branch": "main" })))
        );
        assert_eq!(
            route_slash_command(&text("/review-commit abc123")),
            Some(SlashRoute::Review(json!({ "type": "commit", "sha": "abc123" })))
        );
        // Arg-less branch/commit reviews fall through as plain text (bridge guards).
        assert_eq!(route_slash_command(&text("/review-branch")), None);
        assert_eq!(route_slash_command(&text("/review-commit")), None);
        // Unknown command / plain text / bare slash / non-text first block → None.
        assert_eq!(route_slash_command(&text("/frobnicate now")), None);
        assert_eq!(route_slash_command(&text("hello")), None);
        assert_eq!(route_slash_command(&text("/")), None);
        assert_eq!(route_slash_command(&[]), None);
    }

    /// The shared name parser owns the slash grammar. Assert it matches the
    /// exact rules `route_slash_command` used inline before the extraction:
    /// must start with `/` (no leading whitespace), name runs to the first
    /// Unicode whitespace, must be non-empty, and the `/` is stripped.
    #[test]
    fn slash_command_name_parses_leading_name() {
        assert_eq!(slash_command_name("/compact"), Some("compact"));
        assert_eq!(slash_command_name("/review focus on X"), Some("review"));
        assert_eq!(slash_command_name("/review-branch main"), Some("review-branch"));
        // Trailing content after the first whitespace is ignored; a newline also
        // terminates the name (Unicode whitespace).
        assert_eq!(slash_command_name("/init\nmore"), Some("init"));
        // Not a command: no leading slash, leading whitespace, or empty name.
        assert_eq!(slash_command_name("hello"), None);
        assert_eq!(slash_command_name(" /compact"), None);
        assert_eq!(slash_command_name("/"), None);
        assert_eq!(slash_command_name("/ compact"), None);
    }

    /// AC10 (ELECTRON-3RN): the two independently hard-coded codex command-name
    /// sets — `builtin_slash_commands()` (the advertised catalog / recognition
    /// source) and `route_slash_command()`'s match arms (the real translation to
    /// native ops) — MUST stay in lock-step. If either table gains or loses a
    /// command without the other following, this fails immediately.
    #[test]
    fn builtin_slash_commands_match_route_table() {
        use std::collections::BTreeSet;

        // Names the advertised catalog claims codex supports.
        let advertised: BTreeSet<String> = builtin_slash_commands().into_iter().map(|c| c.name).collect();

        // Every advertised name must actually route to a native op. `review-branch`
        // / `review-commit` need a non-empty argument to satisfy their guards, so
        // probe with an argument; a bare name would false-negative on those two.
        for name in &advertised {
            let probe = format!("/{name} arg");
            assert!(
                route_slash_command(&[ContentBlock::Text(probe.clone())]).is_some(),
                "advertised command `{name}` does not route to a native op"
            );
        }

        // And the reverse: no name routes that the catalog fails to advertise.
        // Enumerate the full universe the route table recognizes today; if a new
        // arm is added to `route_slash_command`, add it here AND to the catalog.
        let route_universe = ["review", "review-branch", "review-commit", "init", "compact", "logout"];
        for name in route_universe {
            assert!(
                route_slash_command(&[ContentBlock::Text(format!("/{name} arg"))]).is_some(),
                "route universe entry `{name}` unexpectedly does not route"
            );
            assert!(
                advertised.contains(name),
                "route table recognizes `{name}` but the advertised catalog omits it"
            );
        }
        let route_universe_set: BTreeSet<String> = route_universe.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            advertised, route_universe_set,
            "advertised catalog and route table command-name sets diverged"
        );
    }

    /// #101/ELECTRON-3PX: codex advertises the bridge's static 6-command table
    /// (codex-acp v0.14.0 thread.rs:2894 builtin_commands) so the in-session `/`
    /// menu is no longer empty on the direct-CLI path.
    #[test]
    fn capabilities_advertise_bridge_slash_commands() {
        let names: Vec<String> = codex_capabilities()
            .slash_commands
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(
            names,
            vec!["review", "review-branch", "review-commit", "init", "compact", "logout"]
        );
    }

    #[tokio::test]
    async fn dispatch_send_slash_review_writes_review_start() {
        // `/review` → review/start{threadId, target:{uncommittedChanges}} and runs
        // as a REAL turn (Started + turn_gen bump) — the wire lifecycle is a normal
        // turn/started→turn/completed (verified live:
        // samples/codex-cli/0.144.1/review_start_uncommitted.jsonl).
        let fake = fake_with_binding("th-77", None);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let receipt = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("/review".into())],
                metadata: super::super::types::CommandMeta::default(),
            })
            .await
            .expect("accepted");
        assert_eq!(receipt.admission, Admission::Started);
        assert_eq!(receipt.turn_gen, 1);
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""method":"review/start""#),
            "wrote review/start, got: {written}"
        );
        assert!(
            written.contains(r#""type":"uncommittedChanges""#),
            "bare /review targets uncommitted changes, got: {written}"
        );
        assert!(
            written.contains(r#""threadId":"th-77""#),
            "carries the bound threadId, got: {written}"
        );
    }

    #[tokio::test]
    async fn dispatch_send_slash_compact_writes_thread_compact_start() {
        // `/compact` → thread/compact/start{threadId}; also a real wire turn
        // (verified live: samples/codex-cli/0.144.1/thread_compact.jsonl).
        let fake = fake_with_binding("th-77", None);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let receipt = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("/compact".into())],
                metadata: super::super::types::CommandMeta::default(),
            })
            .await
            .expect("accepted");
        assert_eq!(receipt.admission, Admission::Started);
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""method":"thread/compact/start""#),
            "wrote thread/compact/start, got: {written}"
        );
        assert!(
            !written.contains(r#""input""#),
            "compact carries no input payload, got: {written}"
        );
    }

    #[tokio::test]
    async fn dispatch_send_slash_init_sends_canned_prompt_as_turn() {
        // `/init` → a normal turn/start whose input is the bridge's canned
        // AGENTS.md prompt (codex-acp v0.14.0 thread.rs:3255), NOT the literal
        // "/init" text (codex would treat that as a prompt about a slash).
        let fake = fake_with_binding("th-77", None);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let receipt = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("/init".into())],
                metadata: super::super::types::CommandMeta::default(),
            })
            .await
            .expect("accepted");
        assert_eq!(receipt.admission, Admission::Started);
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""method":"turn/start""#),
            "init rides a normal turn, got: {written}"
        );
        assert!(
            written.contains("AGENTS.md"),
            "carries the canned init prompt, got: {written}"
        );
        assert!(
            !written.contains("/init"),
            "the literal slash text must not reach codex, got: {written}"
        );
    }

    #[tokio::test]
    async fn dispatch_send_slash_logout_writes_account_logout_noturn() {
        use futures_util::StreamExt as _;
        // `/logout` → account/logout{params:null}; NO turn lifecycle follows
        // (bridge: auth.logout() then auth_required) → NoTurn, no turn_gen bump,
        // and a user-visible Notice (there is no other output for this command).
        let fake = fake_with_binding("th-77", None);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let mut events = backend.events();
        let receipt = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("/logout".into())],
                metadata: super::super::types::CommandMeta::default(),
            })
            .await
            .expect("accepted");
        assert_eq!(receipt.admission, Admission::NoTurn);
        assert_eq!(receipt.turn_gen, 0, "no turn_gen bump for logout");
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""method":"account/logout""#),
            "wrote account/logout, got: {written}"
        );
        let notice = tokio::time::timeout(std::time::Duration::from_millis(500), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::Notice { message, .. } = env.event {
                    return Some(message);
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        assert!(
            notice.is_some_and(|m| m.contains("Logged out")),
            "logout emits a user-visible Notice"
        );
    }

    #[tokio::test]
    async fn dispatch_send_unknown_slash_passes_through_as_plain_text() {
        // Unknown slash names fall through verbatim (bridge `_ =>` arm) — never a
        // rejection, never a silent drop.
        let fake = fake_with_binding("th-77", None);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let receipt = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("/frobnicate the widgets".into())],
                metadata: super::super::types::CommandMeta::default(),
            })
            .await
            .expect("accepted");
        assert_eq!(receipt.admission, Admission::Started);
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""method":"turn/start""#),
            "unknown slash rides a normal turn, got: {written}"
        );
        assert!(
            written.contains("/frobnicate the widgets"),
            "text reaches codex verbatim, got: {written}"
        );
    }

    #[tokio::test]
    async fn dispatch_send_during_active_turn_is_noturn_and_opens_no_second_turn_gen() {
        use futures_util::StreamExt as _;
        // 009 R1c: a flight-period Send (a turn is already active) must NOT open a
        // second turn_gen. codex merges the overlapping input into the live turn
        // under one turnId; issuing a second turn/start + fetch_add would phantom-
        // split one wire turn across two turn_gen buckets downstream. Mirror the
        // Cancel arm: accept as NoTurn, write no frame, leave turn_gen unchanged.
        let fake = fake_with_binding("th-77", Some("turn-A"));
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        // Drive the event stream until the binding's turn/started settles, so
        // active_turn_id is populated by the reader BEFORE we dispatch (mirrors
        // dispatch_rewind_while_turn_in_flight).
        let mut events = backend.events();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::TurnStarted { .. }) {
                    break;
                }
            }
        })
        .await;
        let gen_before = backend.turn_gen.load(Ordering::SeqCst);
        let receipt = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("second prompt".into())],
                metadata: super::super::types::CommandMeta::default(),
            })
            .await
            .expect("accepted");
        assert_eq!(
            receipt.admission,
            Admission::NoTurn,
            "flight-period Send merges, not a new turn"
        );
        assert_eq!(
            receipt.turn_gen, gen_before,
            "turn_gen MUST NOT advance during an active turn (no phantom split)"
        );
        assert_eq!(
            backend.turn_gen.load(Ordering::SeqCst),
            gen_before,
            "no fetch_add happened"
        );
        let written = captured_str(&captured).await;
        assert!(
            !written.contains(r#""method":"turn/start""#),
            "must NOT write a second turn/start frame, got: {written}"
        );
    }

    #[tokio::test]
    async fn gap_a_turn_start_response_emits_prompt_accepted_for_client_msg_id() {
        // GAP-A: codex's synchronous turn/start RESPONSE (the "accepted" receipt)
        // must surface as PromptAccepted{client_msg_id} so the conversation pending
        // queue drains. The fake binds the thread (prefix) then, after dispatch
        // writes turn/start (rpc id=1, the first id in build_with_io's no-handshake
        // path), the gated tail replays the matching response.
        let prefix = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-a"}}}"#
        )
        .into_bytes();
        // response to rpc id=1 (the turn/start), carrying the inProgress turn.
        let tail = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","id":1,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}"#
        )
        .into_bytes();
        let fake = FakeAgentIo::new(prefix, None).with_gated_tail(tail);
        let release = fake.stdout_releaser();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let mut events = backend.events();

        let receipt = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("hi".into())],
                metadata: super::super::types::CommandMeta {
                    client_msg_id: Some("m-7".into()),
                    ..Default::default()
                },
            })
            .await
            .expect("accepted");
        assert_eq!(receipt.admission, Admission::Started);
        // dispatch wrote turn/start (rpc id=1); now release the matching response.
        release();

        let saw = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::PromptAccepted { ref client_msg_id } if client_msg_id == "m-7") {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(
            saw,
            "turn/start response → PromptAccepted{{client_msg_id:m-7}} (drains pending)"
        );
    }

    #[tokio::test]
    async fn gap_a_no_client_msg_id_means_no_prompt_accepted() {
        // A Send WITHOUT a client_msg_id registers no correlation → no PromptAccepted
        // (nothing to drain). The turn/start response is then a plain diagnostic.
        let prefix = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-b"}}}"#
        )
        .into_bytes();
        let tail = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","id":1,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}"#
        )
        .into_bytes();
        let fake = FakeAgentIo::new(prefix, None).with_gated_tail(tail);
        let release = fake.stdout_releaser();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let mut events = backend.events();
        backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("hi".into())],
                metadata: super::super::types::CommandMeta::default(), // no client_msg_id
            })
            .await
            .expect("accepted");
        release();
        let saw_pa = tokio::time::timeout(std::time::Duration::from_millis(400), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::PromptAccepted { .. }) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(!saw_pa, "no client_msg_id → no PromptAccepted emitted");
    }

    #[tokio::test]
    async fn dispatch_cancel_writes_turn_interrupt_with_active_turn_id() {
        // Cancel → `turn/interrupt{threadId, turnId}` (hard cancel; the turnId is
        // the optimistic token captured from turn/started).
        let fake = fake_with_binding("th-9", Some("turn-A"));
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        backend
            .dispatch(Command::Cancel {
                target: CancelTarget::Turn,
            })
            .await
            .expect("accepted");
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""method":"turn/interrupt""#),
            "wrote turn/interrupt, got: {written}"
        );
        assert!(
            written.contains(r#""threadId":"th-9""#),
            "carries threadId, got: {written}"
        );
        assert!(
            written.contains(r#""turnId":"turn-A""#),
            "carries the active turnId, got: {written}"
        );
    }

    #[tokio::test]
    async fn dispatch_cancel_tool_is_rejected() {
        // Tool-scoped cancel is not a codex capability → CommandNotSupported.
        let fake = fake_with_binding("th-9", Some("turn-A"));
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let err = backend
            .dispatch(Command::Cancel {
                target: CancelTarget::Tool("call_1".into()),
            })
            .await
            .expect_err("cancel_tool must be rejected");
        assert!(matches!(err, BackendError::CommandNotSupported { command } if command == "cancel_tool"));
    }

    #[tokio::test]
    async fn dispatch_rewind_while_turn_in_flight_is_rejected_and_writes_no_frame() {
        // G3 idle-gate: a rewind issued WHILE a turn is in flight is rejected and
        // writes NO thread/rollback frame (a mid-turn rollback would race the live
        // turn's history). turn_in_flight is set by an active turn (turn/started).
        let fake = fake_with_binding("th-9", Some("turn-A"));
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        // Drive an active turn so turn_in_flight becomes true (the binding fixture's
        // turn/started sets it via the reader).
        let mut events = backend.events();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            while let Some(env) = events.next().await {
                if matches!(env.event, SessionEvent::TurnStarted { .. }) {
                    break;
                }
            }
        })
        .await;
        let err = backend
            .dispatch(Command::Rewind { num_turns: 1 })
            .await
            .expect_err("mid-turn rewind must be rejected");
        assert!(matches!(err, BackendError::Transport(m) if m.contains("in flight")));
        let written = captured_str(&captured).await;
        assert!(
            !written.contains("thread/rollback"),
            "mid-turn rewind MUST NOT write a thread/rollback frame, got: {written}"
        );
    }

    #[tokio::test]
    async fn dispatch_rewind_writes_rollback_and_response_maps_to_rewound() {
        use futures_util::StreamExt as _;
        // G3 down+up: dispatch(Rewind) writes thread/rollback{threadId,numTurns} AND
        // the reader maps the response (post-rollback Thread with re-populated turns)
        // to a Rewound{to_turn} receipt. to_turn = remaining turn count (T17 anchor).
        // The fixture has a bound thread (NO active turn → not in flight) and a gated
        // rollback response so dispatch registers the pending id before it lands.
        let prefix = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-rb"}}}"#
        )
        .into_bytes();
        // The rollback response: rpc id 1 (the first id dispatch mints), post-rollback
        // thread with 2 surviving turns → to_turn=2.
        let tail = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","id":1,"result":{"thread":{"id":"th-rb","turns":[{"id":"t-a"},{"id":"t-b"}]}}}"#
        )
        .into_bytes();
        let fake = FakeAgentIo::new(prefix, None).with_gated_tail(tail);
        let release = fake.stdout_releaser();
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-rb", Box::new(fake)).await;
        // Let the reader bind the thread (the prefix) before dispatch.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            backend.capabilities().supported_commands.rewind,
            "rewind cap is true (G3)"
        );

        let mut events = backend.events();
        let receipt = backend
            .dispatch(Command::Rewind { num_turns: 1 })
            .await
            .expect("rewind accepted (cap=true, not in flight)");
        assert_eq!(receipt.admission, Admission::NoTurn);
        // The down-leg frame hit the wire.
        let written = captured_str(&captured).await;
        assert!(
            written.contains("thread/rollback") && written.contains(r#""numTurns":1"#),
            "wrote thread/rollback with numTurns, got: {written}"
        );
        assert!(
            written.contains(r#""threadId":"th-rb""#),
            "rollback targets the bound thread"
        );

        // Release the gated response; the reader maps it to Rewound{to_turn:2}.
        release();
        let to_turn = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::Rewound { to_turn } = env.event {
                    return Some(to_turn);
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        assert_eq!(
            to_turn,
            Some(2),
            "rollback response (2 surviving turns) → Rewound{{to_turn:2}}"
        );
    }

    #[tokio::test]
    async fn dispatch_rewind_zero_turns_is_rejected() {
        // numTurns must be >= 1 (codex schema); a 0 rewind is rejected without a write.
        let fake = fake_with_binding("th-9", None);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let err = backend
            .dispatch(Command::Rewind { num_turns: 0 })
            .await
            .expect_err("num_turns=0 must be rejected");
        assert!(matches!(err, BackendError::Transport(m) if m.contains(">= 1")));
        let written = captured_str(&captured).await;
        assert!(!written.contains("thread/rollback"), "no frame for an invalid rewind");
    }

    #[test]
    fn rollback_to_turn_counts_surviving_turns() {
        // Shape CALIBRATED to the real wire (protocols/samples/codex-cli/0.139.0/
        // _all_rollback_plan.jsonl): the success result is {thread:{...,turns:[...]}, ...}
        // — the key path result.thread.turns[] is confirmed. The first assertion uses a
        // trimmed slice of the REAL captured result object (extra thread fields omitted
        // for brevity; only the turns path is load-bearing).
        let real = serde_json::json!({
            "thread": {"id": "019ef837", "sessionId": "019ef837", "modelProvider": "amazon-bedrock",
                       "preview": "Reply with exactly: turn-1-ok", "turns": [{"id": "a"}, {"id": "b"}]},
            "model": "gpt-5.5", "modelProvider": "amazon-bedrock", "cwd": "/tmp"
        });
        assert_eq!(rollback_to_turn(&real), 2, "to_turn reads result.thread.turns[].len()");
        // ⚠️ LIVE-OBSERVED (codex 0.139.0): a real numTurns:1 rollback returned
        // thread.turns = [] → to_turn = 0 even with surviving history. Pinned as the
        // honest current behavior (cosmetic only — reducer ignores to_turn).
        assert_eq!(rollback_to_turn(&serde_json::json!({"thread":{"turns":[]}})), 0);
        // Cross-version fallbacks: flat {turns} + missing turns.
        assert_eq!(rollback_to_turn(&serde_json::json!({"turns":[{"id":"a"}]})), 1);
        assert_eq!(rollback_to_turn(&serde_json::json!({"thread":{"id":"x"}})), 0);
    }

    #[tokio::test]
    async fn dispatch_cancel_with_no_active_turn_writes_no_frame() {
        // turn/interrupt requires a turnId (non-Option on the wire). With no active
        // turn we accept as a no-op WITHOUT writing a frame codex would reject — the
        // orchestrator's lowered Cancel folds the FSM to Idle.
        let fake = fake_with_binding("th-9", None); // bound thread, NO active turn
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let receipt = backend
            .dispatch(Command::Cancel {
                target: CancelTarget::Turn,
            })
            .await
            .expect("accepted as no-op");
        assert_eq!(receipt.admission, Admission::NoTurn);
        let written = captured_str(&captured).await;
        assert!(
            !written.contains("turn/interrupt"),
            "no turnId → MUST NOT write a turn/interrupt frame codex rejects, got: {written}"
        );
    }

    #[tokio::test]
    async fn dispatch_cancel_in_flight_but_unbound_waits_for_bind_then_interrupts() {
        // cancel-before-fold (token-burn half): a turn is in flight (dispatch(Send)
        // ran, turn_in_flight=true) but the reader has NOT yet bound active_turn_id
        // from the async turn/started. A cancel here must NOT silently no-op (codex
        // would keep burning tokens) — it polls briefly for the bind, then writes
        // turn/interrupt. We simulate the late bind by setting the id from another
        // task shortly after the cancel begins waiting.
        // thread/started binds the thread but NO turn/started yet (the pre-fold
        // window). A GATED TAIL keeps stdout open (never released) so the reader does
        // not EOF + clear turn_in_flight. We mark the turn in flight by hand (what
        // dispatch(Send) would have done) and bind the active turn id late.
        let prefix = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-late"}}}"#
        )
        .into_bytes();
        let fake = FakeAgentIo::new(prefix, None).with_gated_tail(b"never-released\n".to_vec());
        let captured = fake.captured_stdin();
        let backend = Arc::new(CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await);
        backend.bound_thread().await.expect("thread bound from thread/started");
        backend.mark_turn_in_flight_for_test();

        let binder = backend.clone();
        let late_bind = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            binder.bind_active_turn_for_test("turn-late").await;
        });

        backend
            .dispatch(Command::Cancel {
                target: CancelTarget::Turn,
            })
            .await
            .expect("accepted");
        late_bind.await.unwrap();

        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""method":"turn/interrupt""#) && written.contains(r#""turnId":"turn-late""#),
            "in-flight cancel waited for the late turn/started bind then interrupted, got: {written}"
        );
    }

    #[tokio::test]
    async fn config_changed_emitted_on_thread_settings_updated() {
        // C6 §6: thread/settings/updated → ConfigChanged (the non-optimistic confirmation),
        // NOT AdapterSpecific. Carries model + the permission tier read from
        // `activePermissionProfile.id`. The colon wire id is mapped to the legacy bare token
        // the catalog/frontend uses (`:danger-full-access` → `full-access`) so the picker
        // highlights the matching catalog entry and all locales key off the same value the
        // legacy ACP path presented. Verbatim 0.139.0 shape (the frame ALSO carries
        // collaborationMode, which we must ignore).
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"thread/settings/updated","params":{"threadId":"th1","threadSettings":{"model":"gpt-5.5","activePermissionProfile":{"id":":danger-full-access","extends":null},"collaborationMode":{"mode":"default","settings":{"model":"gpt-5.5"}}}}}"#,
        ])
        .await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::ConfigChanged { model: Some(m), mode: Some(md) } if m == "gpt-5.5" && md == "full-access"
            )),
            "thread/settings/updated → ConfigChanged{{model, mode=legacy bare token of activePermissionProfile.id}}, got {events:?}"
        );
    }

    #[tokio::test]
    async fn config_changed_mode_none_when_permission_profile_null() {
        // feature 012: when the tier was set via the raw sandboxPolicy channel (not our
        // permissions path), `activePermissionProfile` is null → carry no mode (keep the
        // last-known selection) rather than clobber it. We must NOT fall back to
        // collaborationMode.mode.
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"thread/settings/updated","params":{"threadId":"th1","threadSettings":{"model":"gpt-5.5","activePermissionProfile":null,"collaborationMode":{"mode":"default","settings":{"model":"gpt-5.5"}}}}}"#,
        ])
        .await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::ConfigChanged { model: Some(m), mode: None } if m == "gpt-5.5"
            )),
            "null activePermissionProfile → ConfigChanged with mode:None, got {events:?}"
        );
    }

    #[tokio::test]
    async fn dispatch_steer_writes_turn_steer_with_expected_turn_id() {
        // R6 Steer → `turn/steer{threadId, expectedTurnId, input}` (soft injection;
        // NoTurn admission — no new turn_gen). The expectedTurnId is the active turn.
        let fake = fake_with_binding("th-3", Some("turn-X"));
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let receipt = backend
            .dispatch(Command::Steer {
                content: vec![ContentBlock::Text("STEERED".into())],
            })
            .await
            .expect("accepted");
        assert_eq!(receipt.admission, Admission::NoTurn, "steer folds into the live turn");
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""method":"turn/steer""#),
            "wrote turn/steer, got: {written}"
        );
        assert!(
            written.contains(r#""expectedTurnId":"turn-X""#),
            "gated by the active turn token, got: {written}"
        );
        assert!(written.contains("STEERED"), "carries the steer text, got: {written}");
    }

    #[tokio::test]
    async fn dispatch_steer_without_active_turn_is_rejected() {
        // No active turn → nothing to inject into → reject (matches codex's
        // activeTurnNotSteerable; we pre-empt the wire roundtrip).
        let fake = fake_with_binding("th-3", None); // bound thread but NO active turn
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let err = backend
            .dispatch(Command::Steer {
                content: vec![ContentBlock::Text("late".into())],
            })
            .await
            .expect_err("steer with no active turn must be rejected");
        assert!(matches!(err, BackendError::Transport(m) if m.contains("no active turn")));
    }

    #[tokio::test]
    async fn dispatch_set_model_writes_thread_settings_update() {
        // R6 SetModel → `thread/settings/update{threadId, model}` (verified frame).
        let fake = fake_with_binding("th-5", None);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        backend
            .dispatch(Command::SetModel {
                model: "gpt-5.5".into(),
            })
            .await
            .expect("accepted");
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""method":"thread/settings/update""#),
            "wrote thread/settings/update, got: {written}"
        );
        assert!(
            written.contains(r#""model":"gpt-5.5""#),
            "carries the model, got: {written}"
        );
        assert!(
            written.contains(r#""threadId":"th-5""#),
            "carries threadId, got: {written}"
        );
    }

    #[tokio::test]
    async fn dispatch_set_config_option_effort_writes_thread_settings_update() {
        // codex's reasoning effort is a first-class `thread/settings/update{threadId,
        // effort}` field (schema: ThreadSettingsUpdateParams.effort → ReasoningEffort),
        // so SetConfigOption{effort} routes through the same wire as SetModel/SetMode —
        // NOT a CommandNotSupported reject. Each effort alias id maps to the `effort` key.
        for option_id in ["effort", "reasoning_effort", "thought_level"] {
            let fake = fake_with_binding("th-7", None);
            let captured = fake.captured_stdin();
            let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
            backend
                .dispatch(Command::SetConfigOption {
                    option_id: option_id.into(),
                    value: "high".into(),
                })
                .await
                .unwrap_or_else(|e| panic!("effort id `{option_id}` must be accepted, got: {e:?}"));
            let written = captured_str(&captured).await;
            assert!(
                written.contains(r#""method":"thread/settings/update""#),
                "id `{option_id}` wrote thread/settings/update, got: {written}"
            );
            assert!(
                written.contains(r#""effort":"high""#),
                "id `{option_id}` carries the effort value, got: {written}"
            );
            assert!(
                written.contains(r#""threadId":"th-7""#),
                "id `{option_id}` carries threadId, got: {written}"
            );
        }
    }

    #[tokio::test]
    async fn dispatch_set_config_option_non_effort_still_rejects() {
        // Only the effort aliases have a codex wire; any other generic config option
        // still has none and must reject with CommandNotSupported (not silently drop).
        let fake = fake_with_binding("th-8", None);
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let err = backend
            .dispatch(Command::SetConfigOption {
                option_id: "verbosity".into(),
                value: "high".into(),
            })
            .await
            .expect_err("a non-effort config option must reject");
        assert!(
            matches!(
                err,
                BackendError::CommandNotSupported {
                    command: "set_config_option"
                }
            ),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn dispatch_set_mode_writes_permissions_profile_id() {
        // SetMode → `thread/settings/update{threadId, permissions}` where `permissions` is
        // the DISCOVERED colon-prefixed profile id. A colon id passes through verbatim; a
        // legacy bare value (`full-access`) normalizes onto its colon id. NOT the old
        // collaborationMode object, and NOT a bare id (codex rejects a colon-less id).
        // Needs no known model.
        let fake = fake_with_binding("th-6", None);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        // A discovered colon id flows through unchanged (legacy-ACP parity: the wire id IS
        // the selector value).
        backend
            .dispatch(Command::SetMode {
                mode: ":danger-full-access".into(),
            })
            .await
            .expect("accepted");
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""method":"thread/settings/update""#),
            "wrote thread/settings/update, got: {written}"
        );
        assert!(
            written.contains(r#""permissions":":danger-full-access""#),
            "carries the discovered colon-prefixed profile id verbatim, got: {written}"
        );
        assert!(
            !written.contains(r#""collaborationMode""#),
            "must NOT send the old collaborationMode object, got: {written}"
        );
        assert!(
            !written.contains(r#""permissions":"danger-full-access""#),
            "must NOT strip the mandatory leading colon, got: {written}"
        );
    }

    #[tokio::test]
    async fn dispatch_set_mode_normalizes_legacy_persisted_value() {
        // A legacy persisted BARE value (`yolo`) — from an older AionUi that stored the
        // pre-discovery alias — normalizes onto the `:danger-full-access` colon id, so an
        // upgrading user's stored mode applies straight through with zero fallback. This is
        // the codex analogue of legacy ACP `normalize_requested_mode` (alias → native id).
        let fake = fake_with_binding("th-6", None);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        backend
            .dispatch(Command::SetMode { mode: "yolo".into() })
            .await
            .expect("accepted");
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""permissions":":danger-full-access""#),
            "legacy `yolo` normalizes to :danger-full-access, got: {written}"
        );
    }

    #[tokio::test]
    async fn dispatch_set_mode_passes_custom_profile_id_verbatim() {
        // A user `[permissions.<id>]` custom profile — discovered via permissionProfile/list
        // and NOT one of the built-in tiers — must reach the wire UNCHANGED. This is the
        // heart of the legacy-ACP parity: codex owns the value set, AionCore only transports
        // it (no fixed-enum whitelist that would drop a custom profile).
        let fake = fake_with_binding("th-6", None);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        backend
            .dispatch(Command::SetMode {
                mode: ":team-review".into(),
            })
            .await
            .expect("accepted");
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""permissions":":team-review""#),
            "custom profile id reaches the wire verbatim, got: {written}"
        );
    }

    #[tokio::test]
    async fn set_model_error_response_surfaces_notice_not_silently_dropped() {
        use futures_util::StreamExt as _;

        // GAP (codex analogue of the acp_conn fix): dispatch(SetMode/SetModel) writes a
        // `thread/settings/update` request but used to register its rpc id in NO pending
        // map, so a JSON-RPC ERROR response (codex rejecting an invalid model/mode) was
        // claimed by no one and silently dropped — the user never saw the failed set.
        // Now `pending_set` carries "model→<v>" so the reader surfaces the error as a
        // Notice{Warning} (and does NOT emit a second ConfigChanged: success converges
        // via thread/settings/updated). Feed an error response keyed to the registered id.
        let err_resp = r#"{"jsonrpc":"2.0","id":42,"error":{"code":-32602,"message":"model not found"}}"#;
        let bytes = format!("{err_resp}\n").into_bytes();
        let fake = FakeAgentIo::never_exits(bytes);
        let backend = CodexSessionBackend::build_with_io("codex-set", Box::new(fake)).await;
        // Register the pending id the reader will claim (dispatch does this; here we mimic
        // it so the response — keyed 42 — is recognized as a SetModel reconcile target).
        backend.set_pending_set_for_test(42, "model\u{2192}gpt-bogus").await;
        // Subscribe BEFORE the reader consumes the response (broadcast drops pre-subscribe).
        let mut events = backend.events();
        let mut notice = None;
        let mut saw_config_changed = false;
        for _ in 0..40 {
            match tokio::time::timeout(std::time::Duration::from_millis(25), events.next()).await {
                Ok(Some(env)) => match env.event {
                    SessionEvent::Notice { level, message, .. } => {
                        notice = Some((level, message));
                        break;
                    }
                    SessionEvent::ConfigChanged { .. } => saw_config_changed = true,
                    _ => {}
                },
                _ => continue,
            }
        }
        let (level, message) = notice.expect("a rejected set surfaces a Notice, not a silent drop");
        assert!(
            matches!(level, crate::event::NoticeLevel::Warning),
            "rejected set is a Warning, got {level:?}"
        );
        assert!(
            message.contains("model\u{2192}gpt-bogus") && message.contains("model not found"),
            "Notice carries the label + the agent's error message, got: {message}"
        );
        assert!(
            !saw_config_changed,
            "an ERROR response must NOT emit a ConfigChanged (no false convergence)"
        );
    }

    #[tokio::test]
    async fn dispatch_set_mode_obsolete_and_unknown_bare_values_fall_to_workspace() {
        // Legacy-ACP parity: AionCore never ships a frame codex would reject. Any unknown
        // or obsolete BARE value normalizes to the safe `:workspace` tier rather than
        // erroring. `plan` (the OLD collaborationMode token) is the key case — it no longer
        // collaboration-maps but lands on the workspace profile, proving the
        // collaborationMode axis is gone. (Validation of a stale DISCOVERED colon id is the
        // reconcile/reader's job against the live catalog, not this normalize step.)
        let fake = fake_with_binding("th-6", None);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        backend
            .dispatch(Command::SetMode { mode: "plan".into() })
            .await
            .expect("plan normalizes to :workspace, accepted");
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""permissions":":workspace""#),
            "the obsolete `plan` token normalizes to the default workspace profile, got: {written}"
        );
    }

    #[tokio::test]
    async fn dispatch_answer_auth_writes_keyed_refresh_response() {
        // R6/R15 AnswerAuth answers a PENDING `account/chatgptAuthTokens/refresh`
        // reverse-RPC by writing the keyed JSON-RPC RESPONSE carrying the supplied
        // tokens (NOT a fresh account/login/start). The fake first emits the refresh
        // request (id=99) so the reader stashes pending_auth_id, then stays alive so
        // we can capture the response dispatch writes.
        let bytes = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","id":99,"method":"account/chatgptAuthTokens/refresh","params":{"reason":"expired"}}"#
        )
        .into_bytes();
        let fake = FakeAgentIo::never_exits(bytes);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        // Wait for the reader to surface Permission{Auth} (pending_auth_id set).
        {
            let mut events = backend.events();
            let saw_auth = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while let Some(env) = events.next().await {
                    if matches!(
                        env.event,
                        SessionEvent::Permission {
                            kind: PermissionKind::Auth,
                            ..
                        }
                    ) {
                        return true;
                    }
                }
                false
            })
            .await
            .unwrap_or(false);
            assert!(saw_auth, "refresh must surface as Permission(Auth) before answering");
        }
        backend
            .dispatch(Command::AnswerAuth {
                method_id: "chatgptAuthTokens".into(),
                credentials: json!({ "accessToken": "jwt-abc", "chatgptAccountId": "acct-1" }),
            })
            .await
            .expect("accepted");
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""id":99"#),
            "response keyed to the refresh request id, got: {written}"
        );
        assert!(
            written.contains(r#""result""#),
            "wrote a JSON-RPC result (the answer), got: {written}"
        );
        // camelCase = the only schema-evidenced shape (ChatgptAuthTokensLoginAccountParams);
        // ⚠️ unverified against a live re-auth capture (codex defines no refresh-response
        // schema) — see the dispatch impl tripwire. Was snake_case (contradicted all evidence).
        assert!(
            written.contains(r#""accessToken":"jwt-abc""#),
            "carries the access token (camelCase, schema-evidenced), got: {written}"
        );
        assert!(
            written.contains(r#""chatgptAccountId":"acct-1""#),
            "carries the account id (camelCase, schema-evidenced), got: {written}"
        );
    }

    #[tokio::test]
    async fn dispatch_answer_auth_without_pending_is_rejected() {
        // AnswerAuth with no pending refresh → reject (nothing to answer).
        let fake = fake_with_binding("th-8", None);
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let err = backend
            .dispatch(Command::AnswerAuth {
                method_id: "chatgptAuthTokens".into(),
                credentials: json!({ "accessToken": "x" }),
            })
            .await
            .expect_err("no pending auth → reject");
        assert!(matches!(err, BackendError::Transport(m) if m.contains("no pending auth")));
    }

    #[tokio::test]
    async fn server_request_resolved_maps_to_permission_resolved() {
        // serverRequest/resolved → PermissionResolved so the reducer decrements the
        // matching waiting_on_* counter (R9/R15). Default kind=Tool (the common
        // approval case) when no pending auth id matches.
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"serverRequest/resolved","params":{"threadId":"th1","request_id":7}}"#,
        ])
        .await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::PermissionResolved {
                    kind: PermissionKind::Tool,
                    ..
                }
            )),
            "serverRequest/resolved → PermissionResolved(Tool), got {events:?}"
        );
    }

    #[tokio::test]
    async fn dispatch_acknowledge_is_local_noop() {
        // Acknowledge has NO codex wire (folds at the conversation layer). Accept
        // as a no-op; assert nothing was written to stdin.
        let fake = fake_with_binding("th-2", None);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        let receipt = backend
            .dispatch(Command::Acknowledge {
                node_id: "node-1".into(),
            })
            .await
            .expect("accepted");
        assert_eq!(receipt.admission, Admission::NoTurn);
        let written = captured_str(&captured).await;
        assert!(
            written.is_empty() || !written.contains(r#""method""#),
            "Acknowledge writes no client request, got: {written}"
        );
    }

    // ===== R8 dual-terminal reconcile: EXACTLY ONE TurnResult per turn =====

    fn count_turn_results(events: &[SessionEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, SessionEvent::TurnResult { .. }))
            .count()
    }

    /// R8 — turn/completed arrives FIRST, then status→idle. Exactly one
    /// TurnResult (the rich one from turn/completed); the trailing idle is absorbed.
    #[tokio::test]
    async fn r8_completed_then_idle_yields_one_turn_result() {
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"th1","turn":{"id":"t1","status":"completed"}}}"#,
            r#"{"jsonrpc":"2.0","method":"thread/status/changed","params":{"threadId":"th1","status":{"type":"idle"}}}"#,
        ])
        .await;
        assert_eq!(
            count_turn_results(&events),
            1,
            "completed→idle must converge to ONE TurnResult, got {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::TurnResult {
                    is_error: false,
                    outcome: TurnOutcome::Completed { .. },
                    ..
                }
            )),
            "the one TurnResult is the rich Completed from turn/completed"
        );
    }

    /// R8/M3 — the REAL codex ordering: status→idle arrives FIRST, then the
    /// authoritative turn/completed. The idle DEFERS (emits nothing); completed
    /// produces the one rich terminal. Exactly one TurnResult.
    #[tokio::test]
    async fn r8_idle_first_then_completed_yields_one_turn_result() {
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"thread/status/changed","params":{"threadId":"th1","turnId":"t1","status":{"type":"idle"}}}"#,
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"th1","turn":{"id":"t1","status":"completed"}}}"#,
        ])
        .await;
        assert_eq!(
            count_turn_results(&events),
            1,
            "idle-first defers; the authoritative completed produces the ONE TurnResult, got {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::TurnResult {
                    outcome: TurnOutcome::Completed { .. },
                    ..
                }
            )),
            "the terminal is the rich Completed from turn/completed, not a synthesized fallback"
        );
    }

    /// R8 — status→idle with NO turn/completed at all (a dropped/missing terminal).
    /// The deferred idle is flushed as a clean terminal at EOF so the FSM doesn't
    /// hang Running forever (defensive — not observed in real codex).
    #[tokio::test]
    async fn r8_idle_alone_still_terminates() {
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"thread/status/changed","params":{"threadId":"th1","status":{"type":"idle"}}}"#,
        ])
        .await;
        assert_eq!(
            count_turn_results(&events),
            1,
            "a bare status→idle (no turn/completed) MUST still produce one terminal (EOF flush), got {events:?}"
        );
    }

    /// R8/M3 — TWO turns on ONE connection (the real codex shape: app-server is
    /// persistent, multi-turn). Each turn must terminate EXACTLY once. The
    /// `terminated`/`idle_pending` flags MUST reset on the second turn/started —
    /// otherwise the second turn's terminal is absorbed and its FSM hangs Running.
    #[tokio::test]
    async fn r8_two_turns_each_terminate_once() {
        let events = drive_codex(&[
            // turn 1: started → idle → completed
            r#"{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"th1","turn":{"id":"t1"}}}"#,
            r#"{"jsonrpc":"2.0","method":"thread/status/changed","params":{"threadId":"th1","status":{"type":"idle"}}}"#,
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"th1","turn":{"id":"t1","status":"completed"}}}"#,
            // turn 2: started → idle → completed (failed this time)
            r#"{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"th1","turn":{"id":"t2"}}}"#,
            r#"{"jsonrpc":"2.0","method":"thread/status/changed","params":{"threadId":"th1","status":{"type":"idle"}}}"#,
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"th1","turn":{"id":"t2","status":"failed","error":{"message":"boom","codexErrorInfo":{"httpConnectionFailed":{"httpStatusCode":500}}}}}}"#,
        ])
        .await;
        assert_eq!(
            count_turn_results(&events),
            2,
            "each of the two turns terminates exactly once (reset on turn/started), got {events:?}"
        );
        // The second turn's failure outcome must survive (not be absorbed).
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::TurnResult {
                    is_error: true,
                    api_error_status: Some(500),
                    ..
                }
            )),
            "the second turn's failed status survives, got {events:?}"
        );
    }

    /// R8 — a non-idle status change is advisory: produces NO TurnResult.
    #[tokio::test]
    async fn r8_active_status_is_advisory_no_terminal() {
        let events = drive_codex(&[
            r#"{"jsonrpc":"2.0","method":"thread/status/changed","params":{"threadId":"th1","status":{"type":"active","activeFlags":[]}}}"#,
        ])
        .await;
        assert_eq!(count_turn_results(&events), 0, "active status is advisory, no terminal");
    }

    /// B1 (BLOCKER regression): the `initialize` handshake MUST opt into the
    /// experimental API — `capabilities.experimentalApi:true`, NESTED (top-level is
    /// silently ignored by codex). Without it, thread/settings/update (SetMode/
    /// SetModel) + thread/turns/list (ListCheckpoints) are rejected `invalid_request`
    /// by real codex. Asserts the exact frame shape (no live process needed).
    #[test]
    fn b1_initialize_frame_opts_into_experimental_api_nested() {
        let frame = initialize_params().into_frame(1, "initialize");
        assert_eq!(frame["method"], "initialize");
        // NESTED under capabilities — the schema-required location.
        assert_eq!(
            frame["params"]["capabilities"]["experimentalApi"],
            serde_json::Value::Bool(true),
            "experimentalApi MUST be true and nested under capabilities, got: {frame}"
        );
        // A top-level experimentalApi would be silently ignored → must NOT rely on it.
        assert!(
            frame["params"].get("experimentalApi").is_none(),
            "experimentalApi must NOT be top-level (codex ignores it there)"
        );
        assert_eq!(frame["params"]["clientInfo"]["name"], "aionui-session");
    }

    /// thread/start params thread cwd from config; approvalPolicy/sandbox are valid
    /// codex enum values. MODEL IS NEVER EMBEDDED (codex-model-gating regression fix):
    /// the model binds the whole thread and cannot be validated at this instant
    /// (model/list comes AFTER thread/start), so a requested model is applied later
    /// via a validated `SetModel` (`reconcile_codex_model`), never here.
    #[test]
    fn thread_start_frame_threads_cwd_and_never_model() {
        let frame = thread_start_params(&SessionConfig {
            cwd: Some("/work".into()),
            model: Some("gpt-5.5".into()),
            ..Default::default()
        })
        .into_frame(2, "thread/start");
        assert_eq!(frame["method"], "thread/start");
        assert_eq!(frame["params"]["cwd"], "/work");
        assert!(
            frame["params"].get("model").is_none(),
            "model must NOT be bound at thread/start even when config carries one \
             (applied post-discovery via a validated SetModel instead)"
        );
        assert_eq!(frame["params"]["approvalPolicy"], "on-request");
        assert_eq!(frame["params"]["sandbox"], "workspace-write");
        // omitted when config has neither
        let bare = thread_start_params(&SessionConfig::default()).into_frame(3, "thread/start");
        assert!(bare["params"].get("cwd").is_none());
        assert!(bare["params"].get("model").is_none());
        // Wave 0c: a default (empty) init carries NO config/baseInstructions — the
        // pre-0c thread/start is byte-identical for conversations with no MCP/preset.
        assert!(
            bare["params"].get("config").is_none(),
            "empty init → no config override"
        );
        assert!(bare["params"].get("baseInstructions").is_none());
    }

    /// G1-A: thread/start serializes SessionConfig.sandbox_mode data-driven —
    /// `None` keeps the safe `workspace-write` default; a resolved policy (e.g. a
    /// yolo agent → `danger-full-access`) rides the wire. The default path stays
    /// byte-identical (asserted by the test above).
    #[test]
    fn thread_start_sandbox_is_data_driven_from_config() {
        let frame = thread_start_params(&SessionConfig {
            sandbox_mode: Some("danger-full-access".into()),
            ..Default::default()
        })
        .into_frame(2, "thread/start");
        assert_eq!(
            frame["params"]["sandbox"], "danger-full-access",
            "a resolved sandbox_mode overrides the workspace-write default"
        );
        // approvalPolicy keeps its default when only sandbox_mode is set.
        assert_eq!(frame["params"]["approvalPolicy"], "on-request");
        // Restriction rides the SAME axis: a read-only conversation seeds
        // sandbox:"read-only" at thread/start so the FIRST turn is already locked
        // down (the SetMode permission profile applies only on the NEXT turn). This
        // is the wire half of the read-only first-turn-write regression fix.
        let ro = thread_start_params(&SessionConfig {
            sandbox_mode: Some("read-only".into()),
            ..Default::default()
        })
        .into_frame(3, "thread/start");
        assert_eq!(
            ro["params"]["sandbox"], "read-only",
            "a read-only conversation must launch its thread under the read-only sandbox"
        );
    }

    /// thread/start serializes SessionConfig.approval_policy data-driven (sibling of
    /// sandbox) — `None` keeps the safe `on-request` default; a resolved policy
    /// (e.g. a yolo agent → `never`) rides the wire. The default path stays
    /// byte-identical.
    #[test]
    fn thread_start_approval_policy_is_data_driven_from_config() {
        // default: None ⇒ on-request (byte-identical to pre-data-driven handshake)
        let default_frame = thread_start_params(&SessionConfig::default()).into_frame(1, "thread/start");
        assert_eq!(default_frame["params"]["approvalPolicy"], "on-request");
        // resolved: a yolo agent runs unattended → never
        let frame = thread_start_params(&SessionConfig {
            approval_policy: Some("never".into()),
            ..Default::default()
        })
        .into_frame(2, "thread/start");
        assert_eq!(
            frame["params"]["approvalPolicy"], "never",
            "a resolved approval_policy overrides the on-request default"
        );
        // sandbox keeps its default when only approval_policy is set.
        assert_eq!(frame["params"]["sandbox"], "workspace-write");
    }

    /// Wave 0c: MCP servers reach codex via `config.mcp_servers` (a MAP keyed by
    /// name — NOT a per-thread array like ACP), and the preset goes to
    /// `baseInstructions`. The codex stdio shape uses `env` as a MAP (verified live
    /// against codex 0.139.0 + `codex mcp add` TOML output).
    #[test]
    fn thread_start_injects_codex_mcp_map_and_preset() {
        use crate::backend::{McpServerSpec, McpTransport, SessionInit};
        let frame = thread_start_params(&SessionConfig {
            cwd: Some("/work".into()),
            init: SessionInit {
                mcp_servers: vec![
                    McpServerSpec {
                        name: "fs".into(),
                        transport: McpTransport::Stdio {
                            command: "/usr/bin/node".into(),
                            args: vec!["s.js".into()],
                            env: vec![("TOKEN".into(), "x".into())],
                        },
                    },
                    McpServerSpec {
                        name: "remote".into(),
                        transport: McpTransport::Http {
                            url: "https://mcp.example/api".into(),
                            headers: vec![],
                        },
                    },
                ],
                preset_context: Some("You are a helpful assistant.".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .into_frame(2, "thread/start");
        // MCP is a MAP under config.mcp_servers, keyed by name.
        let mcp = &frame["params"]["config"]["mcp_servers"];
        assert_eq!(mcp["fs"]["command"], "/usr/bin/node");
        assert_eq!(mcp["fs"]["args"][0], "s.js");
        // codex env is a MAP {KEY:VAL}, NOT acp's array of {name,value}.
        assert_eq!(mcp["fs"]["env"]["TOKEN"], "x");
        assert_eq!(mcp["remote"]["url"], "https://mcp.example/api");
        // preset → baseInstructions.
        assert_eq!(frame["params"]["baseInstructions"], "You are a helpful assistant.");
    }

    /// thread/resume re-sends the FULL thread/start override surface + threadId.
    /// LIVE-confirmed (0.144.1, `samples/codex-cli/0.144.1/_probe_resume_mcp.py`):
    /// a bare `{threadId}` resume drops the user's MCP servers (inventory EMPTY)
    /// and resets approvalPolicy to its default — the rollout does NOT restore
    /// thread/start overrides. Re-sent params are consumed (servers relaunch,
    /// approvalPolicy echoes applied).
    #[test]
    fn thread_resume_resends_full_start_surface_with_thread_id() {
        use crate::backend::{McpServerSpec, McpTransport, SessionInit};
        let config = SessionConfig {
            cwd: Some("/work".into()),
            approval_policy: Some("never".into()),
            sandbox_mode: Some("danger-full-access".into()),
            init: SessionInit {
                mcp_servers: vec![McpServerSpec {
                    name: "fs".into(),
                    transport: McpTransport::Stdio {
                        command: "/usr/bin/node".into(),
                        args: vec!["s.js".into()],
                        env: vec![],
                    },
                }],
                preset_context: Some("preset".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let frame = thread_resume_params(&config, "th-1").into_frame(2, "thread/resume");
        assert_eq!(frame["method"], "thread/resume");
        assert_eq!(frame["params"]["threadId"], "th-1");
        assert_eq!(frame["params"]["cwd"], "/work");
        assert_eq!(frame["params"]["approvalPolicy"], "never");
        assert_eq!(frame["params"]["sandbox"], "danger-full-access");
        assert_eq!(
            frame["params"]["config"]["mcp_servers"]["fs"]["command"],
            "/usr/bin/node"
        );
        assert_eq!(frame["params"]["baseInstructions"], "preset");
        // Empty init resume: still threadId + the policy defaults, no config /
        // baseInstructions keys (mirrors the bare thread/start shape).
        let bare = thread_resume_params(&SessionConfig::default(), "th-2").into_frame(3, "thread/resume");
        assert_eq!(bare["params"]["threadId"], "th-2");
        assert!(bare["params"].get("config").is_none());
        assert!(bare["params"].get("baseInstructions").is_none());
    }

    /// R4 live spawn: open_session MUST route through the INJECTED Spawner (S14,
    /// never raw-spawn) with the right CommandSpec (`codex app-server`, cwd + any
    /// extra_args). FakeSpawner records the spec then Errs (it can't synthesize a
    /// real ManagedProcess), so we assert the SPEC, not a live process — the real
    /// handshake end-to-end is a real-machine concern (Bedrock AWS_PROFILE=pionex). Mirrors
    /// the 002 T14 "routes through injected spawner" discipline.
    #[tokio::test]
    async fn r4_open_session_routes_through_injected_spawner_with_codex_app_server() {
        use crate::testing::FakeSpawner;
        let spawner = std::sync::Arc::new(FakeSpawner::new());
        let conn = CodexConnection::new(spawner.clone());
        let res = conn
            .open_session(
                SessionSpec::Fresh {
                    session_id: "logical-1".into(),
                },
                SessionConfig {
                    cwd: Some("/tmp/work".into()),
                    extra_args: vec!["--flag".into()],
                    spawn_env: vec![aionui_common::EnvVar {
                        name: "AIONUI_CONVERSATION_ID".into(),
                        value: "conv-1".into(),
                    }],
                    ..Default::default()
                },
            )
            .await;
        // FakeSpawner can't make a real process → open_session Errs at spawn.
        assert!(res.is_err(), "FakeSpawner cannot synthesize a process → spawn Errs");
        assert_eq!(
            spawner.call_count(),
            1,
            "open_session routed through the injected spawner exactly once"
        );
        let spec = spawner.last_command().await.expect("a CommandSpec was recorded");
        assert_eq!(spec.command.to_str(), Some("codex"), "spawns the codex binary");
        assert_eq!(
            spec.args.first().map(String::as_str),
            Some("app-server"),
            "first arg is app-server"
        );
        assert!(
            spec.args.iter().any(|a| a == "--flag"),
            "extra_args threaded into the spawn"
        );
        assert_eq!(spec.cwd.as_deref(), Some("/tmp/work"), "cwd threaded (workspace)");
        // #103 parity with claude_conn: the orchestration-filled spawn env
        // (AIONUI_* runtime context + per-agent overrides) reaches the process.
        assert_eq!(
            spec.env
                .iter()
                .map(|e| (e.name.as_str(), e.value.as_str()))
                .collect::<Vec<_>>(),
            [("AIONUI_CONVERSATION_ID", "conv-1")],
            "spawn_env forwarded into CommandSpec.env"
        );
    }

    /// #410 parity wiring: a spawner failing with the classified
    /// `ProcessError::WorkspaceUnavailable` surfaces from `open_session` as
    /// `BackendError::WorkspaceUnavailable` (path intact), NOT a blanket
    /// `Transport` — the app layer maps it to the dedicated
    /// workspace-unavailable API error (Sentry ELECTRON-3PP follow-up).
    #[tokio::test]
    async fn open_session_classifies_workspace_unavailable_spawn_failure() {
        struct WorkspaceGoneSpawner;
        #[async_trait::async_trait]
        impl aionui_process::Spawner for WorkspaceGoneSpawner {
            async fn spawn(
                &self,
                _spec: aionui_common::CommandSpec,
                _extra_env: &[(String, String)],
                _opaque_owner_tag: &str,
            ) -> Result<std::sync::Arc<aionui_process::ManagedProcess>, aionui_process::ProcessError> {
                Err(aionui_process::ProcessError::workspace_unavailable("/gone/workspace"))
            }
        }

        let conn = CodexConnection::new(std::sync::Arc::new(WorkspaceGoneSpawner));
        let res = conn
            .open_session(
                SessionSpec::Fresh {
                    session_id: "logical-1".into(),
                },
                SessionConfig {
                    cwd: Some("/gone/workspace".into()),
                    ..Default::default()
                },
            )
            .await;
        // `expect_err` needs `T: Debug` and `Arc<dyn SessionBackend>` has none.
        let Err(err) = res else {
            panic!("open_session must fail when the spawner reports workspace-unavailable");
        };
        assert_eq!(err, BackendError::WorkspaceUnavailable("/gone/workspace".into()));
    }

    // ===== B-CODEX-MODEL-LIST: discovery (model/list + permissionProfile/list) =====

    /// feature 012 (R1): the handshake MUST discover `permissionProfile/list` (codex's
    /// mode axis IS its permission axis) and MUST NOT send `collaborationMode/list`
    /// (plan/default has no UI entry — matches legacy ACP). Drives a real `run_handshake`
    /// against captured stdin and asserts the wire.
    #[tokio::test]
    async fn handshake_discovers_permission_profiles_not_collaboration_mode() {
        let fake = FakeAgentIo::never_exits(Vec::new());
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-hs", Box::new(fake)).await;
        backend
            .run_handshake(HandshakeMode::Fresh)
            .await
            .expect("handshake writes");
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""method":"model/list""#),
            "handshake discovers model/list, got: {written}"
        );
        assert!(
            written.contains(r#""method":"permissionProfile/list""#),
            "feature 012: handshake discovers permissionProfile/list, got: {written}"
        );
        assert!(
            !written.contains(r#""method":"collaborationMode/list""#),
            "feature 012: handshake must NOT send collaborationMode/list, got: {written}"
        );
    }

    #[tokio::test]
    async fn b_codex_model_list_response_fills_discovered_and_capabilities() {
        // CALIBRATED TO THE REAL WIRE (README discipline #9 / dimension 25): the
        // response shape below is copied from the live capture
        // protocols/samples/codex-cli/0.137.0/appserver-methods/catalog.jsonl
        // (id:5 model/list, id:7 collaborationMode/list) — NOT a hand-written guess.
        // Both lists live under `result.data[]`; supportedReasoningEfforts is an
        // array of OBJECTS {reasoningEffort, description}; a mode item carries both a
        // display `name` ("Plan") and the lowercase `mode` token ("plan") that
        // SetMode actually sends. The prior fixture used result.models[]/result.modes[]
        // + bare-string efforts — a self-confirming shape that never matched the wire,
        // so model discovery silently produced an empty list → Bedrock 404.
        let model_resp = r#"{"jsonrpc":"2.0","id":50,"result":{"data":[{"id":"openai.gpt-5.5","model":"openai.gpt-5.5","displayName":"GPT-5.5","description":"Frontier model","hidden":false,"supportedReasoningEfforts":[{"reasoningEffort":"low","description":"Fast"},{"reasoningEffort":"medium","description":"Balanced"},{"reasoningEffort":"high","description":"Deep"},{"reasoningEffort":"xhigh","description":"Deepest"}],"defaultReasoningEffort":"medium","isDefault":true},{"id":"openai.gpt-5.4","model":"openai.gpt-5.4","displayName":"gpt-5.4","isDefault":false}],"nextCursor":null}}"#;
        // feature 012: codex's mode catalog is the permission-profile list mapped to the
        // fixed enum (NOT collaborationMode). Verbatim 0.139.0 permissionProfile/list shape.
        let perm_resp = r#"{"jsonrpc":"2.0","id":51,"result":{"data":[{"id":":read-only","description":null},{"id":":workspace","description":null},{"id":":danger-full-access","description":null}],"nextCursor":null}}"#;
        let bytes = format!("{model_resp}\n{perm_resp}\n").into_bytes();
        let fake = FakeAgentIo::never_exits(bytes);
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        // Register the pending discovery ids the reader will claim (open_session does
        // this after the handshake; build_with_io skips the handshake).
        {
            let mut pd = backend.pending_discovery.lock().await;
            pd.insert(50, DiscoveryKind::Models);
            pd.insert(51, DiscoveryKind::Permissions);
        }
        // Subscribe to drive the reader; it consumes the two responses → fill_discovery.
        let _events = backend.events();
        // Poll capabilities() until the merge lands (reader runs async).
        let mut caps = backend.capabilities();
        for _ in 0..40 {
            if !caps.available_models.is_empty() && !caps.available_modes.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            caps = backend.capabilities();
        }
        assert_eq!(
            caps.available_models.len(),
            2,
            "model/list result.data[] filled available_models, got {caps:?}"
        );
        assert!(
            caps.available_models
                .iter()
                .any(|m| m.id == "openai.gpt-5.5" && m.name == "GPT-5.5"),
            "model id+displayName mapped from real wire, got {:?}",
            caps.available_models
        );
        assert!(
            caps.available_models.iter().any(|m| m.reasoning_efforts
                == vec![
                    "low".to_string(),
                    "medium".to_string(),
                    "high".to_string(),
                    "xhigh".to_string()
                ]),
            "supportedReasoningEfforts OBJECT array → reasoningEffort tokens, got {:?}",
            caps.available_models
        );
        // The three built-in permission tiers surface as the LEGACY bare tokens the old ACP
        // path advertised (`:workspace` → `auto`), in discovery order, so the picker's i18n
        // keys off the same value it always did. `normalize_to_profile_id` is the inverse on
        // the return trip; a custom profile (none here) would keep its colon id verbatim.
        assert_eq!(
            caps.available_modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["read-only", "auto", "full-access"],
            "permissionProfile/list built-ins → legacy bare tokens, got {:?}",
            caps.available_modes
        );
    }

    /// The FIX (async catalog-arrival signal): each `model/list` /
    /// `collaborationMode/list` RESPONSE must BROADCAST a `CatalogUpdated` carrying the
    /// current `discovered` snapshot — before this the parser silently filled the cache
    /// with no upward signal, so the frontend (which read an empty `config_options` on
    /// open) never re-fetched and the model selector stayed disabled. The two responses
    /// are SEPARATE, so we assert a snapshot arrives that carries BOTH lists (the second
    /// arrival refines the first).
    #[tokio::test]
    async fn model_list_response_broadcasts_catalog_updated() {
        use futures_util::StreamExt as _;
        let model_resp = r#"{"jsonrpc":"2.0","id":50,"result":{"data":[{"id":"openai.gpt-5.5","displayName":"GPT-5.5"}],"nextCursor":null}}"#;
        // codex's modes come from permissionProfile/list (colon ids on the wire); the
        // built-in `:workspace` tier surfaces to the frontend as the legacy bare token `auto`.
        let perm_resp =
            r#"{"jsonrpc":"2.0","id":51,"result":{"data":[{"id":":workspace","description":null}],"nextCursor":null}}"#;
        let bytes = format!("{model_resp}\n{perm_resp}\n").into_bytes();
        let fake = FakeAgentIo::never_exits(bytes);
        let backend = CodexSessionBackend::build_with_io("codex-1", Box::new(fake)).await;
        {
            let mut pd = backend.pending_discovery.lock().await;
            pd.insert(50, DiscoveryKind::Models);
            pd.insert(51, DiscoveryKind::Permissions);
        }
        let mut events = backend.events();
        // Collect CatalogUpdated events until one carries both a model and a mode (the
        // second, refining, snapshot) — or time out.
        let mut saw_both = false;
        for _ in 0..80 {
            if let Ok(Some(env)) = tokio::time::timeout(std::time::Duration::from_millis(200), events.next()).await
                && let SessionEvent::CatalogUpdated { models, modes, .. } = env.event
                && !models.is_empty()
                && !modes.is_empty()
            {
                assert_eq!(models[0].id, "openai.gpt-5.5");
                assert_eq!(
                    modes[0].id, "auto",
                    ":workspace surfaces as the legacy bare token `auto`"
                );
                saw_both = true;
                break;
            }
        }
        assert!(
            saw_both,
            "a CatalogUpdated snapshot carrying both the model and the mode must be broadcast"
        );
    }

    /// Cross-version fallback (README discipline #9): if a future codex renamed the
    /// model wrapper back to `models` or emitted bare-string reasoning efforts, the
    /// parser must still degrade gracefully (data-first, legacy-fallback) — so a rename
    /// never silently empties the model list again.
    #[tokio::test]
    async fn fill_discovery_accepts_legacy_models_key_and_string_efforts() {
        let discovered = Arc::new(std::sync::Mutex::new(Discovered::default()));
        // Legacy shape: result.models[] + bare-string supportedReasoningEfforts.
        let model_result: Value = serde_json::from_str(
            r#"{"models":[{"id":"legacy-1","displayName":"Legacy","supportedReasoningEfforts":["low","high"]}]}"#,
        )
        .unwrap();
        fill_discovery(DiscoveryKind::Models, &model_result, &discovered);
        let d = discovered.lock().unwrap();
        assert_eq!(d.models.len(), 1, "legacy result.models[] still parses");
        assert_eq!(
            d.models[0].reasoning_efforts,
            vec!["low".to_string(), "high".to_string()],
            "bare-string efforts still accepted"
        );
    }

    /// UT-1: `permissionProfile/list` response (0.139.0 live shape) surfaces every profile
    /// VERBATIM as a colon-prefixed mode id — built-in tiers AND a user custom profile,
    /// preserving discovery order. Legacy-ACP parity: codex defines the value set (like an
    /// ACP agent's `availableModes[]`), AionCore does not translate or whitelist.
    #[tokio::test]
    async fn fill_discovery_surfaces_permission_profiles_verbatim() {
        let discovered = Arc::new(std::sync::Mutex::new(Discovered::default()));
        // 0.139.0 built-in tiers + a user `[permissions.team-review]` custom profile with a
        // display name/description (which we must carry, not drop).
        let perm_result: Value = serde_json::from_str(
            r#"{"data":[{"id":":read-only","description":null},{"id":":workspace","description":null},{"id":":danger-full-access","description":null},{"id":":team-review","name":"Team Review","description":"Custom profile"}],"nextCursor":null}"#,
        )
        .unwrap();
        fill_discovery(DiscoveryKind::Permissions, &perm_result, &discovered);
        let d = discovered.lock().unwrap();
        // The catalog `id` is the FRONTEND-facing value: the three built-in tiers are mapped
        // to the legacy bare tokens the old ACP path advertised (`:workspace` → `auto`) so
        // the picker's i18n keys off the same value; a custom profile has no legacy bare
        // form and keeps its colon id verbatim. SetMode's `normalize_to_profile_id` is the
        // inverse on the return trip.
        assert_eq!(
            d.modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["read-only", "auto", "full-access", ":team-review"],
            "built-in tiers map to legacy bare tokens; custom keeps its colon id, in discovery order"
        );
        // The custom profile's own name/description are carried through (codex sent them).
        let custom = d
            .modes
            .iter()
            .find(|m| m.id == ":team-review")
            .expect("custom profile present");
        assert_eq!(custom.name, "Team Review", "custom profile display name carried");
        assert_eq!(custom.description.as_deref(), Some("Custom profile"));
        // A name-less built-in profile gets the friendly display copied from the legacy ACP
        // bridge (NOT the bare id) — this is the display parity with the old path. The
        // display table is keyed on the colon wire id, so the lookup still resolves after the
        // catalog id was mapped to its bare token.
        let workspace = d.modes.iter().find(|m| m.id == "auto").unwrap();
        assert_eq!(
            workspace.name, "Default",
            "built-in workspace tier gets the legacy friendly name"
        );
        assert_eq!(
            workspace.description.as_deref(),
            Some(
                "Codex can read and edit files in the current workspace, and run commands. Approval is required to access the internet or edit other files. (Identical to Agent mode)"
            ),
            "built-in workspace tier gets the legacy friendly description"
        );
        let read_only = d.modes.iter().find(|m| m.id == "read-only").unwrap();
        assert_eq!(read_only.name, "Read Only");
        let full = d.modes.iter().find(|m| m.id == "full-access").unwrap();
        assert_eq!(full.name, "Full Access");
    }

    /// UT-1b: `normalize_to_profile_id` — the ONE translation AionCore still owns —
    /// rewrites a legacy persisted BARE value onto its colon-prefixed profile id, passes a
    /// discovered/custom colon id through VERBATIM, and never yields a value codex rejects.
    /// Mirrors legacy ACP `normalize_requested_mode` (alias → native id).
    #[test]
    fn codex_perm_normalize_to_profile_id_maps_legacy_and_passes_colon_ids() {
        // Legacy bare values rewrite onto the danger-full-access colon id (parity with
        // legacy `codex_sandbox`: full-access/yolo/yoloNoSandbox → danger-full-access).
        // `agent-full-access` is the #608 canonical id; the rest are pre-021 legacy aliases.
        for legacy in ["agent-full-access", "full-access", "yolo", "yoloNoSandbox"] {
            assert_eq!(codex_perm::normalize_to_profile_id(legacy), ":danger-full-access");
        }
        // read-only bare → its colon id.
        assert_eq!(codex_perm::normalize_to_profile_id("read-only"), ":read-only");
        // Unknown / blank / default-ish bare → the safe workspace-write tier.
        for legacy in ["default", "auto", "autoEdit", "", "  ", "anything-unknown"] {
            assert_eq!(
                codex_perm::normalize_to_profile_id(legacy),
                ":workspace",
                "unknown/blank persisted mode falls to the safe workspace-write tier"
            );
        }
        // A colon-prefixed id (discovered built-in OR user custom profile) passes through
        // verbatim — codex, not AionCore, owns the value set (legacy-ACP parity).
        for id in [
            ":workspace",
            ":danger-full-access",
            ":read-only",
            ":my-custom",
            ":team-review",
        ] {
            assert_eq!(codex_perm::normalize_to_profile_id(id), id);
        }
        // Whitespace around a colon id is trimmed, not treated as a bare value.
        assert_eq!(codex_perm::normalize_to_profile_id("  :read-only  "), ":read-only");
        // A degenerate bare colon is nonsense → safe default, not a passthrough of ":".
        assert_eq!(codex_perm::normalize_to_profile_id(":"), ":workspace");
    }

    /// UT-1c: `mode_to_catalog_value` — the `capabilities.current_mode` seed translation —
    /// lands EVERY accepted mode vocabulary on a catalog value the picker can highlight.
    /// Regression pin for the empty-permission-label bug: resuming an old codex
    /// conversation feeds the #608 canonical id `agent-full-access` (produced by
    /// `normalize_requested_mode` from the persisted mode) into the seed; the outbound
    /// leg alone passed it through verbatim, the catalog only carries
    /// [read-only, auto, full-access], and the frontend rendered "权限 ·" with an empty
    /// label for a current value that matches no option.
    #[test]
    fn codex_perm_mode_to_catalog_value_lands_on_catalog_vocabulary() {
        // The bug's exact input: canonical full-access id → the catalog's bare token.
        assert_eq!(codex_perm::mode_to_catalog_value("agent-full-access"), "full-access");
        // Legacy full-access aliases collapse onto the same catalog value.
        for legacy in ["full-access", "yolo", "yoloNoSandbox"] {
            assert_eq!(codex_perm::mode_to_catalog_value(legacy), "full-access");
        }
        // The other two built-in tiers, in both bare and colon form.
        assert_eq!(codex_perm::mode_to_catalog_value("read-only"), "read-only");
        assert_eq!(codex_perm::mode_to_catalog_value(":read-only"), "read-only");
        assert_eq!(codex_perm::mode_to_catalog_value("auto"), "auto");
        assert_eq!(codex_perm::mode_to_catalog_value("default"), "auto");
        assert_eq!(codex_perm::mode_to_catalog_value(":workspace"), "auto");
        // Older persisted colon id for full access (pre-existing seed behavior kept).
        assert_eq!(codex_perm::mode_to_catalog_value(":danger-full-access"), "full-access");
        // A custom profile id round-trips colon-and-all — it IS its own catalog value.
        assert_eq!(codex_perm::mode_to_catalog_value(":team-review"), ":team-review");
        // Unknown bare tokens land on the workspace tier's value, mirroring the SetMode
        // apply path's bucketing so display and applied tier cannot drift.
        assert_eq!(codex_perm::mode_to_catalog_value("anything-unknown"), "auto");
    }

    /// feature 012 UT-2: an empty `permissionProfile/list` (older codex or drift) leaves
    /// `modes` empty and takes the present-but-empty warn path without panicking.
    #[tokio::test]
    async fn fill_discovery_permissions_empty_does_not_panic() {
        let discovered = Arc::new(std::sync::Mutex::new(Discovered::default()));
        let perm_result: Value = serde_json::from_str(r#"{"data":[],"nextCursor":null}"#).unwrap();
        fill_discovery(DiscoveryKind::Permissions, &perm_result, &discovered);
        assert!(discovered.lock().unwrap().modes.is_empty(), "empty data[] → no modes");
    }

    /// UT-3: a `permissionProfile/list` RESPONSE broadcasts a `CatalogUpdated` whose
    /// `modes` carry the verbatim colon profile ids, and the models field is preserved
    /// (orthogonal, not clobbered).
    #[tokio::test]
    async fn permission_profile_response_broadcasts_catalog_updated() {
        use futures_util::StreamExt as _;
        let model_resp = r#"{"jsonrpc":"2.0","id":50,"result":{"data":[{"id":"openai.gpt-5.5","displayName":"GPT-5.5"}],"nextCursor":null}}"#;
        let perm_resp = r#"{"jsonrpc":"2.0","id":52,"result":{"data":[{"id":":read-only","description":null},{"id":":workspace","description":null},{"id":":danger-full-access","description":null}],"nextCursor":null}}"#;
        let bytes = format!("{model_resp}\n{perm_resp}\n").into_bytes();
        let fake = FakeAgentIo::never_exits(bytes);
        let backend = CodexSessionBackend::build_with_io("codex-perm", Box::new(fake)).await;
        {
            let mut pd = backend.pending_discovery.lock().await;
            pd.insert(50, DiscoveryKind::Models);
            pd.insert(52, DiscoveryKind::Permissions);
        }
        let mut events = backend.events();
        let mut saw_modes = false;
        for _ in 0..80 {
            if let Ok(Some(env)) = tokio::time::timeout(std::time::Duration::from_millis(200), events.next()).await
                && let SessionEvent::CatalogUpdated { models, modes, .. } = env.event
                && !modes.is_empty()
            {
                assert_eq!(
                    modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
                    vec!["read-only", "auto", "full-access"],
                    "three built-in tiers surface as legacy bare tokens on the event"
                );
                // models already arrived → the snapshot must preserve it (orthogonal).
                assert_eq!(models.len(), 1, "permission modes do not clobber the model list");
                saw_modes = true;
                break;
            }
        }
        assert!(
            saw_modes,
            "a CatalogUpdated snapshot carrying the permission-tier modes must be broadcast"
        );
    }

    // ===== codex-model-gating: post-handshake validated model reconcile =====

    /// Build a backend whose reader has already learned a two-model catalog
    /// (`openai.gpt-5.5`, `openai.gpt-5.4`) AND bound a thread — the state
    /// `reconcile_codex_model` runs against. Returns (Arc<backend>, captured
    /// stdin) so a test can drive the reconcile and inspect the frames it writes.
    async fn backend_with_catalog_and_binding() -> (Arc<CodexSessionBackend>, Arc<tokio::sync::Mutex<Vec<u8>>>) {
        let started = r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-rec"}}}"#;
        let model_resp = r#"{"jsonrpc":"2.0","id":50,"result":{"data":[{"id":"openai.gpt-5.5","displayName":"GPT-5.5","isDefault":true},{"id":"openai.gpt-5.4","displayName":"gpt-5.4"}],"nextCursor":null}}"#;
        let bytes = format!("{started}\n{model_resp}\n").into_bytes();
        let fake = FakeAgentIo::never_exits(bytes);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-rec", Box::new(fake)).await;
        backend.pending_discovery.lock().await.insert(50, DiscoveryKind::Models);
        // Drive the reader so it binds the thread + fills the catalog.
        let _events = backend.events();
        for _ in 0..40 {
            let filled = !backend
                .discovered
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .models
                .is_empty();
            let bound = backend.thread_binding.lock().await.is_some();
            if filled && bound {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        (Arc::new(backend), captured)
    }

    /// A requested model that IS in the discovered catalog is applied via a real
    /// `thread/settings/update{model}` (the validated apply) — the same wire a manual
    /// SetModel uses. This is the codex analogue of ACP's reconcile issuing `set_model`
    /// only for a desire that survived `clear_invalid_desired_model`.
    #[tokio::test]
    async fn codex_model_reconcile_applies_valid_model() {
        let (backend, captured) = backend_with_catalog_and_binding().await;
        reconcile_codex_model(&backend, "openai.gpt-5.4".into()).await;
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""method":"thread/settings/update""#),
            "a catalog-valid model is applied via thread/settings/update, got: {written}"
        );
        assert!(
            written.contains(r#""model":"openai.gpt-5.4""#),
            "carries the requested (valid) model, got: {written}"
        );
    }

    /// A requested model that is NOT in the catalog (a stale frontend picker default the
    /// local codex lacks — the exact "新会话首个回复报上游错误" repro) is DROPPED: no
    /// `thread/settings/update` is written (the thread stays on codex's launch default),
    /// and the optimistic open-time `current_model` seed is cleared so a later SetMode
    /// can't build a collaborationMode around a model codex rejected. This is the port of
    /// ACP's `clear_invalid_desired_model`.
    #[tokio::test]
    async fn codex_model_reconcile_drops_invalid_model_and_clears_seed() {
        let (backend, captured) = backend_with_catalog_and_binding().await;
        // Optimistic open-time seed (open_session sets this from config.model).
        *backend.current_model.lock().await = Some("gpt-5.5-that-local-codex-lacks".into());
        // Run the reconcile inline (awaited directly — no detached task).
        reconcile_codex_model(&backend, "gpt-5.5-that-local-codex-lacks".into()).await;
        assert!(
            backend.current_model.lock().await.is_none(),
            "an invalid requested model must clear the optimistic current_model seed"
        );
        let written = captured_str_allow_empty(&captured).await;
        assert!(
            !written.contains(r#""method":"thread/settings/update""#),
            "an invalid model must NOT be applied to codex (no thread/settings/update), got: {written}"
        );
    }

    /// Drain captured stdin WITHOUT requiring non-empty output (the invalid-model
    /// reconcile writes NOTHING, so `captured_str`'s "poll until non-empty" would hang
    /// the full 40 iterations then still assert). Bounded settle, returns whatever is there.
    async fn captured_str_allow_empty(captured: &Arc<tokio::sync::Mutex<Vec<u8>>>) -> String {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        String::from_utf8_lossy(&captured.lock().await.clone()).to_string()
    }

    // ===== codex-mode-gating: post-handshake validated mode reconcile =====

    /// Build a backend whose reader has already learned the discovered permission-tier
    /// catalog (a `permissionProfile/list` response; the wire ids are the colon ids
    /// `:read-only`/`:workspace`/`:danger-full-access`, surfaced to the frontend as the
    /// legacy bare tokens `read-only`/`auto`/`full-access`) AND bound a thread — the state
    /// `reconcile_codex_mode` runs against. Returns (Arc<backend>, captured stdin). The
    /// permissions channel needs no current_model, so the caller seeds nothing.
    async fn backend_with_mode_catalog_and_binding() -> (Arc<CodexSessionBackend>, Arc<tokio::sync::Mutex<Vec<u8>>>) {
        let started = r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-mode"}}}"#;
        let perm_resp = r#"{"jsonrpc":"2.0","id":60,"result":{"data":[{"id":":read-only","description":null},{"id":":workspace","description":null},{"id":":danger-full-access","description":null}],"nextCursor":null}}"#;
        let bytes = format!("{started}\n{perm_resp}\n").into_bytes();
        let fake = FakeAgentIo::never_exits(bytes);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-mode-rec", Box::new(fake)).await;
        backend
            .pending_discovery
            .lock()
            .await
            .insert(60, DiscoveryKind::Permissions);
        let _events = backend.events();
        for _ in 0..40 {
            let filled = !backend
                .discovered
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .modes
                .is_empty();
            let bound = backend.thread_binding.lock().await.is_some();
            if filled && bound {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        (Arc::new(backend), captured)
    }

    /// A requested tier that IS in the discovered catalog is applied via a real
    /// `thread/settings/update{permissions}` (the validated apply) — the codex analogue of
    /// ACP's reconcile issuing `set_mode` only for a desire that survived
    /// `clear_invalid_desired_mode`. codex has NO thread/start permissions param, so this
    /// is ALSO the first time the persisted tier reaches codex at all.
    #[tokio::test]
    async fn codex_mode_reconcile_applies_valid_mode() {
        let (backend, captured) = backend_with_mode_catalog_and_binding().await;
        reconcile_codex_mode(&backend, "full-access".into()).await;
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""method":"thread/settings/update""#),
            "a catalog-valid tier is applied via thread/settings/update, got: {written}"
        );
        assert!(
            written.contains(r#""permissions":":danger-full-access""#),
            "carries the requested tier as its colon profile id, got: {written}"
        );
    }

    /// A legacy persisted BARE value (`yolo`) normalizes onto the `:danger-full-access`
    /// colon id that IS in the discovered catalog and applies straight through — zero
    /// fallback for an upgrading user.
    #[tokio::test]
    async fn codex_mode_reconcile_normalizes_legacy_persisted_value() {
        let (backend, captured) = backend_with_mode_catalog_and_binding().await;
        reconcile_codex_mode(&backend, "yolo".into()).await;
        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""permissions":":danger-full-access""#),
            "legacy `yolo` normalizes to full-access → :danger-full-access, got: {written}"
        );
    }

    /// A requested value that normalizes to a tier NOT in the catalog is DROPPED: no
    /// `thread/settings/update` is written (the thread stays on codex's default). This can
    /// only happen when discovery returned a partial catalog (e.g. only `:workspace`), so
    /// we seed a single-tier catalog and request `read-only`.
    #[tokio::test]
    async fn codex_mode_reconcile_drops_tier_absent_from_partial_catalog() {
        // Build a backend whose catalog has ONLY the default tier.
        let started = r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-mode"}}}"#;
        let perm_resp =
            r#"{"jsonrpc":"2.0","id":61,"result":{"data":[{"id":":workspace","description":null}],"nextCursor":null}}"#;
        let bytes = format!("{started}\n{perm_resp}\n").into_bytes();
        let fake = FakeAgentIo::never_exits(bytes);
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-mode-partial", Box::new(fake)).await;
        backend
            .pending_discovery
            .lock()
            .await
            .insert(61, DiscoveryKind::Permissions);
        let _events = backend.events();
        for _ in 0..40 {
            let filled = !backend
                .discovered
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .modes
                .is_empty();
            let bound = backend.thread_binding.lock().await.is_some();
            if filled && bound {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        reconcile_codex_mode(&backend, "read-only".into()).await;
        let written = captured_str_allow_empty(&captured).await;
        assert!(
            !written.contains(r#""method":"thread/settings/update""#),
            "a tier absent from the (partial) catalog must NOT be applied, got: {written}"
        );
    }

    // ===== O2: thread/turns/list response → CheckpointList event (up-leg) =====

    /// Gap-2: `dispatch(ListCheckpoints)` writes `thread/turns/list` AND the reader
    /// maps the response's `data: Vec<Turn>` to `SessionEvent::CheckpointList`. The
    /// down-leg alone (writing the RPC) was wired; without the up-leg the response
    /// landed with no consumer. This drives the response through the reader and
    /// asserts the event surfaces with the turns mapped to CheckpointEntry.
    #[tokio::test]
    async fn list_checkpoints_response_maps_to_checkpoint_list_event() {
        use futures_util::StreamExt as _;

        // A thread/turns/list response (codex ThreadTurnsListResponse{data:[Turn]}).
        let turns_resp = r#"{"jsonrpc":"2.0","id":77,"result":{"data":[{"id":"turn-a","status":"completed"},{"id":"turn-b","status":"running"}],"next_cursor":null}}"#;
        let bytes = format!("{turns_resp}\n").into_bytes();
        let fake = FakeAgentIo::never_exits(bytes);
        let backend = CodexSessionBackend::build_with_io("codex-ck", Box::new(fake)).await;
        // Register the pending id the reader will claim (dispatch does this; here we
        // mimic it so the response — keyed 77 — is recognized as a Checkpoints query).
        backend
            .pending_discovery
            .lock()
            .await
            .insert(77, DiscoveryKind::Checkpoints);
        // Subscribe BEFORE the reader consumes the response (broadcast drops
        // pre-subscribe messages), then collect until CheckpointList shows up.
        let mut events = backend.events();
        let mut found = None;
        for _ in 0..40 {
            match tokio::time::timeout(std::time::Duration::from_millis(25), events.next()).await {
                Ok(Some(env)) => {
                    if let SessionEvent::CheckpointList { entries } = env.event {
                        found = Some(entries);
                        break;
                    }
                }
                _ => continue,
            }
        }
        let entries = found.expect("reader emitted a CheckpointList event for the turns/list response");
        assert_eq!(entries.len(), 2, "both turns mapped to checkpoint entries");
        assert_eq!(entries[0].id, "turn-a", "Turn.id → CheckpointEntry.id");
        assert_eq!(
            entries[0].label.as_deref(),
            Some("completed"),
            "Turn.status → CheckpointEntry.label"
        );
        assert_eq!(entries[1].id, "turn-b");
        assert!(
            entries.iter().all(|e| e.turn_gen.is_none()),
            "codex turns carry no adapter turn_gen"
        );
    }

    // ===== R5: AnswerAuth vs serverRequest/resolved race on pending_auth_id =====

    /// 🔴 R5 (race characterization) — the reader's `serverRequest/resolved` handler
    /// clears `pending_auth_id` (codex_conn.rs:619) while `dispatch(AnswerAuth)`
    /// `.take()`s it (codex_conn.rs:1427). If the resolved-notif wins the race, the
    /// user's freshly-supplied credentials are DROPPED: AnswerAuth finds None and
    /// returns Err("no pending auth refresh to answer") — the token RESPONSE is never
    /// written, so codex's blocking refresh request can hang, and the user sees a
    /// spurious failure for credentials they correctly supplied.
    ///
    /// This pins the CURRENT behavior of the lose-the-race ordering (resolved arrives
    /// first). It is the highest-blast-radius auth race: if the fix later makes
    /// AnswerAuth idempotent-after-resolved, this test's assertion changes and flags
    /// the intended behavior shift.
    #[tokio::test]
    async fn r5_answer_auth_after_resolved_clears_pending_drops_credentials() {
        // Feed BOTH the refresh request (id=99, sets pending_auth_id) AND the
        // serverRequest/resolved for the same id (clears pending_auth_id) — the
        // reader processes both before we dispatch, modeling "resolved won the race".
        let lines = [
            r#"{"jsonrpc":"2.0","id":99,"method":"account/chatgptAuthTokens/refresh","params":{"reason":"expired"}}"#,
            r#"{"jsonrpc":"2.0","method":"serverRequest/resolved","params":{"request_id":99}}"#,
        ];
        let bytes = format!("{}\n", lines.join("\n")).into_bytes();
        let fake = FakeAgentIo::never_exits(bytes);
        let backend = CodexSessionBackend::build_with_io("codex-r5", Box::new(fake)).await;
        // Drain events until we see PermissionResolved{Auth} — proves the reader
        // processed the resolved notif and cleared pending_auth_id.
        {
            let mut events = backend.events();
            let saw_resolved = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while let Some(env) = events.next().await {
                    if matches!(
                        env.event,
                        SessionEvent::PermissionResolved {
                            kind: PermissionKind::Auth,
                            ..
                        }
                    ) {
                        return true;
                    }
                }
                false
            })
            .await
            .unwrap_or(false);
            assert!(
                saw_resolved,
                "the resolved notif must surface as PermissionResolved(Auth) (pending_auth_id cleared)"
            );
        }
        // Now the user answers — but pending_auth_id was already cleared by resolved.
        let res = backend
            .dispatch(Command::AnswerAuth {
                method_id: "chatgptAuthTokens".into(),
                credentials: json!({ "accessToken": "jwt-late", "chatgptAccountId": "acct-1" }),
            })
            .await;
        // CURRENT behavior: credentials are dropped with a Transport error.
        assert!(
            matches!(&res, Err(BackendError::Transport(m)) if m.contains("no pending auth refresh")),
            "R5: AnswerAuth losing the race to serverRequest/resolved drops the user's \
             credentials with a 'no pending auth refresh' error (got {res:?}). If this \
             assertion changes, the auth-race handling was intentionally reworked."
        );
    }

    /// R10 (ELECTRON-3Q0 fix B) — a `turn/start` ERROR response is codex REJECTING
    /// the turn. It must NOT emit PromptAccepted, and it must NOT be a silent drop
    /// (the pre-fix behavior: the correlation was removed and nothing emitted → the
    /// admitted turn hung Running forever, permanently locking the conversation).
    /// The reader now synthesizes an is_error terminal carrying the codex message,
    /// and a LATE real `turn/completed` is absorbed (I10 — exactly one terminal).
    #[tokio::test]
    async fn r10_turn_start_error_response_synthesizes_single_error_terminal() {
        // A turn/start ERROR response (id=1, the first next_rpc_id) followed by a
        // late turn/completed — the terminal must fire once, from the error.
        let tail = concat!(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"turn rejected"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"th-r10","turn":{"id":"t1","status":"completed"}}}"#,
            "\n"
        );
        let prefix = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-r10"}}}"#
        )
        .into_bytes();
        let fake = FakeAgentIo::new(prefix, None).with_gated_tail(tail.as_bytes().to_vec());
        let release = fake.stdout_releaser();
        let backend = CodexSessionBackend::build_with_io("codex-r10", Box::new(fake)).await;
        let mut events = backend.events();
        // Send with a client_msg_id → registers pending_sends[1].
        let receipt = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("hi".into())],
                metadata: super::super::types::CommandMeta {
                    client_msg_id: Some("m-1".into()),
                    ..Default::default()
                },
            })
            .await
            .expect("send accepted (codex turn/start written)");
        assert!(receipt.accepted);
        // Release the error response (+ late turn/completed) into the reader.
        release();
        let mut saw_prompt_accepted = false;
        let mut terminals: Vec<(bool, String)> = Vec::new();
        for _ in 0..12 {
            match tokio::time::timeout(std::time::Duration::from_millis(100), events.next()).await {
                Ok(Some(env)) => match env.event {
                    SessionEvent::PromptAccepted { .. } => saw_prompt_accepted = true,
                    SessionEvent::TurnResult {
                        is_error, result_text, ..
                    } => terminals.push((is_error, result_text)),
                    _ => {}
                },
                _ => break,
            }
        }
        assert!(
            !saw_prompt_accepted,
            "a turn/start ERROR response must NOT emit PromptAccepted (the turn never started)"
        );
        assert_eq!(
            terminals.len(),
            1,
            "exactly ONE terminal: the synthesized error; the late turn/completed is absorbed (I10), got {terminals:?}"
        );
        assert!(
            terminals[0].0 && terminals[0].1.contains("turn rejected"),
            "the terminal is is_error and carries the codex message verbatim, got {terminals:?}"
        );
    }

    /// ELECTRON-3Q0 fix A — codex REJECTED the `thread/resume` (dead resume anchor,
    /// "no rollout found for thread id …", verified:
    /// samples/codex-cli/0.144.1/dead_resume.jsonl). The reader must clear the
    /// pre-seeded binding and poison the bound-thread wait so the next Send fails
    /// FAST with `BackendError::SessionNotFound` carrying the codex message —
    /// previously the poisoned binding made turn/start hit the dead threadId and
    /// the turn hung forever.
    #[tokio::test]
    async fn thread_resume_error_clears_binding_and_poisons_dispatch() {
        let err_resp =
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32600,"message":"no rollout found for thread id th-dead"}}"#;
        let fake = FakeAgentIo::new(Vec::new(), None).with_gated_tail(format!("{err_resp}\n").into_bytes());
        let release = fake.stdout_releaser();
        let backend = CodexSessionBackend::build_with_io("codex-resume-dead", Box::new(fake)).await;
        // Mirror run_handshake's Resume arm: pre-seeded binding + registered rpc id.
        backend.seed_thread_binding_for_test("th-dead").await;
        backend.register_pending_resume_for_test(7).await;
        release();
        // The reader claims the error response: binding cleared, poison set.
        for _ in 0..40 {
            if backend.thread_binding.lock().await.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            backend.thread_binding.lock().await.is_none(),
            "the poisoned pre-seeded binding must be cleared on the thread/resume error"
        );
        let res = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("hi".into())],
                metadata: super::super::types::CommandMeta::default(),
            })
            .await;
        assert!(
            matches!(&res, Err(BackendError::SessionNotFound(m)) if m.contains("no rollout found for thread id th-dead")),
            "a Send after the rejected resume fails FAST as SessionNotFound with the codex cause (got {res:?})"
        );
        assert!(
            !backend.turn_in_flight.load(Ordering::SeqCst),
            "a dispatch that never reached the wire must not leave the turn-in-flight mark set"
        );
    }

    /// The happy-path counterpart: a `thread/resume` SUCCESS response leaves the
    /// pre-seeded binding intact and sets no poison.
    #[tokio::test]
    async fn thread_resume_success_leaves_binding_intact() {
        let ok_resp = r#"{"jsonrpc":"2.0","id":7,"result":{}}"#;
        let fake = FakeAgentIo::new(Vec::new(), None).with_gated_tail(format!("{ok_resp}\n").into_bytes());
        let release = fake.stdout_releaser();
        let backend = CodexSessionBackend::build_with_io("codex-resume-ok", Box::new(fake)).await;
        backend.seed_thread_binding_for_test("th-live").await;
        backend.register_pending_resume_for_test(7).await;
        release();
        // Give the reader a beat to process the response.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            backend.thread_binding.lock().await.as_deref(),
            Some("th-live"),
            "a successful resume keeps the pre-seeded binding"
        );
        assert!(
            backend.resume_poison.lock().await.is_none(),
            "a successful resume must not poison the bound-thread wait"
        );
    }

    /// ELECTRON-3Q0 fix B, NoTurn flavor — an `account/logout` ERROR response has
    /// no turn to terminate (dispatch admitted it as NoTurn): it surfaces as a
    /// `Notice`, and NO TurnResult is synthesized.
    #[tokio::test]
    async fn logout_error_response_surfaces_notice_not_terminal() {
        // dispatch(/logout) issues rpc id 1 (first next_rpc_id).
        let err_resp = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"not signed in"}}"#;
        let fake = FakeAgentIo::new(Vec::new(), None).with_gated_tail(format!("{err_resp}\n").into_bytes());
        let release = fake.stdout_releaser();
        let backend = CodexSessionBackend::build_with_io("codex-logout-err", Box::new(fake)).await;
        let mut events = backend.events();
        let receipt = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("/logout".into())],
                metadata: super::super::types::CommandMeta {
                    client_msg_id: Some("m-1".into()),
                    ..Default::default()
                },
            })
            .await
            .expect("logout dispatch accepted");
        assert!(matches!(receipt.admission, Admission::NoTurn));
        release();
        let mut saw_failure_notice = false;
        let mut saw_terminal = false;
        for _ in 0..12 {
            match tokio::time::timeout(std::time::Duration::from_millis(100), events.next()).await {
                Ok(Some(env)) => match env.event {
                    SessionEvent::Notice { message, .. } if message.contains("logout failed") => {
                        saw_failure_notice = true;
                    }
                    SessionEvent::TurnResult { .. } => saw_terminal = true,
                    _ => {}
                },
                _ => break,
            }
        }
        assert!(
            saw_failure_notice,
            "a rejected account/logout must surface a Notice with the codex cause"
        );
        assert!(
            !saw_terminal,
            "a NoTurn request must NOT synthesize a turn terminal on rejection"
        );
    }

    /// 🔴 R8 (bound_thread timeout during a never-arriving handshake) — every
    /// turn/* dispatch calls `bound_thread()` which polls up to ~2s for the async
    /// `thread/started` (codex_conn.rs:480-488). If the handshake never arrives
    /// (process crashed at startup / slow app-server), the dispatch blocks the full
    /// window then errors `BackendError::Transport`. Since the dispatch errors, no
    /// PromptAccepted fires → a conversation that already pushed a pending message
    /// leaks it (same ghost-pending family as R10). Pins the timeout-then-error.
    #[tokio::test]
    async fn r8_send_during_never_arriving_handshake_times_out_and_errors() {
        // No thread/started ever — bound_thread exhausts its budget. Use a short env
        // budget so the test does not wait the full 30s (the real cold-start window).
        // SAFETY: restored after; this assertion is about the ERROR CLASSIFICATION.
        let saved = std::env::var("AIONUI_HANDSHAKE_TIMEOUT_SECS").ok();
        unsafe { std::env::set_var("AIONUI_HANDSHAKE_TIMEOUT_SECS", "1") };
        let fake = FakeAgentIo::never_exits(Vec::new());
        let backend = CodexSessionBackend::build_with_io("codex-r8", Box::new(fake)).await;
        let res = backend
            .dispatch(Command::Send {
                content: vec![ContentBlock::Text("hi".into())],
                metadata: super::super::types::CommandMeta {
                    client_msg_id: Some("m-1".into()),
                    ..Default::default()
                },
            })
            .await;
        match saved {
            Some(v) => unsafe { std::env::set_var("AIONUI_HANDSHAKE_TIMEOUT_SECS", v) },
            None => unsafe { std::env::remove_var("AIONUI_HANDSHAKE_TIMEOUT_SECS") },
        }
        // codex-500 fix: the error must be the RETRYABLE HandshakeTimeout (agent still
        // starting), NOT a bare Transport that mapped to an opaque 500. (Was Transport
        // before — this test documented the bug as if it were correct.)
        assert!(
            matches!(&res, Err(BackendError::HandshakeTimeout(m)) if m.contains("threadId not bound")),
            "R8: a Send during a never-arriving handshake must error HandshakeTimeout (retryable), got {res:?}"
        );
    }

    /// 🔴 R4 (active_turn_id clear-wins-race vs Steer) — the reader clears
    /// `active_turn_id=None` on turn/completed (codex_conn.rs:597-598) while
    /// dispatch(Steer) reads it (codex_conn.rs:1363). When a turn finishes at the
    /// instant the user steers, the CLEAR-WINS ordering makes Steer read `None` →
    /// return Err("no active turn to steer"). Consumer symptom: a steer issued at
    /// end-of-turn is reported as a failure even though the user's intent (inject
    /// text into the turn) simply arrived a beat too late. This pins the clear-wins
    /// ordering (deterministic: the turn is fully completed before we steer).
    #[tokio::test]
    async fn r4_steer_after_turn_completed_clears_active_id_is_rejected() {
        // Full turn lifecycle: thread/started (bind) + turn/started (set active) +
        // turn/completed (CLEAR active_turn_id). All processed before we steer.
        let lines = [
            r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-r4"}}}"#,
            r#"{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"th-r4","turn":{"id":"turn-r4"}}}"#,
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"th-r4","turn":{"id":"turn-r4","status":"completed"}}}"#,
        ];
        let bytes = format!("{}\n", lines.join("\n")).into_bytes();
        let fake = FakeAgentIo::never_exits(bytes);
        let backend = CodexSessionBackend::build_with_io("codex-r4", Box::new(fake)).await;
        // Drain events until TurnResult — proves the reader processed turn/completed
        // (and therefore cleared active_turn_id).
        {
            let mut events = backend.events();
            let saw_terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while let Some(env) = events.next().await {
                    if matches!(env.event, SessionEvent::TurnResult { .. }) {
                        return true;
                    }
                }
                false
            })
            .await
            .unwrap_or(false);
            assert!(
                saw_terminal,
                "turn/completed must produce a TurnResult (active_turn_id cleared)"
            );
        }
        // Now steer — the active turn is gone (cleared by the reader).
        let res = backend
            .dispatch(Command::Steer {
                content: vec![ContentBlock::Text("wait, also do X".into())],
            })
            .await;
        assert!(
            matches!(&res, Err(BackendError::Transport(m)) if m.contains("no active turn to steer")),
            "R4: a Steer after the turn completed (active_turn_id cleared) is rejected with \
             'no active turn to steer' (got {res:?}) — the user's end-of-turn steer text \
             vanishes as a failure. If a fix queues a just-missed steer as a fresh turn, \
             this assertion changes."
        );
    }

    /// 🟡 R1 (concurrent-send turn_gen safety invariant) — two `dispatch(Send)`
    /// calls racing on the SAME backend each do `turn_gen.fetch_add` (codex_conn.rs:
    /// 1307). The epoch ORDER vs wire order can reorder (the full reorder is a
    /// loom-class property), but the LOAD-BEARING safety invariant is cheaper and
    /// must hold under any interleaving: the two receipts get DISTINCT, monotonic
    /// turn_gens (never the same epoch — a collision would conflate two turns' FSM
    /// epochs and break the cross-turn stale-result guard). This pins that invariant
    /// across a real concurrent race (AtomicU64 fetch_add guarantees it; the test
    /// would catch a regression that made turn_gen assignment non-atomic).
    #[tokio::test]
    async fn r1_concurrent_sends_get_distinct_monotonic_turn_gen() {
        use std::sync::Arc;
        // Bind the thread (NO active turn) so both sends reach the real fetch_add
        // arm. NOTE (009 R1c): we deliberately do NOT pre-bind an active turn here.
        // With an active turn the correct behavior is now NoTurn (a flight-period
        // Send merges, opening no second turn_gen — see
        // dispatch_send_during_active_turn_is_noturn_…). This test isolates the
        // OTHER invariant — that turn_gen assignment is atomic under a real
        // concurrent race when both sends legitimately open turns (no active turn
        // to merge into) — so the two arms get DISTINCT monotonic epochs.
        let lines = [r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-r1"}}}"#];
        let fake = FakeAgentIo::never_exits(format!("{}\n", lines.join("\n")).into_bytes());
        let backend = Arc::new(CodexSessionBackend::build_with_io("codex-r1", Box::new(fake)).await);
        // Let the reader bind the thread before dispatching.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let send = |b: Arc<CodexSessionBackend>, n: usize| async move {
            b.dispatch(Command::Send {
                content: vec![ContentBlock::Text(format!("msg-{n}"))],
                metadata: super::super::types::CommandMeta {
                    client_msg_id: Some(format!("m-{n}")),
                    ..Default::default()
                },
            })
            .await
        };
        let (r1, r2) = tokio::join!(send(backend.clone(), 1), send(backend.clone(), 2));
        let g1 = r1.expect("send 1 accepted").turn_gen;
        let g2 = r2.expect("send 2 accepted").turn_gen;
        assert_ne!(
            g1, g2,
            "R1: two concurrent sends MUST get distinct turn_gens (no epoch collision); got {g1} and {g2}"
        );
        let (lo, hi) = (g1.min(g2), g1.max(g2));
        assert_eq!(
            (lo, hi),
            (1, 2),
            "R1: concurrent sends get monotonic epochs 1 and 2 (no skip/dup), got {g1} and {g2}"
        );
    }

    // ===== CAPSTONE: a REAL CodexSessionBackend folded through the Orchestrator =====

    /// The 007 thesis end-to-end: a real `CodexSessionBackend` parsing actual codex
    /// JSON-RPC, driven through `Orchestrator::run()`, produces the unlock via
    /// `StateSnapshot.can_send` — with the SAME reducer/FSM claude uses (R11). This
    /// proves the seam is transport-agnostic: codex wire → SessionEvent → step() →
    /// snapshot, no codex-specific reducer path. The fake replays a full turn:
    /// thread/started → turn/started → agentMessage deltas → turn/completed.
    #[tokio::test]
    async fn codex_backend_folds_through_orchestrator_to_unlock() {
        use super::super::Orchestrator;
        use crate::state::SessionState;
        use futures_util::StreamExt as _;

        // Two-phase fixture (mirrors production ordering): the HANDSHAKE prefix
        // (`thread/started`) flows immediately so `send`'s bound_thread resolves;
        // the TURN tail is GATED until run() has subscribed to events() (broadcast
        // drops messages sent before subscribe). NOTE: no turn/started fold here —
        // the orchestrator lowers TurnStarted itself on send() (I9); the backend's
        // turn/started is optimistic (maps to vec![]). turn/completed folds
        // Running→Idle (the unlock).
        let prefix = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-cap"}}}"#
        )
        .into_bytes();
        let tail_lines = [
            r#"{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"th-cap","turn":{"id":"turn-cap"}}}"#,
            r#"{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"hello "}}"#,
            r#"{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"world"}}"#,
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"th-cap","turn":{"id":"turn-cap","status":"completed"}}}"#,
        ];
        let tail = format!("{}\n", tail_lines.join("\n")).into_bytes();
        let fake = FakeAgentIo::new(
            prefix,
            Some(crate::event::ExitStatusLite {
                code: Some(0),
                signal: None,
            }),
        )
        .with_gated_tail(tail);
        fake.release_exit();
        // Grab a release handle BEFORE the fake is boxed into the backend.
        let release = fake.stdout_releaser();
        let backend = CodexSessionBackend::build_with_io("sess-cap", Box::new(fake)).await;

        let orch = std::sync::Arc::new(Orchestrator::new(256));
        // Subscribe BEFORE the turn so we capture every snapshot.
        let mut states = orch.subscribe_state("sess-cap");

        // send() lowers TurnStarted (Idle→Running, can_send=false). The dispatch
        // writes turn/start, but the FakeAgentIo doesn't service requests — the
        // turn is driven by the SCRIPTED notifications above. bound_thread resolves
        // from the thread/started the reader parses.
        let receipt = orch
            .send(
                &backend,
                "sess-cap",
                vec![ContentBlock::Text("hi".into())],
                super::super::types::CommandMeta::default(),
            )
            .await
            .expect("send accepted");
        assert_eq!(receipt.admission, Admission::Started);

        // Run the fold loop until the backend stream ends (EOF after turn/completed).
        // `send` only borrowed `backend`; we now move it into the run task.
        let run = {
            let orch = orch.clone();
            tokio::spawn(async move { orch.run(&backend).await })
        };
        // run() now subscribes to events() inside the task; give it a tick to land,
        // THEN open the stdout gate so the scripted turn drives into a live subscriber.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        release();

        // Collect snapshots until we see the unlock (can_send=true while Idle).
        let unlocked = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut saw_locked_running = false;
            while let Some(snap) = states.next().await {
                if snap.session_id != "sess-cap" {
                    continue;
                }
                if matches!(snap.state, SessionState::Running { .. }) && !snap.can_send {
                    saw_locked_running = true;
                }
                if matches!(snap.state, SessionState::Idle) && snap.can_send && saw_locked_running {
                    return true; // locked during the turn, unlocked at the end
                }
            }
            false
        })
        .await
        .expect("must not hang");

        assert!(
            unlocked,
            "a real codex turn folded through the orchestrator must lock during the turn and unlock (can_send=true) at turn/completed"
        );
        // The run loop ends when the backend stream EOFs.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), run).await;
    }

    /// F-4 default: build_with_io → idle_ttl=None → never suspends (no timer, slot
    /// Active for life). Protects the parse/dispatch contract from any F-4 cost.
    #[tokio::test]
    async fn f4_off_by_default_no_suspension() {
        let backend =
            CodexSessionBackend::build_with_io("codex-1", Box::new(FakeAgentIo::never_exits(Vec::new()))).await;
        assert!(backend.idle_timer.is_none(), "no idle timer when idle_ttl is None");
        assert!(backend.suspend.is_active().await, "slot Active");
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert!(backend.suspend.is_active().await, "stays Active (production parity)");
    }

    /// F-4 suspend→wake: a configured idle_ttl suspends the idle app-server; the
    /// next dispatch(Send) wakes by re-spawning `codex app-server` through the
    /// injected spawner (then replaying the thread/resume handshake against the
    /// bound threadId). FakeSpawner records the spawn then Errs, so dispatch
    /// surfaces the wake error — the hermetic proof the resume re-spawn ran.
    #[tokio::test]
    async fn f4_suspend_then_wake_respawns_through_spawner() {
        use crate::testing::FakeSpawner;
        let spawner = Arc::new(FakeSpawner::new());
        let backend = CodexSessionBackend::build_with_io_suspending(
            "codex-resume-1",
            Box::new(FakeAgentIo::never_exits(Vec::new())),
            spawner.clone(),
            40,
        )
        .await;
        // The resume anchor that survives the suspend (live path binds it from
        // thread/started; seed it here).
        backend.seed_thread_binding_for_test("th-anchor-1").await;
        assert!(backend.idle_timer.is_some(), "idle timer spawned when ttl is Some");

        // Force a suspend without waiting on the timer cadence.
        assert!(
            backend
                .suspend
                .suspend_if_idle(aionui_common::now_ms() + 10_000, false)
                .await
        );
        assert!(!backend.suspend.is_active().await, "now Dormant");

        // Next Send must wake → re-spawn `codex app-server` through the spawner.
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
        assert_eq!(spec.command.to_str(), Some("codex"), "wake re-spawns the codex binary");
        assert!(
            spec.args.iter().any(|a| a == "app-server"),
            "wake re-spawns `codex app-server`, got {:?}",
            spec.args
        );
        drop(backend);
    }

    /// codex `fileChange` completed item → `ToolResultContent::FilePath` per change.
    /// WIRED (was the Gap #6 TRIPWIRE): a real completed-item fixture (0.139.0,
    /// missing-wire-probe) confirmed the shape is `changes:[{path, kind:{type}, diff}]`
    /// — NOT a flat top-level `path`. map_item now mints one FilePath per change,
    /// carrying the unified `diff` as `new_text` so the TurnFinalizer renders a
    /// FileDiff card. The `aggregatedOutput` Text still rides alongside.
    #[test]
    fn codex_filechange_extracts_filepath_from_changes() {
        let params = serde_json::json!({
            "item": {
                "type": "fileChange",
                "id": "call_1",
                "status": "completed",
                "changes": [{
                    "path": "/w/patch_target.py",
                    "kind": { "type": "update", "move_path": null },
                    "diff": "@@ -3,2 +3,3 @@\n \n-# TODO\n+def subtract(a, b):\n+    return a - b\n"
                }]
            }
        });
        let events = map_item(&params, true);
        let content: Vec<&crate::event::ToolResultContent> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::ToolResult { content, .. } => Some(content.iter()),
                _ => None,
            })
            .flatten()
            .collect();
        let filepath = content
            .iter()
            .find_map(|c| match c {
                crate::event::ToolResultContent::FilePath { path, new_text, .. } => {
                    Some((path.clone(), new_text.clone()))
                }
                _ => None,
            })
            .expect("fileChange change → FilePath");
        assert_eq!(filepath.0, "/w/patch_target.py");
        assert!(
            filepath.1.as_deref().is_some_and(|d| d.contains("def subtract")),
            "the unified diff rides FilePath.new_text, got {:?}",
            filepath.1
        );
    }

    /// codex `imageGeneration` completed → the produced image's PATH reaches a
    /// FilePath ToolResultContent. The wire key is `savedPath` (source-verified
    /// v2/item.rs:372-380 ImageGeneration{result:String(base64), saved_path → camelCase
    /// `savedPath`}), NOT a guessed `path`, and NOT the base64 `result` (which would
    /// dump bytes). Was a TRIPWIRE recording the drop ("shape not captured"); the
    /// shape WAS in the schema all along — flipped to assert the path is carried.
    #[test]
    fn codex_imagegeneration_saved_path_reaches_file_path() {
        let params = serde_json::json!({
            "item": {
                "type": "imageGeneration",
                "id": "call_x",
                "status": "completed",
                "result": "iVBORw0KGgo=", // base64 bytes — must NOT be dumped as Text
                "savedPath": "/w/generated.png"
            }
        });
        let events = map_item(&params, true);
        let content: Vec<&crate::event::ToolResultContent> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::ToolResult { content, .. } => Some(content.iter()),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(
            content.iter().any(|c| matches!(c,
                crate::event::ToolResultContent::FilePath { path, .. } if path == "/w/generated.png")),
            "imageGeneration savedPath must reach a FilePath ToolResultContent, got {content:?}"
        );
        assert!(
            !content.iter().any(|c| matches!(c,
                crate::event::ToolResultContent::Text(t) if t.contains("iVBORw0KGgo"))),
            "the base64 `result` bytes must NOT be dumped as Text, got {content:?}"
        );
    }

    /// LC-8a: codex `turn/plan/updated` → `SessionEvent::Plan`. step→content,
    /// camelCase `inProgress`→InProgress, priority None (codex has none), explanation
    /// carried.
    ///
    /// WIRE-CONFIRMED (live capture protocols/samples/codex-cli/0.139.0/
    /// _all_rollback_plan.jsonl): a real turn/plan/updated is
    /// `{threadId, turnId, explanation:null, plan:[{step:"...", status:"pending"}, ...]}`
    /// — the `{step, status}` per-entry keys + top-level `explanation` match exactly.
    /// The live run only exhibited status "pending" (all steps pending at plan creation);
    /// the `inProgress`/`completed` values below are the schema-defined enum tokens
    /// (TurnPlanStepStatus, 0.137.0) the normalizer also handles — kept to pin the
    /// status mapping across all three states.
    #[test]
    fn turn_plan_updated_maps_to_plan_event() {
        use crate::event::{PlanStatus, SessionEvent};
        let params = serde_json::json!({
            "threadId": "th1",
            "turnId": "t1",
            "explanation": "stepwise",
            "plan": [
                { "step": "read the code", "status": "completed" },
                { "step": "write the fix", "status": "inProgress" },
                { "step": "run tests", "status": "pending" },
            ],
        });
        let events = map_notification("turn/plan/updated", &params);
        match &events[..] {
            [SessionEvent::Plan { entries, explanation }] => {
                assert_eq!(entries.len(), 3);
                assert_eq!(entries[0].content, "read the code");
                assert_eq!(entries[0].status, PlanStatus::Completed);
                assert_eq!(
                    entries[1].status,
                    PlanStatus::InProgress,
                    "camelCase inProgress normalized"
                );
                assert_eq!(entries[2].status, PlanStatus::Pending);
                assert!(entries[0].priority.is_none(), "codex carries no per-step priority");
                assert_eq!(explanation.as_deref(), Some("stepwise"));
            }
            other => panic!("expected one Plan event, got {other:?}"),
        }
    }

    /// Regression-by-rewrite (codex-500): the bound-thread handshake budget must be the
    /// legacy 30s (aionui-agent-rest INIT_TIMEOUT_SECS), NOT the magic 2s the rewrite
    /// introduced. Pins the value so a future shrink reds here, and that the env override
    /// is honored. (Pure — no timing.)
    #[test]
    fn handshake_budget_is_legacy_30s_default_env_overridable() {
        // Default (no env): 30s parity with legacy ACP. We assert >= 15s so a future
        // tweak within reason passes, but the old 2s would fail loudly.
        // SAFETY: single-threaded assertion on a process-global; we restore after.
        let saved = std::env::var("AIONUI_HANDSHAKE_TIMEOUT_SECS").ok();
        unsafe { std::env::remove_var("AIONUI_HANDSHAKE_TIMEOUT_SECS") };
        assert!(
            super::super::handshake_budget() >= std::time::Duration::from_secs(15),
            "handshake budget must cover a cold start (>=15s), NOT the old magic 2s"
        );
        unsafe { std::env::set_var("AIONUI_HANDSHAKE_TIMEOUT_SECS", "45") };
        assert_eq!(
            super::super::handshake_budget(),
            std::time::Duration::from_secs(45),
            "env override honored"
        );
        match saved {
            Some(v) => unsafe { std::env::set_var("AIONUI_HANDSHAKE_TIMEOUT_SECS", v) },
            None => unsafe { std::env::remove_var("AIONUI_HANDSHAKE_TIMEOUT_SECS") },
        }
    }

    /// Regression-by-rewrite (codex-500): when thread/started never arrives, bound_thread
    /// must return the RETRYABLE `HandshakeTimeout` (agent still starting), NOT a bare
    /// `Transport` (which mapped to an opaque 500). Uses a tiny injected budget so the
    /// timeout branch is exercised deterministically (no 30s wait, no global env).
    #[tokio::test]
    async fn bound_thread_timeout_is_handshake_timeout_not_transport() {
        // never_exits + empty stdout → no thread/started → never binds.
        let fake = FakeAgentIo::never_exits(Vec::new());
        let backend = CodexSessionBackend::build_with_io("codex-noth", Box::new(fake)).await;
        let err = backend
            .bound_thread_within(std::time::Duration::from_millis(120))
            .await
            .expect_err("no thread/started → must time out");
        assert!(
            matches!(err, BackendError::HandshakeTimeout(_)),
            "handshake wait timeout must be the RETRYABLE HandshakeTimeout (not Transport→500), got {err:?}"
        );
    }

    /// Positive: a thread/started that arrives LATE (past the old 2s would have failed)
    /// but within budget still binds — proving the longer budget covers a slow start.
    #[tokio::test]
    async fn bound_thread_binds_when_thread_started_arrives_within_budget() {
        let fake = FakeAgentIo::never_exits(Vec::new());
        let backend = CodexSessionBackend::build_with_io("codex-late", Box::new(fake)).await;
        // Simulate a slow handshake: bind the thread after 150ms (the reader would do
        // this on a real late thread/started). bound_thread_within(2s) must still succeed.
        let binding = backend.thread_binding.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            *binding.lock().await = Some("th-late".to_string());
        });
        let tid = backend
            .bound_thread_within(std::time::Duration::from_secs(2))
            .await
            .expect("a within-budget late binding must succeed");
        assert_eq!(
            tid, "th-late",
            "the late-arriving threadId binds (no premature timeout)"
        );
    }

    /// Fork handshake down-leg: `run_handshake(Fork)` writes `thread/fork` with
    /// the PARENT threadId + `lastTurnId`, and — unlike Resume — pre-seeds NO
    /// thread binding (the NEW thread's id arrives via `thread/started`).
    #[tokio::test]
    async fn fork_handshake_writes_thread_fork_without_preseeding_binding() {
        use futures_util::StreamExt as _;
        // Gated tail: the `thread/started` for the forked (NEW) thread, released
        // only after the down-leg assertions.
        let tail = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-child"}}}"#
        )
        .into_bytes();
        let fake = FakeAgentIo::new(Vec::new(), None).with_gated_tail(tail);
        let release = fake.stdout_releaser();
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::build_with_io("codex-fork", Box::new(fake)).await;
        let mut events = backend.events();

        backend
            .run_handshake(HandshakeMode::Fork {
                parent_thread_id: "th-parent",
                last_turn_id: Some("turn-3"),
            })
            .await
            .expect("fork handshake writes");

        let written = captured_str(&captured).await;
        assert!(
            written.contains(r#""method":"thread/fork""#),
            "wrote thread/fork, got: {written}"
        );
        assert!(
            written.contains(r#""threadId":"th-parent""#) && written.contains(r#""lastTurnId":"turn-3""#),
            "fork targets the parent thread at the anchor turn, got: {written}"
        );
        assert!(
            backend.thread_binding.lock().await.is_none(),
            "Fork must NOT pre-seed the binding — the parent id is only the fork source"
        );

        // Release the thread/started for the NEW thread: the reader binds it and
        // lowers BackendBound{th-child} (the fork conversation's resume anchor).
        release();
        let bound = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::BackendBound { backend_session_id } = env.event {
                    return backend_session_id;
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        assert_eq!(bound.as_deref(), Some("th-child"), "the NEW thread id is lowered");
        assert_eq!(
            backend.thread_binding.lock().await.as_deref(),
            Some("th-child"),
            "the reader bound the forked thread"
        );
    }

    /// Fork handshake error-leg: a `thread/fork` ERROR (parent rollout gone)
    /// poisons the bound-thread wait so the first Send fails fast with the real
    /// cause — never a silent fresh/HEAD fallback, never an opaque timeout.
    #[tokio::test]
    async fn fork_handshake_error_poisons_bound_thread_wait() {
        // rpc ids: initialize=1, thread/fork=2 → the error must carry id 2.
        let tail = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32603,"message":"no rollout found for thread id th-parent"}}"#
        )
        .into_bytes();
        let fake = FakeAgentIo::new(Vec::new(), None).with_gated_tail(tail);
        let release = fake.stdout_releaser();
        let backend = CodexSessionBackend::build_with_io("codex-fork-err", Box::new(fake)).await;

        backend
            .run_handshake(HandshakeMode::Fork {
                parent_thread_id: "th-parent",
                last_turn_id: None,
            })
            .await
            .expect("the down-leg write itself succeeds");
        release();
        // Give the reader a beat to claim the error response.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let err = backend
            .bound_thread_within(std::time::Duration::from_millis(200))
            .await
            .expect_err("poisoned wait fails fast");
        assert!(
            matches!(&err, BackendError::SessionNotFound(m) if m.contains("thread/fork failed") && m.contains("no rollout")),
            "fork rejection surfaces as SessionNotFound with the codex cause, got {err:?}"
        );
    }

    /// Fork anchoring: `turn/started` lowers codex's OWN turn id as
    /// BackendTurnBound so the conversation layer can stamp it onto the turn's
    /// message rows (the `thread/fork` lastTurnId lookup key).
    #[tokio::test]
    async fn turn_started_emits_backend_turn_bound() {
        use futures_util::StreamExt as _;
        let tail = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","method":"turn/started","params":{"turn":{"id":"turn-7"}}}"#
        )
        .into_bytes();
        let fake = FakeAgentIo::new(Vec::new(), None).with_gated_tail(tail);
        let release = fake.stdout_releaser();
        let backend = CodexSessionBackend::build_with_io("codex-turnid", Box::new(fake)).await;
        let mut events = backend.events();
        release();
        let bound = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(env) = events.next().await {
                if let SessionEvent::BackendTurnBound { backend_turn_id } = env.event {
                    return Some(backend_turn_id);
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        assert_eq!(bound.as_deref(), Some("turn-7"), "turn/started lowers codex's turn id");
    }

    /// Late-subscriber self-heal: `thread/started` can land BEFORE the
    /// orchestration layer subscribes (codex answers thread/start in
    /// single-digit ms), which used to lose BackendBound forever — the
    /// conversation never persisted its resume anchor and the fork API
    /// refused with FORK_PARENT_UNBOUND. `events()` must replay the current
    /// binding as a synthetic BackendBound to every new subscriber.
    #[tokio::test]
    async fn events_replays_binding_to_late_subscribers() {
        use futures_util::StreamExt as _;
        let prefix = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-late-bind"}}}"#
        )
        .into_bytes();
        let fake = FakeAgentIo::never_exits(prefix);
        let backend = CodexSessionBackend::build_with_io("codex-late-sub", Box::new(fake)).await;
        // Let the reader consume thread/started BEFORE anyone subscribes —
        // the live BackendBound broadcast goes to zero receivers.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert_eq!(
            backend.thread_binding.lock().await.as_deref(),
            Some("th-late-bind"),
            "precondition: the reader already bound the thread"
        );

        let mut events = backend.events();
        let first = tokio::time::timeout(std::time::Duration::from_secs(1), events.next())
            .await
            .expect("the synthetic preface arrives immediately")
            .expect("stream open");
        assert!(
            matches!(
                first.event,
                SessionEvent::BackendBound { backend_session_id: Some(ref tid) } if tid == "th-late-bind"
            ),
            "a late subscriber's first event is the replayed binding, got {:?}",
            first.event
        );
    }
}
