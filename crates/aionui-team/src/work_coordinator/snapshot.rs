use std::collections::{BTreeSet, HashSet};
use std::sync::MutexGuard;

use super::coordinator::{CoordinatorState, SlotWorkCoordinator};
use super::model::{RunWorkSummary, RuntimeConstraint, SlotPhase, SlotWorkSnapshot, WorkIntentState};

impl SlotWorkCoordinator {
    pub(super) fn run_summaries_locked(
        state: &CoordinatorState,
        run_ids: impl IntoIterator<Item = String>,
    ) -> Vec<RunWorkSummary> {
        run_ids
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|run_id| Self::run_summary_locked(state, &run_id))
            .collect()
    }

    pub(super) fn publish_run_summaries(&self, summaries: Vec<RunWorkSummary>) {
        for summary in summaries {
            self.run_causality.apply_work_summary(summary);
        }
    }

    pub(super) fn all_run_ids(state: &CoordinatorState) -> BTreeSet<String> {
        state
            .intents
            .values()
            .filter_map(|intent| intent.team_run_id.clone())
            .chain(
                state
                    .enqueue_leases
                    .values()
                    .filter_map(|record| record.lease.team_run_id.clone()),
            )
            .collect()
    }

    pub(super) fn run_summary_locked(state: &CoordinatorState, team_run_id: &str) -> RunWorkSummary {
        let associated = state
            .intents
            .values()
            .filter(|intent| intent.team_run_id.as_deref() == Some(team_run_id))
            .collect::<Vec<_>>();
        let queued_intent_count = associated
            .iter()
            .filter(|intent| matches!(intent.state, WorkIntentState::Queued))
            .count();
        let starting_batch_count = associated
            .iter()
            .filter_map(|intent| match &intent.state {
                WorkIntentState::Starting { batch_id, .. } => Some(batch_id),
                _ => None,
            })
            .collect::<HashSet<_>>()
            .len();
        let running_batch_count = associated
            .iter()
            .filter_map(|intent| match &intent.state {
                WorkIntentState::Running { batch_id, .. } => Some(batch_id),
                _ => None,
            })
            .collect::<HashSet<_>>()
            .len();
        let paused_intent_count = associated
            .iter()
            .filter(|intent| {
                matches!(intent.state, WorkIntentState::Queued)
                    && state.slots.get(&intent.slot_id).is_some_and(|slot| slot.paused)
            })
            .count();
        let failed_intent_count = associated
            .iter()
            .filter(|intent| matches!(intent.state, WorkIntentState::Failed { .. }))
            .count();
        let slot_ids = associated
            .iter()
            .map(|intent| intent.slot_id.as_str())
            .chain(
                state
                    .enqueue_leases
                    .values()
                    .filter(|record| record.lease.team_run_id.as_deref() == Some(team_run_id))
                    .map(|record| record.lease.slot_id.as_str()),
            )
            .collect::<BTreeSet<_>>();
        RunWorkSummary {
            team_run_id: team_run_id.to_owned(),
            queued_intent_count,
            starting_batch_count,
            running_batch_count,
            active_enqueue_lease_count: state
                .enqueue_leases
                .values()
                .filter(|record| record.lease.team_run_id.as_deref() == Some(team_run_id))
                .count(),
            paused_intent_count,
            failed_intent_count,
            slots: slot_ids
                .into_iter()
                .filter_map(|slot_id| Self::slot_snapshot_locked(state, slot_id))
                .collect(),
        }
    }

    pub(super) fn slot_snapshot_locked(state: &CoordinatorState, slot_id: &str) -> Option<SlotWorkSnapshot> {
        let slot = state.slots.get(slot_id)?;
        let active_batch = slot.active.as_ref().map(|active| active.batch.clone());
        let active_turn_id = slot.active.as_ref().and_then(|active| active.turn_id.clone());
        let active_turn_started_at_ms = slot.active.as_ref().and_then(|active| active.started_at_ms);
        let state_phase = if slot.paused {
            SlotPhase::Paused
        } else if active_turn_id.is_some() {
            SlotPhase::Running
        } else if active_batch.is_some() {
            SlotPhase::Starting
        } else if !matches!(slot.runtime_constraint, RuntimeConstraint::Ready) {
            SlotPhase::Blocked
        } else if slot.queued_ids().next().is_some() {
            SlotPhase::Queued
        } else {
            SlotPhase::Idle
        };
        let team_run_id = active_batch
            .as_ref()
            .and_then(|batch| batch.team_run_ids.first().cloned())
            .or_else(|| {
                slot.queued_ids().find_map(|intent_id| {
                    state
                        .intents
                        .get(intent_id)
                        .and_then(|intent| intent.team_run_id.clone())
                })
            });
        Some(SlotWorkSnapshot {
            slot_id: slot_id.to_owned(),
            role: slot.role.clone(),
            state: state_phase,
            queued_foreground_count: slot.foreground.len(),
            queued_background_count: slot.control.len() + slot.background.len(),
            active_batch,
            active_turn_id,
            active_turn_started_at_ms,
            runtime_constraint: slot.runtime_constraint.clone(),
            team_run_id,
        })
    }

    pub(super) fn lock_state(&self) -> MutexGuard<'_, CoordinatorState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
