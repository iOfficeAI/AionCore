use std::collections::HashMap;

use aionui_api_types::TeamRunTargetRole;
use aionui_common::{TimestampMs, now_ms};

use crate::wake::{TeamWakeClass, TeamWakeSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakeGateDecision {
    Record,
    Suppress,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SlotWakeSnapshot {
    pub paused: bool,
    pub suppressed_wake_count: usize,
    pub foreground_pending_count: usize,
    pub background_pending_count: usize,
}

#[derive(Debug, Clone)]
struct PausedSlotState {
    paused: bool,
    #[allow(dead_code)]
    paused_at: TimestampMs,
    role_hint: TeamRunTargetRole,
    #[allow(dead_code)]
    reason: String,
    suppressed_wake_count: usize,
    foreground_pending_count: usize,
    background_pending_count: usize,
    last_suppressed_source: Option<TeamWakeSource>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SlotWakeGate {
    slots: HashMap<String, PausedSlotState>,
}

impl SlotWakeGate {
    pub(crate) fn pause(&mut self, slot_id: &str, role: TeamRunTargetRole, reason: impl Into<String>) {
        let reason = reason.into();
        let now = now_ms();
        let entry = self.slots.entry(slot_id.to_owned()).or_insert_with(|| PausedSlotState {
            paused: true,
            paused_at: now,
            role_hint: role.clone(),
            reason: reason.clone(),
            suppressed_wake_count: 0,
            foreground_pending_count: 0,
            background_pending_count: 0,
            last_suppressed_source: None,
        });
        entry.paused = true;
        entry.role_hint = role;
        entry.paused_at = now;
        entry.reason = reason;
    }

    pub(crate) fn before_wake(
        &mut self,
        slot_id: &str,
        source: TeamWakeSource,
        trigger_message_id: Option<String>,
    ) -> WakeGateDecision {
        if source.resumes_paused_slot() {
            if let Some(entry) = self.slots.get_mut(slot_id) {
                entry.paused = false;
                entry.foreground_pending_count += usize::from(trigger_message_id.is_some());
                entry.reason = "resumed_by_user".into();
            }
            return WakeGateDecision::Record;
        }

        let Some(entry) = self.slots.get_mut(slot_id) else {
            return WakeGateDecision::Record;
        };

        if source.bypasses_pause() {
            return WakeGateDecision::Record;
        }

        match source.class() {
            TeamWakeClass::Background | TeamWakeClass::SystemRecovery => {
                entry.suppressed_wake_count += 1;
                entry.last_suppressed_source = Some(source);
                WakeGateDecision::Suppress
            }
            TeamWakeClass::Foreground | TeamWakeClass::Lifecycle => WakeGateDecision::Record,
        }
    }

    pub(crate) fn snapshot_for_slot(&self, slot_id: &str) -> SlotWakeSnapshot {
        self.slots
            .get(slot_id)
            .map(|entry| SlotWakeSnapshot {
                paused: entry.paused,
                suppressed_wake_count: entry.suppressed_wake_count,
                foreground_pending_count: entry.foreground_pending_count,
                background_pending_count: entry.background_pending_count,
            })
            .unwrap_or_default()
    }

    #[allow(dead_code)]
    pub(crate) fn role_hint(&self, slot_id: &str) -> Option<TeamRunTargetRole> {
        self.slots.get(slot_id).map(|entry| entry.role_hint.clone())
    }

    #[allow(dead_code)]
    pub(crate) fn slot_ids(&self) -> impl Iterator<Item = &String> {
        self.slots.keys()
    }

    pub(crate) fn has_retained_work(&self) -> bool {
        self.slots.values().any(|entry| {
            entry.paused
                || entry.suppressed_wake_count > 0
                || entry.foreground_pending_count > 0
                || entry.background_pending_count > 0
        })
    }

    #[allow(dead_code)]
    pub(crate) fn add_suppressed(&mut self, slot_id: &str, count: usize) {
        if count == 0 {
            return;
        }
        if let Some(entry) = self.slots.get_mut(slot_id) {
            entry.suppressed_wake_count += count;
            entry
                .last_suppressed_source
                .get_or_insert(TeamWakeSource::McpSendMessage);
        }
    }

    pub(crate) fn release_suppressed_if_resumed(&mut self, slot_id: &str) -> Option<TeamWakeSource> {
        let entry = self.slots.get_mut(slot_id)?;
        if entry.paused || entry.suppressed_wake_count == 0 {
            return None;
        }
        let source = entry.last_suppressed_source.unwrap_or(TeamWakeSource::McpSendMessage);
        entry.suppressed_wake_count = 0;
        entry.background_pending_count += 1;
        Some(source)
    }

    #[allow(dead_code)]
    pub(crate) fn clear_foreground_pending(&mut self, slot_id: &str) {
        if let Some(entry) = self.slots.get_mut(slot_id) {
            entry.foreground_pending_count = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aionui_api_types::TeamRunTargetRole;

    use crate::wake::TeamWakeSource;

    #[test]
    fn paused_slot_suppresses_background_but_not_lifecycle() {
        let mut gate = SlotWakeGate::default();
        gate.pause("lead-1", TeamRunTargetRole::Lead, "user_stop");

        let background = gate.before_wake("lead-1", TeamWakeSource::McpSendMessage, None);
        assert_eq!(background, WakeGateDecision::Suppress);
        assert_eq!(gate.snapshot_for_slot("lead-1").suppressed_wake_count, 1);

        let lifecycle = gate.before_wake("lead-1", TeamWakeSource::McpShutdownRequest, None);
        assert_eq!(lifecycle, WakeGateDecision::Record);
        assert!(gate.snapshot_for_slot("lead-1").paused);
    }

    #[test]
    fn user_intervention_resumes_and_records_priority_wake() {
        let mut gate = SlotWakeGate::default();
        gate.pause("worker-1", TeamRunTargetRole::Teammate, "user_stop");
        assert!(gate.snapshot_for_slot("worker-1").paused);

        let decision = gate.before_wake(
            "worker-1",
            TeamWakeSource::UserIntervention,
            Some("msg-user".to_owned()),
        );

        assert_eq!(decision, WakeGateDecision::Record);
        let snapshot = gate.snapshot_for_slot("worker-1");
        assert!(!snapshot.paused);
        assert_eq!(snapshot.foreground_pending_count, 1);
    }

    #[test]
    fn release_suppressed_returns_one_background_drain_when_resumed() {
        let mut gate = SlotWakeGate::default();
        gate.pause("lead-1", TeamRunTargetRole::Lead, "user_stop");
        assert_eq!(
            gate.before_wake("lead-1", TeamWakeSource::InterruptedNotification, None),
            WakeGateDecision::Suppress
        );
        assert_eq!(
            gate.before_wake("lead-1", TeamWakeSource::IdleNotification, None),
            WakeGateDecision::Suppress
        );
        assert_eq!(
            gate.before_wake("lead-1", TeamWakeSource::UserIntervention, Some("msg-user".into())),
            WakeGateDecision::Record
        );

        let released = gate.release_suppressed_if_resumed("lead-1");
        assert_eq!(released, Some(TeamWakeSource::IdleNotification));
        let snapshot = gate.snapshot_for_slot("lead-1");
        assert_eq!(snapshot.suppressed_wake_count, 0);
        assert_eq!(snapshot.background_pending_count, 1);
    }
}
