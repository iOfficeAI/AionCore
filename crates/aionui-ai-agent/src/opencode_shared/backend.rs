//! `SessionBackend` / `BackendConnection` implementation over the shared
//! `opencode serve` HTTP/SSE server (see [`super::pool`]).
//!
//! One [`OpencodeSessionBackend`] == one opencode session (`ses_…`) hosted by
//! the SHARED server; N conversations == N backends == ONE bun process.
//!
//! Pump architecture (all tasks per-session, all cheap localhost HTTP):
//! * **SSE pump** — attaches `GET /api/session/{id}/event?after=<cursor>`,
//!   parses `data:` JSON lines, feeds [`translate`], advances the durable
//!   cursor; on transport loss reconnects from the cursor (server replays the
//!   gap — verified across restarts).
//! * **Pending poller** — `GET …/permission` + `GET …/question` and diffs the
//!   local maps: asked events are NOT durable (captured: a turn blocked on
//!   approval emits no asked event), so the poller is their only source, and
//!   it makes pendings restart-resilient. Disappearance → auto-resolve.
//! * **Active poller** — runs only while a turn is in flight; the session
//!   leaving `GET /api/session/active` is the authoritative turn end
//!   (captured: `/wait` answers 503 in 1.18.30 and `session.idle` is not in
//!   the durable history), with a settle grace to drain trailing SSE frames.
//! * **Death watcher** — pool liveness watch → `Detached` (crash avalanche:
//!   every attached conversation detaches; the orchestrator's crash-respawn
//!   re-opens against a fresh server from the persisted `ses_…` anchor).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use futures_util::StreamExt as _; // for stream::iter(..).chain(..) in events()
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;

use aionui_session::{
    Admission, BackendConnection, BackendError, BlockSet, Capabilities, CapabilityTier, Command, CommandReceipt,
    CommandSet, ContentBlock, PendingPermissionView, PermissionDecision, PermissionKind, PromptAcceptedSource,
    QuestionAnswer, SessionBackend, SessionConfig, SessionEnvelope, SessionEvent, SessionSpec, SignalSet,
};

use super::HttpError;
use super::client::{OpencodeClient, PromptAdmit, sse_data_to_event};
use super::pool::{ServerLease, global_pool};
use super::translate::{self, TranslateState};

/// Turn-end settle grace: let the SSE pump ingest the final frames (tool
/// results / trailing text) racing the active-map flip.
const TURN_SETTLE_GRACE: std::time::Duration = std::time::Duration::from_millis(600);
const ACTIVE_POLL: std::time::Duration = std::time::Duration::from_millis(700);
const PENDING_POLL: std::time::Duration = std::time::Duration::from_millis(1200);
const SSE_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(5);

/// `dispatch` return for a command whose capability bit is off.
fn not_supported(name: &'static str) -> BackendError {
    BackendError::CommandNotSupported { command: name }
}

pub struct OpencodeConnection;

struct SessionTasks {
    sse: JoinHandle<()>,
    pending: JoinHandle<()>,
    active: JoinHandle<()>,
    alive: JoinHandle<()>,
    _lease: Arc<ServerLease>,
}

impl Default for OpencodeConnection {
    fn default() -> Self {
        Self::new()
    }
}

impl OpencodeConnection {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl BackendConnection for OpencodeConnection {
    async fn open_session(
        &self,
        spec: SessionSpec,
        config: SessionConfig,
    ) -> Result<Arc<dyn SessionBackend>, BackendError> {
        let program = config
            .cli_program
            .clone()
            .ok_or_else(|| BackendError::SetupRejected("opencode shared: no cli_program resolved".into()))?;
        // SessionConfig.cwd is a plain String path (same one the ACP path spawns
        // in); empty/None falls back to the process cwd.
        let directory = match config.cwd.as_ref().filter(|c| !c.is_empty()) {
            Some(c) => c.clone(),
            None => std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| ".".to_string()),
        };
        if !std::path::Path::new(&directory).is_dir() {
            return Err(BackendError::WorkspaceUnavailable(directory));
        }

        let spawn_env: Vec<(String, String)> = config
            .spawn_env
            .iter()
            .map(|e| (e.name.clone(), e.value.clone()))
            .collect();
        let acquired = global_pool()
            .acquire(Path::new(&program), &spawn_env)
            .await
            .map_err(BackendError::Transport)?;
        let client = OpencodeClient::new(&acquired.lease.info);

        // Two-id model: opencode's `ses_…` is the BACKEND id; the orchestrator's
        // envelope always carries the LOGICAL conversation id.
        let logical_id = match &spec {
            SessionSpec::Fresh { session_id } | SessionSpec::Resume { session_id, .. } => session_id.clone(),
            // Fork is refused at the factory (see build_opencode_instance);
            // defensive: treat as Fresh-on-parent-id impossible here, so fail
            // loudly rather than silently opening the wrong context.
            SessionSpec::Fork { .. } => {
                return Err(BackendError::SetupRejected(
                    "opencode shared backend does not support fork (product decision, mirrors antigravity)".into(),
                ));
            }
        };

        // Resolve the opencode session id: resume-if-alive, else create fresh.
        // A dead anchor falls back to a NEW session — history continuity is the
        // orchestrator's rehydrate's job (DB), and the fresh id is reported via
        // BackendBound so the anchor repo self-heals.
        let requested = match &spec {
            SessionSpec::Resume {
                backend_session_id: Some(id),
                ..
            } => Some(id.clone()),
            _ => None,
        };
        let (backend_id, _created_fresh) = match &requested {
            Some(id) if client.get_session(id).await.is_ok() => (id.clone(), false),
            _ => {
                let (id, _project) = client.create_session(&directory).await.map_err(transport_err)?;
                (id, true)
            }
        };

        // Apply the selected model (POST …/model switches the session model
        // mid-life — live-verified; create body cannot carry it, so both fresh
        // and resumed sessions converge here). Failures are non-fatal: a
        // wrong/stale id just keeps the server default, matching how the ACP
        // path treats an unresolvable --model.
        if let Some(model) = config.model.as_deref().filter(|m| !m.trim().is_empty())
            && let Err(e) = client.set_model(&backend_id, model).await
        {
            tracing::warn!(backend_session_id = %backend_id, model, error = %e,
                "opencode shared: model switch rejected (server default stays in effect)");
        }

        let (tx, _rx0) = broadcast::channel::<SessionEnvelope>(2048);
        let epoch = Arc::new(AtomicU64::new(0));
        let turn_in_flight = Arc::new(AtomicBool::new(false));
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let translate = Arc::new(Mutex::new(TranslateState::default()));
        let cursor = Arc::new(AtomicU64::new(0));
        // History tail first: seeds the cursor at the durable tip so the live
        // subscription never replays old turns (they live in the DB via
        // rehydrate), and marks restart-surviving pendings.
        let tip = {
            let hist = client.history_tail(&backend_id).await;
            hist.iter().rev().find_map(|e| e.seq).unwrap_or(0)
        };
        cursor.store(tip, Ordering::SeqCst);

        let st = Arc::new(SessionState {
            logical_id: logical_id.clone(),
            backend_id: backend_id.clone(),
            client: client.clone(),
            tx,
            epoch: epoch.clone(),
            turn_in_flight: turn_in_flight.clone(),
            cancel_requested: cancel_requested.clone(),
            translate: translate.clone(),
            cursor: cursor.clone(),
            pending_perms: Arc::new(Mutex::new(HashMap::new())),
            pending_asks: Arc::new(Mutex::new(HashMap::new())),
            // First-prompt context injection (preset + skill index), consumed
            // by the first Send — the SessionInit channel the shared spawn has
            // no process-level surface for.
            pending_context: Arc::new(Mutex::new(config.init.preset_context.clone())),
        });

        let sse = spawn_sse_pump(st.clone());
        let pending = spawn_pending_poller(st.clone());
        let alive = spawn_death_watcher(st.clone(), acquired.alive.clone());
        // Turn closer: flips TurnResult when the active map empties. Lives for
        // the session's lifetime but only WORKS while a turn is in flight.
        let active = spawn_active_poller(st.clone());

        st.emit(SessionEvent::BackendBound {
            backend_session_id: Some(backend_id.clone()),
        });

        let backend = Arc::new(OpencodeSessionBackend {
            state: st,
            // Teardown is Drop-driven (same contract as the codex backend's
            // Drop-reap): abort pumps, release the pool lease. close_session
            // is therefore a no-op — the orchestrator releases its Arc and the
            // Drop fires; a lingering orchestrator clone keeps the pumps alive
            // until the task truly unwinds, which is exactly desired.
            tasks: std::sync::Mutex::new(Some(SessionTasks {
                sse,
                pending,
                active,
                alive,
                _lease: acquired.lease,
            })),
        });
        Ok(backend)
    }

    async fn close_session(&self, session_id: &str) -> Result<(), BackendError> {
        // Drop-driven teardown (see above). Teardown is idempotent either way;
        // nothing to retract here.
        let _ = session_id;
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        opencode_capabilities()
    }
}

fn transport_err(e: HttpError) -> BackendError {
    BackendError::Transport(e.to_string())
}

/// Shared per-session handle state used by every pump.
struct SessionState {
    logical_id: String,
    backend_id: String,
    client: OpencodeClient,
    tx: broadcast::Sender<SessionEnvelope>,
    epoch: Arc<AtomicU64>,
    turn_in_flight: Arc<AtomicBool>,
    cancel_requested: Arc<AtomicBool>,
    translate: Arc<Mutex<TranslateState>>,
    cursor: Arc<AtomicU64>,
    /// request_id → card metadata (for pending_permission_requests() recovery
    /// and auto-resolve on disappearance).
    pending_perms: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    pending_asks: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    pending_context: Arc<Mutex<Option<String>>>,
}

impl SessionState {
    fn emit(&self, event: SessionEvent) {
        let env = SessionEnvelope {
            session_id: self.logical_id.clone(),
            turn_gen: self.epoch.load(Ordering::SeqCst),
            event,
        };
        // Send fails only when nobody listens (closed session): drop silently,
        // same tolerance as the codex adapter's emit helper.
        let _ = self.tx.send(env);
    }
}

/// SSE pump with cursor-resumable reconnect.
fn spawn_sse_pump(st: Arc<SessionState>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = std::time::Duration::from_millis(250);
        loop {
            let after = st.cursor.load(Ordering::SeqCst);
            match st.client.event_stream(&st.backend_id, Some(after)).await {
                Ok(mut resp) => {
                    backoff = std::time::Duration::from_millis(250);
                    // chunk() (not bytes_stream()) so the workspace's
                    // feature-light reqwest suffices — no extra feature gate.
                    let mut buf: Vec<u8> = Vec::new();
                    let mut gone = false;
                    // chunk() yields Result<Option<Bytes>> — None is a clean
                    // end-of-stream, Err is a broken connection.
                    loop {
                        match resp.chunk().await {
                            Ok(Some(bytes)) => {
                                buf.extend_from_slice(&bytes);
                                // SSE frames end at a blank line; process by
                                // DATA lines (opencode never uses `event:`).
                                while let Some(pos) = find_line_end(&buf) {
                                    let line: Vec<u8> = buf.drain(..=pos).collect();
                                    let line = String::from_utf8_lossy(&line);
                                    let line = line.trim_end_matches(['\r', '\n']);
                                    let Some(payload) = line.strip_prefix("data:") else {
                                        continue;
                                    };
                                    let payload = payload.trim();
                                    if payload.is_empty() {
                                        continue;
                                    }
                                    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
                                        continue;
                                    };
                                    let Some(ev) = sse_data_to_event(v) else { continue };
                                    if let Some(seq) = ev.seq {
                                        st.cursor.fetch_max(seq + 1, Ordering::SeqCst);
                                    }
                                    handle_stream_event(&st, &ev).await;
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                tracing::debug!(backend = %st.backend_id, error = %e,
                                    "opencode shared sse stream broke; reconnecting");
                                gone = true;
                                break;
                            }
                        }
                    }
                    if !gone {
                        // Clean end-of-stream (server closed the subscription):
                        // reconnect too.
                        tracing::debug!(backend = %st.backend_id, "opencode shared sse ended; reconnecting");
                    }
                }
                Err(e) => {
                    tracing::debug!(backend = %st.backend_id, error = %e,
                        "opencode shared sse connect failed; retrying");
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(SSE_RETRY_MAX);
        }
    })
}

fn find_line_end(buf: &[u8]) -> Option<usize> {
    buf.iter().position(|b| *b == b'\n')
}

/// SSE-side handling: translate + a few lifecycle cases the translate layer
/// cannot decide alone (turn-end promotion of error/idle, asked-card dedupe).
async fn handle_stream_event(st: &SessionState, ev: &super::client::StreamEvent) {
    match ev.event_type.as_str() {
        // Durable prompt echo frames are owned by dispatch's receipts.
        "session.next.prompt.admitted" | "session.next.prompted" => {}
        "session.idle" => {
            finalize_turn(st, /*from_error*/ false).await;
        }
        "session.error" => {
            // Promote to a failed turn-end when one is in flight (the durable
            // `tool.failed`/`step.ended` trail already told the story; the
            // Notice below rides too so a non-turn error still surfaces).
            finalize_turn(st, /*from_error*/ true).await;
            let mut tr = st.translate.lock().await;
            for e in translate::translate(ev, &mut tr) {
                st.emit(e);
            }
        }
        "permission.v2.asked" => {
            upsert_permission(st, &ev.data).await;
        }
        "question.v2.asked" => {
            upsert_question(st, &ev.data).await;
        }
        _ => {
            let mut tr = st.translate.lock().await;
            for e in translate::translate(ev, &mut tr) {
                st.emit(e);
            }
        }
    }
}

async fn upsert_permission(st: &SessionState, d: &serde_json::Value) {
    let id = d
        .get("requestID")
        .and_then(|v| v.as_str())
        .or_else(|| d.get("id").and_then(|v| v.as_str()))
        .unwrap_or_default()
        .to_string();
    if id.is_empty() {
        return;
    }
    let is_new = !st.pending_perms.lock().await.contains_key(&id);
    if !is_new {
        return;
    }
    let tool_name = d.get("action").and_then(|v| v.as_str()).map(str::to_string);
    let input = d.get("resources").cloned().or_else(|| d.get("metadata").cloned());
    st.pending_perms.lock().await.insert(id.clone(), d.clone());
    st.emit(SessionEvent::Permission {
        request_id: id.clone(),
        kind: PermissionKind::Tool,
        metadata: None,
        tool_name,
        input,
    });
}

async fn upsert_question(st: &SessionState, d: &serde_json::Value) {
    let id = d
        .get("requestID")
        .and_then(|v| v.as_str())
        .or_else(|| d.get("id").and_then(|v| v.as_str()))
        .unwrap_or_default()
        .to_string();
    if id.is_empty() {
        return;
    }
    {
        let mut map = st.pending_asks.lock().await;
        if map.contains_key(&id) {
            return;
        }
        map.insert(id.clone(), d.clone());
    }
    // The UI's Ask card consumes {questions:[{question,header,options[…]}]};
    // opencode's request carries exactly that array (captured shape) — pass it
    // through under the same key (plus the tool info for display context).
    let payload = serde_json::json!({
        "questions": d.get("questions").cloned().unwrap_or_else(|| serde_json::json!([])),
    });
    st.emit(SessionEvent::Ask {
        request_id: id,
        questions: payload,
    });
}

/// Pending permission/question poller: authoritative ask/retract diff.
fn spawn_pending_poller(st: Arc<SessionState>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(PENDING_POLL).await;
            poll_pending_once(&st).await;
        }
    })
}

async fn poll_pending_once(st: &SessionState) {
    // ── permissions ──
    let live: HashMap<String, serde_json::Value> = st
        .client
        .pending_permissions(&st.backend_id)
        .await
        .into_iter()
        .filter_map(|v| {
            let id = v
                .get("id")
                .or_else(|| v.get("requestID"))
                .and_then(|i| i.as_str())
                .map(str::to_string)?;
            Some((id, v))
        })
        .collect();
    let mut map = st.pending_perms.lock().await;
    let gone: Vec<String> = map.keys().filter(|k| !live.contains_key(*k)).cloned().collect();
    for id in &gone {
        map.remove(id);
    }
    let fresh: Vec<(String, serde_json::Value)> = live
        .iter()
        .filter(|(id, _)| !map.contains_key(*id))
        .map(|(id, d)| (id.clone(), d.clone()))
        .collect();
    drop(map);
    for id in gone {
        st.emit(SessionEvent::PermissionResolved {
            request_id: id,
            kind: PermissionKind::Tool,
        });
    }
    for (_id, d) in fresh {
        upsert_permission(st, &d).await;
    }

    // ── questions ──
    let live_q: HashMap<String, serde_json::Value> = st
        .client
        .pending_questions(&st.backend_id)
        .await
        .into_iter()
        .filter_map(|v| {
            let id = v
                .get("id")
                .or_else(|| v.get("requestID"))
                .and_then(|i| i.as_str())
                .map(str::to_string)?;
            Some((id, v))
        })
        .collect();
    let mut map = st.pending_asks.lock().await;
    let gone_q: Vec<String> = map.keys().filter(|k| !live_q.contains_key(*k)).cloned().collect();
    for id in &gone_q {
        map.remove(id);
    }
    let fresh_q: Vec<(String, serde_json::Value)> = live_q
        .iter()
        .filter(|(id, _)| !map.contains_key(*id))
        .map(|(id, d)| (id.clone(), d.clone()))
        .collect();
    drop(map);
    for id in gone_q {
        st.emit(SessionEvent::AskResolved { request_id: id });
    }
    for (_id, d) in fresh_q {
        upsert_question(st, &d).await;
    }
}

/// Active-map turn closer: when the in-flight session leaves
/// `GET /api/session/active`, settle + finalize.
fn spawn_active_poller(st: Arc<SessionState>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(ACTIVE_POLL).await;
            if !st.turn_in_flight.load(Ordering::SeqCst) {
                continue;
            }
            match st.client.session_active(&st.backend_id).await {
                Ok(true) => {}
                Ok(false) => {
                    tokio::time::sleep(TURN_SETTLE_GRACE).await;
                    // Re-check: a steer may have re-activated the session
                    // inside the grace window.
                    if matches!(st.client.session_active(&st.backend_id).await, Ok(true)) {
                        continue;
                    }
                    finalize_turn(&st, false).await;
                }
                Err(_) => {
                    // Transport blip during the turn: the crash path (pool
                    // watchdog → Detached) or a later successful poll will
                    // settle this; do not finalize on unknown state.
                }
            }
        }
    })
}

/// Close the turn exactly once per dispatch (guarded by the
/// turn_in_flight flip).
async fn finalize_turn(st: &SessionState, from_error: bool) {
    if !st.turn_in_flight.swap(false, Ordering::SeqCst) {
        return; // already finalized (idle beat the poller, or Cancel settled)
    }
    let cancelled = st.cancel_requested.swap(false, Ordering::SeqCst);
    // Drain the translate accumulators under the lock AFTER trailing frames
    // were handled — the finalizer runs on pump-adjacent tasks that hold no
    // lock, and the grace above ordered them.
    let usage = {
        let tr = st.translate.lock().await;
        tr.turn_usage_event()
    };
    st.emit(usage);
    // Stamp the epoch onto the built terminal event.
    let mut terminal = translate::turn_result_event(from_error, cancelled);
    if let SessionEvent::TurnResult { epoch, .. } = &mut terminal {
        *epoch = st.epoch.load(Ordering::SeqCst);
    }
    st.emit(terminal);
}

fn spawn_death_watcher(st: Arc<SessionState>, mut alive: tokio::sync::watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if alive.changed().await.is_err() {
                return; // sender gone (pool entry reaped normally)
            }
            if !(*alive.borrow()) {
                st.emit(SessionEvent::Detached {
                    exit: None,
                    redacted_summary: Some("shared opencode server stopped".into()),
                });
                return;
            }
        }
    })
}

/// The per-session backend actor.
pub struct OpencodeSessionBackend {
    state: Arc<SessionState>,
    /// Real pump tasks + pool lease; aborted on Drop (same teardown contract
    /// as the codex backend's Drop-reap).
    tasks: std::sync::Mutex<Option<SessionTasks>>,
}

impl Drop for OpencodeSessionBackend {
    fn drop(&mut self) {
        if let Some(t) = self.tasks.lock().ok().and_then(|mut o| o.take()) {
            t.sse.abort();
            t.pending.abort();
            t.active.abort();
            t.alive.abort();
            // _lease drops with the tasks → pool refcount −1 (server stops at
            // last lease; foreign servers are never killed).
        }
    }
}

#[async_trait::async_trait]
impl SessionBackend for OpencodeSessionBackend {
    async fn dispatch(&self, command: Command) -> Result<CommandReceipt, BackendError> {
        let st = &self.state;
        match command {
            Command::Send { content, metadata } => {
                if st.turn_in_flight.load(Ordering::SeqCst) {
                    // Orchestrator normally guards this; steer through Send is
                    // explicitly a Command below.
                    return Err(BackendError::Transport(
                        "a turn is already in flight (use Steer)".into(),
                    ));
                }
                let mut text = String::new();
                for b in &content {
                    // Layer-2 rule (§C6): reject un-advertised blocks with the
                    // stable content_block:<kind> name, never a silent drop.
                    if let ContentBlock::Text(t) = b {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    } else {
                        return Err(BackendError::CommandNotSupported {
                            command: aionui_session::block_kind_name(b),
                        });
                    }
                }
                if text.trim().is_empty() {
                    return Err(BackendError::Transport("empty prompt".into()));
                }
                // Consume the one-shot injected context (preset rules + skill
                // index composed by the factory) ahead of the first prompt.
                let injected = st.pending_context.lock().await.take();
                let final_text = match injected {
                    Some(ctx) if !ctx.trim().is_empty() => format!("{}\n\n{}", ctx.trim_end(), text),
                    _ => text.clone(),
                };
                let admit: PromptAdmit = st
                    .client
                    .prompt(&st.backend_id, &final_text)
                    .await
                    .map_err(transport_err)?;
                let next_gen = st.epoch.fetch_add(1, Ordering::SeqCst) + 1;
                st.turn_in_flight.store(true, Ordering::SeqCst);
                st.cancel_requested.store(false, Ordering::SeqCst);
                st.translate.lock().await.begin_turn();
                st.emit(SessionEvent::TurnStarted { epoch: next_gen });
                if let Some(cid) = metadata.client_msg_id {
                    st.emit(SessionEvent::PromptAccepted { client_msg_id: cid });
                }
                tracing::debug!(backend = %st.backend_id, admitted_seq = ?admit.admitted_seq, id = %admit.id,
                    "opencode shared: prompt admitted");
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::Started,
                    turn_gen: next_gen,
                })
            }
            Command::Steer { content, client_msg_id } => {
                // opencode's delivery:"steer" admits durably and the scheduler
                // folds it into the running loop — same semantics, no gate.
                let mut text = String::new();
                for b in &content {
                    if let ContentBlock::Text(t) = b {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    }
                }
                if text.trim().is_empty() {
                    return Err(BackendError::Transport("empty steer".into()));
                }
                st.client.prompt(&st.backend_id, &text).await.map_err(transport_err)?;
                if let Some(cid) = client_msg_id {
                    st.emit(SessionEvent::PromptAccepted { client_msg_id: cid });
                }
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: st.epoch.load(Ordering::SeqCst),
                })
            }
            Command::Cancel { target } => {
                use aionui_session::CancelTarget as T;
                match target {
                    T::Tool(_) => Err(not_supported("cancel_tool")),
                    T::Turn | T::Session => {
                        st.client.interrupt(&st.backend_id).await.map_err(transport_err)?;
                        st.cancel_requested.store(true, Ordering::SeqCst);
                        // Finalize proactively: the active map will settle the
                        // same way; whoever flips first owns the single
                        // TurnResult{Cancelled}.
                        finalize_turn(&self.state, false).await;
                        Ok(CommandReceipt {
                            accepted: true,
                            admission: Admission::NoTurn,
                            turn_gen: st.epoch.load(Ordering::SeqCst),
                        })
                    }
                }
            }
            Command::AnswerPermission {
                request_id,
                decision,
                // opencode permission replies are the single reply string;
                // multi-choice selections and question-form answers belong to
                // the Ask channel, not this one.
                selected: _,
                answers: _,
            } => {
                let reply = match decision {
                    PermissionDecision::Approved => "once",
                    PermissionDecision::AllowAlways => "always",
                    PermissionDecision::Denied => "reject",
                };
                st.client
                    .reply_permission(&st.backend_id, &request_id, reply)
                    .await
                    .map_err(transport_err)?;
                let mut m = st.pending_perms.lock().await;
                m.remove(&request_id);
                drop(m);
                st.emit(SessionEvent::PermissionResolved {
                    request_id,
                    kind: PermissionKind::Tool,
                });
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: st.epoch.load(Ordering::SeqCst),
                })
            }
            Command::AnswerAsk { request_id, answers } => {
                match answers {
                    Some(qas) => {
                        // QuestionV2.Answer = one string-array per question, in
                        // question order (flat arrays 400 — captured). AionUi
                        // hands us QuestionAnswer{question, labels}; the labels
                        // are the answer, order preserved.
                        let mapped: Vec<Vec<String>> = qas.iter().map(|q: &QuestionAnswer| q.labels.clone()).collect();
                        st.client
                            .reply_question(&st.backend_id, &request_id, mapped)
                            .await
                            .map_err(transport_err)?;
                    }
                    None => {
                        st.client
                            .reject_question(&st.backend_id, &request_id)
                            .await
                            .map_err(transport_err)?;
                    }
                }
                let mut m = st.pending_asks.lock().await;
                m.remove(&request_id);
                drop(m);
                st.emit(SessionEvent::AskResolved { request_id });
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: st.epoch.load(Ordering::SeqCst),
                })
            }
            Command::SetModel { model } => {
                st.client
                    .set_model(&st.backend_id, &model)
                    .await
                    .map_err(transport_err)?;
                st.emit(SessionEvent::ConfigChanged {
                    mode: None,
                    model: Some(model),
                });
                Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: st.epoch.load(Ordering::SeqCst),
                })
            }
            Command::Acknowledge { .. } => Ok(CommandReceipt {
                accepted: true,
                admission: Admission::NoTurn,
                turn_gen: st.epoch.load(Ordering::SeqCst),
            }),
            Command::SetMode { .. } => Err(not_supported("set_mode")),
            Command::AnswerAuth { .. } => Err(not_supported("answer_auth")),
            Command::Rewind { .. } => Err(not_supported("rewind")),
            Command::ListCheckpoints => Err(not_supported("list_checkpoints")),
            Command::SetConfigOption { .. } => Err(not_supported("set_config_option")),
            Command::QuerySessionInfo { .. } => Err(not_supported("query_session_info")),
        }
    }

    fn events(&self) -> futures_util::stream::BoxStream<'static, SessionEnvelope> {
        // Codex precedent (codex_conn::events): the open-time BackendBound is
        // broadcast before the orchestrator subscribes and would be lost — the
        // conversation would never persist its `ses_` resume anchor (and a
        // crash-time redacted_summary would be lost). Replay the binding,
        // immutable since open, to EVERY new subscriber; the same-value
        // re-emit is idempotent downstream.
        let st = &self.state;
        let preface = vec![SessionEnvelope {
            session_id: st.logical_id.clone(),
            turn_gen: st.epoch.load(Ordering::SeqCst),
            event: SessionEvent::BackendBound {
                backend_session_id: Some(st.backend_id.clone()),
            },
        }];
        let rx = st.tx.subscribe();
        let live = futures_util::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(env) => return Some((env, rx)),
                    // Lagging drops events but never stalls the reader forever
                    // (the durable SSE cursor catches up on reconnect): keep
                    // the stream alive like codex does.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
        Box::pin(futures_util::stream::iter(preface).chain(live))
    }

    fn capabilities(&self) -> Capabilities {
        opencode_capabilities()
    }

    fn pending_permission_requests(&self) -> Vec<PendingPermissionView> {
        // REST recovery for a reloaded waiting_confirmation conversation — the
        // registry this reads survives server RESTARTS (the poller re-lists
        // pendings on each pass), which the transient SSE cards cannot do.
        self.state
            .pending_perms
            .blocking_lock()
            .iter()
            .map(|(id, d)| PendingPermissionView {
                request_id: id.clone(),
                tool_name: d.get("action").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                // opencode permissions are plain tool approvals (questions
                // ride the Ask channel), so no question-card rebuild.
                questions: None,
            })
            .collect()
    }

    async fn terminate(&self) {
        // A per-session terminate cannot kill the SHARED server — cancel the
        // running turn instead (opencode has no heavier per-session kill
        // surface in this build; lease release through Drop is the real
        // teardown, and the pool only stops the server at last lease).
        let st = &self.state;
        if st.turn_in_flight.load(Ordering::SeqCst) {
            let _ = st.client.interrupt(&st.backend_id).await;
            st.cancel_requested.store(true, Ordering::SeqCst);
            finalize_turn(st, false).await;
        }
    }
}

/// Capability advertisement drives UI affordances; every `false` here is
/// matched by a `CommandNotSupported` in dispatch (cap-behavior invariant,
/// same discipline as codex's comment block).
pub fn opencode_capabilities() -> Capabilities {
    Capabilities {
        // Native structured event stream (same grade as codex's JSON-RPC
        // notifications) — no regex/text parsing anywhere.
        tier: CapabilityTier::Hook,
        emits: SignalSet {
            // tool.progress frames ride as Heartbeat; the durable SSE stream
            // carries full tool lifecycle and an authoritative TurnResult
            // (active-map settle), so the orchestrator suppresses its
            // generated fallbacks.
            heartbeat: true,
            tool_lifecycle: true,
            terminal_result: true,
        },
        supported_commands: CommandSet {
            // POST /prompt with delivery:"steer" is the real mid-turn wire.
            steer: true,
            // opencode has no per-tool abort — only session interrupt.
            cancel_tool: false,
            answer_permission: true,
            // No ACP auth handshake; opencode sign-in is out of band.
            answer_auth: false,
            // Permission cards are answered via REST and self-resolve; the
            // node_ack path is accepted as a no-op (no node is ever emitted).
            acknowledge: true,
            // No mode selector on the v2 API (no plan/agent toggle endpoint).
            set_mode: false,
            // POST /api/session/{id}/model → 204 (live-verified).
            set_model: true,
            rewind: false,
            list_checkpoints: false,
            query_session_info: false,
        },
        // Text-only: the v2 prompt body is plain text (verified — images and
        // resources have no wire here; dispatch answers with the stable
        // content_block:<kind> refusal instead of silently dropping).
        prompt_blocks: BlockSet {
            text: true,
            image: false,
            audio: false,
            resource: false,
            at_mention: false,
        },
        // opencode's POST /prompt 200 {admittedSeq} is exactly the Native ack
        // the enum docs cite.
        prompt_accepted: PromptAcceptedSource::Native,
        // Mid-turn user input goes through Command::Steer, which genuinely
        // reaches the running turn over HTTP; the orchestrator owns the
        // idle-time queue (Send mid-turn is refused), so no proactive-input.
        accepts_proactive_input: false,
        supports_midturn_delivery: true,
        ..Default::default()
    }
}
