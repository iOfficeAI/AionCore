//! 007 §3 / C7: the `ConversationSession` — the conversation-side façade over
//! the seam. This is the SKELETON 007 ships (R14); the actual WIRING into
//! `aionui-conversation` (replacing `RuntimeCompletionPublisher` + the hardcoded
//! `turn.completed{canSendMessage}`) is the parallel conversation agent's P2
//! work. 007 freezes the contract here and proves the skeleton compiles + folds.
//!
//! ## What it is
//! A thin handle a conversation layer holds per logical session. It owns:
//!  - an `Arc<Orchestrator>` (the fold loop / broadcast fan-out),
//!  - an `Arc<dyn SessionBackend>` (the per-session transport handle; also the
//!    on-demand source of `Capabilities`),
//!  - a local `pending` queue (§9.11 — pending lives on the conversation side,
//!    the session never owns it; drained on `PromptAccepted{client_msg_id}`).
//!
//! ## What it is NOT (the frozen MUST-NOTs, §C7)
//! It NEVER calls `step()`, NEVER recomputes the unlock via `can_send_message`
//! (it reads `StateSnapshot.can_send`), NEVER mints `turn_gen`, NEVER blocks on a
//! dispatch return for turn completion, NEVER reaches into the adapter/transport.
//! It MAY ONLY: send Commands (threading `client_msg_id`), consume the demuxed
//! `SessionEnvelope` stream, drain pending on `PromptAccepted`, subscribe to the
//! read-only `StateSnapshot`/unlock streams, read `Capabilities` on demand (G4 —
//! pass-through to the backend so async discovery is reflected), and call
//! `reconnect()` on `Lagged`/transport drop.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::types::{
    Admission, BackendError, CancelTarget, CommandMeta, CommandReceipt, ContentBlock, PermissionDecision,
    SessionEnvelope, StateSnapshot,
};
use super::{Orchestrator, SessionBackend};
use crate::capability::Capabilities;
use futures_util::stream::BoxStream;

/// 009 R4 / §2: the lifecycle status of an outstanding message. A message is
/// ALWAYS enqueued `Held` first (the user may type any time, §Decision①); the flush
/// engine dispatches it (`Held`→`Sent`) on a `can_send||can_queue` rising edge;
/// the matching `PromptAccepted` confirms it (`Sent`→`Accepted`). `Canceled` is
/// the terminal for a Held message removed before dispatch (T7) or a session-wide
/// teardown (T7c); `Error` for a dispatch that failed (PC-ERROR-7).
///
/// ⚠️ The cancel BOUNDARY is "has it been dispatched", NOT "was PromptAccepted
/// seen": only `Held` (never dispatched) may be locally dropped; `Sent`/`Accepted`
/// are in flight at the backend and must go through Cancel{Turn} (T8), never a
/// local delete (that would leave a ghost turn — PC-RACE-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgStatus {
    /// Enqueued, not yet dispatched. The ONLY status a local cancel may drop.
    Held,
    /// Dispatched to the backend (bytes written), awaiting `PromptAccepted`.
    Sent,
    /// Backend confirmed via `PromptAccepted{client_msg_id}`.
    Accepted,
    /// Dispatch returned an error (transport dead / FSM Error) — never confirmed.
    Error,
    /// Removed before dispatch (T7) or cleared by session teardown (T7c).
    Canceled,
}

/// A message the conversation has accepted from the user (§9.11 / Addendum 3).
/// Lives ONLY on the conversation side — the session core never owns pending.
/// Carries a lifecycle `status` (009 R4/§2) and a monotonic `enqueue_ordinal` so
/// the flush engine can pick the head deterministically (HashMap/Vec order is not
/// relied on for FIFO).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingMessage {
    /// Correlation key echoed by the adapter on `PromptAccepted`. The façade
    /// mints it and threads it through `CommandMeta.client_msg_id`.
    pub client_msg_id: String,
    /// The user content awaiting backend confirmation.
    pub content: Vec<ContentBlock>,
    /// Lifecycle status (009 R4/§2). New entries start `Held`.
    pub status: MsgStatus,
    /// Monotonic enqueue order — the flush engine dispatches the lowest-ordinal
    /// `Held` entry first, so FIFO does not depend on container iteration order.
    pub enqueue_ordinal: u64,
}

/// The conversation-side façade (§3). Holds the orchestrator + backend + the
/// local pending queue. `Capabilities` are read on demand from the backend (G4),
/// not cached, so async discovery (codex model/list, ACP authMethods) is seen.
pub struct ConversationSession {
    /// Stable logical session id (§4.1) — the demux key for every stream.
    session_id: String,
    /// The fold loop / broadcast fan-out. Shared (the run loop is spawned
    /// elsewhere; the façade only sends + subscribes).
    orchestrator: Arc<Orchestrator>,
    /// The per-session transport handle. `dispatch` is `&self`-concurrent;
    /// `capabilities()` is a cheap read-only snapshot (sync, no await).
    backend: Arc<dyn SessionBackend>,
    /// Local pending queue (§9.11). Monotonic client_msg_id counter + the queue.
    pending: Mutex<Pending>,
    /// Per-INSTANCE prefix for minted `client_msg_id`s (= the wire-frame `uuid`
    /// claude stores in its resume message-tree). MUST differ across instances of
    /// the same conversation: the counter resets to 0 on every fresh instance, so a
    /// purely ordinal id (`m-1`, `m-2`, …) re-collides with the uuids the PRIOR run
    /// already persisted into the resumed `.jsonl`. claude dedups an incoming prompt
    /// whose `uuid` already exists in the tree → it runs NO turn (the "reopen an old
    /// conversation, send a message, nothing happens" stall). A per-instance random
    /// prefix makes every resume mint a fresh uuid namespace, so no collision.
    msg_id_prefix: String,
    /// 009 R4b: flush-engine reentrancy guard. The head-of-queue Held message is
    /// dispatched by EITHER the send-time try-flush OR a background rising-edge
    /// flush; this serializes them so the same head is never dispatched twice
    /// (PC-FLUSH-RACE-ESC-14). Held across the await of a single dispatch.
    flush_lock: Mutex<()>,
}

#[derive(Default)]
struct Pending {
    next_id: u64,
    queue: Vec<PendingMessage>,
}

impl ConversationSession {
    /// Open a façade over an orchestrated backend. Capabilities are read on demand
    /// from the backend (§5.5 G4 — NOT frozen at open), so async discovery (codex
    /// model/list, ACP initialize authMethods) that lands AFTER open is reflected.
    pub fn new(
        session_id: impl Into<String>,
        orchestrator: Arc<Orchestrator>,
        backend: Arc<dyn SessionBackend>,
    ) -> Self {
        // A fresh random prefix per instance. A resumed conversation gets a NEW
        // ConversationSession (the counter restarts at 0); without a per-instance
        // namespace the minted `m-1`, `m-2`… would re-collide with the uuids the
        // prior run persisted into claude's resume message-tree, and claude would
        // dedup our new prompt as already-seen → no turn (see `msg_id_prefix`).
        let prefix = format!("m{}", uuid::Uuid::new_v4().simple());
        Self::with_msg_id_prefix(session_id, orchestrator, backend, prefix)
    }

    /// Like [`new`], but with an explicit `client_msg_id` prefix. Tests pin a stable
    /// prefix (`"m"`) so the minted ids are the readable `m-1`, `m-2`, … they assert
    /// on; production uses [`new`], which mints a random per-instance prefix to keep
    /// the wire `uuid` namespace distinct across resume.
    pub fn with_msg_id_prefix(
        session_id: impl Into<String>,
        orchestrator: Arc<Orchestrator>,
        backend: Arc<dyn SessionBackend>,
        msg_id_prefix: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            orchestrator,
            backend,
            pending: Mutex::new(Pending::default()),
            msg_id_prefix: msg_id_prefix.into(),
            flush_lock: Mutex::new(()),
        }
    }

    /// The stable logical session id (the demux key).
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    // ======================================================================
    // DOWNWARD — Commands (conversation → session)
    // ======================================================================

    /// Send user content (§3). MUST push a `PendingMessage{client_msg_id}` to the
    /// LOCAL pending queue first, then dispatch with that id; the queue drains on
    /// the matching `PromptAccepted{client_msg_id}` (see `subscribe_events`).
    /// NEVER blocks on turn completion — the turn flows up `subscribe_events`.
    pub async fn send(&self, content: Vec<ContentBlock>) -> Result<CommandReceipt, BackendError> {
        // §2: enqueue Held FIRST (the user may type any time, regardless of
        // can_send/can_queue). Mint the correlation id + a monotonic ordinal.
        {
            let mut pending = self.pending.lock().await;
            pending.next_id += 1;
            let ordinal = pending.next_id;
            let id = format!("{}-{ordinal}", self.msg_id_prefix);
            pending.queue.push(PendingMessage {
                client_msg_id: id,
                content,
                status: MsgStatus::Held,
                enqueue_ordinal: ordinal,
            });
        }
        // Then try to flush the head NOW: if the current snapshot already permits
        // (can_send || can_queue, or pre-first-transition initial Idle), the head
        // dispatches immediately and we return its real receipt. If a turn is in
        // flight / blocked on a permission (probeD: claude ignores stdin in RA),
        // the message stays Held and the BACKGROUND flush engine dispatches it on
        // the next can_send/can_queue rising edge (T7b: "Esc → Held auto-flushes").
        // Either way no message is ever lost — it lives in the permanent queue.
        match self.try_flush_head().await {
            Some(receipt) => receipt,
            // Stayed Held (gate closed). Report Queued — the conv queue is the
            // authority (§2); the receipt is just an ack, not a dispatch result.
            None => Ok(CommandReceipt {
                accepted: true,
                admission: Admission::Queued,
                turn_gen: self
                    .orchestrator
                    .latest_snapshot(&self.session_id)
                    .map(|s| s.turn_gen)
                    .unwrap_or(0),
            }),
        }
    }

    /// 009 R4b: the flush engine's core step. Dispatch the lowest-ordinal `Held`
    /// message IF the current snapshot permits (`can_send || can_queue`; a missing
    /// snapshot = pre-first-transition initial Idle ⇒ can_send). One head at a
    /// time, behind `flush_lock` so a send-time flush and a background rising-edge
    /// flush never dispatch the same head twice (PC-FLUSH-RACE-ESC-14). Returns
    /// the dispatched message's receipt, or `None` if nothing was dispatched (no
    /// Held head, or the gate is closed → message stays Held for a later edge).
    async fn try_flush_head(&self) -> Option<Result<CommandReceipt, BackendError>> {
        // The send-time path: an outstanding un-accepted dispatch this window closes the
        // gate (bug-hunt #8, see `try_flush_head_inner`).
        self.try_flush_head_inner(false).await
    }

    /// Core flush step. `from_rising_edge` distinguishes the two callers:
    ///   - `send()` (false): in the PRE-FOLD window `latest_snapshot()` is None →
    ///     gate_open=true even though a prior send this window already dispatched a turn
    ///     (TurnStarted lowered async, not yet folded to Running). A second send would
    ///     then double-dispatch into the in-flight turn (bug-hunt #8: two TurnStarted,
    ///     turn_gen bumped twice, two prompts to the CLI). So the send-time path ALSO
    ///     holds if any message is already `Sent` (dispatched, awaiting PromptAccepted):
    ///     that Sent IS the turn just opened — wait for it.
    ///   - `run_flush_engine` (true): only fires on a real snapshot FALSE→TRUE rising
    ///     edge, which means the prior turn actually folded (Running→terminal) — so
    ///     dispatching the next head is the documented one-head-per-edge contract, and
    ///     the Sent-outstanding guard must NOT apply (a prior Sent awaiting its
    ///     PromptAccepted is normal here and the next edge legitimately advances).
    async fn try_flush_head_inner(&self, from_rising_edge: bool) -> Option<Result<CommandReceipt, BackendError>> {
        let _flush = self.flush_lock.lock().await;
        // Gate: read the pre-derived snapshot — NEVER recompute (§C7). No snapshot
        // yet ⇒ initial Idle ⇒ can_send true.
        let gate_open = match self.orchestrator.latest_snapshot(&self.session_id) {
            Some(s) => s.can_send || s.can_queue,
            None => true,
        };
        if !gate_open {
            return None;
        }
        // Pick the head = lowest-ordinal Held entry; clone what dispatch needs.
        let (client_msg_id, content) = {
            let pending = self.pending.lock().await;
            // Bug-hunt #8: send-time, with a turn already dispatched-but-unconfirmed in
            // this pre-fold window, hold — do not open a second concurrent turn.
            if !from_rising_edge && pending.queue.iter().any(|m| m.status == MsgStatus::Sent) {
                return None;
            }
            let head = pending
                .queue
                .iter()
                .filter(|m| m.status == MsgStatus::Held)
                .min_by_key(|m| m.enqueue_ordinal)?;
            (head.client_msg_id.clone(), head.content.clone())
        };
        let meta = CommandMeta {
            client_msg_id: Some(client_msg_id.clone()),
            ..Default::default()
        };
        // Dispatch. Success ⇒ Sent (awaiting PromptAccepted); failure ⇒ Error so
        // the UI bubble rolls back instead of hanging Held forever (PC-ERROR-7).
        let result = self
            .orchestrator
            .send(self.backend.as_ref(), &self.session_id, content, meta)
            .await;
        self.mark_status(
            &client_msg_id,
            if result.is_ok() {
                MsgStatus::Sent
            } else {
                MsgStatus::Error
            },
        )
        .await;
        Some(result)
    }

    /// Test-only: drive the rising-edge flush path (what `run_flush_engine` invokes on
    /// a real snapshot FALSE→TRUE edge). Tests that SIMULATE an edge (seed an open
    /// snapshot then flush) must use this, not the public send-time `try_flush_head`
    /// (whose #8 Sent-outstanding guard correctly blocks a second same-window dispatch).
    #[cfg(test)]
    async fn try_flush_on_edge_for_test(&self) -> Option<Result<CommandReceipt, BackendError>> {
        self.try_flush_head_inner(true).await
    }

    /// 009 R4b: run the background flush engine until the snapshot stream ends.
    /// Subscribes to `StateSnapshot` and, on a `can_send || can_queue` FALSE→TRUE
    /// RISING EDGE (a turn ended / a permission resolved), flushes the head Held
    /// message. This is what makes T7b work: a message typed during a turn stays
    /// Held, then auto-dispatches as the next turn the moment the turn finishes —
    /// no user action needed. Rising-edge (not level) + the flush_lock together
    /// prevent re-dispatching on every snapshot or racing the send-time flush.
    /// Spawn this once per session (the conversation owns the task handle).
    pub async fn run_flush_engine(self: Arc<Self>) {
        use futures_util::StreamExt as _;
        let mut snaps = self.orchestrator.subscribe_state(self.session_id.clone());
        let mut prev_open = false;
        while let Some(s) = snaps.next().await {
            let open = s.can_send || s.can_queue;
            if open && !prev_open {
                // Rising edge → dispatch EXACTLY ONE head (the lowest-ordinal Held).
                // NOT a drain-all loop: dispatching one opens a turn (state → Running),
                // and the next Held waits for the NEXT rising edge. Draining the whole
                // queue on a single edge would overrun the backend's FIFO window /
                // reorder (§12.6.10 "dispatch only one queue head at a time", PC-MS-13). The remaining
                // Held heads flush on subsequent edges as each turn completes. This is a
                // real snapshot edge (prior turn folded), so pass from_rising_edge=true:
                // the #8 Sent-outstanding guard does not apply here.
                let _ = self.try_flush_head_inner(true).await;
            }
            prev_open = open;
        }
    }

    /// Set the status of an outstanding message by id (no-op if absent). The flush
    /// engine / send path / drain path all funnel status changes through here.
    async fn mark_status(&self, client_msg_id: &str, status: MsgStatus) {
        let mut pending = self.pending.lock().await;
        if let Some(m) = pending.queue.iter_mut().find(|m| m.client_msg_id == client_msg_id) {
            m.status = status;
        }
    }

    /// T7 (009 R4): cancel a single OUTSTANDING message that has NOT been
    /// dispatched yet (`Held`). This is a pure conversation-local removal — the
    /// backend never received the bytes, so NO wire traffic (no Cancel{Turn})
    /// must be issued. Returns true if a Held entry was removed.
    ///
    /// ⚠️ A `Sent`/`Accepted` message has already been dispatched (bytes are in
    /// the backend's stdin / a turn is running); it MUST be cancelled via
    /// `cancel(CancelTarget::Turn)` (T8), never locally dropped — a local delete
    /// would leave the backend running a turn with no bubble (ghost turn,
    /// PC-RACE-5). This method refuses to touch a non-Held entry.
    pub async fn cancel_held(&self, client_msg_id: &str) -> bool {
        let mut pending = self.pending.lock().await;
        if let Some(pos) = pending
            .queue
            .iter()
            .position(|m| m.client_msg_id == client_msg_id && m.status == MsgStatus::Held)
        {
            pending.queue.remove(pos);
            true
        } else {
            false
        }
    }

    /// T7c (009 R4): session-level teardown of the conv queue. Marks EVERY
    /// non-terminal outstanding message (`Held`/`Sent`) `Canceled` so the flush
    /// engine will never dispatch them again and the UI rolls every pending bubble
    /// back. Returns how many were canceled. This is the conv-queue half of
    /// `close_session` (the delete hook also kills the backend + clears the
    /// roster); it does NOT issue per-turn wire interrupts — process death (kill)
    /// is covered by `Detached`. Idempotent: re-running finds nothing non-terminal.
    ///
    /// ⚠️ This is the SESSION-CLOSE semantic, NOT `Cancel{Turn}` (T7b): Cancel{Turn}
    /// (Esc) leaves Held in place to auto-flush; only a session teardown clears the
    /// whole queue.
    pub async fn cancel_all_outstanding(&self) -> usize {
        let mut pending = self.pending.lock().await;
        let mut n = 0;
        for m in pending.queue.iter_mut() {
            if matches!(m.status, MsgStatus::Held | MsgStatus::Sent) {
                m.status = MsgStatus::Canceled;
                n += 1;
            }
        }
        n
    }

    /// Cancel (§3). `Turn`/`Session` accepted by all backends; `Tool` only where
    /// `caps.supported_commands.cancel_tool` — else the dispatch returns
    /// `CommandNotSupported` (surfaced to the user, never silently dropped).
    pub async fn cancel(&self, target: CancelTarget) -> Result<CommandReceipt, BackendError> {
        self.orchestrator
            .cancel(self.backend.as_ref(), &self.session_id, target)
            .await
    }

    /// Answer a permission the backend raised (§3). `request_id` MUST be the
    /// upward `Permission{request_id}` (the control-correlation key — NOT a
    /// tool_use_id, §9 U10). `selected` is the chosen option label/id for a single
    /// pick-one prompt (claude single-question AskUserQuestion / ACP optionId); pass
    /// `None` for a plain allow/deny. `answers` carries the FULL claude
    /// `AskUserQuestion` set (multi-question / multi-select, task #83); pass an empty
    /// vec for non-claude backends or the single-question degrade.
    pub async fn answer_permission(
        &self,
        request_id: String,
        decision: PermissionDecision,
        selected: Option<String>,
        answers: Vec<super::types::QuestionAnswer>,
    ) -> Result<CommandReceipt, BackendError> {
        self.backend
            .dispatch(super::types::Command::AnswerPermission {
                request_id,
                decision,
                selected,
                answers,
            })
            .await
    }

    /// Gap #8: switch the backend's mode (§3 / Addendum 7). Forwards
    /// `Command::SetMode` to the backend, which confirms it non-optimistically via
    /// an upward `ConfigChanged{mode}` (config is orthogonal — the FSM/can_send is
    /// untouched). Gated up-front by `can_set_mode()`; an unsupported backend
    /// rejects with `BackendError::CommandNotSupported` (never silently dropped).
    pub async fn set_mode(&self, mode: String) -> Result<CommandReceipt, BackendError> {
        self.backend.dispatch(super::types::Command::SetMode { mode }).await
    }

    /// Gap #8: switch the backend's model (§3 / Addendum 7). Forwards
    /// `Command::SetModel`; confirmed via upward `ConfigChanged{model}`. Gated
    /// up-front by `can_set_model()`; unsupported → `CommandNotSupported`.
    pub async fn set_model(&self, model: String) -> Result<CommandReceipt, BackendError> {
        self.backend.dispatch(super::types::Command::SetModel { model }).await
    }

    /// #99: set a generic backend config option (e.g. effort → claude
    /// `apply_flag_settings{effortLevel}`). Forwards `Command::SetConfigOption`;
    /// an unsupported option / backend rejects with `CommandNotSupported` (never
    /// silently dropped). The backend confirms via its own channel (claude effort is
    /// read back via get_settings; there is no mode/model-style `ConfigChanged`).
    pub async fn set_config_option(&self, option_id: String, value: String) -> Result<CommandReceipt, BackendError> {
        self.backend
            .dispatch(super::types::Command::SetConfigOption { option_id, value })
            .await
    }

    /// Bug-hunt #2: read-only cumulative session-info query (context-usage / cost).
    /// Forwards `Command::QuerySessionInfo`; the answer flows back via the
    /// SessionEvent::SessionInfo stream (not this receipt). `kind` is the
    /// conversation-side string ("context_usage" | "session_cost"); an unknown kind
    /// defaults to ContextUsage. Backends that don't advertise it reject (surfaced up).
    pub async fn query_session_info(&self, kind: &str) -> Result<CommandReceipt, BackendError> {
        use super::types::SessionInfoKind;
        let kind = match kind {
            "session_cost" | "cost" => SessionInfoKind::SessionCost,
            _ => SessionInfoKind::ContextUsage,
        };
        self.backend
            .dispatch(super::types::Command::QuerySessionInfo { kind })
            .await
    }

    // ======================================================================
    // UPWARD — subscriptions (session → conversation), all demuxed by session_id
    // ======================================================================

    /// Raw `SessionEnvelope` stream, demuxed to this session (transcript /
    /// streaming UI — every delta, Tier-0 push-not-store). The conversation MUST
    /// inspect `PromptAccepted{client_msg_id}` on this stream to drain pending —
    /// `drain_pending_on` does that, kept separate so the stream stays a pure
    /// projection.
    pub fn subscribe_events(&self) -> BoxStream<'static, SessionEnvelope> {
        self.orchestrator.subscribe_events(self.session_id.clone())
    }

    /// Full `StateSnapshot` stream, demuxed to this session (§9.12 — every change
    /// is a FULL snapshot, never incremental).
    pub fn subscribe_state(&self) -> BoxStream<'static, StateSnapshot> {
        self.orchestrator.subscribe_state(self.session_id.clone())
    }

    /// The unlock stream: `StateSnapshot.can_send` (PRE-DERIVED by the
    /// orchestrator, §3 line 728). The conversation reads this bool — it MUST NOT
    /// recompute `can_send_message(state)`.
    pub fn subscribe_unlock(&self) -> BoxStream<'static, bool> {
        self.orchestrator.subscribe_unlock(self.session_id.clone())
    }

    /// The current `Capabilities` (G4: read on demand from the backend, NOT a
    /// frozen open-time snapshot). `backend.capabilities()` is a cheap sync
    /// read-only merge, so a model/list or initialize-authMethods response that
    /// lands after open is reflected on the next read (the empty-switcher gap fix).
    pub fn capabilities(&self) -> Capabilities {
        self.backend.capabilities()
    }

    /// Read-only passthrough of the backend's currently-open (unanswered) permission
    /// requests, for REST recovery (`GET /confirmations`) of a reloaded
    /// `waiting_confirmation` conversation. Pure sync read, no await/spawn — same
    /// discipline as `capabilities`/`live_snapshot`. Backends without a pending
    /// registry return empty (the trait default).
    pub fn pending_permission_requests(&self) -> Vec<super::types::PendingPermissionView> {
        self.backend.pending_permission_requests()
    }

    /// This session's LATEST cached `StateSnapshot` (the same lag-recovering cache
    /// `subscribe_state` re-emits on `Lagged`), or `None` before the first
    /// transition. A synchronous read for callers that must correlate an action to
    /// the live turn — e.g. a turn-targeted cancel that no-ops unless the live
    /// `turn_gen` matches and `can_cancel` is set (Route B s9d). Reads the same
    /// `orchestrator.latest_snapshot` the internal flush gate consults.
    pub fn live_snapshot(&self) -> Option<StateSnapshot> {
        self.orchestrator.latest_snapshot(&self.session_id)
    }

    /// This session's STICKY last-terminal `(turn_gen, TransitionReason)` — the
    /// lag-recovering terminal oracle (the orchestrator's `latest_terminal` cache).
    /// `None` before any turn settled. A stall-intolerant consumer that may have
    /// missed the live terminal (a `Lagged` drop on the domain ring) reads this to
    /// recover the most-recently-settled turn's outcome; the `turn_gen` confirms
    /// WHICH turn settled.
    pub fn latest_terminal(&self) -> Option<(u64, super::types::TransitionReason)> {
        self.orchestrator.latest_terminal(&self.session_id)
    }

    /// Re-deliver current truth after a transport drop / `Lagged` (§9.3 / C7).
    /// This is an ORCHESTRATION SIGNAL, NOT a `Command` — it never reaches a
    /// backend or the FSM. The conversation calls it on a `Lagged` or WS reconnect;
    /// the orchestrator re-broadcasts this session's cached StateSnapshot (so
    /// `subscribe_state`/`subscribe_unlock` immediately re-see the current
    /// can_send + full FSM) and emits a `Snapshot` envelope on the event stream.
    /// P0 = re-emit cached truth; Tier-1 transcript backfill + live-tail-resume are
    /// the deferred P2 slice.
    pub async fn reconnect(&self) {
        self.orchestrator.reconnect(&self.session_id).await;
    }

    // ======================================================================
    // Pending-queue management (§9.11 / Addendum 3)
    // ======================================================================

    /// Drain pending on a `PromptAccepted{client_msg_id}` envelope. The
    /// conversation calls this for each event it observes on `subscribe_events`;
    /// it removes EXACTLY the matching pending entry (others stay). Returns the
    /// drained message if one matched. MUST be driven by `PromptAccepted`, NOT the
    /// optimistic `TurnStarted` (which would drain a not-yet-confirmed message).
    pub async fn drain_pending_on(&self, env: &SessionEnvelope) -> Option<PendingMessage> {
        let crate::event::SessionEvent::PromptAccepted { client_msg_id } = &env.event else {
            return None;
        };
        // §2: the queue is a PERMANENT record (Held→Sent→Accepted), so a
        // PromptAccepted marks the matching entry `Accepted` (optimistic bubble
        // confirmed) — it does NOT remove it. Returns the now-Accepted message if
        // one matched (drives the bubble's sending→sent transition). Precise
        // single-id match: a stale/unknown id drains nothing (PC-MS-9 / never the
        // optimistic TurnStarted, which would confirm a not-yet-accepted message).
        let mut pending = self.pending.lock().await;
        let m = pending.queue.iter_mut().find(|m| &m.client_msg_id == client_msg_id)?;
        m.status = MsgStatus::Accepted;
        Some(m.clone())
    }

    /// A snapshot of the current pending queue (for UI rendering — the queue is
    /// conversation-owned state).
    pub async fn pending(&self) -> Vec<PendingMessage> {
        self.pending.lock().await.queue.clone()
    }

    // ======================================================================
    // Layer-1 UP-FRONT capability gating (§5.2) — proactive affordance hiding
    // ======================================================================

    pub fn can_steer(&self) -> bool {
        self.backend.capabilities().supported_commands.steer
    }
    pub fn accepts_images(&self) -> bool {
        self.backend.capabilities().prompt_blocks.image
    }
    /// Whether the backend accepts file attachments (`ResourceLink`): claude (Read
    /// tool path-ref) + ACP (native resource_link) + codex (`UserInput::Mention`
    /// @file by path) = true; aionrs = false. The UI gates the file-picker on this
    /// (additive parity with accepts_images).
    pub fn accepts_files(&self) -> bool {
        self.backend.capabilities().prompt_blocks.resource
    }
    pub fn can_rewind(&self) -> bool {
        self.backend.capabilities().supported_commands.rewind
    }
    pub fn can_set_mode(&self) -> bool {
        self.backend.capabilities().supported_commands.set_mode
    }
    pub fn can_set_model(&self) -> bool {
        self.backend.capabilities().supported_commands.set_model
    }
    pub fn available_auth_methods(&self) -> Vec<String> {
        self.backend.capabilities().auth_methods
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::CodexSessionBackend;
    use crate::event::SessionEvent;
    use crate::testing::FakeAgentIo;

    /// Build a ConversationSession over a real (fake-io) codex backend + a fresh
    /// orchestrator. The backend binds threadId from a thread/started prefix.
    async fn facade(session_id: &str) -> (ConversationSession, Arc<Orchestrator>, Arc<dyn SessionBackend>) {
        let prefix = format!(
            "{}\n",
            r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-x"}}}"#
        )
        .into_bytes();
        let fake = FakeAgentIo::new(prefix, None);
        let backend: Arc<dyn SessionBackend> =
            Arc::new(CodexSessionBackend::build_with_io(session_id, Box::new(fake)).await);
        let orch = Arc::new(Orchestrator::new(256));
        // Pin the prefix to "m" so minted ids are the readable m-1, m-2, … asserted below.
        let convo = ConversationSession::with_msg_id_prefix(session_id, orch.clone(), backend.clone(), "m");
        (convo, orch, backend)
    }

    #[tokio::test]
    async fn caps_read_once_drive_up_front_gating() {
        let (convo, _orch, _backend) = facade("s1").await;
        // codex caps (the matrix): steer/set_mode/set_model true, image true.
        assert!(convo.can_steer(), "codex supports steer");
        // G3: rewind = true — the seam now wires thread/rollback (down) + Rewound
        // (up), so the conversation may surface a rewind/fork affordance.
        assert!(convo.can_rewind(), "codex rewind is wired (G3)");
        assert!(convo.can_set_mode());
        assert!(convo.can_set_model());
        assert!(convo.accepts_images());
        assert!(
            convo
                .available_auth_methods()
                .contains(&"chatgptAuthTokens".to_string()),
            "codex advertises mid-session auth methods"
        );
    }

    /// ENUMERATION INVARIANT (capability forwarding). The 7 `can_*`/`accepts_*` getters
    /// are thin forwards onto `backend.capabilities()`. `caps_read_once...` asserts
    /// codex's HARDCODED values — but that wouldn't catch a getter wired to the WRONG
    /// field (e.g. `can_set_mode()` reading `.set_model`). This pins every getter ==
    /// its exact source field, across TWO backends (claude + codex) so the forwarding
    /// is proven structurally, not against one backend's happens-to-be values.
    /// (Limit: claude & codex both have set_mode==set_model==true, so a getter wired to
    /// the wrong-but-equal field is data-invisible here — steer/rewind/image DO differ
    /// claude-vs-codex, so the forwarding itself is proven non-vacuous via those.)
    #[tokio::test]
    async fn capability_getters_mirror_backend_capabilities_exactly() {
        for session_id in ["fwd-codex", "fwd-claude"] {
            let backend: Arc<dyn SessionBackend> = if session_id == "fwd-codex" {
                let prefix = format!(
                    "{}\n",
                    r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-y"}}}"#
                )
                .into_bytes();
                Arc::new(CodexSessionBackend::build_with_io(session_id, Box::new(FakeAgentIo::new(prefix, None))).await)
            } else {
                Arc::new(
                    crate::backend::ClaudeSessionBackend::build_with_io(
                        session_id,
                        Box::new(FakeAgentIo::never_exits(Vec::new())),
                    )
                    .await,
                )
            };
            let convo = ConversationSession::new(session_id, Arc::new(Orchestrator::new(64)), backend.clone());
            let caps = backend.capabilities();

            assert_eq!(
                convo.can_steer(),
                caps.supported_commands.steer,
                "[{session_id}] can_steer"
            );
            assert_eq!(
                convo.can_rewind(),
                caps.supported_commands.rewind,
                "[{session_id}] can_rewind"
            );
            assert_eq!(
                convo.can_set_mode(),
                caps.supported_commands.set_mode,
                "[{session_id}] can_set_mode"
            );
            assert_eq!(
                convo.can_set_model(),
                caps.supported_commands.set_model,
                "[{session_id}] can_set_model"
            );
            assert_eq!(
                convo.accepts_images(),
                caps.prompt_blocks.image,
                "[{session_id}] accepts_images"
            );
            assert_eq!(
                convo.accepts_files(),
                caps.prompt_blocks.resource,
                "[{session_id}] accepts_files"
            );
            assert_eq!(
                convo.available_auth_methods(),
                caps.auth_methods,
                "[{session_id}] available_auth_methods"
            );
        }
    }

    /// 🖥️ R9 (caps-freeze vs async discovery race) — G4 FIX. The façade no longer
    /// freezes caps at open; `capabilities()` passes through to
    /// `backend.capabilities()` on demand. codex fills `available_models`
    /// ASYNCHRONOUSLY from the model/list response (fire-and-forget at
    /// open_session). Before G4, a response landing after `new()` was invisible to
    /// the façade (the empty-model-switcher gap). Now the façade reflects it on the
    /// next read: open with NO model → drive discovery → the façade DOES see it.
    #[tokio::test]
    async fn r9_conversation_session_caps_reflect_late_discovery() {
        // codex backend with a model/list response queued (id=50) but not yet read.
        let model_resp = r#"{"jsonrpc":"2.0","id":50,"result":{"models":[{"id":"gpt-5.5","displayName":"GPT-5.5"}]}}"#;
        let fake = FakeAgentIo::never_exits(format!("{model_resp}\n").into_bytes());
        let backend_concrete = CodexSessionBackend::build_with_io("s-r9", Box::new(fake)).await;
        // Register the pending discovery id the reader will claim (open_session does
        // this after the handshake; build_with_io skips it).
        backend_concrete.register_model_discovery_for_test(50).await;
        let backend: Arc<dyn SessionBackend> = Arc::new(backend_concrete);
        let orch = Arc::new(Orchestrator::new(256));

        // Open the façade BEFORE the reader processes the model/list response → at
        // this instant the (on-demand) caps still have an empty model list.
        let convo = ConversationSession::new("s-r9", orch, backend.clone());
        assert!(
            convo.capabilities().available_models.is_empty(),
            "before discovery the on-demand caps still have no model"
        );

        // Drive discovery: subscribing to events() runs the reader, which claims the
        // model/list response and fills `discovered`. Poll the FAÇADE caps directly.
        let _ev = backend.events();
        let mut facade_has_model = false;
        for _ in 0..40 {
            if !convo.capabilities().available_models.is_empty() {
                facade_has_model = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        // THE G4 FIX: the façade's on-demand caps now reflect the late discovery —
        // a model/list response landing AFTER new() IS visible to the UI switcher.
        assert!(
            facade_has_model,
            "R9/G4: ConversationSession::capabilities() reads through to the backend on \
             demand, so a model/list response that lands AFTER new() IS reflected \
             (the empty-model-switcher gap is fixed)"
        );
    }

    /// Regression: the "reopen an old conversation, send a message, nothing happens"
    /// stall. The minted `client_msg_id` IS the wire-frame `uuid` claude stores in its
    /// resume message-tree. A resumed conversation gets a FRESH ConversationSession
    /// (the ordinal counter restarts at 0), so a purely ordinal id (`m-1`, `m-2`, …)
    /// re-collides with the uuids the PRIOR run already persisted — claude dedups the
    /// incoming prompt as already-seen and runs no turn. Two instances of the SAME
    /// conversation must therefore mint DISJOINT id namespaces. Live-proven: stamping a
    /// colliding uuid stalls `--resume` 5/5; a unique one produces a turn.
    #[tokio::test]
    async fn resumed_instance_mints_disjoint_client_msg_ids() {
        // Build a fake-io backend for a logical conversation (helper mirrors `facade`).
        async fn backend(session_id: &str) -> Arc<dyn SessionBackend> {
            let prefix = format!(
                "{}\n",
                r#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"th-x"}}}"#
            )
            .into_bytes();
            let fake = FakeAgentIo::new(prefix, None);
            Arc::new(CodexSessionBackend::build_with_io(session_id, Box::new(fake)).await)
        }
        // Two independent instances of the SAME logical conversation (= a restart /
        // resume), each built via the production `new` (random per-instance prefix).
        let first = ConversationSession::new("conv-x", Arc::new(Orchestrator::new(64)), backend("conv-x").await);
        let second = ConversationSession::new("conv-x", Arc::new(Orchestrator::new(64)), backend("conv-x").await);

        first.send(vec![ContentBlock::Text("a".into())]).await.ok();
        second.send(vec![ContentBlock::Text("a".into())]).await.ok();

        let id1 = first.pending().await[0].client_msg_id.clone();
        let id2 = second.pending().await[0].client_msg_id.clone();
        assert_ne!(
            id1, id2,
            "two instances of the same conversation must mint DISJOINT client_msg_ids \
             (else a resume re-emits a uuid already in claude's tree → dedup → no turn)"
        );
    }

    #[tokio::test]
    async fn send_pushes_pending_with_threaded_client_msg_id() {
        let (convo, _orch, _backend) = facade("s1").await;
        let receipt = convo
            .send(vec![ContentBlock::Text("hi".into())])
            .await
            .expect("send accepted");
        assert!(receipt.accepted);
        // The local pending queue holds exactly the one message, with a minted id.
        let pending = convo.pending().await;
        assert_eq!(pending.len(), 1, "one message queued");
        assert_eq!(pending[0].client_msg_id, "m-1", "client_msg_id minted + threaded");
        assert_eq!(pending[0].content, vec![ContentBlock::Text("hi".into())]);
    }

    #[tokio::test]
    async fn prompt_accepted_marks_exactly_the_matching_pending_accepted() {
        // 009 R4/§2: the queue is a PERMANENT record. A PromptAccepted{m-1} marks
        // EXACTLY m-1 Accepted; m-2 keeps its prior status. Both entries stay in the
        // queue (precise single-id match, never an optimistic multi-drain).
        // NOTE (#8): in this None-snapshot window only the FIRST send dispatches (→Sent);
        // the second stays Held (the pre-fold double-dispatch guard) — it auto-flushes on
        // the next rising edge. m-1 dispatched, so PromptAccepted{m-1} is the case here.
        let (convo, _orch, _backend) = facade("s1").await;
        convo.send(vec![ContentBlock::Text("first".into())]).await.expect("ok");
        convo.send(vec![ContentBlock::Text("second".into())]).await.expect("ok");
        assert_eq!(convo.pending().await.len(), 2);

        let env = SessionEnvelope {
            session_id: "s1".into(),
            turn_gen: 1,
            event: SessionEvent::PromptAccepted {
                client_msg_id: "m-1".into(),
            },
        };
        let drained = convo.drain_pending_on(&env).await;
        assert_eq!(drained.map(|m| m.client_msg_id), Some("m-1".to_string()));
        let pending = convo.pending().await;
        assert_eq!(pending.len(), 2, "permanent record: both entries remain");
        let m1 = pending.iter().find(|m| m.client_msg_id == "m-1").unwrap();
        let m2 = pending.iter().find(|m| m.client_msg_id == "m-2").unwrap();
        assert_eq!(m1.status, MsgStatus::Accepted, "exactly the matched entry is Accepted");
        assert_eq!(
            m2.status,
            MsgStatus::Held,
            "the other entry keeps its prior status — Held, NOT dispatched (#8: only one \
             head dispatches per pre-fold window; m-2 auto-flushes on the next edge)"
        );
    }

    /// Bug-hunt #2: ConversationSession::query_session_info forwards
    /// Command::QuerySessionInfo to the backend — making the (previously unreachable)
    /// claude get_context_usage / SessionInfo reply pipeline triggerable. Asserts the
    /// real wire frame (claude writes control_request{get_context_usage}).
    #[tokio::test]
    async fn query_session_info_forwards_to_backend_wire() {
        use crate::testing::FakeAgentIo;
        use crate::{ClaudeSessionBackend, Orchestrator};
        let fake = FakeAgentIo::never_exits(Vec::new());
        let captured = fake.captured_stdin();
        let backend: Arc<dyn SessionBackend> =
            Arc::new(ClaudeSessionBackend::build_with_io("qsi", Box::new(fake)).await);
        let convo = ConversationSession::new("qsi", Arc::new(Orchestrator::new(64)), backend);

        let receipt = convo
            .query_session_info("context_usage")
            .await
            .expect("claude advertises it");
        assert!(
            receipt.accepted && receipt.admission == Admission::NoTurn,
            "read-only query, NoTurn"
        );

        let written = {
            let mut s = String::new();
            for _ in 0..40 {
                s = String::from_utf8_lossy(&captured.lock().await.clone()).to_string();
                if s.contains("get_context_usage") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            s
        };
        assert!(
            written.contains(r#""subtype":"get_context_usage""#),
            "#2: query_session_info writes the claude get_context_usage control_request, got: {written}"
        );
    }

    /// Bug-hunt #8: two sends in the SAME pre-fold window (latest_snapshot==None, gate
    /// open) must NOT both dispatch — the first opens a turn (TurnStarted lowered async,
    /// not yet folded to Running), the second would double-fire into the in-flight turn
    /// (two TurnStarted, turn_gen bumped twice, two prompts to the CLI). The send-time
    /// flush holds the second as Held when a prior Sent is outstanding; it auto-flushes
    /// on the next rising edge. (The rising-edge path is exempt — see the FIFO test.)
    #[tokio::test]
    async fn two_sends_in_prefold_window_dispatch_only_one_head() {
        let (convo, _orch, _backend) = facade("s1").await; // NO snapshot seeded = None window
        convo.send(vec![ContentBlock::Text("a".into())]).await.expect("ok");
        convo.send(vec![ContentBlock::Text("b".into())]).await.expect("ok");
        let q = convo.pending().await;
        let by_id = |id: &str| q.iter().find(|m| m.client_msg_id == id).unwrap().status;
        assert_eq!(by_id("m-1"), MsgStatus::Sent, "first send opens the turn (dispatched)");
        assert_eq!(
            by_id("m-2"),
            MsgStatus::Held,
            "#8: the second send in the pre-fold window must NOT double-dispatch — stays Held"
        );
        // A later rising edge (prior turn folded) dispatches m-2 — no message lost.
        convo.orchestrator.seed_latest_for_test(snap("s1", true, false));
        let r = convo.try_flush_on_edge_for_test().await;
        assert!(matches!(r, Some(Ok(_))), "m-2 auto-flushes on the next rising edge");
        assert_eq!(
            convo
                .pending()
                .await
                .iter()
                .find(|m| m.client_msg_id == "m-2")
                .unwrap()
                .status,
            MsgStatus::Sent
        );
    }

    #[tokio::test]
    async fn send_marks_held_then_sent() {
        // 009 R4/§2: a successful send enqueues Held then (dispatch ok) marks Sent.
        let (convo, _orch, _backend) = facade("s1").await;
        convo.send(vec![ContentBlock::Text("hi".into())]).await.expect("ok");
        let pending = convo.pending().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, MsgStatus::Sent, "dispatched ok → Sent");
        assert_eq!(pending[0].enqueue_ordinal, 1, "monotonic ordinal minted");
    }

    #[tokio::test]
    async fn cancel_held_drops_only_undispatched_and_never_a_sent_entry() {
        // 009 R4 / T7: cancel_held removes a Held entry (conv-local, no wire) but
        // REFUSES a Sent/Accepted entry (already dispatched → must go through
        // Cancel{Turn}/T8, never a local delete that would leave a ghost turn).
        let (convo, _orch, _backend) = facade("s1").await;
        // m-1 dispatched (Sent); inject a second entry that stays Held by mutating
        // status directly (simulates "enqueued, flush engine has not dispatched yet").
        convo.send(vec![ContentBlock::Text("a".into())]).await.expect("ok");
        convo.send(vec![ContentBlock::Text("b".into())]).await.expect("ok");
        convo.mark_status("m-2", MsgStatus::Held).await; // force m-2 back to Held (un-dispatched)

        // T7: cancel the Held m-2 → removed, no wire.
        assert!(convo.cancel_held("m-2").await, "Held entry removed");
        let pending = convo.pending().await;
        assert!(!pending.iter().any(|m| m.client_msg_id == "m-2"), "m-2 gone");

        // T8 guard: cancel_held must REFUSE the Sent m-1 (it's dispatched).
        assert!(
            !convo.cancel_held("m-1").await,
            "Sent entry must NOT be locally dropped"
        );
        assert!(
            convo.pending().await.iter().any(|m| m.client_msg_id == "m-1"),
            "m-1 (Sent) stays — cancel it via Cancel{{Turn}} (T8), not a local delete"
        );
    }

    #[tokio::test]
    async fn cancel_all_outstanding_clears_queue_for_session_teardown() {
        // 009 R4 / T7c: session teardown marks every non-terminal entry Canceled
        // (NOT a per-turn Esc — that's T7b which leaves Held to auto-flush). After
        // it, the flush engine finds no Held head to dispatch, and the count
        // reflects exactly the non-terminal entries. Idempotent.
        let (convo, _orch, _backend) = facade("s1").await;
        convo.send(vec![ContentBlock::Text("a".into())]).await.expect("ok"); // → Sent
        convo.send(vec![ContentBlock::Text("b".into())]).await.expect("ok");
        convo.mark_status("m-2", MsgStatus::Held).await; // m-2 un-dispatched
        let n = convo.cancel_all_outstanding().await;
        assert_eq!(n, 2, "both the Sent and the Held entry are canceled");
        let pending = convo.pending().await;
        assert!(
            pending.iter().all(|m| m.status == MsgStatus::Canceled),
            "every entry is Canceled after teardown, got {pending:?}"
        );
        // try_flush_head finds no Held head → nothing dispatches.
        assert!(
            convo.try_flush_head().await.is_none(),
            "no Held head to flush after teardown"
        );
        assert_eq!(
            convo.cancel_all_outstanding().await,
            0,
            "idempotent: nothing non-terminal left"
        );
    }

    /// Build a StateSnapshot with chosen gate values (009 R4b flush tests).
    fn snap(sid: &str, can_send: bool, can_queue: bool) -> StateSnapshot {
        StateSnapshot {
            session_id: sid.into(),
            state: if can_send {
                crate::state::SessionState::Idle
            } else {
                crate::state::SessionState::Running {
                    since_epoch: 1,
                    saw_substantive_output: false,
                    terminal_result_seen: false,
                    requires_action: crate::state::RequiresActionSet {
                        waiting_on_approval: if can_queue { 0 } else { 1 },
                        waiting_on_auth: 0,
                    },
                    subagents: Vec::new(),
                }
            },
            can_send,
            has_activity: !can_send,
            can_queue,
            can_cancel: !can_send,
            turn_gen: 1,
            last_reason: None,
        }
    }

    #[tokio::test]
    async fn send_while_gate_closed_stays_held_no_dispatch() {
        // 009 R4b / probeD: when can_send=false AND can_queue=false (a turn blocked
        // on a permission — RA), a send must NOT dispatch (claude ignores stdin in
        // RA → a blind write is swallowed). The message stays Held for a later
        // rising edge; the receipt reports Queued (the conv queue is the authority).
        let (convo, orch, _backend) = facade("s1").await;
        orch.seed_latest_for_test(snap("s1", false, false)); // gate closed
        let receipt = convo.send(vec![ContentBlock::Text("held".into())]).await.expect("ok");
        assert_eq!(
            receipt.admission,
            Admission::Queued,
            "gate closed → Queued, not dispatched"
        );
        let pending = convo.pending().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, MsgStatus::Held, "stayed Held — never dispatched");
    }

    #[tokio::test]
    async fn flush_engine_dispatches_held_head_on_rising_edge() {
        // 009 R4b / T7b: a message typed while gate closed stays Held; when the
        // turn ends (can_send false→true RISING EDGE), the background flush engine
        // dispatches the head Held → Sent, no user action. This is "Esc → Held
        // auto-flushes as next turn".
        let (convo, orch, _backend) = facade("s1").await;
        orch.seed_latest_for_test(snap("s1", false, false)); // gate closed
        let convo = Arc::new(convo);
        convo.send(vec![ContentBlock::Text("queued".into())]).await.expect("ok");
        assert_eq!(convo.pending().await[0].status, MsgStatus::Held, "starts Held");

        // Spawn the flush engine, then broadcast a rising edge (Idle, can_send=true).
        let engine = {
            let convo = convo.clone();
            tokio::spawn(async move { convo.run_flush_engine().await })
        };
        // Seed cache so try_flush_head's gate read sees open, then push the edge.
        orch.seed_latest_for_test(snap("s1", true, false));
        let _ = orch.state_tx_for_test().send(snap("s1", true, false));

        // Poll until the head transitions Held → Sent (the engine dispatched it).
        let mut dispatched = false;
        for _ in 0..40 {
            if convo.pending().await.iter().any(|m| m.status == MsgStatus::Sent) {
                dispatched = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        engine.abort();
        assert!(dispatched, "flush engine dispatched the Held head on the rising edge");
    }

    /// proactive-queue (audit): the flush gate is `can_send || can_queue`
    /// (try_flush_head, §C7). The can_queue disjunct — a turn IS in flight
    /// (can_send=false) but the backend accepts proactive input (can_queue=true) —
    /// was executed in production but NEVER asserted (every existing snap() is
    /// (false,false) or (true,false), never (false,true)). This pins the
    /// can_queue-only arm: a Held head dispatches when ONLY can_queue is open.
    #[tokio::test]
    async fn flush_dispatches_head_when_only_can_queue_open() {
        let (convo, orch, _backend) = facade("s1").await;
        orch.seed_latest_for_test(snap("s1", false, false)); // gate fully closed
        convo.send(vec![ContentBlock::Text("queued".into())]).await.expect("ok");
        assert_eq!(
            convo.pending().await[0].status,
            MsgStatus::Held,
            "stays Held while the gate is fully closed"
        );
        // Open the gate via can_queue ONLY (can_send stays false: a turn is in flight,
        // no requires_action). The snapshot is opaque gate INPUT — using a codex-backed
        // facade is fine; we are testing the gate logic, not the backend's real caps.
        orch.seed_latest_for_test(snap("s1", false, true));
        let r = convo.try_flush_head().await;
        assert!(
            matches!(r, Some(Ok(_))),
            "a can_queue-open gate dispatches the Held head, got {r:?}"
        );
        assert_eq!(
            convo.pending().await[0].status,
            MsgStatus::Sent,
            "Held → Sent on a can_queue-only flush (proactive in-flight send)"
        );
    }

    /// proactive-queue rising-edge: run_flush_engine's edge is `can_send || can_queue`
    /// too — a false→true edge driven PURELY by can_queue flipping true must
    /// auto-flush the Held head (mirror of flush_engine_dispatches_held_head_on_rising_edge
    /// but with the (false,true) snapshot the existing test never exercises).
    #[tokio::test]
    async fn flush_engine_fires_on_can_queue_rising_edge() {
        let (convo, orch, _backend) = facade("s1").await;
        orch.seed_latest_for_test(snap("s1", false, false));
        let convo = Arc::new(convo);
        convo.send(vec![ContentBlock::Text("q".into())]).await.expect("ok");

        let engine = {
            let convo = convo.clone();
            tokio::spawn(async move { convo.run_flush_engine().await })
        };
        // Seed cache so the engine's gate read sees can_queue open, then push the edge.
        orch.seed_latest_for_test(snap("s1", false, true));
        let _ = orch.state_tx_for_test().send(snap("s1", false, true));

        let mut dispatched = false;
        for _ in 0..40 {
            if convo.pending().await.iter().any(|m| m.status == MsgStatus::Sent) {
                dispatched = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        engine.abort();
        assert!(
            dispatched,
            "a rising edge driven purely by can_queue (proactive in-flight) auto-flushes the Held head"
        );
    }

    /// race-audit conn-13 (PC-FLUSH-RACE-ESC-14): the flush engine is reentrant —
    /// a send-time `try_flush_head` and the background rising-edge
    /// `run_flush_engine` (which also calls `try_flush_head`) can fire against the
    /// SAME lowest-ordinal Held head at the same time. Both production entrypoints
    /// funnel through `try_flush_head`, so contending two such calls on one Held
    /// head IS the reentrancy test. The `flush_lock` + the in-lock Held→Sent flip
    /// must make this dispatch EXACTLY ONCE: one call wins (Some(dispatched)), the
    /// other finds no Held head (None). Without the lock, both read `Held` before
    /// either marks `Sent` and double-dispatch (two turns opened, FIFO overrun).
    #[tokio::test]
    async fn concurrent_flush_dispatches_the_held_head_exactly_once() {
        let (convo, orch, _backend) = facade("s1").await;
        // Enqueue ONE Held head while the gate is closed (so send() does not flush it).
        orch.seed_latest_for_test(snap("s1", false, false));
        let convo = Arc::new(convo);
        convo.send(vec![ContentBlock::Text("race".into())]).await.expect("ok");
        assert_eq!(convo.pending().await[0].status, MsgStatus::Held, "starts Held");
        assert_eq!(convo.pending().await.len(), 1, "exactly one queued entry");

        // Open the gate, then fire TWO flushers concurrently at the same Held head
        // (the send-time path and the rising-edge path both reach try_flush_head).
        orch.seed_latest_for_test(snap("s1", true, false));
        let (a, b) = {
            let c1 = convo.clone();
            let c2 = convo.clone();
            tokio::join!(async move { c1.try_flush_head().await }, async move {
                c2.try_flush_head().await
            },)
        };

        // EXACTLY ONE dispatched (Some), the other found no Held head (None).
        let dispatched = [&a, &b].iter().filter(|r| r.is_some()).count();
        let no_head = [&a, &b].iter().filter(|r| r.is_none()).count();
        assert_eq!(
            (dispatched, no_head),
            (1, 1),
            "flush_lock must serialize: one dispatch wins, the other sees the head already Sent → None (got a={a:?}, b={b:?})"
        );
        // The winner's dispatch succeeded and the single entry is Sent (once).
        assert!(a.or(b).expect("one Some").is_ok(), "the winning dispatch succeeded");
        let pending = convo.pending().await;
        assert_eq!(pending.len(), 1, "still exactly one entry (no duplicate enqueued)");
        assert_eq!(
            pending.iter().filter(|m| m.status == MsgStatus::Sent).count(),
            1,
            "the Held head is dispatched exactly once (Sent), never double-dispatched"
        );
    }

    #[tokio::test]
    async fn prompt_accepted_for_unknown_id_drains_nothing() {
        let (convo, _orch, _backend) = facade("s1").await;
        convo.send(vec![ContentBlock::Text("x".into())]).await.expect("ok");
        let env = SessionEnvelope {
            session_id: "s1".into(),
            turn_gen: 1,
            event: SessionEvent::PromptAccepted {
                client_msg_id: "m-999".into(),
            },
        };
        assert!(
            convo.drain_pending_on(&env).await.is_none(),
            "no match → nothing drained"
        );
        assert_eq!(convo.pending().await.len(), 1, "pending unchanged");
    }

    #[tokio::test]
    async fn non_prompt_accepted_event_never_drains() {
        let (convo, _orch, _backend) = facade("s1").await;
        convo.send(vec![ContentBlock::Text("x".into())]).await.expect("ok");
        // A delta (not PromptAccepted) must NOT drain pending (the optimistic-drain
        // anti-pattern, §C7 MUST-NOT).
        let env = SessionEnvelope {
            session_id: "s1".into(),
            turn_gen: 1,
            event: SessionEvent::MessageDelta {
                item_id: "m1".into(),
                text: "hello".into(),
            },
        };
        assert!(convo.drain_pending_on(&env).await.is_none());
        assert_eq!(convo.pending().await.len(), 1, "delta does not drain pending");
    }

    #[tokio::test]
    async fn subscribe_state_demuxes_to_this_session() {
        // A façade for s1 must only see s1 snapshots (cross-session isolation).
        let (convo, orch, _backend) = facade("s1").await;
        let mut states = convo.subscribe_state();
        // Push a snapshot for ANOTHER session directly; s1's stream must not see it.
        let _ = orch.state_tx_for_test().send(StateSnapshot {
            session_id: "s2".into(),
            state: crate::state::SessionState::Idle,
            can_send: true,
            has_activity: false,
            can_queue: false,
            can_cancel: false,
            turn_gen: 0,
            last_reason: None,
        });
        let _ = orch.state_tx_for_test().send(StateSnapshot {
            session_id: "s1".into(),
            state: crate::state::SessionState::Idle,
            can_send: true,
            has_activity: false,
            can_queue: false,
            can_cancel: false,
            turn_gen: 0,
            last_reason: None,
        });
        let first = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            futures_util::StreamExt::next(&mut states),
        )
        .await
        .expect("not hang")
        .expect("a snapshot");
        assert_eq!(first.session_id, "s1", "only this session's snapshots are delivered");
    }

    #[tokio::test]
    async fn live_snapshot_reflects_cached_turn_gen_and_can_cancel() {
        // `live_snapshot` is the synchronous read a turn-targeted cancel correlates
        // against (Route B s9d): it returns the SAME cached snapshot the orchestrator
        // serves, carrying turn_gen + can_cancel. None before any transition.
        let (convo, orch, _backend) = facade("s1").await;
        assert!(convo.live_snapshot().is_none(), "no snapshot before any transition");

        // Seed a Running/cancellable snapshot for THIS session (as a real fold would).
        orch.seed_latest_for_test(StateSnapshot {
            session_id: "s1".into(),
            state: crate::state::SessionState::Running {
                since_epoch: 7,
                saw_substantive_output: false,
                terminal_result_seen: false,
                requires_action: crate::state::RequiresActionSet::default(),
                subagents: Vec::new(),
            },
            can_send: false,
            has_activity: true,
            can_queue: false,
            can_cancel: true,
            turn_gen: 7,
            last_reason: None,
        });
        let snap = convo.live_snapshot().expect("snapshot after seed");
        assert_eq!(snap.turn_gen, 7, "live turn_gen is readable for cancel correlation");
        assert!(snap.can_cancel, "a Running turn is cancellable");

        // A snapshot for ANOTHER session must not leak into this one's read.
        orch.seed_latest_for_test(StateSnapshot {
            session_id: "s2".into(),
            state: crate::state::SessionState::Idle,
            can_send: true,
            has_activity: false,
            can_queue: false,
            can_cancel: false,
            turn_gen: 99,
            last_reason: None,
        });
        assert_eq!(
            convo.live_snapshot().map(|s| s.turn_gen),
            Some(7),
            "demuxed to this session"
        );
    }

    #[tokio::test]
    async fn gap_f_reconnect_reemits_cached_snapshot_and_snapshot_event() {
        // GAP-F: reconnect() re-delivers current truth — the cached StateSnapshot on
        // the state stream (so a re-subscriber re-sees can_send) AND a Snapshot
        // envelope on the event stream (the first producer of that variant).
        use crate::event::SessionEvent;
        let (convo, orch, _backend) = facade("s1").await;
        // Seed the cache as a real fold would (Running, locked).
        orch.seed_latest_for_test(StateSnapshot {
            session_id: "s1".into(),
            state: crate::state::SessionState::Running {
                since_epoch: 4,
                saw_substantive_output: true,
                terminal_result_seen: false,
                requires_action: crate::state::RequiresActionSet::default(),
                subagents: Vec::new(),
            },
            can_send: false,
            has_activity: true,
            // Running (no RA) → cancellable; can_queue not asserted by this seed
            // test (its focus is the locked can_send + cache recovery).
            can_queue: false,
            can_cancel: true,
            turn_gen: 4,
            last_reason: None,
        });
        // Subscribe AFTER the cache is seeded but the streams are otherwise idle.
        let mut states = convo.subscribe_state();
        let mut events = convo.subscribe_events();
        convo.reconnect().await;

        let snap = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            futures_util::StreamExt::next(&mut states),
        )
        .await
        .expect("not hang")
        .expect("a re-emitted snapshot");
        assert_eq!(snap.session_id, "s1");
        assert!(!snap.can_send, "reconnect re-emits the cached (Running, locked) truth");

        let env = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            futures_util::StreamExt::next(&mut events),
        )
        .await
        .expect("not hang")
        .expect("a Snapshot envelope");
        assert!(
            matches!(env.event, SessionEvent::Snapshot { turn_gen: 4, .. }),
            "reconnect emits a Snapshot event (the first producer), got {:?}",
            env.event
        );
    }

    // ======================================================================
    // Combinatorial-timing coverage (2026-06-17 audit gaps #1/#4/#5). The
    // existing flush tests only ever hold ONE Held entry; these pin the
    // MULTI-pending FIFO order, the dispatch-failure rollback arm, and the
    // T7b-vs-T7c cancel semantics over a populated queue.
    // ======================================================================

    /// Gap #1 (audit): MULTIPLE pending stacked → FIFO drain by `enqueue_ordinal`,
    /// lowest-first, ONE head per rising edge. Every prior flush test enqueues a
    /// single Held; the `enqueue_ordinal` ORDERING across 3 entries (and the
    /// "exactly one head per edge, not drain-all" rule, §12.6.10) was untested.
    #[tokio::test]
    async fn multiple_pending_drain_fifo_one_head_per_rising_edge() {
        let (convo, orch, _backend) = facade("s1").await;
        // Gate closed → all three sends stay Held (no send-time flush).
        orch.seed_latest_for_test(snap("s1", false, false));
        let convo = Arc::new(convo);
        convo.send(vec![ContentBlock::Text("a".into())]).await.expect("ok");
        convo.send(vec![ContentBlock::Text("b".into())]).await.expect("ok");
        convo.send(vec![ContentBlock::Text("c".into())]).await.expect("ok");
        let q = convo.pending().await;
        assert_eq!(q.len(), 3, "three Held entries stacked");
        assert!(q.iter().all(|m| m.status == MsgStatus::Held), "all Held, got {q:?}");
        assert_eq!(
            q.iter().map(|m| m.enqueue_ordinal).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "monotonic ordinals minted in send order"
        );

        // A rising edge dispatches EXACTLY ONE head = the lowest-ordinal Held (m-1),
        // NOT a drain-all. We open the gate, fire one try_flush_head, and assert only
        // m-1 went Sent while m-2/m-3 stay Held.
        orch.seed_latest_for_test(snap("s1", true, false));
        let r1 = convo.try_flush_on_edge_for_test().await;
        assert!(matches!(r1, Some(Ok(_))), "first edge dispatches a head, got {r1:?}");
        let q = convo.pending().await;
        let by_id = |id: &str| q.iter().find(|m| m.client_msg_id == id).unwrap().status;
        assert_eq!(by_id("m-1"), MsgStatus::Sent, "lowest ordinal dispatched first");
        assert_eq!(by_id("m-2"), MsgStatus::Held, "m-2 waits for the next edge");
        assert_eq!(by_id("m-3"), MsgStatus::Held, "m-3 waits for the next edge");

        // Next edge → m-2 (the new lowest-ordinal Held), still leaving m-3 Held.
        let r2 = convo.try_flush_on_edge_for_test().await;
        assert!(matches!(r2, Some(Ok(_))), "second edge dispatches the next head");
        let q = convo.pending().await;
        let by_id = |id: &str| q.iter().find(|m| m.client_msg_id == id).unwrap().status;
        assert_eq!(by_id("m-2"), MsgStatus::Sent, "m-2 dispatched second (FIFO)");
        assert_eq!(by_id("m-3"), MsgStatus::Held, "m-3 still waiting");

        // Third edge → m-3, queue fully drained to Sent.
        let r3 = convo.try_flush_on_edge_for_test().await;
        assert!(matches!(r3, Some(Ok(_))), "third edge dispatches the last head");
        assert!(
            convo.pending().await.iter().all(|m| m.status == MsgStatus::Sent),
            "all three dispatched in FIFO ordinal order"
        );
        // Nothing left to flush.
        assert!(
            convo.try_flush_head().await.is_none(),
            "no Held head remains after the queue is drained"
        );
    }

    /// A `SessionBackend` whose `dispatch(Send)` ALWAYS fails — used to drive the
    /// PC-ERROR-7 rollback arm (`try_flush_head` marks the head `Error`). Other
    /// commands succeed as NoTurn; `events()` is empty (we only test the send path).
    struct FailingSendBackend;
    #[async_trait::async_trait]
    impl SessionBackend for FailingSendBackend {
        async fn dispatch(&self, c: crate::backend::types::Command) -> Result<CommandReceipt, BackendError> {
            match c {
                crate::backend::types::Command::Send { .. } => Err(BackendError::Transport("stdin dead".into())),
                _ => Ok(CommandReceipt {
                    accepted: true,
                    admission: Admission::NoTurn,
                    turn_gen: 0,
                }),
            }
        }
        fn events(&self) -> BoxStream<'static, SessionEnvelope> {
            use futures_util::StreamExt as _;
            futures_util::stream::empty().boxed()
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
    }

    /// Gap #4 (audit): a send whose backend dispatch FAILS marks the pending entry
    /// `Error` (not stuck `Held` forever) and surfaces the Err to the caller, so the
    /// UI bubble rolls back. `MsgStatus::Error` is the production failure arm in
    /// `try_flush_head` (conversation_session.rs:220) and had ZERO test coverage.
    #[tokio::test]
    async fn send_dispatch_failure_marks_pending_error_and_returns_err() {
        let backend: Arc<dyn SessionBackend> = Arc::new(FailingSendBackend);
        let orch = Arc::new(Orchestrator::new(256));
        // No seeded snapshot → initial gate is open (pre-first-transition Idle), so
        // send() flushes the head immediately and we observe the failure arm now.
        let convo = ConversationSession::new("s-fail", orch, backend);

        let r = convo.send(vec![ContentBlock::Text("doomed".into())]).await;
        assert!(
            r.is_err(),
            "a failed dispatch must surface the Err to the caller (bubble rollback), got {r:?}"
        );

        // The pending entry is marked Error — NOT left Held (which would hang the
        // bubble) and NOT Sent (it never reached the backend).
        let q = convo.pending().await;
        assert_eq!(q.len(), 1, "the entry stays in the permanent record");
        assert_eq!(
            q[0].status,
            MsgStatus::Error,
            "a dispatch failure marks the entry Error (PC-ERROR-7 rollback arm), got {:?}",
            q[0].status
        );

        // And it is terminal for the flush engine: no Held head, nothing re-dispatches.
        assert!(
            convo.try_flush_head().await.is_none(),
            "an Error entry is NOT a Held head — the flush engine never retries it"
        );
    }

    /// Gap #5 (audit): T7b (Esc = `Cancel{Turn}`) vs T7c (`cancel_all_outstanding`
    /// = session teardown) have OPPOSITE semantics for a Held queue, but no test
    /// exercised them over a populated queue. T7b leaves Held in place (it
    /// auto-flushes on the next rising edge); ONLY T7c clears the whole queue.
    #[tokio::test]
    async fn esc_cancel_turn_leaves_held_but_teardown_clears_it() {
        let (convo, orch, _backend) = facade("s1").await;
        // A turn is running (m-1 Sent) and the user typed m-2 while blocked → Held.
        orch.seed_latest_for_test(snap("s1", true, false));
        convo
            .send(vec![ContentBlock::Text("running".into())])
            .await
            .expect("ok"); // → Sent
        orch.seed_latest_for_test(snap("s1", false, false)); // gate now closed (turn in flight)
        convo
            .send(vec![ContentBlock::Text("typed-while-busy".into())])
            .await
            .expect("ok"); // → Held
        let q = convo.pending().await;
        assert_eq!(
            q.iter().find(|m| m.client_msg_id == "m-1").unwrap().status,
            MsgStatus::Sent
        );
        assert_eq!(
            q.iter().find(|m| m.client_msg_id == "m-2").unwrap().status,
            MsgStatus::Held
        );

        // T7b: Esc cancels the TURN (a wire Cancel{Turn}). It must NOT touch the
        // local queue — m-2 stays Held to auto-flush as the next turn.
        convo.cancel(CancelTarget::Turn).await.expect("Cancel{Turn} dispatched");
        let q = convo.pending().await;
        assert_eq!(
            q.iter().find(|m| m.client_msg_id == "m-2").unwrap().status,
            MsgStatus::Held,
            "T7b (Esc) leaves the Held message in place to auto-flush — it must NOT be dropped"
        );
        // Proof of "auto-flush": once the gate reopens (turn ended), the Held head
        // dispatches with no user action — a real post-cancel rising edge.
        orch.seed_latest_for_test(snap("s1", true, false));
        let r = convo.try_flush_on_edge_for_test().await;
        assert!(
            matches!(r, Some(Ok(_))),
            "Held m-2 auto-flushes on the post-cancel rising edge"
        );
        assert_eq!(
            convo
                .pending()
                .await
                .iter()
                .find(|m| m.client_msg_id == "m-2")
                .unwrap()
                .status,
            MsgStatus::Sent,
            "T7b: the survived Held message becomes the next turn"
        );

        // T7c: session teardown is the OPPOSITE — it clears the whole queue. Re-arm a
        // fresh Held entry, then tear down: every non-terminal entry → Canceled.
        orch.seed_latest_for_test(snap("s1", false, false));
        convo.send(vec![ContentBlock::Text("after".into())]).await.expect("ok"); // → Held (m-3)
        let n = convo.cancel_all_outstanding().await;
        assert!(n >= 1, "teardown cancels the outstanding (Held/Sent) entries, got {n}");
        assert!(
            convo
                .pending()
                .await
                .iter()
                .all(|m| matches!(m.status, MsgStatus::Canceled | MsgStatus::Accepted)),
            "T7c teardown leaves nothing Held/Sent — the whole queue is cleared"
        );
        assert!(
            convo.try_flush_head().await.is_none(),
            "after teardown there is no Held head to auto-flush (opposite of T7b)"
        );
    }
}
