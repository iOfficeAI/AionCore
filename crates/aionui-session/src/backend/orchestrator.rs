//! 007 §3 / §9.12: the orchestrator fold loop — the ONE place `step()` is
//! called (preserving I9). It drives a `SessionBackend`'s `events()` stream:
//! routes each `SessionEnvelope` by `session_id` to a per-session FSM, folds it
//! through the monomorphic reducer, and broadcasts BOTH the raw envelope (to
//! UI/transcript/persistence) AND — on every state change — a FULL
//! `StateSnapshot` (Addendum 8: full push, not incremental; a late/reconnecting
//! subscriber gets complete truth in its first message).
//!
//! Two backend-agnostic details handled here (NOT in the reducer):
//!  - epoch restamp (§5.4): the adapter emits `TurnResult{epoch:0}` (it has no
//!    turn context); the orchestrator stamps the live `env.turn_gen` before
//!    `step()` so the reducer's cross-turn stale-result guard works on a
//!    persistent process.
//!  - I14 subagent prune: terminal subagents are pruned from `Running.subagents`
//!    at a turn boundary (orchestrator layer — the reducer only upserts).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, broadcast, mpsc};

use super::SessionBackend;
use super::types::{SessionEnvelope, StateSnapshot, TransitionReason};
use crate::event::{SessionEvent, SubagentStatus};
use crate::reducer::{Transition, step};
use crate::state::{SessionState, can_send_message, has_foreground_activity};
use futures_util::stream::{BoxStream, StreamExt};

/// Broadcast fan-out for one orchestrated backend. Conversation subscribers
/// resubscribe to either stream; everything is demuxed by `session_id`.
#[derive(Clone)]
pub struct Orchestrator {
    /// Raw envelopes (transcript / streaming UI / persistence).
    event_tx: broadcast::Sender<SessionEnvelope>,
    /// Full state snapshots (the unlock signal lives on `can_send`).
    state_tx: broadcast::Sender<StateSnapshot>,
    /// Orchestration-LOWERED events injected by `send`/`cancel` (TurnStarted,
    /// Cancel) — merged into the fold loop alongside backend events. This is how
    /// `Command::Send` becomes `TurnStarted{epoch}` (§3 lower): the orchestrator,
    /// NOT the backend, produces it (TurnStarted is orchestration-lowered, never
    /// backend-produced — I9).
    lowered_tx: mpsc::UnboundedSender<SessionEnvelope>,
    lowered_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<SessionEnvelope>>>>,
    /// Per-session LATEST state snapshot (the G1 fix). Updated on every push.
    /// A state subscriber that LAGS (broadcast ring overwrote its un-consumed
    /// snapshots, possibly including the single unlock) recovers the current
    /// truth from here on `Lagged` — so the unlock can never be permanently lost
    /// to lag. Also seeds a late/reconnect subscriber's first snapshot (G8).
    /// Mirrors Addendum 8's "full snapshot is always re-derivable" guarantee.
    latest: Arc<std::sync::Mutex<HashMap<String, StateSnapshot>>>,
    /// Per-session STICKY last-terminal `(turn_gen, TransitionReason)` — written
    /// only when a turn folds to a terminal phase (Idle/Error), and NOT overwritten
    /// by the next turn's activity-edge pushes (unlike `latest`, whose `last_reason`
    /// is clobbered the moment the next turn starts). This is the lag-recovering
    /// terminal oracle a stall-intolerant consumer (the Route B team reactor) reads
    /// after a `Lagged` drop on the domain ring to learn "did the last turn complete
    /// or was it cancelled?" without trusting it caught the live `TurnCompleted`.
    /// Holds `TransitionReason` (the session-native outcome: `Completed`/`Cancelled`
    /// /`Errored`); the conversation layer maps it to its own `TurnOutcomeTag`
    /// (one-directional dep — `aionui-session` never names the upper-layer tag).
    last_terminal: Arc<std::sync::Mutex<HashMap<String, (u64, TransitionReason)>>>,
}

impl Orchestrator {
    /// Create an orchestrator with bounded broadcast buffers. `cap` sizes both
    /// fan-out rings (a slow subscriber that lags surfaces `Lagged`/`Closed` on
    /// its own receiver, never blocking the fold loop).
    pub fn new(cap: usize) -> Self {
        let (event_tx, _) = broadcast::channel(cap);
        let (state_tx, _) = broadcast::channel(cap);
        let (lowered_tx, lowered_rx) = mpsc::unbounded_channel();
        Self {
            event_tx,
            state_tx,
            lowered_tx,
            lowered_rx: Arc::new(Mutex::new(Some(lowered_rx))),
            latest: Arc::new(std::sync::Mutex::new(HashMap::new())),
            last_terminal: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// THE command entry (§3 lower). Dispatch `Send` to the backend, then lower a
    /// `TurnStarted{epoch: receipt.turn_gen}` into the fold loop so the FSM goes
    /// Idle→Running. This closes the dispatch↔fold-loop loop: a caller sends, the
    /// turn starts, the unlock flips false — all without the backend ever
    /// producing TurnStarted (I9). The backend's PromptAccepted / deltas /
    /// TurnResult then flow up `events()` and fold normally.
    pub async fn send(
        &self,
        backend: &dyn SessionBackend,
        session_id: &str,
        content: Vec<super::types::ContentBlock>,
        metadata: super::types::CommandMeta,
    ) -> Result<super::types::CommandReceipt, super::types::BackendError> {
        let receipt = backend
            .dispatch(super::types::Command::Send { content, metadata })
            .await?;
        // Only a Started admission begins a turn; a Queued one will be lowered
        // when it is promoted (P0: Started is the path; Queued lowering is a
        // later admission-policy slice).
        if matches!(receipt.admission, super::types::Admission::Started) {
            let _ = self.lowered_tx.send(SessionEnvelope {
                session_id: session_id.to_string(),
                turn_gen: receipt.turn_gen,
                event: SessionEvent::TurnStarted {
                    epoch: receipt.turn_gen,
                },
            });
        }
        Ok(receipt)
    }

    /// Lower a user `Cancel` (§004 S14): dispatch to the backend AND fold a
    /// `SessionEvent::Cancel` so the FSM folds Running→Idle immediately (the UI
    /// unlocks without waiting for the backend's trailing terminal).
    pub async fn cancel(
        &self,
        backend: &dyn SessionBackend,
        session_id: &str,
        target: super::types::CancelTarget,
    ) -> Result<super::types::CommandReceipt, super::types::BackendError> {
        let receipt = backend.dispatch(super::types::Command::Cancel { target }).await?;
        let _ = self.lowered_tx.send(SessionEnvelope {
            session_id: session_id.to_string(),
            turn_gen: receipt.turn_gen,
            event: SessionEvent::Cancel,
        });
        Ok(receipt)
    }

    /// Raw `SessionEnvelope` stream, demuxed to one `session_id` (transcript /
    /// streaming UI). Every delta flows here in real time (Tier-0 push-not-store).
    ///
    /// §9.4 backpressure: when a slow subscriber overflows the bounded broadcast
    /// buffer, the channel reports `RecvError::Lagged(n)` (n events dropped). We
    /// SURFACE that as one `SessionEvent::Lagged{skipped:n}` envelope for this
    /// session (reducer-IGNORED, orchestration-lowered) so the consumer KNOWS it
    /// missed deltas and can `reconnect()` to catch up — rather than silently
    /// continuing with a hole. turn_gen=0 (the lag signal is not tied to a turn).
    pub fn subscribe_events(&self, session_id: impl Into<String>) -> BoxStream<'static, SessionEnvelope> {
        let me = session_id.into();
        let rx = self.event_tx.subscribe();
        futures_util::stream::unfold((rx, me), |(mut rx, me)| async move {
            loop {
                match rx.recv().await {
                    Ok(env) if env.session_id == me => return Some((env, (rx, me))),
                    Ok(_) => continue, // another session — demux skip
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Surface the drop to THIS session's consumer once, then
                        // keep reading (the consumer reconnects to refill Tier-1).
                        let env = SessionEnvelope {
                            session_id: me.clone(),
                            turn_gen: 0,
                            event: SessionEvent::Lagged { skipped: n },
                        };
                        return Some((env, (rx, me)));
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
        .boxed()
    }

    /// 009 R4b: synchronous read of this session's LATEST cached snapshot (the
    /// same cache `subscribe_state` recovers from on Lagged). The flush engine /
    /// `send` try-now path reads `can_send`/`can_queue` from here to decide whether
    /// a just-enqueued Held message can dispatch immediately. `None` = no snapshot
    /// folded yet (pre-first-transition → treat as initial Idle: can_send=true).
    pub fn latest_snapshot(&self, session_id: &str) -> Option<StateSnapshot> {
        self.latest.lock().ok().and_then(|m| m.get(session_id).cloned())
    }

    /// Synchronous read of this session's STICKY last-terminal `(turn_gen, reason)`
    /// — the lag-recovering terminal oracle (see the `last_terminal` field). Returns
    /// `None` before any turn has folded a terminal phase. A consumer that may have
    /// missed the live `TurnCompleted` (e.g. on a `Lagged` domain-ring drop) reads
    /// this to recover the outcome of the most-recently-settled turn; pairing the
    /// `turn_gen` lets it confirm the terminal it recovers is the turn it cares about.
    pub fn latest_terminal(&self, session_id: &str) -> Option<(u64, TransitionReason)> {
        self.last_terminal.lock().ok().and_then(|m| m.get(session_id).cloned())
    }

    /// Full `StateSnapshot` stream, demuxed to one `session_id`. The conversation
    /// reads `snapshot.can_send` for the unlock (NEVER recomputes — §C7).
    ///
    /// LAG-RECOVERING (G1 fix) + LATE-SEEDING (G8 fix): on `Lagged`, instead of
    /// silently skipping (which could drop the single unlock snapshot forever),
    /// re-emit this session's LATEST cached snapshot — so the current `can_send`
    /// is always eventually delivered, even after the broadcast ring overwrote
    /// the live copy. Also seeds the FIRST item from the cache so a late/reconnect
    /// subscriber immediately learns the current phase (Addendum 8 reconnect).
    pub fn subscribe_state(&self, session_id: impl Into<String>) -> BoxStream<'static, StateSnapshot> {
        let me = session_id.into();
        let rx = self.state_tx.subscribe();
        let latest = self.latest.clone();
        // Seed: the current cached snapshot for this session (if any), so a late
        // subscriber is not blind until the next transition.
        let seed = latest.lock().ok().and_then(|m| m.get(&me).cloned());
        // State = (receiver, latest-cache, my session id, pending seed). The seed
        // is delivered on the first poll then dropped to None.
        futures_util::stream::unfold((rx, latest, me, seed), |(mut rx, latest, me, seed)| async move {
            // First poll: deliver the seed (if any) before touching the channel.
            if let Some(s) = seed {
                return Some((s, (rx, latest, me, None)));
            }
            loop {
                match rx.recv().await {
                    Ok(s) if s.session_id == me => return Some((s, (rx, latest, me, None))),
                    Ok(_) => continue, // another session — demux skip
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // G1: lag may have dropped our unlock. Recover this
                        // session's LATEST snapshot from the cache so can_send is
                        // never permanently lost.
                        if let Some(s) = latest.lock().ok().and_then(|m| m.get(&me).cloned()) {
                            return Some((s, (rx, latest, me, None)));
                        }
                        // HOLE-G1-A fix: the cache is empty ⟺ this session has had
                        // NO transition yet ⟺ it is at its initial Idle. Without
                        // this, an empty-cache Lagged would `continue`-spin forever
                        // if the session never produces an own-event (e.g. a Queued
                        // send that never lowered TurnStarted) — a permanent UI lock,
                        // exactly the bug G1 exists to kill, relocated to the
                        // pre-first-transition window. Synthesize the truthful
                        // initial Idle(can_send=true) ONCE so the subscriber is never
                        // blind. A later real transition supersedes it normally.
                        let initial = StateSnapshot {
                            session_id: me.clone(),
                            state: SessionState::Idle,
                            can_send: true,
                            // Idle + no background source → not active (§1.6).
                            has_activity: false,
                            // Idle: nothing in flight → cannot queue, cannot cancel.
                            can_queue: false,
                            can_cancel: false,
                            turn_gen: 0,
                            last_reason: None,
                        };
                        return Some((initial, (rx, latest, me, None)));
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
        .boxed()
    }

    /// The derived unlock-bool stream (a `map` over snapshots): `can_send`
    /// flipping true is the ONLY unlock signal, decoupled from any blocking
    /// return (§C7). Replaces the deleted hardcoded `turn.completed{canSend}`.
    pub fn subscribe_unlock(&self, session_id: impl Into<String>) -> BoxStream<'static, bool> {
        self.subscribe_state(session_id).map(|s| s.can_send).boxed()
    }

    /// GAP-F (§9.3 / C7 reconnect, Addendum 8): re-deliver the current truth to a
    /// (re)subscribing consumer. Re-broadcasts this session's cached `latest`
    /// StateSnapshot (so a fresh `subscribe_state` immediately re-sees can_send +
    /// full FSM) AND emits a `SessionEvent::Snapshot{state_repr, turn_gen}` on the
    /// event stream (the first — and only — producer of that variant). This is an
    /// ORCHESTRATION SIGNAL, NOT a `Command` (it never touches a backend or the
    /// FSM). P0 scope = re-emit cached truth; Tier-1 transcript backfill +
    /// live-tail-resume are the deferred P2 conversation-side slice. No-op if the
    /// session has no cached snapshot yet (nothing has happened to re-deliver).
    pub async fn reconnect(&self, session_id: &str) {
        let snap = self
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned();
        if let Some(snap) = snap {
            let _ = self.event_tx.send(SessionEnvelope {
                session_id: session_id.to_string(),
                turn_gen: snap.turn_gen,
                event: SessionEvent::Snapshot {
                    state_repr: format!("{:?}", snap.state),
                    turn_gen: snap.turn_gen,
                },
            });
            let _ = self.state_tx.send(snap);
        }
    }

    /// Test-only: a handle to the state broadcast sender, so a test can push a
    /// synthetic `StateSnapshot` (e.g. to verify cross-session demux without
    /// driving a full turn). Gated so production callers can't bypass `fold_one`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn state_tx_for_test(&self) -> broadcast::Sender<StateSnapshot> {
        self.state_tx.clone()
    }

    /// Test-only: a handle to the EVENT broadcast sender, so a test can flood the
    /// ring to force `RecvError::Lagged` and verify `subscribe_events` surfaces a
    /// `SessionEvent::Lagged`. Gated so production callers can't bypass the backend.
    #[cfg(any(test, feature = "test-support"))]
    pub fn event_tx_for_test(&self) -> broadcast::Sender<SessionEnvelope> {
        self.event_tx.clone()
    }

    /// Test-only: seed the per-session `latest` cache directly (production fills it
    /// via `fold_one`), so a test can exercise `reconnect`'s re-emit without driving
    /// a full turn.
    #[cfg(any(test, feature = "test-support"))]
    pub fn seed_latest_for_test(&self, snap: StateSnapshot) {
        self.latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(snap.session_id.clone(), snap);
    }

    /// Test-only: seed the per-session sticky `last_terminal` cache directly
    /// (production fills it in `fold_one` at a terminal fold), so a test can exercise
    /// the lag-recovering `latest_terminal` read without driving a full turn.
    #[cfg(any(test, feature = "test-support"))]
    pub fn seed_last_terminal_for_test(&self, session_id: impl Into<String>, turn_gen: u64, reason: TransitionReason) {
        self.last_terminal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.into(), (turn_gen, reason));
    }

    /// Drive a backend through the reducer until its event stream ends (backend
    /// dropped / process exited). The ONLY `step()` call site (I9). MERGES two
    /// inputs into one fold lane: (a) the backend's `events()` (backend-produced)
    /// and (b) the orchestration-LOWERED events from `send`/`cancel` (TurnStarted
    /// /Cancel). The lowered TurnStarted is what moves the FSM Idle→Running — the
    /// backend never produces it. Runs to completion; spawn it on a task.
    pub async fn run(&self, backend: &dyn SessionBackend) {
        // 009 R2: the backend's proactive-input capability is fixed for the
        // session; snapshot it once so every fold can derive `can_queue` without
        // re-querying. (claude=true via stdin FIFO; codex/acp/aionrs=false.)
        let accepts_proactive_input = backend.capabilities().accepts_proactive_input;
        let mut events = backend.events();
        // Take the lowered-event receiver (single consumer = this run loop).
        let mut lowered = self
            .lowered_rx
            .lock()
            .await
            .take()
            .expect("Orchestrator::run called more than once");
        // One FSM lane per session (a connection may multiplex many — §4).
        let mut fsms: HashMap<String, SessionState> = HashMap::new();
        // 009 R6: the BACKGROUND-plane workflow roster, one map per session. Lives
        // alongside `fsms` (single-owner = this run loop). Unlike `Running.subagents`
        // (which the FSM drops when the turn leaves Running), a roster entry OUTLIVES
        // its turn — so `has_activity`'s background half stays true while a Workflow
        // runs past the turn that spawned it (semantic-②).
        let mut rosters: HashMap<String, HashMap<String, crate::state::WorkflowAgentState>> = HashMap::new();

        loop {
            // Bias toward lowered events (a TurnStarted lowered by `send` must be
            // folded BEFORE the backend's resulting deltas) — `tokio::select!` with
            // the lowered branch first + `biased` ordering.
            let env = tokio::select! {
                biased;
                Some(low) = lowered.recv() => low,
                next = events.next() => match next {
                    Some(env) => env,
                    None => break, // backend stream ended
                },
            };
            self.fold_one(&mut fsms, &mut rosters, env, accepts_proactive_input);
        }

        // Drain any remaining lowered events (e.g. a Cancel issued as the stream
        // ended) so a late unlock still fires.
        while let Ok(env) = lowered.try_recv() {
            self.fold_one(&mut fsms, &mut rosters, env, accepts_proactive_input);
        }
    }

    /// Fold one envelope (backend-produced OR lowered) through the reducer +
    /// broadcast. Extracted so `run`'s merged select stays readable.
    fn fold_one(
        &self,
        fsms: &mut HashMap<String, SessionState>,
        rosters: &mut HashMap<String, HashMap<String, crate::state::WorkflowAgentState>>,
        mut env: SessionEnvelope,
        accepts_proactive_input: bool,
    ) {
        // (1) epoch restamp (§5.4): a persistent-process adapter emits
        // TurnResult.epoch=0 (no turn context); stamp the live turn_gen so the
        // reducer's cross-turn stale-result guard works. Other events unchanged.
        restamp_epoch(&mut env);

        // (1b) OBSERVABILITY (turn-lifecycle boundaries only): the fold loop is the
        // single chokepoint every SessionEvent passes through, so log the turn-shape
        // markers here at info — without this the send→fold→terminal path is a black
        // hole (a turn that never produces output leaves no trace of WHERE it stalled).
        // Only the boundary/terminal variants are logged (NOT the high-frequency
        // deltas), and only event SHAPE (variant + turn_gen) — never prompt/output text
        // (AGENTS.md: no sensitive payloads in production logs).
        match &env.event {
            SessionEvent::TurnStarted { epoch } => tracing::info!(
                conversation_id = %env.session_id,
                turn_gen = env.turn_gen,
                epoch,
                "fold: TurnStarted"
            ),
            SessionEvent::TurnResult {
                is_error,
                api_error_status,
                ..
            } => tracing::info!(
                conversation_id = %env.session_id,
                turn_gen = env.turn_gen,
                is_error,
                api_error_status,
                "fold: TurnResult (terminal)"
            ),
            SessionEvent::Detached { exit, .. } => tracing::info!(
                conversation_id = %env.session_id,
                turn_gen = env.turn_gen,
                exit_code = exit.as_ref().and_then(|e| e.code),
                "fold: Detached (backend gone)"
            ),
            SessionEvent::Cancel => tracing::info!(
                conversation_id = %env.session_id,
                turn_gen = env.turn_gen,
                "fold: Cancel"
            ),
            SessionEvent::PromptAccepted { .. } => tracing::info!(
                conversation_id = %env.session_id,
                turn_gen = env.turn_gen,
                "fold: PromptAccepted"
            ),
            _ => {}
        }

        // (2) raw envelope to UI/transcript/persistence (every event, incl. every
        // delta — Tier-0 push-not-store, §7). Lowered TurnStarted/Cancel also
        // flow here so a transcript sees the turn boundary.
        let _ = self.event_tx.send(env.clone());

        // (009 R6) BACKGROUND plane: mirror SubagentUpdate into the session's
        // workflow_roster (which outlives the turn, unlike the FSM's
        // Running.subagents). This is what keeps `has_activity` true while a
        // Workflow runs past its spawning turn (semantic-②). Terminal absorption +
        // the same FIFO ref keying as the FSM plane. Capture the background-active
        // bit BEFORE and AFTER the update so a background-only edge (a workflow
        // appears / finishes with no FSM phase change) is detected by the push-gate.
        let roster = rosters.entry(env.session_id.clone()).or_default();
        let bg_before = crate::state::background_active(roster);
        // 009 R6 cleanup path 3 + crash parity: the process is GONE, so any
        // still-running workflow will NEVER deliver its terminal task_notification.
        // Clear the roster now, else has_activity stays stuck true forever (§12.7
        // liveness leak). Two structurally-identical "process gone" signals:
        //   - BackendSuspended: idle-reap suspended the backend (the documented closer).
        //   - Detached: the process hit a real EOF/exit (crash) mid-workflow → the
        //     reducer folds this to Error{Crashed}, but the dead process can no longer
        //     emit the per-ref terminal SubagentUpdate that would terminalize the
        //     entries. Without clearing, a workflow that was Running at crash time keeps
        //     background_active() true → has_activity stuck on the Error snapshot. Cancel
        //     never reaches here as a crash (I10 absorbs the post-cancel Detached, see
        //     event.rs), so this clear only fires on genuine process loss.
        // Other events enrich/upsert as usual.
        if matches!(
            env.event,
            SessionEvent::BackendSuspended | SessionEvent::Detached { .. }
        ) {
            roster.clear();
        } else {
            update_roster(roster, &env.event);
        }
        let bg_after = crate::state::background_active(roster);

        // (3) fold through the reducer (the one step() call site).
        let state = fsms.entry(env.session_id.clone()).or_insert(SessionState::Idle);
        let prev = state.clone();
        let (mut next, transitions) = step(state, env.event.clone());

        // (4) I14 prune: terminal subagents dropped at a turn boundary
        // (orchestrator-layer — the reducer only upserts).
        prune_terminal_subagents_on_boundary(&prev, &mut next);

        *state = next;

        // (5) push a FULL snapshot (Addendum 8) on a real phase change OR a
        // has_activity edge. Record it as this session's LATEST first (G1 fix) so a
        // lagging subscriber can recover it even if the broadcast ring overwrites
        // the live copy.
        //
        // §1.6(3) push-gate widening: an FSM phase change always pushes. But
        // `has_foreground_activity` also flips on a `SubagentUpdate` that upserts
        // into `Running.subagents` WITHOUT crossing an external phase (the reducer
        // returns no Transition for a roster-only mutation — reducer.rs §6b b1). The
        // canonical case: the main turn blocks on a permission (Running +
        // requires_action, has_activity=false) while a previously-spawned subagent is
        // still executing (any_subagent_active → has_activity flips true), then that
        // subagent finishes (flips back false). Gating ONLY on `transitions` would
        // strand the subscriber on the pre-flip value — the spinner would never start
        // (or never stop) for a subagent running concurrently with an approval. So we
        // also push when `has_foreground_activity` differs across this fold. We
        // compare `prev` (the pre-step clone) against the post-prune `*state` (the
        // SAME value the snapshot carries), so a boundary prune that drops a terminal
        // subagent is reflected in the comparison. We do NOT compare the raw roster
        // (status/label churn within the active set is task-side detail, not an
        // activity edge) — `has_foreground_activity`'s boolean is the exact projection
        // §1.6 specifies. Accumulator-only flips (`saw_substantive_output`,
        // `terminal_result_seen`) never reach here: `has_foreground_activity` reads
        // only phase + requires_action + the subagent-active set, so a delta flood
        // produces NO extra push.
        // 009 R6: has_activity = foreground half ∥ BACKGROUND half. Edge = either
        // half changing. Foreground compares prev vs post-step FSM; background
        // compares the roster bit captured before vs after this event's update.
        // A background-only edge (a workflow appears/finishes with no FSM phase
        // change) must push, else a reconnecting/lagging subscriber strands on the
        // stale activity bit.
        let has_activity = has_foreground_activity(state) || bg_after;
        let prev_has_activity = has_foreground_activity(&prev) || bg_before;
        let activity_changed = prev_has_activity != has_activity;
        if !transitions.is_empty() || activity_changed {
            // activity-only push: transitions is empty → derive_reason returns None
            // (no phase change to attribute), which is correct — last_reason names the
            // last PHASE transition, not a roster edge.
            let last_reason = derive_reason(transitions.last(), &env.event);
            let snap = StateSnapshot {
                session_id: env.session_id.clone(),
                state: state.clone(),
                can_send: can_send_message(state),
                // §1.6 / 009 R6: has_activity = the FOREGROUND half (Starting /
                // Running working / any FSM subagent active) ∥ the BACKGROUND half
                // (`background_active` over this session's workflow_roster, which
                // outlives the spawning turn). This realizes semantic-② — a Workflow
                // (Task tool) is non-blocking and runs past its turn, so after the
                // turn folds Idle the FSM is quiet but the roster keeps has_activity
                // true (spinner stays on) until task_notification clears the entry.
                // (Was a hardcoded `false` background half — the F6 UNWIRED-bug.)
                has_activity,
                // 009 R2: capability-gated proactive-queue + FSM-only cancel,
                // pre-derived so conversation reads the fields, never recomputes.
                can_queue: crate::state::can_queue_message(state, accepts_proactive_input),
                can_cancel: crate::state::can_cancel(state),
                turn_gen: env.turn_gen,
                last_reason,
            };
            if let Ok(mut map) = self.latest.lock() {
                map.insert(env.session_id.clone(), snap.clone());
            }
            // Sticky terminal cache: record (turn_gen, reason) ONLY when this fold
            // lands the turn in a terminal phase (Idle/Error) with an attributable
            // reason. Last-write-wins on the session key so the G14 cancel-vs-late-
            // TurnResult race (a stray TurnResult folding one Idle after a Cancel)
            // settles on whichever terminal the reducer emitted last, never a stale
            // mix. Activity-only pushes (transitions empty ⇒ last_reason None) and
            // non-terminal transitions never touch it, so it survives into the NEXT
            // turn as the answer to "what was the last terminal outcome".
            if matches!(state, SessionState::Idle | SessionState::Error { .. })
                && let Some(reason) = snap.last_reason.clone()
                && let Ok(mut term) = self.last_terminal.lock()
            {
                term.insert(env.session_id.clone(), (env.turn_gen, reason));
            }
            // Resume-anchor self-heal (Wave-5: ownership moved from the legacy
            // conversation-side `spawn_claude_transition_subscriber` to the session
            // layer). When a turn lands in Error because the persisted resume anchor
            // is dead (claude "No conversation found" / `error_during_execution`),
            // the binding the conversation persisted as `backend_session_id` is no
            // longer usable. Emit `BackendBound { None }` — the documented "backend
            // session lost → clear the column" channel — so the facade wipes the stale
            // anchor and the NEXT send starts Fresh instead of re-failing the resume.
            // The reducer ignores BackendBound (no FSM effect); the facade is the sole
            // consumer (clears conversations.backend_session_id).
            if let SessionState::Error { reason } = state
                && crate::state::is_unrecoverable_resume_error(reason)
            {
                let _ = self.event_tx.send(SessionEnvelope {
                    session_id: env.session_id.clone(),
                    turn_gen: env.turn_gen,
                    event: SessionEvent::BackendBound {
                        backend_session_id: None,
                    },
                });
            }
            let _ = self.state_tx.send(snap);
        }
    }
}

impl Default for Orchestrator {
    fn default() -> Self {
        Self::new(1024)
    }
}

/// §5.4: stamp the live `turn_gen` onto a `TurnResult` the adapter left
/// unstamped (epoch 0). Leaves an already-stamped result and all other events
/// untouched.
fn restamp_epoch(env: &mut SessionEnvelope) {
    if let SessionEvent::TurnResult { epoch, .. } = &mut env.event
        && *epoch == 0
    {
        *epoch = env.turn_gen;
    }
}

/// I14: drop terminal subagents from the live roster. NOTE despite the name this
/// runs on EVERY Running fold (not only the turn boundary): a terminal entry is
/// removed the moment its update folds, so a `Completed`/`Errored`/`Shutdown`
/// subagent never lingers in `Running.subagents`. (The `was_running && !still_running`
/// boundary branch is a documented no-op hook for the future "subagents survive
/// across turns" model — leaving Running drops the whole roster anyway.) The
/// `Interrupted` status is deliberately NOT pruned (it is a non-terminal pause that
/// may resume) but also does NOT count as active in `any_subagent_active`, so an
/// interrupted subagent correctly contributes neither a roster entry removal nor a
/// has_activity edge.
fn prune_terminal_subagents_on_boundary(prev: &SessionState, next: &mut SessionState) {
    let was_running = matches!(prev, SessionState::Running { .. });
    let still_running = matches!(next, SessionState::Running { .. });
    if was_running && !still_running {
        // Leaving Running — the carry would otherwise be discarded entirely on
        // the next TurnStarted anyway, so nothing to prune in `next` (it's a
        // terminal variant with no subagents field). This hook exists for the
        // future "subagents survive across turns" model; today it is a no-op
        // documenting WHERE the prune belongs (orchestrator, not reducer).
    }
    if let SessionState::Running { subagents, .. } = next {
        subagents.retain(|s| {
            !matches!(
                s.status,
                SubagentStatus::Completed | SubagentStatus::Errored | SubagentStatus::Shutdown
            )
        });
    }
}

/// 009 R6: mirror a `SubagentUpdate` into the BACKGROUND-plane workflow_roster.
/// Upsert by `ref` (= ref_id) with §11.4 terminal absorption: once an entry's
/// task_status is terminal, a late non-terminal update does NOT resurrect it
/// (mirrors the reducer's foreground-plane rule, so a lagged `progress` after a
/// `Completed` can't re-ignite the background spinner). Only `SubagentUpdate`
/// touches the roster; every other event is a no-op here. Rich fields (model /
/// tokens / loop state) are filled by the claude workflow_progress[] parser in a
/// follow-on (R6b); this step carries ref_id + task_status + label.
fn update_roster(roster: &mut HashMap<String, crate::state::WorkflowAgentState>, event: &SessionEvent) {
    use crate::state::{WorkflowAgentState, WorkflowTaskStatus};
    // 009 R6b: rich per-agent detail (claude workflow_progress[]) fills the
    // display fields on an existing or new roster entry. Keyed by the same `ref`
    // as SubagentUpdate. Never changes task_status (that is SubagentUpdate's job /
    // §11.4 absorption); only enriches model/tokens/tools/loop-state.
    if let SessionEvent::SubagentDetail {
        r#ref,
        label,
        loop_state,
        model,
        tokens,
        tool_calls,
        last_tool_name,
        ..
    } = event
    {
        let slot = roster.entry(r#ref.clone()).or_insert_with(|| WorkflowAgentState {
            ref_id: r#ref.clone(),
            // Detail can arrive before the SubagentUpdate that sets a real status;
            // default Running (a detail frame means the agent is active). A later
            // SubagentUpdate refines/terminalizes it.
            task_status: WorkflowTaskStatus::Running,
            // A detail-ONLY entry carries no lifecycle: its `agentId`/label child gets
            // no `task_notification` terminal (that terminalizes the container task_id),
            // so it must not drive background_active. A subsequent SubagentUpdate on the
            // SAME ref (rare — child refs differ from container refs) would set this true.
            has_lifecycle: false,
            retain: None,
            label: None,
            state: None,
            model: None,
            last_tool_name: None,
            tokens: None,
            tool_calls: None,
        });
        if label.is_some() {
            slot.label = label.clone();
        }
        if loop_state.is_some() {
            slot.state = *loop_state;
        }
        if model.is_some() {
            slot.model = model.clone();
        }
        if tokens.is_some() {
            slot.tokens = *tokens;
        }
        if tool_calls.is_some() {
            slot.tool_calls = *tool_calls;
        }
        if last_tool_name.is_some() {
            slot.last_tool_name = last_tool_name.clone();
        }
        return;
    }
    let SessionEvent::SubagentUpdate {
        r#ref, label, status, ..
    } = event
    else {
        return;
    };
    // Map the 6-state subagent lifecycle onto the task outcome (orthogonal to the
    // claude-only LLM-loop `state`): active states → Running; the rest terminal.
    let task_status = match status {
        SubagentStatus::PendingInit | SubagentStatus::Running => WorkflowTaskStatus::Running,
        SubagentStatus::Completed => WorkflowTaskStatus::Completed,
        SubagentStatus::Errored => WorkflowTaskStatus::Failed,
        SubagentStatus::Interrupted | SubagentStatus::Shutdown => WorkflowTaskStatus::Stopped,
    };
    match roster.get_mut(r#ref) {
        // §11.4 absorption: terminal entry + non-terminal update → ignore.
        Some(slot) if slot.task_status.is_terminal() && !task_status.is_terminal() => {}
        Some(slot) => {
            slot.task_status = task_status;
            // A SubagentUpdate is the lifecycle signal: this ref has a real
            // task_notification terminal path, so it counts toward background activity
            // (upgrades a detail-first-created entry from detail-only to lifecycle).
            slot.has_lifecycle = true;
            if label.is_some() {
                slot.label = label.clone();
            }
        }
        None => {
            roster.insert(
                r#ref.clone(),
                WorkflowAgentState {
                    ref_id: r#ref.clone(),
                    task_status,
                    // Created by a SubagentUpdate → lifecycle-bearing (drives background).
                    has_lifecycle: true,
                    retain: None,
                    label: label.clone(),
                    state: None,
                    model: None,
                    last_tool_name: None,
                    tokens: None,
                    tool_calls: None,
                },
            );
        }
    }
}

/// Derive the typed `TransitionReason` from the transition's destination + the
/// triggering event (the existing `Transition` carries only from/to/epoch, so
/// the reason is computed here — orchestration-derived, reducer never sees it).
fn derive_reason(t: Option<&Transition>, event: &SessionEvent) -> Option<TransitionReason> {
    let t = t?;
    let reason = match &t.to {
        SessionState::Running { .. } => match event {
            SessionEvent::TurnStarted { .. } => TransitionReason::Started,
            SessionEvent::PermissionResolved { .. } => TransitionReason::PermissionResolved,
            _ => TransitionReason::Started,
        },
        SessionState::Idle => match event {
            SessionEvent::Cancel => TransitionReason::Cancelled(crate::event::CancelReason::UserCancel),
            SessionEvent::TurnResult { outcome, .. } => match outcome {
                crate::event::TurnOutcome::Cancelled { reason } => TransitionReason::Cancelled(*reason),
                crate::event::TurnOutcome::Completed { stop_reason } => {
                    TransitionReason::Completed(stop_reason.clone())
                }
                _ => TransitionReason::Completed(crate::event::StopReason::EndTurn),
            },
            _ => TransitionReason::Completed(crate::event::StopReason::EndTurn),
        },
        SessionState::Error { reason } => TransitionReason::Errored(reason.clone()),
        SessionState::Starting => TransitionReason::Started,
    };
    // RequiresAction is a sub-condition of Running — if we just crossed INTO it,
    // report the permission request reason.
    if crate::state::is_requires_action(&t.to)
        && !crate::state::is_requires_action(&t.from)
        && let SessionEvent::Permission { kind, .. } = event
    {
        return Some(TransitionReason::PermissionRequested(*kind));
    }
    Some(reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::types::{Admission, BackendError, Command, CommandReceipt};
    use crate::event::SessionEvent;

    /// A scripted backend that emits a fixed envelope sequence — drives the fold
    /// loop deterministically (the live claude/codex backends are tested in their
    /// own modules; here we pin the ORCHESTRATOR's fold/snapshot/restamp/prune).
    struct ScriptBackend(Vec<SessionEnvelope>);

    #[async_trait::async_trait]
    impl SessionBackend for ScriptBackend {
        async fn dispatch(&self, c: Command) -> Result<CommandReceipt, BackendError> {
            // A real dispatch so the orchestrator's send()/cancel() lowering path
            // can be exercised: Send → Started{turn_gen:1}; others → NoTurn.
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
            futures_util::stream::iter(self.0.clone()).boxed()
        }
        fn capabilities(&self) -> crate::capability::Capabilities {
            crate::capability::Capabilities::default()
        }
    }

    fn env(session_id: &str, turn_gen: u64, event: SessionEvent) -> SessionEnvelope {
        SessionEnvelope {
            session_id: session_id.into(),
            turn_gen,
            event,
        }
    }

    async fn collect_snaps(orch: &Orchestrator, sid: &str, backend: ScriptBackend) -> Vec<StateSnapshot> {
        let mut snaps = orch.subscribe_state(sid);
        let run = {
            let orch = orch.clone();
            tokio::spawn(async move { orch.run(&backend).await })
        };
        let mut out = Vec::new();
        for _ in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), snaps.next()).await {
                Ok(Some(s)) => {
                    out.push(s.clone());
                    if s.can_send && matches!(s.state, SessionState::Idle) {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = run.await;
        out
    }

    /// Like `collect_snaps` but drains EVERY snapshot until the backend stream ends
    /// (the script has no terminal → never reaches the Idle break). Used by the
    /// subagent/has_activity tests whose sequences stay in Running.
    async fn collect_all_snaps(orch: &Orchestrator, sid: &str, backend: ScriptBackend) -> Vec<StateSnapshot> {
        let mut snaps = orch.subscribe_state(sid);
        let run = {
            let orch = orch.clone();
            tokio::spawn(async move { orch.run(&backend).await })
        };
        let mut out = Vec::new();
        // The script backend's event stream is finite; after it drains, run()
        // returns and no more snapshots arrive, so the timeout ends collection.
        for _ in 0..40 {
            match tokio::time::timeout(std::time::Duration::from_millis(300), snaps.next()).await {
                Ok(Some(s)) => out.push(s),
                _ => break,
            }
        }
        let _ = run.await;
        out
    }

    /// BASELINE: a PURE-TEXT turn (no subagent/workflow events) folds to Idle with
    /// has_activity=false. The roster is empty, so the background half is false and
    /// `has_foreground_activity(Idle)` is false. If this ever goes true, the backend
    /// is leaking activity into a plain chat turn.
    #[tokio::test]
    async fn plain_text_turn_idle_snapshot_has_activity_false() {
        let orch = Orchestrator::new(256);
        let seq = collect_all_snaps(
            &orch,
            "p",
            ScriptBackend(vec![
                env("p", 1, SessionEvent::TurnStarted { epoch: 1 }),
                env(
                    "p",
                    1,
                    SessionEvent::MessageDelta {
                        item_id: "m".into(),
                        text: "hi".into(),
                    },
                ),
                env(
                    "p",
                    1,
                    SessionEvent::TurnResult {
                        is_error: false,
                        api_error_status: None,
                        result_text: "hi".into(),
                        epoch: 0,
                        outcome: crate::event::TurnOutcome::default(),
                    },
                ),
            ]),
        )
        .await;
        let idle = seq
            .iter()
            .rev()
            .find(|s| matches!(s.state, SessionState::Idle))
            .expect("a final Idle snapshot");
        assert!(
            !idle.has_activity,
            "a plain-text turn's Idle snapshot must have has_activity=false, got {idle:?}"
        );
    }

    /// REGRESSION (stale-has-activity, WS-captured 2026-06-22): a session that ran a
    /// workflow earlier leaves the per-child `SubagentDetail` roster entry (keyed by
    /// agentId) at task_status=Running FOREVER — `task_notification` only terminalizes
    /// the CONTAINER (task_id ref), never the per-agent detail entry, and the per-agent
    /// "done" arrives only as a loop_state (which `background_active` does not read). The
    /// orchestrator's `rosters` map is run()-scoped (lives across ALL turns of the
    /// process), and only Detached/BackendSuspended clear it — so a plain TurnResult→Idle
    /// does NOT. Result: EVERY subsequent turn (even a pure chat one) reports
    /// has_activity=true → the sidebar spins forever. This pins the bug (currently RED on
    /// the second, plain-text turn's Idle snapshot).
    #[tokio::test]
    async fn workflow_child_detail_leaks_has_activity_into_later_plain_turns() {
        use crate::event::SubagentStatus;
        let orch = Orchestrator::new(256);
        let seq = collect_all_snaps(
            &orch,
            "w",
            ScriptBackend(vec![
                // ── Turn 1: a workflow runs (container + one per-agent child). ──
                env("w", 1, SessionEvent::TurnStarted { epoch: 1 }),
                // container (task_id ref) starts...
                env(
                    "w",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "task-1".into(),
                        label: Some("Build".into()),
                        status: SubagentStatus::Running,
                        parent_ref: Some("toolu-1".into()),
                        kind: None,
                    },
                ),
                // per-agent child detail (agentId ref) — task_status defaults Running,
                // enriched only with loop_state; NOTHING ever terminalizes this ref. Use
                // `Progress` (NOT Done): fixture-verified, ~half of workflow_agent refs
                // emit only start/progress and never a `done`, so this is the worst case
                // — a loop_state-based fix would miss it; the has_lifecycle fix must not.
                env(
                    "w",
                    1,
                    SessionEvent::SubagentDetail {
                        r#ref: "agent-A".into(),
                        parent_ref: Some("task-1".into()),
                        label: Some("run:A".into()),
                        loop_state: Some(crate::state::WorkflowLoopState::Progress),
                        model: Some("opus".into()),
                        tokens: Some(10),
                        tool_calls: Some(2),
                        last_tool_name: None,
                        phase_index: None,
                        phase_title: None,
                        last_tool_summary: None,
                        duration_ms: None,
                    },
                ),
                // container completes (terminalizes task-1, NOT agent-A).
                env(
                    "w",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "task-1".into(),
                        label: None,
                        status: SubagentStatus::Completed,
                        parent_ref: Some("toolu-1".into()),
                        kind: None,
                    },
                ),
                env(
                    "w",
                    1,
                    SessionEvent::TurnResult {
                        is_error: false,
                        api_error_status: None,
                        result_text: "done".into(),
                        epoch: 0,
                        outcome: crate::event::TurnOutcome::default(),
                    },
                ),
                // ── Turn 2: a PLAIN-TEXT turn, no subagent events at all. ──
                env("w", 2, SessionEvent::TurnStarted { epoch: 2 }),
                env(
                    "w",
                    2,
                    SessionEvent::MessageDelta {
                        item_id: "m2".into(),
                        text: "hello".into(),
                    },
                ),
                env(
                    "w",
                    2,
                    SessionEvent::TurnResult {
                        is_error: false,
                        api_error_status: None,
                        result_text: "hello".into(),
                        epoch: 0,
                        outcome: crate::event::TurnOutcome::default(),
                    },
                ),
            ]),
        )
        .await;
        // The LAST Idle snapshot is turn 2 (the plain-text turn). It must NOT report
        // has_activity — the workflow child must not haunt a later plain turn.
        let last_idle = seq
            .iter()
            .rev()
            .find(|s| matches!(s.state, SessionState::Idle) && s.turn_gen == 2)
            .expect("a turn-2 Idle snapshot");
        assert!(
            !last_idle.has_activity,
            "a plain turn AFTER a finished workflow must have has_activity=false (the per-agent \
             detail entry must not pin the background half forever), got {last_idle:?}"
        );
    }

    /// §3/§9.12 end-to-end: a clean turn folds Idle→Running(can_send=false)→
    /// Idle(can_send=true) and the orchestrator pushes a FULL snapshot on each
    /// phase change. The unlock is a snapshot field, decoupled from any return.
    #[tokio::test]
    async fn fold_loop_pushes_full_snapshot_on_phase_change() {
        let orch = Orchestrator::new(256);
        let seq = collect_snaps(
            &orch,
            "x",
            ScriptBackend(vec![
                env("x", 1, SessionEvent::TurnStarted { epoch: 1 }),
                env(
                    "x",
                    1,
                    SessionEvent::MessageDelta {
                        item_id: "m".into(),
                        text: "hi".into(),
                    },
                ),
                env(
                    "x",
                    1,
                    SessionEvent::TurnResult {
                        is_error: false,
                        api_error_status: None,
                        result_text: "hi".into(),
                        epoch: 0, // unstamped → orchestrator restamps to turn_gen=1
                        outcome: crate::event::TurnOutcome::default(),
                    },
                ),
            ]),
        )
        .await;

        assert!(
            seq.iter()
                .any(|s| !s.can_send && matches!(s.state, SessionState::Running { .. })),
            "Running snapshot with can_send=false, got {seq:?}"
        );
        let idle = seq.iter().find(|s| s.can_send && matches!(s.state, SessionState::Idle));
        assert!(idle.is_some(), "Idle snapshot with can_send=true (unlock), got {seq:?}");
        assert_eq!(idle.unwrap().session_id, "x", "snapshot demuxed by session_id");
    }

    /// Wave-5 resume-anchor self-heal: when a turn lands in Error because the
    /// persisted resume anchor is dead (claude "No conversation found"), the
    /// orchestrator emits `BackendBound { None }` on the event stream so the
    /// conversation facade clears the stale `backend_session_id`. Ownership moved
    /// here from the legacy conversation-side transition subscriber.
    #[tokio::test]
    async fn unrecoverable_resume_error_emits_backend_bound_none() {
        let orch = Orchestrator::new(256);
        let mut events = orch.subscribe_events("x");
        let run = {
            let orch = orch.clone();
            let backend = ScriptBackend(vec![
                env("x", 1, SessionEvent::TurnStarted { epoch: 1 }),
                // A bad `--resume` surfaces as an is_error TurnResult while Starting,
                // folding to Error{Backend{message:"No conversation found"}}.
                env(
                    "x",
                    1,
                    SessionEvent::TurnResult {
                        is_error: true,
                        api_error_status: None,
                        result_text: "No conversation found".into(),
                        epoch: 0,
                        outcome: crate::event::TurnOutcome::Failed,
                    },
                ),
            ]);
            tokio::spawn(async move { orch.run(&backend).await })
        };

        let mut saw_clear = false;
        for _ in 0..40 {
            match tokio::time::timeout(std::time::Duration::from_millis(300), events.next()).await {
                Ok(Some(env)) => {
                    if matches!(
                        env.event,
                        SessionEvent::BackendBound {
                            backend_session_id: None
                        }
                    ) {
                        saw_clear = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = run.await;
        assert!(
            saw_clear,
            "unrecoverable resume error must emit BackendBound{{None}} to clear the stale anchor"
        );
    }

    /// SS-2 (Route B HB#1): the STICKY `latest_terminal` cache records a settled
    /// turn's `(turn_gen, TransitionReason)` and survives into later activity — the
    /// lag-recovering oracle a consumer reads after missing the live terminal.
    /// `None` before any terminal; a clean turn → `Completed(EndTurn)`; a later turn
    /// that ends differently OVERWRITES (sticky-latest, keyed per session).
    #[tokio::test]
    async fn latest_terminal_is_sticky_and_carries_outcome() {
        let orch = Orchestrator::new(256);
        assert!(orch.latest_terminal("x").is_none(), "no terminal before any turn");

        // Two turns through ONE run loop (the orchestrator's spawn-once `lowered_rx`
        // forbids a second `run`): turn 1 ends cleanly (EndTurn), turn 2 errors. The
        // sticky cache must end pointing at turn 2 (latest-wins, per-session key).
        let _ = collect_all_snaps(
            &orch,
            "x",
            ScriptBackend(vec![
                env("x", 1, SessionEvent::TurnStarted { epoch: 1 }),
                env(
                    "x",
                    1,
                    SessionEvent::TurnResult {
                        is_error: false,
                        api_error_status: None,
                        result_text: "ok".into(),
                        epoch: 0,
                        outcome: crate::event::TurnOutcome::default(), // EndTurn
                    },
                ),
                env("x", 2, SessionEvent::TurnStarted { epoch: 2 }),
                env(
                    "x",
                    2,
                    SessionEvent::TurnResult {
                        is_error: true,
                        api_error_status: None,
                        result_text: "boom".into(),
                        epoch: 0,
                        outcome: crate::event::TurnOutcome::Failed,
                    },
                ),
            ]),
        )
        .await;

        let (g, r) = orch.latest_terminal("x").expect("terminal recorded");
        assert_eq!(
            g, 2,
            "sticky cache points at the LATEST settled turn (overwrite, not append)"
        );
        assert!(
            matches!(r, TransitionReason::Errored(_)),
            "turn 2 ended in error → Errored, got {r:?}"
        );

        // A DIFFERENT session never inherits this one's terminal (per-session key).
        assert!(orch.latest_terminal("other").is_none(), "terminal is per-session");
    }

    /// 009 R1d / §12.8 CR-15: cross-session FSM isolation. `fold_one` keys one
    /// FSM lane per `session_id` (HashMap), so one session crashing must NOT
    /// pollute a concurrent healthy session. This holds by construction today,
    /// but a refactor back to a single global FSM would silently reintroduce the
    /// pollution — so pin it. Interleave a crash on `s_a` (TurnStarted then
    /// Detached → Error{Crashed}) with a clean turn on `s_b`, subscribe to `s_b`,
    /// and assert `s_b` still reaches a clean Idle unlock (never Error).
    #[tokio::test]
    async fn fold_isolates_one_session_crash_from_another() {
        let orch = Orchestrator::new(256);
        let backend = ScriptBackend(vec![
            env("s_a", 1, SessionEvent::TurnStarted { epoch: 1 }),
            env("s_b", 1, SessionEvent::TurnStarted { epoch: 1 }),
            // s_a's backend process dies mid-turn → Error{Crashed}.
            env(
                "s_a",
                1,
                SessionEvent::Detached {
                    exit: Some(crate::event::ExitStatusLite {
                        code: None,
                        signal: Some(9),
                    }),
                    redacted_summary: None,
                },
            ),
            // s_b finishes cleanly AFTER s_a crashed.
            env(
                "s_b",
                1,
                SessionEvent::TurnResult {
                    is_error: false,
                    api_error_status: None,
                    result_text: "ok".into(),
                    epoch: 0,
                    outcome: crate::event::TurnOutcome::default(),
                },
            ),
        ]);
        let seq = collect_snaps(&orch, "s_b", backend).await;
        assert!(
            seq.iter().all(|s| s.session_id == "s_b"),
            "demux: s_b subscriber sees only s_b snapshots, got {seq:?}"
        );
        assert!(
            !seq.iter().any(|s| matches!(s.state, SessionState::Error { .. })),
            "s_a's crash must NOT pollute s_b into Error, got {seq:?}"
        );
        assert!(
            seq.iter().any(|s| s.can_send && matches!(s.state, SessionState::Idle)),
            "s_b reaches a clean Idle unlock despite s_a crashing, got {seq:?}"
        );
    }

    /// 🖥️ UI-1 — the derived `subscribe_unlock()` bool stream (the simplest
    /// consumer-facing unlock signal: the send button toggling locked→unlocked) had
    /// NO direct test. The frontend binds the composer enabled-state to this bool;
    /// if it regressed (wrong field projected, or a lag-recovered snapshot leaked a
    /// stale can_send) the send button would never re-enable. Drives a clean turn
    /// and asserts the bool stream goes false (Running) then true (Idle unlock).
    #[tokio::test]
    async fn ui1_subscribe_unlock_bool_stream_toggles_false_then_true() {
        let orch = Orchestrator::new(256);
        let mut unlock = orch.subscribe_unlock("u1");
        let backend = ScriptBackend(vec![
            env("u1", 1, SessionEvent::TurnStarted { epoch: 1 }),
            env(
                "u1",
                1,
                SessionEvent::MessageDelta {
                    item_id: "m".into(),
                    text: "hi".into(),
                },
            ),
            env(
                "u1",
                1,
                SessionEvent::TurnResult {
                    is_error: false,
                    api_error_status: None,
                    result_text: "hi".into(),
                    epoch: 0,
                    outcome: crate::event::TurnOutcome::default(),
                },
            ),
        ]);
        let run = {
            let orch = orch.clone();
            tokio::spawn(async move { orch.run(&backend).await })
        };
        let mut bools = Vec::new();
        for _ in 0..10 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), unlock.next()).await {
                Ok(Some(b)) => {
                    bools.push(b);
                    if b {
                        break; // saw the unlock
                    }
                }
                _ => break,
            }
        }
        let _ = run.await;
        assert!(
            bools.contains(&false),
            "unlock bool stream must emit false while Running (composer locked), got {bools:?}"
        );
        assert_eq!(
            bools.last(),
            Some(&true),
            "unlock bool stream must end true on Idle (composer re-enabled), got {bools:?}"
        );
    }

    /// 🖥️ UI-2 — `StateSnapshot.last_reason` (the data the frontend renders the
    /// turn-end badge from: completed / cancelled / errored) was never asserted on
    /// the broadcast snapshot. A clean turn's terminal snapshot must carry
    /// `Some(Completed(..))`; a cancel must carry `Some(Cancelled(UserCancel))`.
    #[tokio::test]
    async fn ui2_state_snapshot_carries_typed_last_reason() {
        use crate::backend::types::TransitionReason;
        // Clean completion → last_reason = Completed.
        let orch = Orchestrator::new(256);
        let seq = collect_snaps(
            &orch,
            "r",
            ScriptBackend(vec![
                env("r", 1, SessionEvent::TurnStarted { epoch: 1 }),
                env(
                    "r",
                    1,
                    SessionEvent::MessageDelta {
                        item_id: "m".into(),
                        text: "hi".into(),
                    },
                ),
                env(
                    "r",
                    1,
                    SessionEvent::TurnResult {
                        is_error: false,
                        api_error_status: None,
                        result_text: "hi".into(),
                        epoch: 0,
                        outcome: crate::event::TurnOutcome::default(),
                    },
                ),
            ]),
        )
        .await;
        let idle = seq
            .iter()
            .find(|s| s.can_send && matches!(s.state, SessionState::Idle))
            .expect("idle snapshot");
        assert!(
            matches!(idle.last_reason, Some(TransitionReason::Completed(_))),
            "clean turn's terminal snapshot last_reason = Completed, got {:?}",
            idle.last_reason
        );

        // Cancel → last_reason = Cancelled(UserCancel). Drive via send()→cancel()
        // (the proven path: send lowers TurnStarted→Running, cancel lowers
        // Cancel→Idle), both enqueued before run() drains them.
        let orch2 = Orchestrator::new(256);
        let mut snaps = orch2.subscribe_state("c");
        // Backend with NO terminal — only the lowered Cancel can settle it.
        let backend = ScriptBackend(vec![env(
            "c",
            1,
            SessionEvent::MessageDelta {
                item_id: "m".into(),
                text: "thinking".into(),
            },
        )]);
        orch2
            .send(
                &backend,
                "c",
                vec![crate::backend::types::ContentBlock::Text("go".into())],
                crate::backend::types::CommandMeta::default(),
            )
            .await
            .expect("send");
        orch2
            .cancel(&backend, "c", crate::backend::types::CancelTarget::Turn)
            .await
            .expect("cancel");
        let run = {
            let orch2 = orch2.clone();
            tokio::spawn(async move { orch2.run(&backend).await })
        };
        let mut cancel_reason = None;
        for _ in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), snaps.next()).await {
                Ok(Some(s)) => {
                    if matches!(s.state, SessionState::Idle) && s.can_send {
                        cancel_reason = s.last_reason.clone();
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = run.await;
        assert!(
            matches!(
                cancel_reason,
                Some(TransitionReason::Cancelled(crate::event::CancelReason::UserCancel))
            ),
            "cancel's terminal snapshot last_reason = Cancelled(UserCancel), got {cancel_reason:?}"
        );
    }

    /// 🖥️ UI-3 — the permission closed loop through the ORCHESTRATOR snapshot stream
    /// (not just the pure reducer). The frontend gates the composer AND renders the
    /// permission card off the broadcast StateSnapshot. A Permission(Tool) must
    /// produce a Running snapshot in requires-action with can_send=false +
    /// last_reason=PermissionRequested(Tool); the PermissionResolved must return to
    /// plain Running. (The turn then completes → Idle unlock.)
    #[tokio::test]
    async fn ui3_permission_closed_loop_through_snapshot_stream() {
        use crate::backend::types::TransitionReason;
        use crate::event::PermissionKind;
        let orch = Orchestrator::new(256);
        let seq = collect_snaps(
            &orch,
            "p",
            ScriptBackend(vec![
                env("p", 1, SessionEvent::TurnStarted { epoch: 1 }),
                env(
                    "p",
                    1,
                    SessionEvent::Permission {
                        request_id: "req-1".into(),
                        kind: PermissionKind::Tool,
                        metadata: None,
                        tool_name: None,
                        input: None,
                    },
                ),
                env(
                    "p",
                    1,
                    SessionEvent::PermissionResolved {
                        request_id: "req-1".into(),
                        kind: PermissionKind::Tool,
                    },
                ),
                env(
                    "p",
                    1,
                    SessionEvent::TurnResult {
                        is_error: false,
                        api_error_status: None,
                        result_text: "done".into(),
                        epoch: 0,
                        outcome: crate::event::TurnOutcome::default(),
                    },
                ),
            ]),
        )
        .await;
        // A snapshot in requires-action: Running, can_send=false, reason=PermissionRequested(Tool).
        let perm_snap = seq.iter().find(|s| {
            matches!(&s.state, SessionState::Running { requires_action, .. } if requires_action.waiting_on_approval > 0)
        });
        assert!(
            perm_snap.is_some(),
            "a requires-action snapshot must surface, got {seq:?}"
        );
        let perm_snap = perm_snap.unwrap();
        assert!(!perm_snap.can_send, "composer locked during permission");
        assert!(
            matches!(
                perm_snap.last_reason,
                Some(TransitionReason::PermissionRequested(PermissionKind::Tool))
            ),
            "permission snapshot last_reason = PermissionRequested(Tool), got {:?}",
            perm_snap.last_reason
        );
        // After resolve + result, the final snapshot is the Idle unlock.
        let idle = seq.iter().find(|s| s.can_send && matches!(s.state, SessionState::Idle));
        assert!(
            idle.is_some(),
            "turn completes to Idle unlock after permission resolved, got {seq:?}"
        );
    }

    // ===== §1.6(3): subagent-driven has_activity push-gate (the fold_one widening) =====
    //
    // A `SubagentUpdate` upserts into `Running.subagents` WITHOUT an FSM phase change
    // (reducer §6b b1 → no Transition). When the main turn is parked on a permission
    // (Running + requires_action ⇒ has_activity=false), a concurrently-running
    // subagent flips `has_foreground_activity` true — and back to false when it
    // finishes. The OLD `if !transitions.is_empty()` gate stranded the subscriber on
    // the pre-flip value (spinner never starts / never stops). These pin that the
    // widened gate (`|| activity_changed`) makes both edges observable on
    // subscribe_state, WITHOUT pushing on accumulator/roster-detail noise.

    /// false→true edge: requires_action parks the turn (has_activity=false), then a
    /// subagent starts running → a Running snapshot with has_activity=true reaches
    /// the subscriber even though no phase changed. Without the widening this frame
    /// never arrives (the subscriber stays on the false from the Permission snapshot).
    #[tokio::test]
    async fn subagent_start_during_requires_action_pushes_has_activity_true() {
        use crate::event::{PermissionKind, SubagentStatus};
        let orch = Orchestrator::new(256);
        let seq = collect_all_snaps(
            &orch,
            "sa1",
            ScriptBackend(vec![
                env("sa1", 1, SessionEvent::TurnStarted { epoch: 1 }),
                // park on a tool permission: Running + requires_action, NO subagent yet.
                env(
                    "sa1",
                    1,
                    SessionEvent::Permission {
                        request_id: "req-1".into(),
                        kind: PermissionKind::Tool,
                        metadata: None,
                        tool_name: None,
                        input: None,
                    },
                ),
                // a subagent starts while the main turn is parked → has_activity flips true.
                env(
                    "sa1",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "sub-1".into(),
                        label: Some("research".into()),
                        status: SubagentStatus::Running,
                        parent_ref: None,
                        kind: None,
                    },
                ),
            ]),
        )
        .await;

        // The Permission snapshot: requires_action, has_activity=false (no subagent yet).
        let parked = seq.iter().find(|s| {
            matches!(&s.state, SessionState::Running { requires_action, subagents, .. }
                if requires_action.waiting_on_approval > 0 && subagents.is_empty())
        });
        assert!(
            parked.is_some(),
            "a parked (requires_action, no subagent) snapshot, got {seq:?}"
        );
        assert!(
            !parked.unwrap().has_activity,
            "parked-on-approval with no subagent → has_activity=false"
        );
        assert!(!parked.unwrap().can_send, "parked → composer locked");

        // THE fix: a snapshot where the subagent is running → has_activity=true,
        // can_send still false (state is still Running+requires_action). This frame
        // ONLY exists because the push gate was widened to the has_activity edge.
        let spinning = seq.iter().find(|s| {
            matches!(&s.state, SessionState::Running { subagents, .. }
                if subagents.iter().any(|sub| matches!(sub.status, SubagentStatus::Running)))
        });
        assert!(
            spinning.is_some(),
            "a subagent-running snapshot must be PUSHED (the §1.6(3) widening), got {seq:?}"
        );
        let spinning = spinning.unwrap();
        assert!(
            spinning.has_activity,
            "subagent running during requires_action → has_activity=true (spinner on)"
        );
        assert!(
            !spinning.can_send,
            "can_send stays false — has_activity is orthogonal to the unlock"
        );
    }

    #[tokio::test]
    async fn workflow_outlives_turn_keeps_has_activity_with_can_send_true() {
        // 009 R6 / F6 / semantic-②: a Workflow spawned in a turn OUTLIVES it. After
        // the turn folds Idle (TurnResult), the FSM is quiet (foreground half false)
        // but the workflow_roster entry survives → has_activity stays TRUE while
        // can_send is TRUE. This is the F6 UNWIRED-bug fix (the background half was a
        // hardcoded false). Then the workflow completes → has_activity flips false.
        use crate::event::SubagentStatus;
        let orch = Orchestrator::new(256);
        let seq = collect_all_snaps(
            &orch,
            "wf1",
            ScriptBackend(vec![
                env("wf1", 1, SessionEvent::TurnStarted { epoch: 1 }),
                // a background workflow agent starts during the turn.
                env(
                    "wf1",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "wkflow-1".into(),
                        label: Some("build".into()),
                        status: SubagentStatus::Running,
                        parent_ref: None,
                        kind: None,
                    },
                ),
                // the foreground turn finishes — folds Idle.
                env(
                    "wf1",
                    1,
                    SessionEvent::TurnResult {
                        is_error: false,
                        api_error_status: None,
                        result_text: "started the workflow".into(),
                        epoch: 0,
                        outcome: crate::event::TurnOutcome::default(),
                    },
                ),
                // later the workflow itself completes (outlived the turn).
                env(
                    "wf1",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "wkflow-1".into(),
                        label: Some("build".into()),
                        status: SubagentStatus::Completed,
                        parent_ref: None,
                        kind: None,
                    },
                ),
            ]),
        )
        .await;

        // THE F6 fix: an Idle snapshot (can_send=true) that STILL has_activity=true,
        // because the background workflow outlived the turn (semantic-②).
        let talking_while_busy = seq
            .iter()
            .find(|s| s.can_send && matches!(s.state, SessionState::Idle) && s.has_activity);
        assert!(
            talking_while_busy.is_some(),
            "after the turn folds Idle, the outliving workflow keeps has_activity=true \
             with can_send=true (semantic-②), got {seq:?}"
        );

        // The LAST snapshot (workflow completed) → background half false again.
        let last = seq.last().expect("at least one snapshot");
        assert!(
            !last.has_activity,
            "once the workflow completes the background half clears → has_activity=false, got {last:?}"
        );
        assert!(last.can_send, "still Idle / can_send after the workflow finishes");
    }

    #[tokio::test]
    async fn subagent_detail_fills_rich_roster_fields_keeps_activity() {
        // 009 R6b + stale-has-activity fix: a SubagentDetail enriches a per-AGENT
        // roster entry (model/tokens/tools/loop-state). On the REAL wire the detail's
        // ref is the `agentId` (here "agent-C") while the lifecycle SubagentUpdate's ref
        // is the CONTAINER `task_id` (here "wf") — DIFFERENT refs (the detail's agentId
        // never receives a SubagentUpdate). So background activity is driven by the
        // lifecycle-bearing CONTAINER, not the detail-only child: the running container
        // keeps has_activity true past the turn (semantic-②), and the container's
        // terminal SubagentUpdate clears it — even though the detail child entry is never
        // terminalized (which is exactly why a detail-only entry must NOT drive activity).
        use crate::event::SubagentStatus;
        let orch = Orchestrator::new(256);
        let seq = collect_all_snaps(
            &orch,
            "wd",
            ScriptBackend(vec![
                env("wd", 1, SessionEvent::TurnStarted { epoch: 1 }),
                // The lifecycle container (task_id ref) — drives background activity.
                env(
                    "wd",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "wf".into(),
                        label: Some("Build".into()),
                        status: SubagentStatus::Running,
                        parent_ref: Some("toolu-1".into()),
                        kind: None,
                    },
                ),
                // The per-agent child detail (agentId ref, distinct from the container)
                // — enrichment only, never terminalized.
                env(
                    "wd",
                    1,
                    SessionEvent::SubagentDetail {
                        r#ref: "agent-C".into(),
                        parent_ref: Some("wf".into()),
                        label: Some("run:C".into()),
                        loop_state: Some(crate::state::WorkflowLoopState::Progress),
                        model: Some("opus".into()),
                        tokens: Some(8576),
                        tool_calls: Some(0),
                        last_tool_name: None,
                        phase_index: None,
                        phase_title: None,
                        last_tool_summary: None,
                        duration_ms: None,
                    },
                ),
                env(
                    "wd",
                    1,
                    SessionEvent::TurnResult {
                        is_error: false,
                        api_error_status: None,
                        result_text: "spawned".into(),
                        epoch: 0,
                        outcome: crate::event::TurnOutcome::default(),
                    },
                ),
                // terminalize the CONTAINER (task_id ref).
                env(
                    "wd",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "wf".into(),
                        label: Some("Build".into()),
                        status: SubagentStatus::Completed,
                        parent_ref: Some("toolu-1".into()),
                        kind: None,
                    },
                ),
            ]),
        )
        .await;
        // The running CONTAINER kept the background plane active through the Idle fold.
        assert!(
            seq.iter()
                .any(|s| s.can_send && matches!(s.state, SessionState::Idle) && s.has_activity),
            "a running workflow CONTAINER keeps has_activity=true past the turn (semantic-②), got {seq:?}"
        );
        // The container's terminal SubagentUpdate clears it (the never-terminalized
        // detail-only child does NOT keep it pinned — that is the leak this fix closes).
        assert!(
            !seq.last().unwrap().has_activity,
            "terminal container clears background activity despite the still-Running detail child"
        );
    }

    #[tokio::test]
    async fn backend_suspended_clears_roster_no_stuck_activity() {
        // 009 R6 cleanup path 3 / §12.7 liveness: a workflow is running (background
        // has_activity=true) when idle-reap suspends the backend. The process is
        // gone, so the workflow's terminal task_notification will NEVER arrive — if
        // the roster weren't cleared, has_activity would be stuck true forever.
        // BackendSuspended clears it → has_activity returns to false.
        use crate::event::SubagentStatus;
        let orch = Orchestrator::new(256);
        let seq = collect_all_snaps(
            &orch,
            "sus",
            ScriptBackend(vec![
                env("sus", 1, SessionEvent::TurnStarted { epoch: 1 }),
                env(
                    "sus",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "w".into(),
                        label: None,
                        status: SubagentStatus::Running,
                        parent_ref: None,
                        kind: None,
                    },
                ),
                env(
                    "sus",
                    1,
                    SessionEvent::TurnResult {
                        is_error: false,
                        api_error_status: None,
                        result_text: "go".into(),
                        epoch: 0,
                        outcome: crate::event::TurnOutcome::default(),
                    },
                ),
                // background workflow still running here (has_activity true) ...
                // ... then idle-reap suspends the backend.
                env("sus", 1, SessionEvent::BackendSuspended),
            ]),
        )
        .await;
        // Before suspend: an Idle snapshot with has_activity=true (workflow outlived).
        assert!(
            seq.iter()
                .any(|s| matches!(s.state, SessionState::Idle) && s.has_activity),
            "the running workflow kept has_activity=true before suspend, got {seq:?}"
        );
        // After suspend: roster cleared → has_activity false (no stuck spinner).
        assert!(
            !seq.last().unwrap().has_activity,
            "BackendSuspended clears the roster → has_activity=false (no liveness leak), got {seq:?}"
        );
    }

    #[tokio::test]
    async fn crash_mid_workflow_clears_roster_no_stuck_activity() {
        // §12.7 liveness, crash parity with BackendSuspended: a workflow is running
        // (background has_activity=true) when the process dies mid-turn (Detached →
        // Error{Crashed}). The dead process can no longer emit the per-ref terminal
        // SubagentUpdate, so without clearing the roster on Detached the entry stays
        // Running → background_active() true → has_activity stuck true on the Error
        // snapshot forever. Detached must clear the roster exactly like suspend.
        use crate::event::SubagentStatus;
        let orch = Orchestrator::new(256);
        let seq = collect_all_snaps(
            &orch,
            "crash",
            ScriptBackend(vec![
                env("crash", 1, SessionEvent::TurnStarted { epoch: 1 }),
                env(
                    "crash",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "w".into(),
                        label: None,
                        status: SubagentStatus::Running,
                        parent_ref: None,
                        kind: None,
                    },
                ),
                // The background workflow is running (has_activity true) ...
                // ... then the process crashes mid-turn (real EOF/exit).
                env(
                    "crash",
                    1,
                    SessionEvent::Detached {
                        exit: None,
                        redacted_summary: None,
                    },
                ),
            ]),
        )
        .await;
        // Before crash: a snapshot with has_activity=true (workflow running).
        assert!(
            seq.iter().any(|s| s.has_activity),
            "the running workflow kept has_activity=true before the crash, got {seq:?}"
        );
        // After crash: the turn is Error AND the roster was cleared → has_activity
        // false (no stuck spinner on a dead session).
        let last = seq.last().unwrap();
        assert!(
            matches!(last.state, SessionState::Error { .. }),
            "Detached folds the turn to Error, got {last:?}"
        );
        assert!(
            !last.has_activity,
            "Detached clears the roster → has_activity=false (crash parity with suspend), got {seq:?}"
        );
    }

    /// ENUMERATION INVARIANT (anti "isomorphic-branch-not-fully-enumerated" defect):
    /// the PROCESS-LOSS equivalence class — events meaning "the process is gone, so the
    /// per-ref terminal SubagentUpdate that would terminalize a Running workflow can
    /// NEVER arrive" — is `{Detached, BackendSuspended}`. EVERY member MUST clear the
    /// roster, else a Running workflow leaves a ghost entry → has_activity stuck true.
    /// This is the bug that shipped: fold_one's clear-arm matched only BackendSuspended,
    /// Detached (crash) was isomorphic but omitted. A per-source test (only suspend)
    /// could not catch it. This table drives a Running workflow then EACH member and
    /// asserts has_activity falls false — adding a new process-loss event without a
    /// clear-arm trips this. (Cancel is NOT in the class: I10 absorbs the post-cancel
    /// Detached. TurnResult is a turn-end, not process loss.)
    #[tokio::test]
    async fn every_process_loss_event_clears_roster() {
        use crate::event::SubagentStatus;
        // The process-loss equivalence class, enumerated. Extend this when a new
        // "process is gone" SessionEvent is added — and fold_one must clear on it.
        let process_loss: Vec<(&str, SessionEvent)> = vec![
            ("BackendSuspended", SessionEvent::BackendSuspended),
            (
                "Detached",
                SessionEvent::Detached {
                    exit: None,
                    redacted_summary: None,
                },
            ),
        ];
        for (name, loss_event) in process_loss {
            let orch = Orchestrator::new(256);
            let seq = collect_all_snaps(
                &orch,
                "pl",
                ScriptBackend(vec![
                    env("pl", 1, SessionEvent::TurnStarted { epoch: 1 }),
                    env(
                        "pl",
                        1,
                        SessionEvent::SubagentUpdate {
                            r#ref: "w".into(),
                            label: None,
                            status: SubagentStatus::Running,
                            parent_ref: None,
                            kind: None,
                        },
                    ),
                    // The turn ends but the workflow OUTLIVES it (background_active →
                    // has_activity stays true on Idle). This is the realistic setup the
                    // process-loss event must then tear down (uniform for both members;
                    // a mid-turn crash without this TurnResult is also valid but the
                    // outlive shape exercises the background-plane leak the bug was about).
                    env(
                        "pl",
                        1,
                        SessionEvent::TurnResult {
                            is_error: false,
                            api_error_status: None,
                            result_text: "go".into(),
                            epoch: 1,
                            outcome: crate::event::TurnOutcome::default(),
                        },
                    ),
                    env("pl", 1, loss_event),
                ]),
            )
            .await;
            assert!(
                seq.iter().any(|s| s.has_activity),
                "[{name}] workflow was running (has_activity=true) before the loss, got {seq:?}"
            );
            assert!(
                !seq.last().unwrap().has_activity,
                "[{name}] process-loss MUST clear the roster → has_activity=false \
                 (isomorphic-branch guard: every process-loss event clears, not just suspend), got {seq:?}"
            );
        }
    }

    /// ENUMERATION INVARIANT (snapshot push-gate, §1.6(3)). The fold loop pushes a full
    /// snapshot iff `!transitions.is_empty() || activity_changed` — i.e. on (1) an FSM
    /// phase change OR (2) a has_activity edge (foreground requires-action OR background
    /// subagent set), and NEVER on (3) an accumulator-only churn (delta flood). Each
    /// family had its own test; this pins the whole DECISION in one table so a new
    /// push-trigger (or a regression that drops one) is forced through here. The oracle
    /// is "did a NEW snapshot arrive attributable to the last event" — measured as the
    /// snapshot count strictly growing across the trigger event vs not growing across a
    /// no-op event appended to the same prefix.
    #[tokio::test]
    async fn snapshot_push_gate_fires_on_phase_and_activity_edges_only() {
        use crate::event::{PermissionKind, SubagentStatus};
        let ts = |g| env("pg", g, SessionEvent::TurnStarted { epoch: g });
        let delta = || {
            env(
                "pg",
                1,
                SessionEvent::MessageDelta {
                    item_id: "m".into(),
                    text: "x".into(),
                },
            )
        };
        // Park on a tool permission: Running + requires_action → foreground activity
        // is FALSE, so a subsequent subagent edge is OBSERVABLE as a has_activity flip
        // (during plain Running, foreground is already true and a subagent wouldn't
        // change the bit — the real activity-edge case needs the parked turn).
        let park = || {
            env(
                "pg",
                1,
                SessionEvent::Permission {
                    request_id: "req-1".into(),
                    kind: PermissionKind::Tool,
                    metadata: None,
                    tool_name: None,
                    input: None,
                },
            )
        };
        let sub = |status| {
            env(
                "pg",
                1,
                SessionEvent::SubagentUpdate {
                    r#ref: "w".into(),
                    label: None,
                    status,
                    parent_ref: None,
                    kind: None,
                },
            )
        };

        // Each row: (label, prefix events, the probe event, expect_push?). We run the
        // prefix+probe and compare snapshot count to prefix-only — a push ⟺ the count grew.
        struct Row {
            label: &'static str,
            prefix: Vec<SessionEnvelope>,
            probe: SessionEnvelope,
            expect_push: bool,
        }
        let rows = vec![
            // (1) PHASE change: Idle→Running on TurnStarted.
            Row {
                label: "phase-change(TurnStarted)",
                prefix: vec![],
                probe: ts(1),
                expect_push: true,
            },
            // (2a) ACTIVITY edge ON: parked on approval (foreground false), a subagent
            // starts → has_activity flips true (no FSM phase change — pure roster edge).
            Row {
                label: "activity-on(subagent Running while parked)",
                prefix: vec![ts(1), park()],
                probe: sub(SubagentStatus::Running),
                expect_push: true,
            },
            // (2b) ACTIVITY edge OFF: that subagent completes while still parked →
            // has_activity flips back false (again no phase change).
            Row {
                label: "activity-off(subagent Completed while parked)",
                prefix: vec![ts(1), park(), sub(SubagentStatus::Running)],
                probe: sub(SubagentStatus::Completed),
                expect_push: true,
            },
            // (3) NO-OP: a MessageDelta after another delta — no phase, no activity edge.
            Row {
                label: "noop(delta flood)",
                prefix: vec![ts(1), delta()],
                probe: delta(),
                expect_push: false,
            },
        ];

        for Row {
            label,
            prefix,
            probe,
            expect_push,
        } in rows
        {
            let base = {
                let orch = Orchestrator::new(256);
                collect_all_snaps(&orch, "pg", ScriptBackend(prefix.clone()))
                    .await
                    .len()
            };
            let with_probe = {
                let orch = Orchestrator::new(256);
                let mut script = prefix.clone();
                script.push(probe);
                collect_all_snaps(&orch, "pg", ScriptBackend(script)).await.len()
            };
            if expect_push {
                assert!(
                    with_probe > base,
                    "[{label}] must push a snapshot (count {base} → {with_probe})"
                );
            } else {
                assert_eq!(
                    with_probe, base,
                    "[{label}] must NOT push (accumulator churn is not an edge; count {base} → {with_probe})"
                );
            }
        }
    }

    #[tokio::test]
    async fn terminal_workflow_not_resurrected_in_roster_by_late_update() {
        // 009 R6 / §11.4 on the BACKGROUND plane: once a roster entry is terminal,
        // a late non-terminal SubagentUpdate must NOT re-ignite background activity.
        use crate::event::SubagentStatus;
        let orch = Orchestrator::new(256);
        let seq = collect_all_snaps(
            &orch,
            "wf2",
            ScriptBackend(vec![
                env("wf2", 1, SessionEvent::TurnStarted { epoch: 1 }),
                env(
                    "wf2",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "w".into(),
                        label: None,
                        status: SubagentStatus::Running,
                        parent_ref: None,
                        kind: None,
                    },
                ),
                env(
                    "wf2",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "w".into(),
                        label: None,
                        status: SubagentStatus::Completed,
                        parent_ref: None,
                        kind: None,
                    },
                ),
                env(
                    "wf2",
                    1,
                    SessionEvent::TurnResult {
                        is_error: false,
                        api_error_status: None,
                        result_text: "ok".into(),
                        epoch: 0,
                        outcome: crate::event::TurnOutcome::default(),
                    },
                ),
                // late, out-of-order non-terminal update for the already-completed workflow.
                env(
                    "wf2",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "w".into(),
                        label: None,
                        status: SubagentStatus::Running,
                        parent_ref: None,
                        kind: None,
                    },
                ),
            ]),
        )
        .await;
        let last = seq.last().expect("snapshot");
        assert!(
            !last.has_activity,
            "a late non-terminal update must NOT resurrect the terminal workflow's background activity, got {last:?}"
        );
    }

    /// true→false edge (the other half — the spinner that never stops): a subagent
    /// runs (has_activity=true), the turn parks on a permission (still true, subagent
    /// active), then the subagent COMPLETES → has_activity=false reaches the
    /// subscriber. Without the widening the subscriber is stranded on the true.
    #[tokio::test]
    async fn subagent_completion_during_requires_action_pushes_has_activity_false() {
        use crate::event::{PermissionKind, SubagentStatus};
        let orch = Orchestrator::new(256);
        let seq = collect_all_snaps(
            &orch,
            "sa2",
            ScriptBackend(vec![
                env("sa2", 1, SessionEvent::TurnStarted { epoch: 1 }),
                env(
                    "sa2",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "sub-1".into(),
                        label: Some("worker".into()),
                        status: SubagentStatus::Running,
                        parent_ref: None,
                        kind: None,
                    },
                ),
                // park on a permission while the subagent is still running.
                env(
                    "sa2",
                    1,
                    SessionEvent::Permission {
                        request_id: "req-1".into(),
                        kind: PermissionKind::Tool,
                        metadata: None,
                        tool_name: None,
                        input: None,
                    },
                ),
                // subagent finishes → it is pruned from the roster, has_activity flips
                // false (parked, no active subagent left).
                env(
                    "sa2",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "sub-1".into(),
                        label: Some("worker".into()),
                        status: SubagentStatus::Completed,
                        parent_ref: None,
                        kind: None,
                    },
                ),
            ]),
        )
        .await;

        // The last snapshot: still Running+requires_action, but has_activity now false
        // (the completed subagent was pruned; no active work remains while parked).
        let last = seq.last().expect("at least one snapshot");
        assert!(
            matches!(&last.state, SessionState::Running { requires_action, .. } if requires_action.waiting_on_approval > 0),
            "final snapshot still Running+requires_action, got {seq:?}"
        );
        assert!(
            !last.has_activity,
            "after the subagent completes (pruned), has_activity flips back false — the \
             true→false edge must be PUSHED (spinner stops), got {seq:?}"
        );
        // The terminal-status subagent was pruned at the orchestrator layer (I14).
        assert!(
            matches!(&last.state, SessionState::Running { subagents, .. } if subagents.is_empty()),
            "completed subagent pruned from the live roster, got {seq:?}"
        );
    }

    /// Interrupted status (challenger edge): a subagent that is `Interrupted` is
    /// NEITHER active (any_subagent_active) NOR pruned (only Completed/Errored/
    /// Shutdown are). So interrupting the only running subagent during a parked turn
    /// flips has_activity false (it stops counting) yet the entry STAYS in the roster.
    #[tokio::test]
    async fn interrupted_subagent_flips_activity_false_but_stays_in_roster() {
        use crate::event::{PermissionKind, SubagentStatus};
        let orch = Orchestrator::new(256);
        let seq = collect_all_snaps(
            &orch,
            "sa3",
            ScriptBackend(vec![
                env("sa3", 1, SessionEvent::TurnStarted { epoch: 1 }),
                env(
                    "sa3",
                    1,
                    SessionEvent::Permission {
                        request_id: "req-1".into(),
                        kind: PermissionKind::Tool,
                        metadata: None,
                        tool_name: None,
                        input: None,
                    },
                ),
                env(
                    "sa3",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "sub-1".into(),
                        label: None,
                        status: SubagentStatus::Running,
                        parent_ref: None,
                        kind: None,
                    },
                ),
                env(
                    "sa3",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "sub-1".into(),
                        label: None,
                        status: SubagentStatus::Interrupted,
                        parent_ref: None,
                        kind: None,
                    },
                ),
            ]),
        )
        .await;

        let last = seq.last().expect("a snapshot");
        assert!(
            !last.has_activity,
            "Interrupted subagent does not count as active → has_activity=false, got {seq:?}"
        );
        assert!(
            matches!(&last.state, SessionState::Running { subagents, .. }
                if subagents.iter().any(|s| matches!(s.status, SubagentStatus::Interrupted))),
            "Interrupted is non-terminal → it STAYS in the roster (not pruned), got {seq:?}"
        );
    }

    /// LATE subscriber sees fresh has_activity (HOLE-G1-A guard): the activity-only
    /// push also updates the `latest` cache, so a subscriber that attaches AFTER the
    /// flips seeds the most-recent has_activity, not a stale value.
    #[tokio::test]
    async fn late_subscriber_seeds_current_has_activity_after_subagent_flips() {
        use crate::event::{PermissionKind, SubagentStatus};
        let orch = Orchestrator::new(256);
        // Drive a turn that parks then starts a subagent (has_activity ends true),
        // collecting through the live subscriber so run() finishes.
        let _ = collect_all_snaps(
            &orch,
            "sa4",
            ScriptBackend(vec![
                env("sa4", 1, SessionEvent::TurnStarted { epoch: 1 }),
                env(
                    "sa4",
                    1,
                    SessionEvent::Permission {
                        request_id: "r".into(),
                        kind: PermissionKind::Tool,
                        metadata: None,
                        tool_name: None,
                        input: None,
                    },
                ),
                env(
                    "sa4",
                    1,
                    SessionEvent::SubagentUpdate {
                        r#ref: "sub-1".into(),
                        label: None,
                        status: SubagentStatus::Running,
                        parent_ref: None,
                        kind: None,
                    },
                ),
            ]),
        )
        .await;

        // A LATE subscriber: its first (seed) snapshot must carry the fresh
        // has_activity=true (the activity-only push wrote `latest`), not a stale false.
        let mut late = orch.subscribe_state("sa4");
        let seed = tokio::time::timeout(std::time::Duration::from_secs(1), late.next())
            .await
            .ok()
            .flatten()
            .expect("late subscriber seeds from latest cache");
        assert!(
            seed.has_activity,
            "late subscriber seeds the CURRENT has_activity=true (activity-only push refreshed \
             the latest cache — HOLE-G1-A guard), got {seed:?}"
        );
    }

    /// Noise guard (the negative): a delta flood within Running produces NO extra
    /// activity-only snapshots. `has_foreground_activity` reads only phase +
    /// requires_action + the active-subagent set, NOT `saw_substantive_output`, so a
    /// MessageDelta flipping that accumulator must NOT widen-push. Exactly the two
    /// phase snapshots (Running, Idle) appear — proving the widening did not degrade
    /// into a per-delta push.
    #[tokio::test]
    async fn delta_flood_produces_no_extra_activity_snapshots() {
        let orch = Orchestrator::new(256);
        let seq = collect_all_snaps(
            &orch,
            "sa5",
            ScriptBackend(vec![
                env("sa5", 1, SessionEvent::TurnStarted { epoch: 1 }),
                env(
                    "sa5",
                    1,
                    SessionEvent::MessageDelta {
                        item_id: "m".into(),
                        text: "a".into(),
                    },
                ),
                env(
                    "sa5",
                    1,
                    SessionEvent::MessageDelta {
                        item_id: "m".into(),
                        text: "b".into(),
                    },
                ),
                env(
                    "sa5",
                    1,
                    SessionEvent::ThoughtDelta {
                        item_id: "t".into(),
                        text: "hmm".into(),
                    },
                ),
                env(
                    "sa5",
                    1,
                    SessionEvent::TurnResult {
                        is_error: false,
                        api_error_status: None,
                        result_text: "done".into(),
                        epoch: 0,
                        outcome: crate::event::TurnOutcome::default(),
                    },
                ),
            ]),
        )
        .await;
        // Exactly two snapshots: the Running phase change + the Idle terminal. The
        // three deltas (saw_substantive_output flip on the first MessageDelta) add
        // NONE — has_foreground_activity does not read that accumulator.
        assert_eq!(
            seq.len(),
            2,
            "only the 2 phase snapshots (Running, Idle); deltas must NOT widen-push, got {seq:?}"
        );
        assert!(matches!(seq[0].state, SessionState::Running { .. }) && !seq[0].can_send);
        assert!(matches!(seq[1].state, SessionState::Idle) && seq[1].can_send);
    }

    /// Restamp survives the fold: a stale-low-epoch TurnResult arriving during a
    /// NEW turn is dropped by the reducer's guard ONLY because the orchestrator
    /// stamps the live turn_gen onto the new turn's events. Here we assert the
    /// current turn's own unstamped result settles (epoch 0 → restamped to the
    /// live turn_gen == since_epoch, so it is NOT dropped).
    #[tokio::test]
    async fn restamped_current_turn_result_settles_to_idle() {
        let orch = Orchestrator::new(256);
        let seq = collect_snaps(
            &orch,
            "x",
            ScriptBackend(vec![
                env("x", 5, SessionEvent::TurnStarted { epoch: 5 }),
                env(
                    "x",
                    5,
                    SessionEvent::TurnResult {
                        is_error: false,
                        api_error_status: None,
                        result_text: "done".into(),
                        epoch: 0, // restamped to 5 == since_epoch → settles (not dropped)
                        outcome: crate::event::TurnOutcome::default(),
                    },
                ),
            ]),
        )
        .await;
        assert!(
            seq.last()
                .map(|s| s.can_send && matches!(s.state, SessionState::Idle))
                .unwrap_or(false),
            "the current turn's own (unstamped→restamped) result settles to Idle, got {seq:?}"
        );
    }

    /// Restamp unit: an unstamped TurnResult (epoch 0) gets the envelope turn_gen.
    #[test]
    fn restamp_stamps_unstamped_turn_result() {
        let mut env = SessionEnvelope {
            session_id: "s".into(),
            turn_gen: 7,
            event: SessionEvent::TurnResult {
                is_error: false,
                api_error_status: None,
                result_text: "x".into(),
                epoch: 0,
                outcome: crate::event::TurnOutcome::default(),
            },
        };
        restamp_epoch(&mut env);
        assert!(matches!(env.event, SessionEvent::TurnResult { epoch: 7, .. }));
    }

    /// ⭐ THE dispatch↔fold-loop closure (orchestrator-lowers-Command). `send()`
    /// dispatches the prompt AND lowers `TurnStarted{epoch:turn_gen}` — NO
    /// manually injected TurnStarted, NO backend-produced TurnStarted (I9). We
    /// enqueue send() BEFORE run() so the lowered TurnStarted is the first folded
    /// event (biased select), faithfully modeling production ordering: a request/
    /// response backend (claude --print) emits NOTHING until it reads the prompt,
    /// so its deltas+result always FOLLOW the lowered TurnStarted. The backend's
    /// events here (deltas+result, NO TurnStarted) represent that response.
    #[tokio::test]
    async fn send_lowers_turn_started_and_drives_full_turn() {
        let orch = Orchestrator::new(256);
        let mut snaps = orch.subscribe_state("s1");

        // The backend's RESPONSE to the prompt: deltas + result (the request/
        // response wire — NO TurnStarted; that's orchestration-lowered).
        let backend = ScriptBackend(vec![
            env(
                "s1",
                1,
                SessionEvent::MessageDelta {
                    item_id: "m".into(),
                    text: "hi".into(),
                },
            ),
            env(
                "s1",
                1,
                SessionEvent::TurnResult {
                    is_error: false,
                    api_error_status: None,
                    result_text: "hi".into(),
                    epoch: 0,
                    outcome: crate::event::TurnOutcome::default(),
                },
            ),
        ]);

        // send() BEFORE run(): dispatches (Started{turn_gen:1}) + lowers
        // TurnStarted into the mpsc. run()'s biased select folds it before the
        // backend events.
        let receipt = orch
            .send(
                &backend,
                "s1",
                vec![crate::backend::types::ContentBlock::Text("hello".into())],
                crate::backend::types::CommandMeta::default(),
            )
            .await
            .expect("send accepted");
        assert_eq!(receipt.turn_gen, 1);

        let run = {
            let orch = orch.clone();
            tokio::spawn(async move { orch.run(&backend).await })
        };

        let mut saw_running_locked = false;
        let mut saw_idle_unlocked = false;
        for _ in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), snaps.next()).await {
                Ok(Some(s)) => {
                    assert_eq!(s.session_id, "s1");
                    if matches!(s.state, SessionState::Running { .. }) && !s.can_send {
                        saw_running_locked = true;
                    }
                    if matches!(s.state, SessionState::Idle) && s.can_send {
                        saw_idle_unlocked = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = run.await;
        assert!(
            saw_running_locked,
            "send() lowered TurnStarted → Running, can_send=false"
        );
        assert!(
            saw_idle_unlocked,
            "backend result folded → Idle, can_send=true (unlock)"
        );
    }

    /// `cancel()` lowers `SessionEvent::Cancel` → the FSM folds Running→Idle
    /// immediately, WITHOUT a backend terminal (UI unlocks at once, §004 S14).
    /// The backend here emits NO terminal (only a delta) — so the ONLY thing that
    /// can reach Idle is the lowered Cancel.
    #[tokio::test]
    async fn cancel_lowers_and_unlocks_without_backend_terminal() {
        let orch = Orchestrator::new(256);
        let mut snaps = orch.subscribe_state("s2");

        // Backend response with NO terminal — just a delta (turn would hang on the
        // backend forever). Only the lowered Cancel can unlock.
        let backend = ScriptBackend(vec![env(
            "s2",
            1,
            SessionEvent::MessageDelta {
                item_id: "m".into(),
                text: "thinking".into(),
            },
        )]);

        // send() lowers TurnStarted, THEN cancel() lowers Cancel — both enqueued
        // before run() drains them (biased, in-order).
        orch.send(
            &backend,
            "s2",
            vec![crate::backend::types::ContentBlock::Text("go".into())],
            crate::backend::types::CommandMeta::default(),
        )
        .await
        .expect("send");
        orch.cancel(&backend, "s2", crate::backend::types::CancelTarget::Turn)
            .await
            .expect("cancel");

        let run = {
            let orch = orch.clone();
            tokio::spawn(async move { orch.run(&backend).await })
        };

        let mut saw_running = false;
        let mut saw_idle_unlocked = false;
        for _ in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), snaps.next()).await {
                Ok(Some(s)) => {
                    if matches!(s.state, SessionState::Running { .. }) {
                        saw_running = true;
                    }
                    if matches!(s.state, SessionState::Idle) && s.can_send {
                        saw_idle_unlocked = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = run.await;
        assert!(saw_running, "send lowered TurnStarted → Running");
        assert!(
            saw_idle_unlocked,
            "cancel() lowered Cancel → Idle + unlock, with NO backend terminal"
        );
    }

    // ======================================================================
    // L3 broadcast/subscription TIMING tests (timing-coverage audit G1/G8/G14).
    // Ported from the prior framework's runtime_broadcast_lag.rs methodology:
    // small CAP to force Lagged + a deliberately-slow consumer + a timeout-
    // bounded relay-like consume so a hang surfaces as "no unlock seen".
    // ======================================================================

    const LAG_CAP: usize = 16;
    const RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    /// Consume the state-snapshot stream like a real (slow) UI relay: loop recv,
    /// Lagged→count+continue, stop at the unlock (Idle+can_send) or Closed/timeout.
    /// Returns (saw_unlock, lagged_total). A genuine "unlock dropped by lag" hang
    /// surfaces as saw_unlock=false (timeout), NOT a blocked test.
    async fn slow_consume_until_unlock(
        mut snaps: BoxStream<'static, StateSnapshot>,
        per_event_delay: std::time::Duration,
    ) -> (bool, usize) {
        let mut saw_unlock = false;
        let mut got = 0usize;
        loop {
            match tokio::time::timeout(RECV_TIMEOUT, snaps.next()).await {
                Ok(Some(s)) => {
                    got += 1;
                    // Simulate a slow subscriber: yield/sleep between receives so
                    // the broadcast ring overwrites un-consumed snapshots.
                    tokio::time::sleep(per_event_delay).await;
                    if s.can_send && matches!(s.state, SessionState::Idle) {
                        saw_unlock = true;
                        break;
                    }
                }
                Ok(None) => break, // stream closed
                Err(_) => break,   // timed out — unlock never arrived (would-be hang)
            }
        }
        (saw_unlock, got)
    }

    /// 🔴 G1 — the unlock snapshot must survive broadcast lag. A turn's terminal
    /// Idle(can_send=true) snapshot is emitted EXACTLY ONCE and is the SOLE unlock
    /// path (§C7). If a slow subscriber lags AND events are emitted AFTER the
    /// unlock (more deltas, or another session's snapshots flooding the shared
    /// state_tx), the unlock can age out of the ring → UI locked forever.
    ///
    /// This mirrors runtime_broadcast_lag's `terminal_lost_only_if_events_follow_it`
    /// shape. We drive ONE session to its unlock, then FLOOD the SAME state_tx with
    /// a second session's transitions (>cap), then let the slow consumer drain.
    /// The §C7 contract REQUIRES the first session's subscriber still observes its
    /// unlock — a state subscriber is demuxed by session_id, so the flood is noise
    /// it must skip without losing its own terminal.
    #[tokio::test]
    async fn g1_unlock_snapshot_survives_lag_with_events_after() {
        let orch = Orchestrator::new(LAG_CAP);
        // Subscribe to session "s1" BEFORE anything runs.
        let snaps_s1 = orch.subscribe_state("s1");

        // Backend for s1: a clean turn (TurnStarted lowered by send → result).
        let s1_backend = ScriptBackend(vec![env(
            "s1",
            1,
            SessionEvent::TurnResult {
                is_error: false,
                api_error_status: None,
                result_text: "done".into(),
                epoch: 0,
                outcome: crate::event::TurnOutcome::default(),
            },
        )]);
        orch.send(
            &s1_backend,
            "s1",
            vec![crate::backend::types::ContentBlock::Text("go".into())],
            crate::backend::types::CommandMeta::default(),
        )
        .await
        .expect("send");

        // A SECOND session floods the SAME state_tx with many transitions AFTER
        // s1's unlock — this is what can overwrite s1's single unlock in the ring.
        // We push these directly onto the shared state broadcast to model the
        // multiplexed-session flood deterministically.
        let run = {
            let orch = orch.clone();
            tokio::spawn(async move {
                orch.run(&s1_backend).await;
                // After s1's turn folded (unlock emitted), flood the shared ring
                // with >cap unrelated snapshots so the slow consumer lags past it.
                for i in 0..(LAG_CAP * 4) {
                    let _ = orch.state_tx.send(StateSnapshot {
                        session_id: "s2".into(),
                        state: SessionState::Running {
                            since_epoch: i as u64,
                            saw_substantive_output: false,
                            terminal_result_seen: false,
                            requires_action: Default::default(),
                            subagents: Vec::new(),
                        },
                        can_send: false,
                        has_activity: true,
                        can_queue: false, // flood noise (another session); never asserted
                        can_cancel: true, // Running
                        turn_gen: i as u64,
                        last_reason: None,
                    });
                }
            })
        };

        // Slow consumer: 5ms between receives → guarantees it lags behind the flood.
        let (saw_unlock, _got) = slow_consume_until_unlock(snaps_s1, std::time::Duration::from_millis(5)).await;
        let _ = run.await;

        assert!(
            saw_unlock,
            "G1: s1's unlock snapshot (Idle,can_send=true) MUST survive broadcast lag + a \
             post-unlock flood from another session — else the UI hangs locked forever. \
             If this fails, the fix is a Lagged-recheck or sticky last-snapshot-on-subscribe."
        );
    }

    /// 🔴 Gap-3 — `subscribe_events` must SURFACE broadcast lag as a
    /// `SessionEvent::Lagged{skipped}`, not silently swallow it. Before the fix the
    /// event demux did `RecvError::Lagged(_) => continue`, so a slow consumer that
    /// overflowed the ring lost deltas with NO signal it had a hole. Here we flood
    /// the event ring past `cap` while a subscriber is parked, then drain and assert
    /// a `Lagged` envelope (skipped>0) for this session arrives.
    #[tokio::test]
    async fn subscribe_events_surfaces_lagged_on_broadcast_overflow() {
        let orch = Orchestrator::new(LAG_CAP);
        // Park a subscriber, then flood WITHOUT consuming so its receiver lags.
        let mut events = orch.subscribe_events("s1");
        let tx = orch.event_tx_for_test();
        for i in 0..(LAG_CAP * 4) {
            let _ = tx.send(SessionEnvelope {
                session_id: "s1".into(),
                turn_gen: 0,
                event: SessionEvent::MessageDelta {
                    item_id: "m1".into(),
                    text: format!("d{i}"),
                },
            });
        }
        // Drain: the first item the parked receiver yields after overflow is the
        // synthesized Lagged (the ring dropped the early deltas).
        let mut saw_lagged = None;
        for _ in 0..(LAG_CAP * 4 + 4) {
            match tokio::time::timeout(std::time::Duration::from_millis(200), events.next()).await {
                Ok(Some(env)) => {
                    if let SessionEvent::Lagged { skipped } = env.event {
                        saw_lagged = Some(skipped);
                        break;
                    }
                }
                _ => break,
            }
        }
        let skipped = saw_lagged.expect(
            "subscribe_events must surface a SessionEvent::Lagged when the broadcast ring \
             overflows a slow subscriber — not silently continue (the Gap-3 silent-swallow defect)",
        );
        assert!(
            skipped > 0,
            "Lagged.skipped reports how many events were dropped, got {skipped}"
        );
    }

    /// 🟡 G8 (FIXED) — a late/reconnect subscriber joining mid-turn immediately
    /// learns the current phase from the seeded latest-snapshot, instead of being
    /// blind until the next transition. This is the G8 fix (subscribe_state seeds
    /// its first item from the per-session `latest` cache, Addendum 8 reconnect).
    #[tokio::test]
    async fn g8_late_subscriber_gets_seeded_running_snapshot() {
        let orch = Orchestrator::new(256);
        // A backend that goes Running then STAYS (no terminal): only a delta.
        let backend = ScriptBackend(vec![env(
            "s3",
            1,
            SessionEvent::MessageDelta {
                item_id: "m".into(),
                text: "streaming".into(),
            },
        )]);
        orch.send(
            &backend,
            "s3",
            vec![crate::backend::types::ContentBlock::Text("go".into())],
            crate::backend::types::CommandMeta::default(),
        )
        .await
        .expect("send");
        let run = {
            let orch = orch.clone();
            tokio::spawn(async move { orch.run(&backend).await })
        };
        // Let the turn reach Running, THEN subscribe late.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut late = orch.subscribe_state("s3");
        // The late subscriber's FIRST snapshot is the seeded current state:
        // Running(can_send=false) — NOT blind, NOT stale-unlocked.
        let first = tokio::time::timeout(std::time::Duration::from_millis(500), late.next()).await;
        run.abort();
        match first {
            Ok(Some(s)) => {
                assert!(
                    matches!(s.state, SessionState::Running { .. }) && !s.can_send,
                    "G8: late subscriber's first (seeded) snapshot is Running(can_send=false), got {s:?}"
                );
            }
            other => panic!("G8: late subscriber must get a seeded Running snapshot, got {other:?}"),
        }
    }

    /// 🟡 G14 — Cancel racing a backend terminal: exactly ONE Idle snapshot, and
    /// the outcome is Idle (cancel folds first, the backend TurnResult is absorbed
    /// by I10). Mutation target: removing `biased` from run()'s select must break
    /// the ordering guarantee this relies on.
    #[tokio::test]
    async fn g14_cancel_with_racing_backend_terminal_yields_one_idle() {
        let orch = Orchestrator::new(256);
        let mut snaps = orch.subscribe_state("s4");
        // Backend emits a delta THEN an immediate TurnResult — racing the lowered
        // Cancel. The Cancel is lowered before run() drains, biased-folded first
        // (Running→Idle); the backend TurnResult then arrives while Idle → I10
        // absorbs it (no second transition, no second snapshot).
        let backend = ScriptBackend(vec![
            env(
                "s4",
                1,
                SessionEvent::MessageDelta {
                    item_id: "m".into(),
                    text: "partial".into(),
                },
            ),
            env(
                "s4",
                1,
                SessionEvent::TurnResult {
                    is_error: false,
                    api_error_status: None,
                    result_text: "late".into(),
                    epoch: 0,
                    outcome: crate::event::TurnOutcome::default(),
                },
            ),
        ]);
        orch.send(
            &backend,
            "s4",
            vec![crate::backend::types::ContentBlock::Text("go".into())],
            crate::backend::types::CommandMeta::default(),
        )
        .await
        .expect("send");
        orch.cancel(&backend, "s4", crate::backend::types::CancelTarget::Turn)
            .await
            .expect("cancel");
        let run = {
            let orch = orch.clone();
            tokio::spawn(async move { orch.run(&backend).await })
        };

        // Collect all snapshots until the stream closes (backend stream ends).
        let mut idle_unlocked = 0usize;
        let mut final_state_idle = false;
        for _ in 0..30 {
            match tokio::time::timeout(std::time::Duration::from_millis(300), snaps.next()).await {
                Ok(Some(s)) => {
                    if matches!(s.state, SessionState::Idle) && s.can_send {
                        idle_unlocked += 1;
                        final_state_idle = true;
                    }
                    // an Error snapshot here would mean the late TurnResult wrongly
                    // settled — assert it never happens.
                    assert!(
                        !matches!(s.state, SessionState::Error { .. }),
                        "G14: a Cancel-then-late-TurnResult must NEVER settle as Error (I10 absorbs it)"
                    );
                }
                _ => break,
            }
        }
        run.abort();
        assert!(final_state_idle, "G14: cancel yields a final Idle(unlock) snapshot");
        assert_eq!(
            idle_unlocked, 1,
            "G14: exactly ONE Idle-unlock snapshot (the Cancel's); the racing backend \
             TurnResult is absorbed by I10 → no second transition/snapshot. Removing `biased` \
             from run()'s select should break this."
        );
    }

    /// Flood the SHARED state_tx with N snapshots from an UNRELATED session to
    /// force a `Lagged` on a slow subscriber to a DIFFERENT session.
    fn flood_other_session(orch: &Orchestrator, sid: &str, n: usize) {
        for i in 0..n {
            let _ = orch.state_tx.send(StateSnapshot {
                session_id: sid.into(),
                state: SessionState::Running {
                    since_epoch: i as u64,
                    saw_substantive_output: false,
                    terminal_result_seen: false,
                    requires_action: Default::default(),
                    subagents: Vec::new(),
                },
                can_send: false,
                has_activity: true,
                can_queue: false, // flood noise (another session); never asserted
                can_cancel: true, // Running
                turn_gen: i as u64,
                last_reason: None,
            });
        }
    }

    /// ⭐ HOLE-G1-A regression guard (found by the G1 verification workflow). A
    /// subscriber that subscribes to a session BEFORE that session's first
    /// transition has latest[sid]=None. If the shared ring then Lags (flooded by
    /// other sessions) and the session never produces an own event (e.g. a Queued
    /// send that never lowered TurnStarted), the empty-cache Lagged arm used to
    /// `continue`-spin forever → permanent UI lock. The fix synthesizes a truthful
    /// initial Idle(can_send=true). This test was RED before the fix.
    #[tokio::test]
    async fn g1a_subscribed_before_first_transition_then_lag_recovers_initial_idle() {
        let orch = Orchestrator::new(LAG_CAP);
        // Subscribe to "s1" BEFORE any s1 transition (latest["s1"] = None).
        let mut snaps = orch.subscribe_state("s1");
        // Flood a DIFFERENT session past cap → forces Lagged on s1's receiver,
        // while s1 itself never transitions (models a Queued/never-started send).
        flood_other_session(&orch, "s2", LAG_CAP * 4);
        // s1's subscriber must NOT hang: the empty-cache Lagged recovers a
        // truthful initial Idle(can_send=true) instead of spinning forever.
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), snaps.next()).await;
        match first {
            Ok(Some(s)) => {
                assert_eq!(s.session_id, "s1", "demux: only s1 snapshots");
                assert!(
                    s.can_send && matches!(s.state, SessionState::Idle),
                    "G1-A: empty-cache Lagged must recover initial Idle(can_send=true), got {s:?}"
                );
            }
            other => panic!("G1-A: subscriber hung on empty-cache Lagged (the bug) — got {other:?}"),
        }
    }

    /// EC-1 hardened (L1 liveness, the core G1 risk stated as the invariant):
    /// a subscriber that HAS seen its session's transitions recovers the unlock
    /// from the cache even when a post-unlock flood from another session laps the
    /// ring. (This is g1_unlock_snapshot_survives_lag_with_events_after re-asserted
    /// as L1; kept separate so the invariant is named.)
    #[tokio::test]
    async fn g1_l1_unlock_recoverable_after_own_transition_and_flood() {
        let orch = Orchestrator::new(LAG_CAP);
        let snaps = orch.subscribe_state("s1");
        let backend = ScriptBackend(vec![env(
            "s1",
            1,
            SessionEvent::TurnResult {
                is_error: false,
                api_error_status: None,
                result_text: "done".into(),
                epoch: 0,
                outcome: crate::event::TurnOutcome::default(),
            },
        )]);
        orch.send(
            &backend,
            "s1",
            vec![crate::backend::types::ContentBlock::Text("go".into())],
            crate::backend::types::CommandMeta::default(),
        )
        .await
        .expect("send");
        let run = {
            let orch = orch.clone();
            tokio::spawn(async move {
                orch.run(&backend).await;
                // post-unlock flood from another session laps the ring.
                flood_other_session(&orch, "s2", LAG_CAP * 4);
            })
        };
        let (saw_unlock, _) = slow_consume_until_unlock(snaps, std::time::Duration::from_millis(5)).await;
        let _ = run.await;
        assert!(
            saw_unlock,
            "L1: unlock recoverable via latest-cache after own transition + flood"
        );
    }
}
