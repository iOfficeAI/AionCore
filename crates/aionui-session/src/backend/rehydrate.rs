//! 007 §C4 / §7.3: cold-start `SessionState` rebuild from a `Tier2Checkpoint`.
//!
//! ⭐ COLLAPSED per conversation C §17 (user-decided 2026-06-11 "1A"): the
//! persisted Tier-2 state is a SINGLE column (`conversations.backend_session_id`)
//! plus `turn_gen` for epoch continuity. Every FSM mid-turn substate
//! (outstanding permissions, last outcome, subagent roster, process-attached,
//! workflow disk-tail) is turn-INTERNAL transient and is NOT persisted — a crash
//! loses it (accepted). Therefore cold recovery is ALWAYS `Idle`: an existing
//! session reopens at a clean turn boundary, the caller resumes the backend via
//! `SessionSpec::Resume { backend_session_id }`, and the user re-drives any
//! in-flight work. The resume anchor + epoch do NOT change the rebuilt phase.
//!
//! This is the COLD-START function (backend not yet up, nothing emitted). HOT
//! rewind is a different path: `Command::Rewind` → backend mutates history →
//! `SessionEvent::Rewound{to_turn}` → the orchestrator drops post-`to_turn`
//! display blocks and (since the FSM is Idle-only at boundaries) leaves the live
//! FSM at Idle (§9.9). No turn-outcome read-back is needed — rewind is a
//! user-initiated, idle-only operation, and a rewound session is Idle.

use super::types::Tier2Checkpoint;
use crate::state::SessionState;

/// Rebuild `SessionState` from a checkpoint. Pure: no I/O, no clock, no deltas.
///
/// Rule (§7.3, collapsed C §17): cold start is `Idle`. The checkpoint carries the
/// resume anchor + epoch for CONTINUITY (the caller threads `backend_session_id`
/// into `SessionSpec::Resume` and the adapter resumes at `turn_gen + 1`), not for
/// phase reconstruction. A crash that happened mid-turn (pending permission,
/// in-flight tool) recovers to `Idle` — the transient substate is gone (accepted);
/// the user re-issues the action.
pub fn rehydrate(_ck: &Tier2Checkpoint) -> SessionState {
    SessionState::Idle
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ck(backend_session_id: Option<&str>, turn_gen: u64) -> Tier2Checkpoint {
        Tier2Checkpoint {
            session_id: "s1".into(),
            backend_session_id: backend_session_id.map(str::to_string),
            turn_gen,
        }
    }

    #[test]
    fn fresh_checkpoint_is_idle() {
        // No prior backend session, epoch 0 → a never-run session rebuilds Idle.
        assert_eq!(rehydrate(&ck(None, 0)), SessionState::Idle);
    }

    #[test]
    fn resumable_checkpoint_is_idle_anchor_does_not_change_phase() {
        // A checkpoint with a live resume anchor + advanced epoch STILL rebuilds
        // Idle (collapsed C §17): the anchor is for SessionSpec::Resume, not for
        // phase. The mid-turn substate that the old rich checkpoint restored
        // (pending-permission badge, roster) is intentionally NOT recovered.
        assert_eq!(rehydrate(&ck(Some("thread-abc"), 7)), SessionState::Idle);
    }
}
