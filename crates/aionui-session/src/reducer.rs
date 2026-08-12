//! The monomorphic, pure reducer (seam b-side, §C 6.4). `step` is the ONLY
//! state-synthesis point, shared by all `BackendAdapter`s. No I/O, no clock, no
//! backend-aware branch (I1). Per-turn memory lives in `SessionState::Running`
//! so the fn output depends only on `(state, event)`.

use crate::event::{ExitStatusLite, Outcome, SessionEvent};
use crate::state::{ErrorReason, RequiresActionSet, SessionState};

/// Emitted on every FSM transition. The reducer (`step`/`settle`) produces these;
/// the orchestrator's `derive_reason` reads `from`/`to` to label the unlock signal.
/// Carries only `SessionState` (no opaque payload), so total equality is
/// meaningful ⇒ derives `Eq` (unlike `SessionEvent`). DUP-10: relocated here from
/// the deleted legacy `command` module (its sole producer is this reducer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub from: SessionState,
    pub to: SessionState,
    /// Minimal turn-epoch (R11/D9), monotonic.
    pub epoch: u64,
}

/// An externally-meaningful descriptor of a state, used to decide whether a
/// `Transition` must be broadcast. Pure-accumulator flips inside `Running`
/// (saw_substantive_output / terminal_result_seen) do NOT change this, so they
/// update state silently; category changes and requires-action crossing zero
/// DO change it and emit a `Transition` (they move can_send / deadline-scope /
/// the RA badge).
#[derive(PartialEq, Eq)]
enum ExternalPhase {
    Starting,
    Running,
    RequiresAction,
    Error,
    Idle,
}

fn external_phase(s: &SessionState) -> ExternalPhase {
    match s {
        SessionState::Starting => ExternalPhase::Starting,
        // Single source of truth for the requires-action predicate: reuse the
        // public `is_requires_action` (its `> 0` lives in state.rs and is
        // directly tested), so there is no duplicate comparison here to drift.
        SessionState::Running { .. } if crate::state::is_requires_action(s) => ExternalPhase::RequiresAction,
        SessionState::Running { .. } => ExternalPhase::Running,
        SessionState::Error { .. } => ExternalPhase::Error,
        SessionState::Idle => ExternalPhase::Idle,
    }
}

/// Build the `(state, transitions)` return: emit a `Transition` iff the external
/// phase changed. `epoch` is the turn epoch to stamp on the transition.
fn settle(from: SessionState, to: SessionState, epoch: u64) -> (SessionState, Vec<Transition>) {
    let transitions = if external_phase(&from) != external_phase(&to) {
        vec![Transition {
            from,
            to: to.clone(),
            epoch,
        }]
    } else {
        Vec::new()
    };
    (to, transitions)
}

/// The turn epoch a state is anchored to (Running's `since_epoch`); terminal /
/// starting states have no anchor (0). Only used to stamp transitions and as
/// the epoch-guard baseline while Running.
fn anchor_epoch(s: &SessionState) -> u64 {
    match s {
        SessionState::Running { since_epoch, .. } => *since_epoch,
        _ => 0,
    }
}

/// seam b-side, monomorphic pure fn. epoch single-home = `TurnStarted{epoch}`
/// (I3): `Running.since_epoch` is the reducer's baseline; `step` takes no
/// separate epoch scalar. Commands arrive as `SessionEvent::TurnStarted`. There
/// is NO clock input: AionCore imposes no auto-timeout on any state (the deadline
/// janitor + `Timeout` event were removed) — a wedged turn ends by user Cancel, a
/// dead process by `Detached`. The reducer never reads a clock.
pub fn step(state: &SessionState, event: SessionEvent) -> (SessionState, Vec<Transition>) {
    match event {
        // ---- TurnStarted: the ONLY event that leaves a terminal state. ----
        // Epoch guard (I3): while Running, a TurnStarted whose epoch does not
        // advance past since_epoch is stale/duplicate → dropped. (Cross-turn
        // stale DATA events can't occur: turn == process boundary, D9.)
        SessionEvent::TurnStarted { epoch } => {
            if let SessionState::Running { since_epoch, .. } = state
                && epoch <= *since_epoch
            {
                return (state.clone(), Vec::new()); // stale / duplicate
            }
            let to = SessionState::Running {
                since_epoch: epoch,
                saw_substantive_output: false,
                terminal_result_seen: false,
                requires_action: RequiresActionSet::default(),
                subagents: Vec::new(), // §6b b1: fresh turn starts with an empty roster
            };
            settle(state.clone(), to, epoch)
        }

        // ---- Terminal-absorbing law (I10): once Idle/Error, ignore all ----
        // events except TurnStarted (handled above). This covers same-turn late
        // results, a self-inflicted-Timeout's later Detached (no re-Crash), a
        // cancel-then-kill's later Detached (no Crash mislabel), etc.
        _ if matches!(state, SessionState::Idle | SessionState::Error { .. }) => (state.clone(), Vec::new()),

        // ---- User cancel (feature 004 S14/R14): fold to Idle terminal. ----
        // Placed BEFORE the Starting guard so a Stop during startup also
        // resolves. Behavior-preserving (mirrors old ACP clean termination);
        // NEVER Error{Crashed}. The manager's subsequent process-kill yields a
        // Detached that I10 (now Idle) absorbs — so crash≠cancel holds by
        // construction. Idempotent: a Cancel while already Idle/Error is
        // absorbed by I10 above.
        SessionEvent::Cancel => {
            let epoch = anchor_epoch(state);
            settle(state.clone(), SessionState::Idle, epoch)
        }

        // ---- Startup-window crash (feature 004 S19/R18): a process that ----
        // exits while still Starting (0-frame exit — bad flag / bad session-id /
        // bad mcp-config / unauthenticated / version too old) MUST resolve to
        // Error{Crashed}, NOT be silently swallowed by the Starting guard below
        // (which would hang until the idle-Timeout). This is the audit-found
        // hole: a 0-frame startup crash never reaches Running, so the
        // Running-arm Detached handling can't catch it. Placed BEFORE the
        // generic Starting guard. exit==0 here is still abnormal (the process
        // left before producing any result/turn), so it is also Crashed.
        SessionEvent::Detached { .. } if matches!(state, SessionState::Starting) => settle(
            state.clone(),
            SessionState::Error {
                reason: ErrorReason::Crashed,
            },
            anchor_epoch(state),
        ),

        // ---- Error TurnResult while Starting (feature 004 R16/3.9): a bad ----
        // `--resume` fails on the FIRST frame with `result{is_error:true}` (e.g.
        // "No conversation found") while still Starting (TurnStarted seen, no
        // stream frame yet). It MUST route to Error{Backend} so the message
        // (the resume-failure cause) reaches the crash-resume self-heal — the
        // generic Starting guard below would drop it, the exit-watcher would
        // then emit Error{Crashed} (cause lost), and the self-heal that matches
        // "No conversation found" would never fire → permanent resume wedge.
        SessionEvent::TurnResult {
            is_error: true,
            api_error_status,
            result_text,
            // No epoch guard while Starting: a bad-`--resume` failure legitimately
            // carries the live epoch, and the resume self-heal MUST see its cause.
            epoch: _,
            // `outcome` (007 §C2/O3) is additive richness the reducer never reads —
            // routing stays on is_error. Ignored here.
            outcome: _,
        } if matches!(state, SessionState::Starting) => settle(
            state.clone(),
            SessionState::Error {
                reason: ErrorReason::Backend {
                    api_error_status,
                    message: result_text,
                },
            },
            anchor_epoch(state),
        ),

        // ---- Events in Starting (pre-spawn window): only TurnStarted is ----
        // meaningful (handled above); anything else is ignored defensively
        // (no frames arrive before the process is up).
        _ if matches!(state, SessionState::Starting) => (state.clone(), Vec::new()),

        // ---- Running arms (state is Running for everything below) ----
        SessionEvent::MessageDelta { text, .. } => {
            let mut to = state.clone();
            if !text.is_empty()
                && let SessionState::Running {
                    saw_substantive_output, ..
                } = &mut to
            {
                *saw_substantive_output = true;
            }
            // accumulator-only: external phase unchanged → no Transition.
            (to, Vec::new())
        }

        // thinking/reasoning is NOT substantive (C5/I5): no state change.
        SessionEvent::ThoughtDelta { .. } => (state.clone(), Vec::new()),

        // tool_use alone is not yet substantive; the COMPLETED tool (ToolResult)
        // is. So ToolCall does not set the accumulator.
        SessionEvent::ToolCall { .. } => (state.clone(), Vec::new()),

        // A completed tool_use = substantive output (C5/I5). ToolResult may
        // arrive WITHOUT a preceding ToolCall (server-managed tools, RFC §3
        // ToolResult-informational law) — we treat it as informational, with no
        // pairing assertion.
        SessionEvent::ToolResult { .. } => {
            let mut to = state.clone();
            if let SessionState::Running {
                saw_substantive_output, ..
            } = &mut to
            {
                *saw_substantive_output = true;
            }
            (to, Vec::new())
        }

        // Heartbeat: a backend liveness signal; the reducer does not change state
        // on it (and there is no deadline to reset — no auto-timeout).
        SessionEvent::Heartbeat => (state.clone(), Vec::new()),

        // Terminal result: route by is_error (C7/R10), NEVER by subtype.
        SessionEvent::TurnResult {
            is_error,
            api_error_status,
            result_text,
            epoch: result_epoch,
            // 007 §C2/O3: the reducer routes the success/error split on `is_error`
            // alone. The ONE refinement (009 R1f): a REFUSAL is a clean turn
            // completion that legitimately carries empty result text — it must
            // NOT be misread as an EmptyTurn error in the OUTPUT-PRESENCE gate
            // below. So we read `outcome` solely to recognize that one case.
            outcome,
        } => {
            // Cross-turn guard (mirrors the TurnStarted guard at the top): a
            // result whose epoch is STRICTLY OLDER than the current Running turn
            // belongs to a prior (cancelled) turn whose trailing `result` claude
            // flushed late — drop it so it can't settle THIS turn as Error. Use
            // `<` not `<=`: the current turn's own result carries
            // `epoch == since_epoch` and MUST settle.
            //
            // `epoch == 0` = UNSTAMPED (the 002 adapter is epoch-agnostic; a
            // backend that never cancels mid-turn leaves it 0). An unstamped
            // result is NEVER dropped — it settles the current turn as before.
            // Only a result carrying a real, strictly-older turn id is stale.
            if let SessionState::Running { since_epoch, .. } = state
                && result_epoch != 0
                && result_epoch < *since_epoch
            {
                return (state.clone(), Vec::new());
            }
            let saw = matches!(
                state,
                SessionState::Running {
                    saw_substantive_output: true,
                    ..
                }
            );
            let epoch = anchor_epoch(state);
            // 009 R1f: a refusal is reported as a clean turn (is_error:false) with
            // empty result text. It is a legitimate completion — the model
            // declined — so the user must be able to send again (fold Idle, NOT
            // Error{EmptyTurn} which would leave can_send stuck false on a
            // perfectly recoverable turn). Only the empty-output gate consults it.
            let refused = matches!(
                outcome,
                crate::event::TurnOutcome::Completed {
                    stop_reason: crate::event::StopReason::Refused { .. }
                }
            );
            let to = if is_error {
                SessionState::Error {
                    reason: ErrorReason::Backend {
                        api_error_status,
                        message: result_text,
                    },
                }
            } else if !saw && result_text.is_empty() && !refused {
                // OUTPUT-PRESENCE (C5): is_error:false AND no substantive output
                // (no non-empty text, no completed tool_use) AND empty result →
                // EmptyTurn, NOT success. A refusal is exempt (handled as Idle).
                SessionState::Error {
                    reason: ErrorReason::EmptyTurn,
                }
            } else {
                SessionState::Idle
            };
            settle(state.clone(), to, epoch)
        }

        // Process-exit edge. drain-before-honor (I11) guarantees any pending
        // TurnResult was applied first. Because S10 mandates TurnResult→terminal
        // DIRECTLY (Idle/Error) and I10 absorbs a post-terminal Detached, by the
        // time a Detached is stepped while STILL Running, no result was seen ⇒
        // `terminal_result_seen` reads false here. (§C 6.2 line 519 acknowledges
        // this: "FollowResult → should not occur (still Running means no result was seen, a contradiction)".)
        // crash_outcome is the pure-fn EXTRACTION of this branch. Its arms are
        // pinned by `crash_outcome_arms` (all four input combos incl. the F46
        // clean-0→CleanNoResult distinction), and the WIRED Detached→Error
        // mappings by `detached_without_result_is_crashed` (signal/non-zero→
        // Crashed), `detached_none_exit_treated_as_terminal_crash` (None→Crashed),
        // and `detached_clean_exit_zero_while_running_is_empty_turn_not_crashed`
        // (clean-0→EmptyTurn). So the arms are not dead at the contract level even
        // though the live path only reaches Crashed/EmptyTurn-while-Running.
        SessionEvent::Detached { exit, .. } => {
            // G2 `redacted_summary` rides this event for the conversation layer
            // (crash ErrorTip); the FSM routes on `exit` only (I10/D3 unchanged).
            let seen = matches!(
                state,
                SessionState::Running {
                    terminal_result_seen: true,
                    ..
                }
            );
            let epoch = anchor_epoch(state);
            match crash_outcome(seen, exit) {
                Outcome::Crashed => settle(
                    state.clone(),
                    SessionState::Error {
                        reason: ErrorReason::Crashed,
                    },
                    epoch,
                ),
                // F46: clean exit-0 with no result is an EMPTY turn, not a crash —
                // distinct ErrorReason so the control plane's recovery disposition
                // is correct (a clean early-EOF is not a SIGKILL).
                Outcome::CleanNoResult => settle(
                    state.clone(),
                    SessionState::Error {
                        reason: ErrorReason::EmptyTurn,
                    },
                    epoch,
                ),
                // Unreachable while Running in the wired path (see above);
                // defensively a no-op so a future drain model that DOES record
                // the flag on Running stays correct.
                Outcome::FollowResult => (state.clone(), Vec::new()),
            }
        }

        // control-request: enter requires-action (+1). Crosses zero on the
        // 0→1 edge → emit Transition (deadline scope / RA badge). 007 §6b b3:
        // route to the counter matching `kind` (Tool→approval, Auth→auth).
        SessionEvent::Permission { kind, .. } => {
            let mut to = state.clone();
            let epoch = anchor_epoch(state);
            if let SessionState::Running { requires_action, .. } = &mut to {
                match kind {
                    crate::event::PermissionKind::Tool => {
                        requires_action.waiting_on_approval = requires_action.waiting_on_approval.saturating_add(1);
                    }
                    crate::event::PermissionKind::Auth => {
                        requires_action.waiting_on_auth = requires_action.waiting_on_auth.saturating_add(1);
                    }
                }
            }
            settle(state.clone(), to, epoch)
        }

        // resolve: -1 on the SAME counter the originating Permission incremented
        // (kind echoed by the adapter, §6b b3). Only the WHOLE set (both counters)
        // reaching zero leaves the requires-action sub-state (I7) — is_requires_action
        // reads both, so external_phase/settle handle the zero-crossing uniformly.
        SessionEvent::PermissionResolved { kind, .. } => {
            let mut to = state.clone();
            let epoch = anchor_epoch(state);
            if let SessionState::Running { requires_action, .. } = &mut to {
                match kind {
                    crate::event::PermissionKind::Tool => {
                        requires_action.waiting_on_approval = requires_action.waiting_on_approval.saturating_sub(1);
                    }
                    crate::event::PermissionKind::Auth => {
                        requires_action.waiting_on_auth = requires_action.waiting_on_auth.saturating_sub(1);
                    }
                }
            }
            settle(state.clone(), to, epoch)
        }

        // Ask/AskResolved: the structured-question twin of Permission/Resolved,
        // on its OWN counter (asking is not authorizing — 2026-08-04 ruling).
        // Mechanically identical: saturating ±1, payload never read (§R9).
        SessionEvent::Ask { .. } => {
            let mut to = state.clone();
            let epoch = anchor_epoch(state);
            if let SessionState::Running { requires_action, .. } = &mut to {
                requires_action.waiting_on_question = requires_action.waiting_on_question.saturating_add(1);
            }
            settle(state.clone(), to, epoch)
        }
        SessionEvent::AskResolved { .. } => {
            let mut to = state.clone();
            let epoch = anchor_epoch(state);
            if let SessionState::Running { requires_action, .. } = &mut to {
                requires_action.waiting_on_question = requires_action.waiting_on_question.saturating_sub(1);
            }
            settle(state.clone(), to, epoch)
        }

        // Opaque escape hatch (I13): count or ignore only; never inspect
        // tag/payload. P0 = ignore (no state change, no Transition).
        SessionEvent::AdapterSpecific { .. } => (state.clone(), Vec::new()),

        // ==================================================================
        // ADDITIVE no-op arms (007 §6a / §9.0). These variants are pure
        // consumer / bracket / orchestration signals — the reducer's decision
        // logic is UNCHANGED; each takes an explicit no-op arm (the mechanical Rust
        // exhaustiveness requirement, NOT a behavior change). The ONE additive
        // variant the reducer READS is SubagentUpdate (§6b b1, handled in P0b).
        // ==================================================================
        SessionEvent::PromptAccepted { .. }
        | SessionEvent::UsageDelta { .. }
        // LC-8a: Plan is a to-do SNAPSHOT — content within a Running turn, NOT a
        // state (cross-protocol verified). The reducer never reads it; only the
        // conversation layer projects it to the UI panel. Pure no-op here.
        | SessionEvent::Plan { .. }
        | SessionEvent::Provisioning { .. }
        | SessionEvent::Rewound { .. }
        | SessionEvent::ConfigChanged { .. }
        // Async catalog discovery (claude initialize / codex model/list response).
        // Pure UI surface — the conversation projects it to the model/mode picker;
        // the FSM never reads a catalog. No-op here (like ConfigChanged).
        | SessionEvent::CatalogUpdated { .. }
        | SessionEvent::ItemStarted { .. }
        | SessionEvent::ItemCompleted { .. }
        // Live tool-output / turn-diff streams: pure display liveness within a
        // Running turn (a tool streaming output / a diff updating is just progress).
        // The FSM never reads them; only the conversation projects them to live panes.
        | SessionEvent::ToolOutputDelta { .. }
        | SessionEvent::TurnDiffUpdated { .. }
        // An out-of-turn advisory (codex warning/deprecation/config). Not a turn
        // signal, not requires-action — the conversation surfaces it; FSM no-op.
        | SessionEvent::Notice { .. }
        | SessionEvent::MessageFinalized(..)
        | SessionEvent::Snapshot { .. }
        | SessionEvent::Lagged { .. }
        | SessionEvent::CheckpointList { .. }
        // SessionInfo is a read-only query reply (context budget / cost) — never a
        // turn signal. The conversation projects it; FSM no-op.
        | SessionEvent::SessionInfo { .. }
        // SessionTitle is an agent-generated conversation title — applied by the
        // conversation layer under the name_source guard. Never a turn signal.
        | SessionEvent::SessionTitle { .. }
        // 009 R6b: SubagentDetail is the rich BACKGROUND-plane roster fill — read
        // ONLY by the orchestrator's workflow_roster, never by the FSM. No-op here.
        | SessionEvent::SubagentDetail { .. }
        // WorkflowPhase is the container's declared phase list — display grouping
        // only, same background plane as SubagentDetail. Never a turn signal.
        | SessionEvent::WorkflowPhase { .. }
        // Addendum 9: BackendBound is a pure pass-through for the conversation
        // (persist backend_session_id). The reducer NEVER touches SessionState for
        // it — it is not a turn signal, not a requires-action, not a roster update.
        | SessionEvent::BackendBound { .. }
        // BackendTurnBound is BackendBound's turn-scoped sibling (fork anchor
        // pass-through for message stamping). Same contract: reducer no-op.
        | SessionEvent::BackendTurnBound { .. }
        // 009 R6: BackendSuspended is FSM-invisible (the wake re-spawns on the
        // same event_tx). The orchestrator clears the roster on it; the reducer
        // does NOT move SessionState (suspend ≠ a turn boundary).
        | SessionEvent::BackendSuspended => (state.clone(), Vec::new()),

        // ⭐ SubagentUpdate: the ONE §6b b1 reducer READ. Upsert into
        // Running.subagents by `ref` (last-write-wins). This is the SOLE
        // non-no-op additive arm — it adds a subagent-visibility capability
        // dimension shared by ALL backends (NOT a per-backend branch), so claim
        // B ("per-backend reducer 0-change") holds. Does NOT emit a Transition
        // (roster changes don't move external phase / can_send); does NOT touch
        // unlock. Only meaningful while Running (a subagent update outside a turn
        // is dropped — terminal states are absorbed by I10 above, Starting by the
        // guard). I14 prune of terminal entries is the orchestrator's job.
        SessionEvent::SubagentUpdate {
            r#ref,
            label,
            status,
            parent_ref,
            // Container kind is a pump-side (suppression-roster) concern; the
            // reducer's display roster tracks every subagent regardless of kind.
            kind: _,
        } => {
            let mut to = state.clone();
            if let SessionState::Running { subagents, .. } = &mut to {
                let next = crate::state::SubagentState {
                    r#ref,
                    label,
                    status,
                    parent_ref,
                };
                match subagents.iter_mut().find(|s| s.r#ref == next.r#ref) {
                    // §11.4 terminal absorption (feature 009): once a subagent
                    // has reached a terminal status, a late/out-of-order
                    // non-terminal update (e.g. a lagged `progress` arriving
                    // after `Completed`) must NOT resurrect it back to active.
                    // Real ordering proven reachable (parent progress can arrive
                    // after child terminal). Any other transition is last-write-wins.
                    Some(slot) if slot.status.is_terminal() && !next.status.is_terminal() => {}
                    Some(slot) => *slot = next,   // last-write-wins
                    None => subagents.push(next), // first sighting
                }
            }
            // roster mutation only — external phase unchanged → no Transition.
            (to, Vec::new())
        }
    }
}

/// Crash discriminator (C6/D3/I6): pure fn, 2 inputs. `None` exit ⇒ treat as
/// terminal exit. Only consulted while a turn is non-terminal (Running);
/// once terminal the `Detached` is absorbed by I10.
pub fn crash_outcome(terminal_result_seen: bool, exit: Option<ExitStatusLite>) -> Outcome {
    // A result already decided the turn → follow it (the Detached is absorbed).
    if terminal_result_seen {
        return Outcome::FollowResult;
    }
    // No terminal result this turn. F46: the exit payload distinguishes a CLEAN
    // early exit (code 0, no signal) from an abnormal crash (signal, non-zero
    // code, or unknown). Conflating them fed the wrong ErrorReason — and thus the
    // wrong recovery disposition — to the control plane. The reducer previously
    // discarded `exit` entirely (`let _ = exit`).
    match exit {
        // Clean exit-0, no result → EmptyTurn-class (CleanNoResult), not Crashed.
        Some(ExitStatusLite {
            code: Some(0),
            signal: None,
        }) => Outcome::CleanNoResult,
        // Signal, non-zero code, or unknown (None) → abnormal → Crashed.
        _ => Outcome::Crashed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- generators (§7.4): build canonical event sequences ----
    fn running(epoch: u64) -> SessionState {
        SessionState::Running {
            since_epoch: epoch,
            saw_substantive_output: false,
            terminal_result_seen: false,
            requires_action: RequiresActionSet::default(),
            subagents: Vec::new(),
        }
    }

    /// Drive a sequence of events from Idle, returning the final state.
    fn drive(events: Vec<SessionEvent>) -> SessionState {
        let mut s = SessionState::Idle;
        for ev in events {
            let (next, _) = step(&s, ev);
            s = next;
        }
        s
    }

    /// Builds a TurnResult for the CURRENT turn (epoch u64::MAX ⇒ never older
    /// than any Running.since_epoch, so it always settles — the pre-epoch-guard
    /// behavior these tests assume). The cross-turn-stale case is exercised
    /// explicitly with a low epoch in the dedicated guard tests.
    fn turn_result(is_error: bool, status: Option<u16>, text: &str) -> SessionEvent {
        SessionEvent::TurnResult {
            is_error,
            api_error_status: status,
            result_text: text.to_string(),
            epoch: u64::MAX,
            outcome: crate::event::TurnOutcome::default(), // additive (007 §C2); reducer ignores it
        }
    }

    // ---- equivalence classes (§7.3) ----

    #[test]
    fn turn_started_enters_running() {
        let (s, t) = step(&SessionState::Idle, SessionEvent::TurnStarted { epoch: 1 });
        assert!(matches!(s, SessionState::Running { since_epoch: 1, .. }));
        assert_eq!(t.len(), 1, "Idle→Running emits a Transition");
    }

    #[test]
    fn happy_turn_with_text_goes_idle() {
        let s = drive(vec![
            SessionEvent::TurnStarted { epoch: 1 },
            SessionEvent::MessageDelta {
                item_id: "a".into(),
                text: "hi".into(),
            },
            turn_result(false, None, "hi"),
        ]);
        assert_eq!(s, SessionState::Idle);
    }

    #[test]
    fn tool_only_turn_is_success_not_empty() {
        // no final text, only a completed tool — substantive (C5/I5).
        let s = drive(vec![
            SessionEvent::TurnStarted { epoch: 1 },
            SessionEvent::ToolCall {
                tool_use_id: "t1".into(),
                name: "Write".into(),
                subagent: crate::event::SubagentKind::Inline,
                input: serde_json::Value::Null,
                parent_tool_use_id: None,
            },
            SessionEvent::ToolResult {
                tool_use_id: "t1".into(),
                is_error: false,
                content: vec![],
                parent_tool_use_id: None,
            },
            turn_result(false, None, ""),
        ]);
        assert_eq!(s, SessionState::Idle, "tool-only turn must be Idle, not EmptyTurn");
    }

    #[test]
    fn empty_turn_thinking_only_is_error() {
        let s = drive(vec![
            SessionEvent::TurnStarted { epoch: 1 },
            SessionEvent::ThoughtDelta {
                item_id: "a".into(),
                text: "hmm".into(),
            },
            turn_result(false, None, ""),
        ]);
        assert_eq!(
            s,
            SessionState::Error {
                reason: ErrorReason::EmptyTurn
            }
        );
    }

    #[test]
    fn refusal_with_empty_result_folds_idle_not_empty_turn() {
        // 009 R1f: a refusal is is_error:false with empty result text — exactly
        // the shape the OUTPUT-PRESENCE gate would mislabel EmptyTurn. But a
        // refusal is a clean turn completion (the model declined), so the user
        // must be able to send again: fold Idle (can_send recoverable), NOT
        // Error{EmptyTurn}. The Refused marker rides outcome (007 §C2/O3).
        let refused = SessionEvent::TurnResult {
            is_error: false,
            api_error_status: None,
            result_text: String::new(),
            epoch: 0,
            outcome: crate::event::TurnOutcome::Completed {
                stop_reason: crate::event::StopReason::Refused { category: None },
            },
        };
        let s = drive(vec![SessionEvent::TurnStarted { epoch: 1 }, refused]);
        assert_eq!(
            s,
            SessionState::Idle,
            "a refusal (empty result, is_error:false) folds Idle so can_send recovers — NOT Error{{EmptyTurn}}"
        );
    }

    #[test]
    fn genuine_empty_turn_still_errors_when_outcome_is_not_refused() {
        // Guard the R1f exemption is narrow: a TRULY empty turn (default outcome,
        // no Refused marker) still folds to Error{EmptyTurn}. Only the Refused
        // marker is exempt — not all empty results.
        let s = drive(vec![
            SessionEvent::TurnStarted { epoch: 1 },
            turn_result(false, None, ""),
        ]);
        assert_eq!(
            s,
            SessionState::Error {
                reason: ErrorReason::EmptyTurn
            },
            "empty turn without a Refused marker is still EmptyTurn"
        );
    }

    #[test]
    fn backend_error_routes_by_is_error_not_subtype() {
        // 400 / 401: is_error:true, result non-empty. MUST be Backend, not Idle.
        for status in [400u16, 401] {
            let s = drive(vec![
                SessionEvent::TurnStarted { epoch: 1 },
                SessionEvent::MessageDelta {
                    item_id: "a".into(),
                    text: "API Error".into(),
                },
                turn_result(true, Some(status), "API Error ..."),
            ]);
            match s {
                SessionState::Error {
                    reason: ErrorReason::Backend { api_error_status, .. },
                } => {
                    assert_eq!(api_error_status, Some(status));
                }
                other => panic!("expected Backend{{{status}}}, got {other:?}"),
            }
        }
    }

    #[test]
    fn heartbeat_does_not_change_state() {
        let before = running(1);
        let (after, t) = step(&before, SessionEvent::Heartbeat);
        assert_eq!(before, after);
        assert!(t.is_empty());
    }

    #[test]
    fn detached_without_result_is_crashed() {
        let (s, _) = step(
            &running(1),
            SessionEvent::Detached {
                exit: Some(ExitStatusLite {
                    code: Some(1),
                    signal: None,
                }),
                redacted_summary: None,
            },
        );
        assert_eq!(
            s,
            SessionState::Error {
                reason: ErrorReason::Crashed
            }
        );
    }

    #[test]
    fn detached_none_exit_treated_as_terminal_crash() {
        // None = exited-status-unknown, MUST be terminal exit not "still running".
        let (s, _) = step(
            &running(1),
            SessionEvent::Detached {
                exit: None,
                redacted_summary: None,
            },
        );
        assert_eq!(
            s,
            SessionState::Error {
                reason: ErrorReason::Crashed
            }
        );
    }

    #[test]
    fn detached_after_result_is_absorbed_not_crashed() {
        // result first (→Idle), then a late Detached must be absorbed (I10).
        let mut s = running(1);
        let (next, _) = step(&s, turn_result(false, None, "done"));
        s = next;
        assert_eq!(s, SessionState::Idle);
        let (s2, t) = step(
            &s,
            SessionEvent::Detached {
                exit: Some(ExitStatusLite {
                    code: Some(0),
                    signal: None,
                }),
                redacted_summary: None,
            },
        );
        assert_eq!(s2, SessionState::Idle, "late Detached absorbed, NOT re-Crashed");
        assert!(t.is_empty());
    }

    // ---- RequiresAction ref-count (I7) ----

    #[test]
    fn permission_enters_requires_action_and_resolve_returns() {
        let (s1, t1) = step(
            &running(1),
            SessionEvent::Permission {
                request_id: "r1".into(),
                kind: crate::event::PermissionKind::Tool,
                metadata: None,
                tool_name: None,
                input: None,
            },
        );
        assert!(crate::state::is_requires_action(&s1));
        assert_eq!(t1.len(), 1, "0→1 crosses zero, emits Transition");

        let (s2, t2) = step(
            &s1,
            SessionEvent::PermissionResolved {
                request_id: "r1".into(),
                kind: crate::event::PermissionKind::Tool,
            },
        );
        assert!(!crate::state::is_requires_action(&s2), "back to plain Running");
        assert_eq!(t2.len(), 1, "1→0 crosses zero, emits Transition");
    }

    #[test]
    fn resolving_one_of_two_does_not_unlock() {
        let mut s = running(1);
        for r in ["r1", "r2"] {
            let (n, _) = step(
                &s,
                SessionEvent::Permission {
                    request_id: r.into(),
                    kind: crate::event::PermissionKind::Tool,
                    metadata: None,
                    tool_name: None,
                    input: None,
                },
            );
            s = n;
        }
        let (s2, t) = step(
            &s,
            SessionEvent::PermissionResolved {
                request_id: "r1".into(),
                kind: crate::event::PermissionKind::Tool,
            },
        );
        assert!(
            crate::state::is_requires_action(&s2),
            "count 2→1 stays in requires-action"
        );
        assert!(t.is_empty(), "no zero-crossing → no Transition");
    }

    /// race-audit ss-8: a STRAY / DUPLICATE `PermissionResolved` (an orphan from a
    /// reconnect, a hook-vs-user double-answer, an external resolver racing the
    /// user) must not underflow the ref-count. `saturating_sub` floors it at 0:
    /// the counter stays 0, the state stays plain Running (NOT requires-action),
    /// and NO spurious Transition is emitted (no false zero-crossing → no bogus
    /// deadline-scope flip / RA-badge toggle). A bare `-1` would wrap to u32::MAX
    /// and wedge the session in requires-action forever (can_send stuck false).
    #[test]
    fn stray_or_duplicate_resolve_floors_at_zero_no_underflow() {
        // (a) Resolve against a plain Running (counter already 0): orphan / late /
        //     reconnect Resolved with no matching Permission outstanding.
        let (s, t) = step(
            &running(1),
            SessionEvent::PermissionResolved {
                request_id: "orphan".into(),
                kind: crate::event::PermissionKind::Tool,
            },
        );
        assert!(
            !crate::state::is_requires_action(&s),
            "stray Resolved must NOT enter requires-action (no underflow to u32::MAX)"
        );
        if let SessionState::Running { requires_action, .. } = &s {
            assert_eq!(
                requires_action.waiting_on_approval, 0,
                "counter floors at 0, not wrapped"
            );
            assert_eq!(requires_action.waiting_on_auth, 0);
        } else {
            panic!("must stay Running, got {s:?}");
        }
        assert!(t.is_empty(), "no zero-crossing → no spurious Transition");

        // (b) DOUBLE Resolved after a single Permission (over-resolve): 0→1 (RA),
        //     1→0 (back to Running, the real crossing), then the SECOND Resolved
        //     must floor at 0 — not re-cross and not wrap.
        let (s1, _) = step(
            &running(1),
            SessionEvent::Permission {
                request_id: "r1".into(),
                kind: crate::event::PermissionKind::Tool,
                metadata: None,
                tool_name: None,
                input: None,
            },
        );
        let (s2, _) = step(
            &s1,
            SessionEvent::PermissionResolved {
                request_id: "r1".into(),
                kind: crate::event::PermissionKind::Tool,
            },
        );
        assert!(
            !crate::state::is_requires_action(&s2),
            "first resolve returns to Running"
        );
        let (s3, t3) = step(
            &s2,
            SessionEvent::PermissionResolved {
                request_id: "r1".into(), // duplicate answer for the same request
                kind: crate::event::PermissionKind::Tool,
            },
        );
        assert!(
            !crate::state::is_requires_action(&s3),
            "duplicate Resolved stays out of requires-action"
        );
        if let SessionState::Running { requires_action, .. } = &s3 {
            assert_eq!(
                requires_action.waiting_on_approval, 0,
                "stays floored at 0 on the duplicate"
            );
        } else {
            panic!("must stay Running, got {s3:?}");
        }
        assert!(
            t3.is_empty(),
            "duplicate Resolved is not a zero-crossing → no Transition"
        );

        // (c) The floor is per-kind: a stray Auth Resolved does not corrupt the Tool
        //     counter (or vice versa).
        let (s4, t4) = step(
            &running(1),
            SessionEvent::PermissionResolved {
                request_id: "orphan-auth".into(),
                kind: crate::event::PermissionKind::Auth,
            },
        );
        if let SessionState::Running { requires_action, .. } = &s4 {
            assert_eq!(requires_action.waiting_on_auth, 0, "auth counter floors at 0");
            assert_eq!(requires_action.waiting_on_approval, 0, "tool counter untouched");
        } else {
            panic!("must stay Running, got {s4:?}");
        }
        assert!(t4.is_empty());
    }

    // ---- terminal-absorbing law (I10) + epoch guard (I3) ----

    #[test]
    fn terminal_absorbs_late_result() {
        let s = SessionState::Error {
            reason: ErrorReason::EmptyTurn,
        };
        let (s2, t) = step(&s, turn_result(false, None, "late"));
        assert_eq!(s2, s, "Error absorbs a same-turn late result");
        assert!(t.is_empty());
    }

    #[test]
    fn new_turn_started_supersedes_terminal() {
        let s = SessionState::Error {
            reason: ErrorReason::Crashed,
        };
        let (s2, _) = step(&s, SessionEvent::TurnStarted { epoch: 2 });
        assert!(matches!(s2, SessionState::Running { since_epoch: 2, .. }));
    }

    /// A Running carrying NON-default per-turn memory, so that the epoch guard's
    /// effect is OBSERVABLE: the guard preserves the carry (returns state.clone),
    /// whereas any guard-less / mis-compared rebuild resets it to all-defaults.
    fn running_with_carry(epoch: u64) -> SessionState {
        SessionState::Running {
            since_epoch: epoch,
            saw_substantive_output: true, // non-default: a rebuild would reset to false
            terminal_result_seen: false,
            requires_action: RequiresActionSet {
                waiting_on_approval: 1, // non-default: a rebuild would reset to 0
                waiting_on_auth: 0,
                waiting_on_question: 0,
            },
            subagents: Vec::new(),
        }
    }

    #[test]
    fn stale_turn_started_is_dropped_while_running() {
        // epoch guard (I3): a TurnStarted NOT advancing past since_epoch is
        // dropped — the Running carry is PRESERVED unchanged. The fixture carries
        // non-default memory so deleting the guard (rebuild → all-defaults) makes
        // this assertion fail. Mutation teeth (§7.6 #11): guard-delete, `<=`→`<`,
        // and `<=`→`==` all flip a sub-case below.
        let s = running_with_carry(5);

        // (a) duplicate epoch (==): guard drops it, carry preserved.
        //     Kills guard-delete AND `<=`→`<` (which would NOT drop epoch==5 →
        //     rebuild resets saw_substantive_output to false / count to 0).
        let (s2, t) = step(&s, SessionEvent::TurnStarted { epoch: 5 });
        assert_eq!(s, s2, "duplicate epoch dropped, carry preserved");
        assert!(t.is_empty());

        // (b) strictly-older epoch (<): guard drops it too, carry preserved.
        //     Kills `<=`→`==` (which would NOT drop epoch=3 since 3!=5 → rebuild
        //     resets the carry).
        let (s3, t3) = step(&s, SessionEvent::TurnStarted { epoch: 3 });
        assert_eq!(s, s3, "older epoch dropped, carry preserved");
        assert!(t3.is_empty());

        // (c) advancing epoch: NOT dropped — starts a fresh turn (carry reset is
        //     correct here, and since_epoch advances).
        let (s4, _t4) = step(&s, SessionEvent::TurnStarted { epoch: 6 });
        match s4 {
            SessionState::Running {
                since_epoch,
                saw_substantive_output,
                requires_action,
                ..
            } => {
                assert_eq!(since_epoch, 6, "advancing epoch starts a fresh turn");
                assert!(!saw_substantive_output, "fresh turn resets the accumulator");
                assert_eq!(requires_action.waiting_on_approval, 0, "fresh turn resets RA");
            }
            other => panic!("expected Running{{6}}, got {other:?}"),
        }
    }

    // ---- illegal classes (§7.3 illegal-1 / illegal-2): never panic ----

    #[test]
    fn adapter_specific_is_ignored() {
        let (s, t) = step(
            &running(1),
            SessionEvent::AdapterSpecific {
                tag: "frobnicate".into(),
                payload: serde_json::json!({"x": 1}),
            },
        );
        assert_eq!(s, running(1));
        assert!(t.is_empty());
    }

    #[test]
    fn backend_bound_is_a_reducer_noop() {
        // Addendum 9: BackendBound is a pure conversation-side pass-through. It must
        // NOT touch SessionState or emit a Transition — assert on a NON-default
        // Running (carry preserved) so a mispredicate would flip an assertion.
        let mut carry = running(7);
        if let SessionState::Running {
            saw_substantive_output,
            requires_action,
            ..
        } = &mut carry
        {
            *saw_substantive_output = true;
            requires_action.waiting_on_approval = 2;
        }
        for ev in [
            SessionEvent::BackendBound {
                backend_session_id: Some("th-xyz".into()),
            },
            SessionEvent::BackendBound {
                backend_session_id: None,
            },
        ] {
            let (s, t) = step(&carry, ev);
            assert_eq!(s, carry, "BackendBound leaves SessionState untouched");
            assert!(t.is_empty(), "BackendBound emits no Transition");
        }
    }

    /// race/scenario audit: EVERY additive no-op signal (the merged arm at
    /// reducer.rs §6a) must leave `SessionState` byte-for-byte unchanged AND emit
    /// no `Transition`. The proptest only guarantees no-panic + determinism — it
    /// does NOT assert no-op, so a bug routing one of these to a state-mutating
    /// arm would slip through. This pins the no-op semantics for every consumer /
    /// bracket / orchestration variant (same model as `backend_bound_is_a_reducer_noop`).
    /// EXCLUDES: SubagentUpdate (the ONE READ arm), BackendBound + AdapterSpecific
    /// (already have dedicated tests). Fed from a NON-DEFAULT Running carry so a
    /// mis-route flips an assertion.
    #[test]
    fn additive_signals_are_reducer_noops() {
        use crate::event::{
            CheckpointEntry, FinalizedMessage, ItemKind, ProvisioningPhase, TruncationInfo, TruncationKind,
        };
        let mut carry = running(7);
        if let SessionState::Running {
            saw_substantive_output,
            requires_action,
            ..
        } = &mut carry
        {
            *saw_substantive_output = true;
            requires_action.waiting_on_approval = 2;
        }

        let events = vec![
            SessionEvent::PromptAccepted {
                client_msg_id: "m".into(),
            },
            SessionEvent::UsageDelta {
                input_tokens: 1,
                output_tokens: 1,
                total_tokens: 2,
                cost_usd: Some(0.5),
                context_window: None,
                breakdown: Default::default(),
            },
            // use the payload-carrying phase (stronger than the proptest's nullary ToolsReady)
            SessionEvent::Provisioning {
                phase: ProvisioningPhase::LoadFailed { reason: "x".into() },
            },
            SessionEvent::Rewound { to_turn: 3 },
            SessionEvent::ConfigChanged {
                mode: Some("plan".into()),
                model: Some("m".into()),
            },
            SessionEvent::ToolOutputDelta {
                item_id: "call_0".into(),
                text: "line\n".into(),
            },
            SessionEvent::TurnDiffUpdated {
                diff: "diff --git a/x b/x".into(),
            },
            SessionEvent::Notice {
                level: crate::event::NoticeLevel::Warning,
                message: "advisory".into(),
                localized: None,
                supersedes_key: None,
            },
            SessionEvent::ItemStarted {
                item_id: "i".into(),
                kind: ItemKind::Tool,
            },
            SessionEvent::ItemCompleted {
                item_id: "i".into(),
                truncation: Some(TruncationInfo {
                    kind: TruncationKind::MaxTokens,
                    partial_text: Some("p".into()),
                }),
            },
            SessionEvent::MessageFinalized(FinalizedMessage {
                item_id: "i".into(),
                kind: ItemKind::Text,
                content: "c".into(),
                truncation: None,
                seq: 1,
            }),
            SessionEvent::Snapshot {
                state_repr: "Idle".into(),
                turn_gen: 2,
            },
            SessionEvent::Lagged { skipped: 4 },
            // non-empty entries: pin that the Vec content does not matter
            SessionEvent::CheckpointList {
                entries: vec![CheckpointEntry {
                    id: "cp1".into(),
                    label: Some("first".into()),
                    turn_gen: Some(1),
                }],
            },
            SessionEvent::SessionInfo {
                context_usage: None,
                cost_text: Some("Total cost: $0".into()),
            },
            // SubagentDetail: the rich background-plane fill (all fields None-able);
            // reducer no-op (read only by the orchestrator's workflow_roster).
            SessionEvent::SubagentDetail {
                r#ref: "s".into(),
                parent_ref: None,
                label: None,
                loop_state: None,
                model: None,
                tokens: None,
                tool_calls: None,
                last_tool_name: None,
                phase_index: None,
                phase_title: None,
                last_tool_summary: None,
                duration_ms: None,
            },
            SessionEvent::BackendSuspended,
        ];

        for ev in events {
            let label = format!("{ev:?}");
            let (s, t) = step(&carry, ev);
            assert_eq!(s, carry, "{label} must leave SessionState unchanged");
            assert!(t.is_empty(), "{label} must emit no Transition");
        }
    }

    /// Every SessionEvent EXCEPT TurnStarted, for constructing the terminal-absorb
    /// enumeration. Keep in lockstep with the SessionEvent enum (the count assert in
    /// `terminal_states_absorb_every_event_except_turn_started` is the tripwire).
    fn all_events_except_turn_started() -> Vec<SessionEvent> {
        use crate::event::{
            CheckpointEntry, ExitStatusLite, FinalizedMessage, ItemKind, PermissionKind, ProvisioningPhase,
            SubagentKind, SubagentStatus, ToolResultContent,
        };
        vec![
            SessionEvent::Cancel,
            SessionEvent::Heartbeat,
            SessionEvent::MessageDelta {
                item_id: "i".into(),
                text: "t".into(),
            },
            SessionEvent::ThoughtDelta {
                item_id: "i".into(),
                text: "t".into(),
            },
            SessionEvent::ToolCall {
                tool_use_id: "t".into(),
                name: "Bash".into(),
                subagent: SubagentKind::Inline,
                input: serde_json::Value::Null,
                parent_tool_use_id: None,
            },
            SessionEvent::ToolResult {
                tool_use_id: "t".into(),
                is_error: false,
                content: vec![ToolResultContent::Text("out".into())],
                parent_tool_use_id: None,
            },
            turn_result(false, None, "done"),
            turn_result(true, Some(401), "boom"),
            SessionEvent::Detached {
                exit: Some(ExitStatusLite {
                    code: Some(0),
                    signal: None,
                }),
                redacted_summary: None,
            },
            SessionEvent::AdapterSpecific {
                tag: "x".into(),
                payload: serde_json::json!({}),
            },
            SessionEvent::Plan {
                entries: vec![],
                explanation: None,
            },
            SessionEvent::Permission {
                request_id: "r".into(),
                kind: PermissionKind::Tool,
                metadata: None,
                tool_name: None,
                input: None,
            },
            SessionEvent::PermissionResolved {
                request_id: "r".into(),
                kind: PermissionKind::Tool,
            },
            SessionEvent::PromptAccepted {
                client_msg_id: "m".into(),
            },
            SessionEvent::UsageDelta {
                input_tokens: 1,
                output_tokens: 1,
                total_tokens: 2,
                cost_usd: None,
                context_window: None,
                breakdown: Default::default(),
            },
            SessionEvent::Provisioning {
                phase: ProvisioningPhase::ToolsWaiting,
            },
            SessionEvent::Rewound { to_turn: 3 },
            SessionEvent::ConfigChanged {
                mode: Some("plan".into()),
                model: None,
            },
            subagent_update("a1", SubagentStatus::Running, None),
            SessionEvent::SubagentDetail {
                r#ref: "s".into(),
                parent_ref: None,
                label: None,
                loop_state: None,
                model: None,
                tokens: None,
                tool_calls: None,
                last_tool_name: None,
                phase_index: None,
                phase_title: None,
                last_tool_summary: None,
                duration_ms: None,
            },
            SessionEvent::ToolOutputDelta {
                item_id: "call_0".into(),
                text: "line\n".into(),
            },
            SessionEvent::TurnDiffUpdated {
                diff: "diff --git a/x b/x".into(),
            },
            SessionEvent::Notice {
                level: crate::event::NoticeLevel::Warning,
                message: "advisory".into(),
                localized: None,
                supersedes_key: None,
            },
            SessionEvent::ItemStarted {
                item_id: "i".into(),
                kind: ItemKind::Tool,
            },
            SessionEvent::ItemCompleted {
                item_id: "i".into(),
                truncation: None,
            },
            SessionEvent::MessageFinalized(FinalizedMessage {
                item_id: "i".into(),
                kind: ItemKind::Text,
                content: "c".into(),
                truncation: None,
                seq: 1,
            }),
            SessionEvent::CheckpointList {
                entries: vec![CheckpointEntry {
                    id: "cp".into(),
                    label: None,
                    turn_gen: Some(1),
                }],
            },
            SessionEvent::SessionInfo {
                context_usage: None,
                cost_text: Some("Total cost: $0".into()),
            },
            SessionEvent::Snapshot {
                state_repr: "Idle".into(),
                turn_gen: 2,
            },
            SessionEvent::Lagged { skipped: 1 },
            SessionEvent::BackendBound {
                backend_session_id: Some("b".into()),
            },
            SessionEvent::BackendSuspended,
        ]
    }

    /// ENUMERATION INVARIANT (terminal-absorbing law I10). Once Idle or Error, the FSM
    /// must IGNORE every event except TurnStarted — same state out, no Transition. The
    /// existing tests spot-checked a handful (late TurnResult, late Detached, late
    /// Cancel); this drives the WHOLE event set × every terminal state, so a future
    /// arm that mishandles e.g. `Idle + Permission` (entering requires-action off a
    /// settled turn) trips here instead of shipping. TurnStarted is the ONE documented
    /// exception (it supersedes the terminal → a new turn) and is asserted separately.
    #[test]
    fn terminal_states_absorb_every_event_except_turn_started() {
        let events = all_events_except_turn_started();
        // 32 SessionEvent variants total; minus TurnStarted = 31, but turn_result appears
        // twice (success + error) → 32 rows. A new variant grows this and trips the assert.
        assert_eq!(
            events.len(),
            32,
            "every non-TurnStarted SessionEvent variant must be in the absorb table (+ the TurnResult ok/err split)"
        );

        let terminals = [
            SessionState::Idle,
            SessionState::Error {
                reason: ErrorReason::Crashed,
            },
            SessionState::Error {
                reason: ErrorReason::EmptyTurn,
            },
            SessionState::Error {
                reason: ErrorReason::Backend {
                    api_error_status: Some(401),
                    message: "auth".into(),
                },
            },
        ];

        for terminal in &terminals {
            for ev in all_events_except_turn_started() {
                let label = format!("{terminal:?} + {ev:?}");
                let (s, t) = step(terminal, ev);
                assert_eq!(
                    &s, terminal,
                    "[{label}] terminal must absorb the event (state unchanged)"
                );
                assert!(t.is_empty(), "[{label}] terminal must emit no Transition");
            }
            // TurnStarted is the exception: it supersedes the terminal into a fresh turn.
            let (s, t) = step(terminal, SessionEvent::TurnStarted { epoch: 99 });
            assert!(
                matches!(s, SessionState::Running { .. }),
                "[{terminal:?} + TurnStarted] supersedes terminal → Running (not absorbed)"
            );
            assert!(!t.is_empty(), "TurnStarted out of a terminal emits a Transition");
        }
        let _ = events;
    }

    #[test]
    fn result_before_any_assistant_does_not_panic() {
        // illegal-2: result with no prior substantive event → EmptyTurn, no panic.
        let s = drive(vec![
            SessionEvent::TurnStarted { epoch: 1 },
            turn_result(false, None, ""),
        ]);
        assert_eq!(
            s,
            SessionState::Error {
                reason: ErrorReason::EmptyTurn
            }
        );
    }

    // ---- gap-closers (cargo-mutants L2): epoch stamping, empty-delta,
    // Starting-state, transition direction ----

    #[test]
    fn transition_stamps_running_epoch() {
        // anchor_epoch must return the Running since_epoch, and TurnResult's
        // Transition must carry it (kills anchor_epoch->0/1 and delete-arm).
        let (_s, t) = step(&running(7), turn_result(false, None, "ok"));
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].epoch, 7, "Transition.epoch must equal Running.since_epoch");
        assert_eq!(t[0].to, SessionState::Idle);
        assert_eq!(t[0].from, running(7));
    }

    #[test]
    fn empty_message_delta_does_not_set_substantive() {
        // kills `delete !` in MessageDelta: an EMPTY text must NOT count as output,
        // so a following empty result must be EmptyTurn (not Idle).
        let s = drive(vec![
            SessionEvent::TurnStarted { epoch: 1 },
            SessionEvent::MessageDelta {
                item_id: "a".into(),
                text: String::new(),
            },
            turn_result(false, None, ""),
        ]);
        assert_eq!(
            s,
            SessionState::Error {
                reason: ErrorReason::EmptyTurn
            },
            "empty MessageDelta is not substantive output"
        );
    }

    #[test]
    fn nonempty_message_delta_then_empty_result_is_idle() {
        // complement: a non-empty delta DOES make an empty-result turn succeed.
        let s = drive(vec![
            SessionEvent::TurnStarted { epoch: 1 },
            SessionEvent::MessageDelta {
                item_id: "a".into(),
                text: "x".into(),
            },
            turn_result(false, None, ""),
        ]);
        assert_eq!(s, SessionState::Idle);
    }

    #[test]
    fn realtime_end_turn_folds_idle_then_lagged_result_is_absorbed() {
        // 009 R5 end-to-end: in --include-partial-messages mode the adapter maps
        // message_delta{end_turn} → a real-time TurnResult (empty text), folding
        // Idle the moment the reply finishes — saw_substantive_output was set by
        // the preceding delta so OUTPUT-PRESENCE folds Idle, not EmptyTurn. The
        // LATER lagged `result` frame (also a TurnResult) must be harmlessly
        // absorbed by I10 (already terminal), NOT re-fold or error.
        let s = drive(vec![
            SessionEvent::TurnStarted { epoch: 1 },
            SessionEvent::MessageDelta {
                item_id: "a".into(),
                text: "the answer".into(),
            },
            // real-time end_turn (adapter's parse_stream_event output): empty text.
            SessionEvent::TurnResult {
                is_error: false,
                api_error_status: None,
                result_text: String::new(),
                epoch: 0,
                outcome: crate::event::TurnOutcome::Completed {
                    stop_reason: crate::event::StopReason::EndTurn,
                },
            },
        ]);
        assert_eq!(
            s,
            SessionState::Idle,
            "real-time end_turn folds Idle (saw output → not EmptyTurn)"
        );

        // The lagged result lands later — I10 absorbs it, stays Idle.
        let (s2, t2) = step(&s, turn_result(false, None, "the answer"));
        assert_eq!(s2, SessionState::Idle, "lagged result absorbed by I10, no re-fold");
        assert!(t2.is_empty(), "absorbed terminal emits no Transition");
    }

    #[test]
    fn startup_window_detached_is_crashed_not_swallowed() {
        // S19/R18 (T21 tooth): a process that exits while still Starting (0-frame
        // startup crash) MUST resolve to Error{Crashed} and emit a Transition so
        // the UI unlocks immediately — NOT be swallowed by the Starting guard
        // (which would hang to the idle-Timeout). Covers exit≠0, exit==0, and
        // unknown (None) — all abnormal pre-result exits.
        for exit in [
            Some(ExitStatusLite {
                code: Some(1),
                signal: None,
            }),
            Some(ExitStatusLite {
                code: Some(0),
                signal: None,
            }),
            None,
        ] {
            let (s, t) = step(
                &SessionState::Starting,
                SessionEvent::Detached {
                    exit,
                    redacted_summary: None,
                },
            );
            assert_eq!(
                s,
                SessionState::Error {
                    reason: ErrorReason::Crashed
                },
                "Starting + Detached({exit:?}) must be Error{{Crashed}}"
            );
            assert_eq!(t.len(), 1, "Starting→Error emits a Transition (immediate unlock)");
        }
    }

    #[test]
    fn error_result_while_starting_routes_to_backend_not_dropped() {
        // R16/3.9: a bad --resume fails on the first frame with
        // result{is_error:true} while still Starting. It MUST become
        // Error{Backend{message}} (so the crash-resume self-heal can match the
        // cause), NOT be dropped by the Starting guard.
        let (s, t) = step(
            &SessionState::Starting,
            turn_result(true, None, "No conversation found with session ID: abc"),
        );
        match s {
            SessionState::Error {
                reason: ErrorReason::Backend { message, .. },
            } => assert!(message.contains("No conversation found")),
            other => panic!("expected Error{{Backend}}, got {other:?}"),
        }
        assert_eq!(
            t.len(),
            1,
            "Starting→Error emits a Transition (unlock + self-heal trigger)"
        );
    }

    #[test]
    fn success_result_while_starting_is_still_ignored() {
        // A non-error result while Starting is defensive-ignored (shouldn't
        // happen — a real turn goes Starting→Running first).
        let (s, t) = step(&SessionState::Starting, turn_result(false, None, "x"));
        assert_eq!(s, SessionState::Starting);
        assert!(t.is_empty());
    }

    #[test]
    fn events_in_starting_are_ignored_except_turn_started() {
        // kills `Starting guard -> false`: a data event while Starting must be a
        // no-op (NOT fall through into the Running arms, which would mishandle it).
        let (s, t) = step(
            &SessionState::Starting,
            SessionEvent::MessageDelta {
                item_id: "a".into(),
                text: "hi".into(),
            },
        );
        assert_eq!(s, SessionState::Starting);
        assert!(t.is_empty());
        // and a heartbeat / result while Starting is also ignored
        let (s2, _) = step(&SessionState::Starting, turn_result(false, None, "x"));
        assert_eq!(s2, SessionState::Starting);
    }

    #[test]
    fn requires_action_transition_only_on_zero_crossing() {
        // kills `> with ==` in external_phase: going 1->2 must NOT emit a
        // transition (still RequiresAction), and the phase must be RequiresAction
        // for any count>0 (not just ==0).
        let (s1, _) = step(
            &running(1),
            SessionEvent::Permission {
                request_id: "a".into(),
                kind: crate::event::PermissionKind::Tool,
                metadata: None,
                tool_name: None,
                input: None,
            },
        );
        let (s2, t2) = step(
            &s1,
            SessionEvent::Permission {
                request_id: "b".into(),
                kind: crate::event::PermissionKind::Tool,
                metadata: None,
                tool_name: None,
                input: None,
            },
        );
        assert!(crate::state::is_requires_action(&s2));
        assert!(t2.is_empty(), "1->2 stays RequiresAction, no Transition");
    }

    // ---- crash_outcome pure fn (I6) ----

    #[test]
    fn crash_outcome_arms() {
        assert_eq!(
            crash_outcome(
                false,
                Some(ExitStatusLite {
                    code: Some(1),
                    signal: None
                })
            ),
            Outcome::Crashed
        );
        assert_eq!(
            crash_outcome(
                true,
                Some(ExitStatusLite {
                    code: Some(0),
                    signal: None
                })
            ),
            Outcome::FollowResult
        );
        assert_eq!(crash_outcome(false, None), Outcome::Crashed);
        // F46 distinction (the WHOLE point of this pure fn): a clean exit-0 with
        // NO result seen is CleanNoResult, NOT Crashed. Without this assertion a
        // `<`→`_` / arm-reorder mutation collapsing CleanNoResult into Crashed
        // would feed the wrong recovery disposition to the control plane and pass
        // every other test (race-audit red-09).
        assert_eq!(
            crash_outcome(
                false,
                Some(ExitStatusLite {
                    code: Some(0),
                    signal: None
                })
            ),
            Outcome::CleanNoResult,
            "clean exit-0 with no result is an EMPTY turn, not a crash"
        );
        // A signal is abnormal → Crashed, never CleanNoResult.
        assert_eq!(
            crash_outcome(
                false,
                Some(ExitStatusLite {
                    code: None,
                    signal: Some(9)
                })
            ),
            Outcome::Crashed,
            "a killed process (signal) is Crashed, not CleanNoResult"
        );
    }

    #[test]
    fn detached_clean_exit_zero_while_running_is_empty_turn_not_crashed() {
        // The WIRED counterpart to crash_outcome_arms' CleanNoResult case: a
        // process that exits cleanly (code:0, no signal) while STILL Running with
        // no terminal result seen folds to Error{EmptyTurn} — a distinct recovery
        // disposition from a SIGKILL crash. `running(1)` carries
        // terminal_result_seen:false, so this exercises the CleanNoResult arm of
        // the Detached match, not the I10 post-terminal absorption (race-audit
        // red-09; pairs with detached_without_result_is_crashed for the signal arm).
        let (s, t) = step(
            &running(1),
            SessionEvent::Detached {
                exit: Some(ExitStatusLite {
                    code: Some(0),
                    signal: None,
                }),
                redacted_summary: None,
            },
        );
        assert_eq!(
            s,
            SessionState::Error {
                reason: ErrorReason::EmptyTurn
            },
            "clean exit-0 while Running (no result) → Error{{EmptyTurn}}, NOT Error{{Crashed}}"
        );
        assert_eq!(t.len(), 1, "the terminal transition is emitted (unlocks can_send)");
    }

    // ---- Feature 004 S14/R14: user cancel folds to Idle (NOT Crashed) ----

    #[test]
    fn cancel_in_running_folds_to_idle() {
        // T15: Running + Cancel → Idle terminal (behavior-preserving), emits a
        // Transition so the UI unlocks immediately (not after an idle-timeout).
        let (s, t) = step(&running(1), SessionEvent::Cancel);
        assert_eq!(s, SessionState::Idle, "cancel folds to Idle, never Error");
        assert_eq!(
            t.len(),
            1,
            "Running→Idle emits exactly one Transition (immediate unlock)"
        );
        assert_eq!(t[0].to, SessionState::Idle);
    }

    #[test]
    fn cancel_during_startup_also_resolves_to_idle() {
        // A Stop clicked while still Starting (process not yet up) must resolve
        // too — the Cancel arm sits before the Starting guard.
        let (s, _) = step(&SessionState::Starting, SessionEvent::Cancel);
        assert_eq!(s, SessionState::Idle);
    }

    #[test]
    fn cancel_then_interrupt_error_result_is_absorbed_not_backend_error() {
        // F1 interrupt-on-cancel: cancel() writes a CLI interrupt, then folds
        // Running→Idle. claude's interrupt terminal is a
        // `result{subtype:"error_during_execution",is_error:true}` — the SAME
        // shape a bad-resume produces. It arrives AFTER the Cancel (Idle) and
        // MUST be absorbed by I10, NOT become Error{Backend} — otherwise the
        // resume-failure self-heal (which matches "error_during_execution") would
        // misfire and wrongly clear a healthy session id on every user cancel.
        let s = drive(vec![
            SessionEvent::TurnStarted { epoch: 1 },
            SessionEvent::Cancel, // → Idle
            SessionEvent::TurnResult {
                is_error: true,
                api_error_status: None,
                result_text: "error_during_execution".into(),
                epoch: 1, // the cancelled turn's own epoch; absorbed by I10 while Idle
                outcome: crate::event::TurnOutcome::default(),
            },
        ]);
        assert_eq!(
            s,
            SessionState::Idle,
            "the interrupt's error_during_execution terminal is absorbed (Idle), NEVER Error{{Backend}} — \
             so a user cancel never misfires the resume-failure self-heal"
        );
    }

    #[test]
    fn stale_turn_result_dropped_while_running_new_turn() {
        // PRODUCTION RACE (conv 0afe571b): user cancels turn 1, then RESENDS
        // ~2.2s later (turn 2). claude flushes the INTERRUPT's trailing
        // `result{is_error,error_during_execution}` ~2.6s after the interrupt —
        // i.e. AFTER turn 2 is already Running. That stale (turn-1) result must
        // be DROPPED, not settle turn 2 as Error (which surfaced as a bogus
        // UNKNOWN_UPSTREAM_ERROR and misfired the resume self-heal). The epoch
        // guard (epoch < since_epoch) makes this impossible by construction —
        // the 500ms interrupt-drain barrier could not (claude took 2.6s).
        let s = drive(vec![
            SessionEvent::TurnStarted { epoch: 1 }, // turn 1
            SessionEvent::Cancel,                   // → Idle (cancel folds first)
            SessionEvent::TurnStarted { epoch: 2 }, // turn 2 (the resend) → Running{since_epoch:2}
            // turn 1's late trailing interrupt result, arriving during turn 2:
            SessionEvent::TurnResult {
                is_error: true,
                api_error_status: None,
                result_text: "error_during_execution".into(),
                epoch: 1, // STALE: belongs to the cancelled turn
                outcome: crate::event::TurnOutcome::default(),
            },
        ]);
        assert!(
            matches!(s, SessionState::Running { since_epoch: 2, .. }),
            "a stale (turn-1) trailing result must be DROPPED while Running turn 2, \
             leaving turn 2 still Running — got {s:?}"
        );
    }

    #[test]
    fn current_turn_result_with_equal_epoch_settles() {
        // The guard uses `<` not `<=`: the CURRENT turn's own result carries
        // epoch == since_epoch and MUST settle (a `<`→`<=` mutation would wrongly
        // drop every real result).
        let s = drive(vec![
            SessionEvent::TurnStarted { epoch: 5 },
            SessionEvent::MessageDelta {
                item_id: "m1".into(),
                text: "hi".into(),
            },
            SessionEvent::TurnResult {
                is_error: false,
                api_error_status: None,
                result_text: "done".into(),
                epoch: 5, // current turn
                outcome: crate::event::TurnOutcome::default(),
            },
        ]);
        assert_eq!(
            s,
            SessionState::Idle,
            "the current turn's own result (epoch==since_epoch) settles to Idle"
        );
    }

    #[test]
    fn older_epoch_error_result_dropped_preserves_running() {
        // A strictly-older error result is dropped and the Running carry
        // (saw_substantive_output) is preserved — no transition emitted.
        let s = drive(vec![
            SessionEvent::TurnStarted { epoch: 5 },
            SessionEvent::ToolResult {
                tool_use_id: "t1".into(),
                is_error: false,
                content: vec![],
                parent_tool_use_id: None,
            }, // substantive output seen
            SessionEvent::TurnResult {
                is_error: true,
                api_error_status: Some(500),
                result_text: "error_during_execution".into(),
                epoch: 3, // stale
                outcome: crate::event::TurnOutcome::default(),
            },
        ]);
        assert!(
            matches!(
                s,
                SessionState::Running {
                    since_epoch: 5,
                    saw_substantive_output: true,
                    ..
                }
            ),
            "stale error result dropped; Running carry preserved — got {s:?}"
        );
    }

    #[test]
    fn cancel_then_kill_detached_is_absorbed_not_crashed() {
        // T15 tooth② (crash≠cancel): after Cancel→Idle, the manager kills the process;
        // the resulting Detached arrives while ALREADY Idle and is absorbed by
        // I10 — it MUST NOT be reclassified as Error{Crashed}.
        let s = drive(vec![
            SessionEvent::TurnStarted { epoch: 1 },
            SessionEvent::Cancel,
            SessionEvent::Detached {
                exit: Some(ExitStatusLite {
                    code: None,
                    signal: Some(9), // SIGKILL from the cancel-kill
                }),
                redacted_summary: None,
            },
        ]);
        assert_eq!(
            s,
            SessionState::Idle,
            "cancel-kill Detached is absorbed (Idle), never mislabeled Error{{Crashed}}"
        );
    }

    #[test]
    fn cancel_is_idempotent_when_already_terminal() {
        // A Cancel after the turn already ended (Idle) is absorbed by I10.
        let (s, t) = step(&SessionState::Idle, SessionEvent::Cancel);
        assert_eq!(s, SessionState::Idle);
        assert!(t.is_empty(), "no phase change → no Transition");
    }

    // ======================================================================
    // 007-P0b: the TWO intentional reducer changes (§6b / §C3.3 verification).
    // ======================================================================
    use crate::event::{PermissionKind, SubagentStatus};
    use crate::state::{can_send_message, is_requires_action};

    fn subagent_update(r: &str, status: SubagentStatus, parent: Option<&str>) -> SessionEvent {
        SessionEvent::SubagentUpdate {
            r#ref: r.into(),
            label: None,
            status,
            parent_ref: parent.map(Into::into),
            kind: None,
        }
    }

    fn subagents_of(s: &SessionState) -> Vec<crate::state::SubagentState> {
        match s {
            SessionState::Running { subagents, .. } => subagents.clone(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn subagent_update_inserts_then_upserts_by_ref() {
        // §6b b1 / V2a: first sighting inserts; second sighting for the same ref
        // is last-write-wins (NOT a duplicate). No Transition (roster ≠ phase).
        let (s1, t1) = step(&running(1), subagent_update("a1", SubagentStatus::Running, None));
        assert_eq!(subagents_of(&s1).len(), 1);
        assert!(t1.is_empty(), "roster change emits no Transition");

        let (s2, _) = step(&s1, subagent_update("a1", SubagentStatus::Completed, None));
        let roster = subagents_of(&s2);
        assert_eq!(roster.len(), 1, "same ref upserts, does not duplicate");
        assert_eq!(roster[0].status, SubagentStatus::Completed, "last-write-wins");
    }

    // ── Feature 009 R1b / §11.4 terminal absorption ──────────────────────────

    #[test]
    fn subagent_terminal_not_resurrected_by_late_nonterminal() {
        // A subagent reaches Completed; a LAGGED/out-of-order non-terminal
        // update (`Running`) arrives afterward. It must NOT flip the slot back
        // to active — otherwise a finished subagent's spinner re-ignites and
        // has_foreground_activity wrongly reports true. (Real ordering: a
        // parent's `progress` can arrive after a child's terminal.)
        let (s1, _) = step(&running(1), subagent_update("a1", SubagentStatus::Running, None));
        let (s2, _) = step(&s1, subagent_update("a1", SubagentStatus::Completed, None));
        let (s3, t3) = step(&s2, subagent_update("a1", SubagentStatus::Running, None));
        let roster = subagents_of(&s3);
        assert_eq!(roster.len(), 1, "no duplicate");
        assert_eq!(
            roster[0].status,
            SubagentStatus::Completed,
            "terminal absorbed the late non-terminal update (NOT resurrected to Running)"
        );
        assert!(t3.is_empty(), "absorbed update emits no Transition");
    }

    #[test]
    fn subagent_terminal_absorption_holds_for_all_terminal_states() {
        // Every terminal status rejects a subsequent non-terminal update.
        for term in [
            SubagentStatus::Completed,
            SubagentStatus::Errored,
            SubagentStatus::Shutdown,
            SubagentStatus::Interrupted,
        ] {
            let (s1, _) = step(&running(1), subagent_update("a1", term, None));
            for late in [SubagentStatus::PendingInit, SubagentStatus::Running] {
                let (s2, _) = step(&s1, subagent_update("a1", late, None));
                assert_eq!(
                    subagents_of(&s2)[0].status,
                    term,
                    "{term:?} must not be resurrected by a late {late:?}"
                );
            }
        }
    }

    #[test]
    fn subagent_terminal_to_terminal_is_still_last_write_wins() {
        // Absorption only blocks terminal→non-terminal. A terminal→terminal
        // correction (e.g. Completed then a corrected Errored) still applies,
        // so the final outcome is not frozen on the first terminal sighting.
        let (s1, _) = step(&running(1), subagent_update("a1", SubagentStatus::Completed, None));
        let (s2, _) = step(&s1, subagent_update("a1", SubagentStatus::Errored, None));
        assert_eq!(
            subagents_of(&s2)[0].status,
            SubagentStatus::Errored,
            "terminal→terminal still LWW"
        );
    }

    #[test]
    fn subagent_multi_level_parent_ref_preserved() {
        // §9.13: flat Vec + parent_ref edges. A child points at its parent's ref.
        let mut s = running(1);
        for ev in [
            subagent_update("root", SubagentStatus::Running, None),
            subagent_update("child", SubagentStatus::Running, Some("root")),
        ] {
            let (n, _) = step(&s, ev);
            s = n;
        }
        let roster = subagents_of(&s);
        assert_eq!(roster.len(), 2);
        let child = roster.iter().find(|x| x.r#ref == "child").unwrap();
        assert_eq!(child.parent_ref.as_deref(), Some("root"), "multi-level edge preserved");
    }

    #[test]
    fn subagent_update_does_not_change_unlock_or_phase() {
        // §9.12 M-12: subagents NEVER affect can_send / requires-action.
        let (s, _) = step(&running(1), subagent_update("a1", SubagentStatus::Errored, None));
        assert!(!can_send_message(&s), "roster does not unlock");
        assert!(!is_requires_action(&s), "roster does not enter requires-action");
        assert!(matches!(s, SessionState::Running { .. }), "stays plain Running");
    }

    #[test]
    fn subagent_update_outside_running_is_dropped() {
        // Terminal states absorb it (I10); Idle is unchanged.
        let (s, t) = step(&SessionState::Idle, subagent_update("a", SubagentStatus::Running, None));
        assert_eq!(s, SessionState::Idle);
        assert!(t.is_empty());
    }

    #[test]
    fn auth_permission_uses_separate_counter() {
        // §6b b3 / V3a: Permission{kind:Auth} → waiting_on_auth (NOT approval),
        // 0→1 crosses zero → Transition to requires-action.
        let (s, t) = step(
            &running(1),
            SessionEvent::Permission {
                request_id: "auth-1".into(),
                kind: PermissionKind::Auth,
                metadata: None,
                tool_name: None,
                input: None,
            },
        );
        match &s {
            SessionState::Running { requires_action, .. } => {
                assert_eq!(requires_action.waiting_on_auth, 1, "auth counter incremented");
                assert_eq!(requires_action.waiting_on_approval, 0, "approval counter untouched");
            }
            other => panic!("expected Running, got {other:?}"),
        }
        assert!(is_requires_action(&s));
        assert_eq!(t.len(), 1, "0→1 auth crosses zero, emits Transition");
    }

    #[test]
    fn auth_resolve_returns_to_running() {
        // V3b: Auth challenge then resolve → back to plain Running.
        let (s1, _) = step(
            &running(1),
            SessionEvent::Permission {
                request_id: "auth-1".into(),
                kind: PermissionKind::Auth,
                metadata: None,
                tool_name: None,
                input: None,
            },
        );
        let (s2, t2) = step(
            &s1,
            SessionEvent::PermissionResolved {
                request_id: "auth-1".into(),
                kind: PermissionKind::Auth,
            },
        );
        assert!(!is_requires_action(&s2), "auth resolved → plain Running");
        assert_eq!(t2.len(), 1, "1→0 auth crosses zero, emits Transition");
    }

    #[test]
    fn tool_and_auth_counters_are_independent() {
        // V3c: a tool approval AND an auth challenge pending; resolving the auth
        // one does NOT unlock (the tool one still holds requires-action).
        let mut s = running(1);
        for ev in [
            SessionEvent::Permission {
                request_id: "tool-1".into(),
                kind: PermissionKind::Tool,
                metadata: None,
                tool_name: None,
                input: None,
            },
            SessionEvent::Permission {
                request_id: "auth-1".into(),
                kind: PermissionKind::Auth,
                metadata: None,
                tool_name: None,
                input: None,
            },
        ] {
            let (n, _) = step(&s, ev);
            s = n;
        }
        let (s2, t) = step(
            &s,
            SessionEvent::PermissionResolved {
                request_id: "auth-1".into(),
                kind: PermissionKind::Auth,
            },
        );
        match &s2 {
            SessionState::Running { requires_action, .. } => {
                assert_eq!(requires_action.waiting_on_auth, 0, "auth resolved");
                assert_eq!(requires_action.waiting_on_approval, 1, "tool still pending");
            }
            other => panic!("expected Running, got {other:?}"),
        }
        assert!(is_requires_action(&s2), "tool approval still holds requires-action");
        assert!(
            t.is_empty(),
            "auth 1→0 but set not empty (tool still 1) → no phase change"
        );
    }

    #[test]
    fn can_send_independent_of_both_counters_and_roster() {
        // §C3.3 C8d: unlock = Idle only, never affected by auth/approval/subagents.
        let mut s = running(1);
        for ev in [
            subagent_update("a1", SubagentStatus::Running, None),
            SessionEvent::Permission {
                request_id: "auth-1".into(),
                kind: PermissionKind::Auth,
                metadata: None,
                tool_name: None,
                input: None,
            },
        ] {
            let (n, _) = step(&s, ev);
            s = n;
        }
        assert!(!can_send_message(&s), "Running with auth + roster never unlocks");
    }
}

/// 009 R3 / §C.0: machine-checked totality of the pure reducer. `step()` is a
/// total function over the ABSTRACT projection domain — the design repeatedly
/// claims "the 4-variant FSM is exhaustively enumerable", and this turns that
/// claim from prose into a CI invariant. The literal domain is ℵ₀ (RequiresAction
/// counters, unbounded subagents Vec, serde Values), so we sample the abstract
/// classes that the derivations actually read: ExternalPhase × ra-class ×
/// subagent-active × {is_error, epoch-relation, …} crossed with one representative
/// of EVERY SessionEvent variant. Two properties: (P1) step never panics over the
/// full cross-product; (P2) step is deterministic (same input → identical output),
/// which would catch a non-deterministic or hidden-state regression. A guarded
/// `_ if` arm that silently swallows an event it should handle is caught by the
/// equivalence-class tests above; this pins that no input combination panics.
#[cfg(test)]
mod proptest_totality {
    use super::*;
    use crate::event::{
        ExitStatusLite, FinalizedMessage, ItemKind, PermissionKind, ProvisioningPhase, StopReason, SubagentStatus,
        TurnOutcome,
    };
    use crate::state::{ErrorReason, RequiresActionSet, SessionState, SubagentState};
    use proptest::prelude::*;

    /// Representative states spanning every abstract projection class.
    fn any_state() -> impl Strategy<Value = SessionState> {
        prop_oneof![
            Just(SessionState::Idle),
            Just(SessionState::Starting),
            // Running × ra-class {0,1,2} × auth {0,1} × subagent {none, active, terminal}.
            (0u32..3, 0u32..2, 0u64..3, prop::option::of(any_substatus())).prop_map(|(appr, auth, epoch, sub)| {
                SessionState::Running {
                    since_epoch: epoch,
                    saw_substantive_output: epoch % 2 == 0,
                    terminal_result_seen: false,
                    requires_action: RequiresActionSet {
                        waiting_on_approval: appr,
                        waiting_on_auth: auth,
                        waiting_on_question: 0,
                    },
                    subagents: sub
                        .map(|st| {
                            vec![SubagentState {
                                r#ref: "s".into(),
                                label: None,
                                status: st,
                                parent_ref: None,
                            }]
                        })
                        .unwrap_or_default(),
                }
            }),
            Just(SessionState::Error {
                reason: ErrorReason::Crashed
            }),
            Just(SessionState::Error {
                reason: ErrorReason::EmptyTurn
            }),
            Just(SessionState::Error {
                reason: ErrorReason::Backend {
                    api_error_status: Some(400),
                    message: "e".into()
                }
            }),
        ]
    }

    fn any_substatus() -> impl Strategy<Value = SubagentStatus> {
        prop_oneof![
            Just(SubagentStatus::PendingInit),
            Just(SubagentStatus::Running),
            Just(SubagentStatus::Interrupted),
            Just(SubagentStatus::Completed),
            Just(SubagentStatus::Errored),
            Just(SubagentStatus::Shutdown),
        ]
    }

    /// One representative per EVERY SessionEvent variant (bounded payloads — the
    /// reducer's behavior depends on the abstract shape, not string contents).
    fn any_event() -> impl Strategy<Value = SessionEvent> {
        prop_oneof![
            (0u64..3).prop_map(|e| SessionEvent::TurnStarted { epoch: e }),
            Just(SessionEvent::Cancel),
            Just(SessionEvent::MessageDelta {
                item_id: "i".into(),
                text: "t".into()
            }),
            Just(SessionEvent::ThoughtDelta {
                item_id: "i".into(),
                text: "t".into()
            }),
            Just(SessionEvent::ToolCall {
                tool_use_id: "tu".into(),
                name: "n".into(),
                subagent: Default::default(),
                input: serde_json::Value::Null,
                parent_tool_use_id: None,
            }),
            prop_oneof![
                Just(SessionEvent::ToolResult {
                    tool_use_id: "tu".into(),
                    is_error: false,
                    content: vec![],
                    parent_tool_use_id: None,
                }),
                Just(SessionEvent::ToolResult {
                    tool_use_id: "tu".into(),
                    is_error: true,
                    content: vec![],
                    parent_tool_use_id: None,
                }),
            ],
            Just(SessionEvent::Heartbeat),
            // TurnResult × is_error × epoch × outcome{EndTurn, Refused, Cancelled}.
            (any::<bool>(), 0u64..3, 0u32..3).prop_map(|(is_error, epoch, oc)| SessionEvent::TurnResult {
                is_error,
                api_error_status: if is_error { Some(400) } else { None },
                result_text: if is_error { "e".into() } else { String::new() },
                epoch,
                outcome: match oc {
                    0 => TurnOutcome::default(),
                    1 => TurnOutcome::Completed {
                        stop_reason: StopReason::Refused { category: None }
                    },
                    _ => TurnOutcome::Completed {
                        stop_reason: StopReason::EndTurn
                    },
                },
            }),
            prop::option::of(Just(ExitStatusLite {
                code: None,
                signal: Some(9)
            }))
            .prop_map(|exit| SessionEvent::Detached {
                exit,
                redacted_summary: None,
            }),
            Just(SessionEvent::AdapterSpecific {
                tag: "x".into(),
                payload: serde_json::Value::Null
            }),
            prop_oneof![Just(PermissionKind::Tool), Just(PermissionKind::Auth)].prop_map(|kind| {
                SessionEvent::Permission {
                    request_id: "r".into(),
                    kind,
                    metadata: None,
                    tool_name: None,
                    input: None,
                }
            }),
            prop_oneof![Just(PermissionKind::Tool), Just(PermissionKind::Auth)].prop_map(|kind| {
                SessionEvent::PermissionResolved {
                    request_id: "r".into(),
                    kind,
                }
            }),
            Just(SessionEvent::PromptAccepted {
                client_msg_id: "m".into()
            }),
            Just(SessionEvent::UsageDelta {
                input_tokens: 1,
                output_tokens: 1,
                total_tokens: 2,
                cost_usd: None,
                context_window: None,
                breakdown: Default::default(),
            }),
            Just(SessionEvent::Provisioning {
                phase: ProvisioningPhase::ToolsReady
            }),
            (0u64..3).prop_map(|t| SessionEvent::Rewound { to_turn: t }),
            Just(SessionEvent::ConfigChanged {
                mode: Some("plan".into()),
                model: None
            }),
            any_substatus().prop_map(|status| SessionEvent::SubagentUpdate {
                r#ref: "s".into(),
                label: None,
                status,
                parent_ref: None,
                kind: None,
            }),
            Just(SessionEvent::ItemStarted {
                item_id: "i".into(),
                kind: ItemKind::Text
            }),
            Just(SessionEvent::ItemCompleted {
                item_id: "i".into(),
                truncation: None
            }),
            Just(SessionEvent::MessageFinalized(FinalizedMessage {
                item_id: "i".into(),
                kind: ItemKind::Text,
                content: "c".into(),
                truncation: None,
                seq: 0,
            })),
            (0u64..3).prop_map(|g| SessionEvent::Snapshot {
                state_repr: "Idle".into(),
                turn_gen: g
            }),
            (0u64..5).prop_map(|n| SessionEvent::Lagged { skipped: n }),
            Just(SessionEvent::CheckpointList { entries: Vec::new() }),
            Just(SessionEvent::BackendBound {
                backend_session_id: Some("b".into())
            }),
            // audit: these two were ABSENT from the generator → not even the
            // no-panic/determinism sweep touched them. Add so totality is complete.
            Just(SessionEvent::BackendSuspended),
            Just(SessionEvent::SubagentDetail {
                r#ref: "s".into(),
                parent_ref: None,
                label: None,
                loop_state: None,
                model: None,
                tokens: None,
                tool_calls: None,
                last_tool_name: None,
                phase_index: None,
                phase_title: None,
                last_tool_summary: None,
                duration_ms: None,
            }),
            // newer additive no-op signals — keep the no-panic/determinism sweep
            // touching them too.
            Just(SessionEvent::ToolOutputDelta {
                item_id: "call_0".into(),
                text: "line\n".into(),
            }),
            Just(SessionEvent::TurnDiffUpdated {
                diff: "diff --git a/x b/x".into(),
            }),
            Just(SessionEvent::Notice {
                level: crate::event::NoticeLevel::Warning,
                message: "advisory".into(),
                localized: None,
                supersedes_key: None,
            }),
            Just(SessionEvent::SessionInfo {
                context_usage: None,
                cost_text: Some("Total cost: $0".into()),
            }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        /// P1: step never panics over the abstract state × event cross-product.
        #[test]
        fn step_never_panics(state in any_state(), event in any_event()) {
            let _ = step(&state, event);
        }

        /// P2: step is deterministic — folding the same (state, event) twice
        /// yields byte-identical (state, transitions). Catches hidden mutable
        /// state or non-determinism sneaking into the "pure" reducer.
        #[test]
        fn step_is_deterministic(state in any_state(), event in any_event()) {
            let (s1, t1) = step(&state, event.clone());
            let (s2, t2) = step(&state, event);
            prop_assert_eq!(s1, s2);
            prop_assert_eq!(t1, t2);
        }
    }
}
