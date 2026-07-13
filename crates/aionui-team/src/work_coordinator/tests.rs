use std::sync::{Arc, Mutex};

use aionui_api_types::TeamRunTargetRole;

use super::*;
use crate::{TeamError, work_source::WorkSource};

#[derive(Default)]
struct RecordingRunCausality {
    summaries: Mutex<Vec<RunWorkSummary>>,
}

impl RunCausalityPort for RecordingRunCausality {
    fn bind_enqueue(&self, request: &EnqueueRequest) -> RunBinding {
        let user_visible = matches!(request.binding, CausalBinding::UserVisible);
        RunBinding {
            team_run_id: user_visible.then(|| "run-1".to_owned()),
            created_new_run: user_visible,
            user_intervention: false,
        }
    }

    fn abort_binding(&self, _binding: &RunBinding) {}

    fn apply_work_summary(&self, summary: RunWorkSummary) {
        self.summaries.lock().unwrap().push(summary);
    }
}

fn coordinator() -> SlotWorkCoordinator {
    SlotWorkCoordinator::new(
        "team-1".into(),
        "generation-1".into(),
        Arc::new(RecordingRunCausality::default()),
    )
}

fn enqueue(coordinator: &SlotWorkCoordinator, source: WorkSource, message_id: &str) {
    let lease = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "lead-1".into(),
            role: TeamRunTargetRole::Lead,
            source,
            binding: CausalBinding::Background,
        })
        .unwrap();
    coordinator.commit_enqueue(&lease, Some(message_id.into())).unwrap();
}

#[test]
fn foreground_precedes_background_and_fifo_is_stable() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::McpSendMessage, "background-1");
    enqueue(&coordinator, WorkSource::UserMessage, "foreground-1");
    enqueue(&coordinator, WorkSource::UserIntervention, "foreground-2");
    enqueue(&coordinator, WorkSource::TeamMembershipChanged, "background-2");

    let ReconcileDecision::Claim(foreground) = coordinator.next("lead-1") else {
        panic!("foreground batch must be claimable");
    };
    assert_eq!(foreground.highest_priority, WorkPriority::Foreground);
    assert_eq!(
        foreground.mailbox_message_ids,
        vec!["foreground-1".to_owned(), "foreground-2".to_owned()]
    );
    assert_eq!(coordinator.complete_batch(&foreground), CommitResult::Committed);

    let ReconcileDecision::Claim(background) = coordinator.next("lead-1") else {
        panic!("background batch must follow foreground");
    };
    assert_eq!(background.highest_priority, WorkPriority::Background);
    assert_eq!(
        background.mailbox_message_ids,
        vec!["background-1".to_owned(), "background-2".to_owned()]
    );
}

#[test]
fn five_enqueues_require_one_reconcile_not_five_signals() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    for index in 1..=5 {
        enqueue(&coordinator, WorkSource::UserMessage, &format!("message-{index}"));
    }

    loop {
        match coordinator.next("lead-1") {
            ReconcileDecision::Claim(batch) => {
                assert_eq!(coordinator.complete_batch(&batch), CommitResult::Committed);
            }
            ReconcileDecision::Quiescent => break,
            other => panic!("unexpected reconcile decision: {other:?}"),
        }
    }

    let snapshot = coordinator.slot_snapshot("lead-1").unwrap();
    assert_eq!(snapshot.queued_foreground_count, 0);
    assert_eq!(snapshot.queued_background_count, 0);
    assert!(snapshot.active_batch.is_none());
    assert_eq!(snapshot.state, SlotPhase::Idle);
}

#[test]
fn messages_consumed_by_one_turn_are_claimed_in_one_batch() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    for message_id in ["m1", "m2", "m3"] {
        enqueue(&coordinator, WorkSource::UserMessage, message_id);
    }
    coordinator.reconcile_mailbox(
        "lead-1",
        &["m1".into(), "m2".into(), "m3".into()],
        TeamRunTargetRole::Lead,
    );

    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("all unread messages must be claimed together");
    };
    assert_eq!(batch.intent_ids.len(), 3);
    assert_eq!(batch.mailbox_message_ids, vec!["m1", "m2", "m3"]);
    assert_eq!(coordinator.next("lead-1"), ReconcileDecision::WaitingForCompletion);
}

#[test]
fn enqueue_during_running_batch_waits_for_next_batch() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "m1");
    let ReconcileDecision::Claim(first) = coordinator.next("lead-1") else {
        panic!("first message must be claimable");
    };
    assert_eq!(coordinator.mark_started(&first, "turn-1"), StartCommitResult::Accepted);

    enqueue(&coordinator, WorkSource::UserMessage, "m2");
    let active = coordinator.slot_snapshot("lead-1").unwrap();
    assert_eq!(active.active_batch.unwrap().mailbox_message_ids, vec!["m1"]);

    assert_eq!(coordinator.complete_batch(&first), CommitResult::Committed);
    let ReconcileDecision::Claim(second) = coordinator.next("lead-1") else {
        panic!("second message must follow the active batch");
    };
    assert_eq!(second.mailbox_message_ids, vec!["m2"]);
}

#[test]
fn retryable_start_returns_the_same_intents_to_the_queue() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "m1");
    let ReconcileDecision::Claim(first) = coordinator.next("lead-1") else {
        panic!("message must be claimable");
    };

    assert_eq!(
        coordinator.retry_start(&first, "already_running"),
        CommitResult::Committed
    );
    let queued = coordinator.intents_for_slot("lead-1");
    assert_eq!(queued[0].intent_id, first.intent_ids[0]);
    assert_eq!(queued[0].state, WorkIntentState::Queued);

    let ReconcileDecision::Claim(second) = coordinator.next("lead-1") else {
        panic!("retried message must be claimable");
    };
    assert_eq!(second.intent_ids, first.intent_ids);
    assert!(second.operation_id > first.operation_id);
}

#[test]
fn foreground_message_resumes_paused_slot() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::McpSendMessage, "background");
    coordinator.pause_slot("lead-1");
    enqueue(&coordinator, WorkSource::UserIntervention, "intervention");

    let ReconcileDecision::Claim(first) = coordinator.next("lead-1") else {
        panic!("foreground intervention must resume the paused slot");
    };
    assert_eq!(first.mailbox_message_ids, vec!["intervention"]);
    assert_eq!(coordinator.complete_batch(&first), CommitResult::Committed);

    let ReconcileDecision::Claim(second) = coordinator.next("lead-1") else {
        panic!("work retained across pause must remain queued");
    };
    assert_eq!(second.mailbox_message_ids, vec!["background"]);
}

#[test]
fn cancelled_batch_rejects_a_late_start_by_cancelling_it_immediately() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "m1");
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("message must be claimable");
    };

    assert_eq!(
        coordinator.cancel_batch(&batch, "run_cancelled"),
        CommitResult::Committed
    );
    assert_eq!(
        coordinator.mark_started(&batch, "turn-late"),
        StartCommitResult::CancelImmediately
    );
}

#[test]
fn runtime_starting_blocks_and_runtime_ready_releases_work() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Starting { operation_id: 7 });
    enqueue(&coordinator, WorkSource::UserMessage, "m1");

    assert_eq!(
        coordinator.next("lead-1"),
        ReconcileDecision::Blocked(RuntimeConstraint::Starting { operation_id: 7 })
    );
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("ready runtime must release the queued intent");
    };
    assert_eq!(batch.mailbox_message_ids, vec!["m1"]);
}

#[test]
fn remove_cancels_queued_and_running_work_and_rejects_new_enqueue() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "m1");
    let ReconcileDecision::Claim(first) = coordinator.next("lead-1") else {
        panic!("first message must be claimable");
    };
    assert_eq!(coordinator.mark_started(&first, "turn-1"), StartCommitResult::Accepted);
    enqueue(&coordinator, WorkSource::UserMessage, "m2");

    let removed = coordinator.remove_slot("lead-1");
    assert_eq!(removed.cancel_target.unwrap().batch, first);
    assert_eq!(removed.terminal_message_ids, vec!["m1", "m2"]);
    assert!(coordinator.intents_for_slot("lead-1").iter().all(|intent| {
        intent.state
            == WorkIntentState::Cancelled {
                classification: "slot_removed",
            }
    }));

    let result = coordinator.acquire_enqueue(EnqueueRequest {
        slot_id: "lead-1".into(),
        role: TeamRunTargetRole::Lead,
        source: WorkSource::UserMessage,
        binding: CausalBinding::UserVisible,
    });
    assert!(matches!(result, Err(TeamError::InvalidRequest(message)) if message.contains("removed")));
}

#[test]
fn stale_generation_and_operation_cannot_commit() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "m1");
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("message must be claimable");
    };
    let mut stale_generation = batch.clone();
    stale_generation.session_generation = "generation-2".into();
    let mut stale_operation = batch.clone();
    stale_operation.operation_id += 1;

    assert_eq!(
        coordinator.mark_started(&stale_generation, "turn-stale"),
        StartCommitResult::StaleOwner
    );
    assert_eq!(
        coordinator.mark_started(&stale_operation, "turn-stale"),
        StartCommitResult::StaleOwner
    );
    assert_eq!(coordinator.slot_snapshot("lead-1").unwrap().active_batch, Some(batch));
}

#[test]
fn unread_without_projection_creates_one_recovery_intent() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    coordinator.reconcile_mailbox("lead-1", &["m1".into()], TeamRunTargetRole::Lead);
    coordinator.reconcile_mailbox("lead-1", &["m1".into()], TeamRunTargetRole::Lead);

    let intents = coordinator.intents_for_slot("lead-1");
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].source, WorkSource::RecoveryDrain);
    assert_eq!(intents[0].mailbox_message_id.as_deref(), Some("m1"));
}

#[test]
fn active_batch_prevents_unread_projection_from_being_rebuilt() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    coordinator.reconcile_mailbox("lead-1", &["m1".into()], TeamRunTargetRole::Lead);
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("recovery message must be claimable");
    };
    coordinator.reconcile_mailbox("lead-1", &["m1".into()], TeamRunTargetRole::Lead);

    assert_eq!(coordinator.intents_for_slot("lead-1").len(), 1);
    assert_eq!(coordinator.slot_snapshot("lead-1").unwrap().active_batch, Some(batch));
}

#[test]
fn pause_cancels_running_batch_and_retains_queued_work() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "running");
    let ReconcileDecision::Claim(running) = coordinator.next("lead-1") else {
        panic!("first message must be claimable");
    };
    assert_eq!(
        coordinator.mark_started(&running, "turn-1"),
        StartCommitResult::Accepted
    );
    enqueue(&coordinator, WorkSource::McpSendMessage, "retained");

    let paused = coordinator.pause_slot("lead-1");
    assert_eq!(paused.cancel_target.unwrap().batch, running);
    assert_eq!(
        coordinator.cancel_batch(&running, "slot_paused"),
        CommitResult::Committed
    );
    assert_eq!(coordinator.next("lead-1"), ReconcileDecision::Quiescent);
    assert_eq!(coordinator.slot_snapshot("lead-1").unwrap().queued_background_count, 1);
}

#[test]
fn cancel_run_terminalizes_every_associated_intent_and_lease() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    let first = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "lead-1".into(),
            role: TeamRunTargetRole::Lead,
            source: WorkSource::UserMessage,
            binding: CausalBinding::UserVisible,
        })
        .unwrap();
    coordinator.commit_enqueue(&first, Some("running".into())).unwrap();
    let ReconcileDecision::Claim(running) = coordinator.next("lead-1") else {
        panic!("first message must be claimable");
    };
    let queued = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "lead-1".into(),
            role: TeamRunTargetRole::Lead,
            source: WorkSource::UserIntervention,
            binding: CausalBinding::UserVisible,
        })
        .unwrap();
    coordinator.commit_enqueue(&queued, Some("queued".into())).unwrap();
    let lease = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "lead-1".into(),
            role: TeamRunTargetRole::Lead,
            source: WorkSource::UserIntervention,
            binding: CausalBinding::UserVisible,
        })
        .unwrap();

    let cancelled = coordinator.cancel_run("run-1");
    assert_eq!(cancelled.cancel_targets[0].batch, running);
    assert_eq!(cancelled.terminal_message_ids, vec!["running", "queued"]);
    assert_eq!(cancelled.summary.active_enqueue_lease_count, 0);
    assert_eq!(cancelled.summary.queued_intent_count, 0);
    assert_eq!(
        coordinator.abort_enqueue(&lease, "late_abort"),
        CommitResult::StaleOwner
    );
}

#[test]
fn background_work_continues_after_unrelated_run_completion() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    let user = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "lead-1".into(),
            role: TeamRunTargetRole::Lead,
            source: WorkSource::UserMessage,
            binding: CausalBinding::UserVisible,
        })
        .unwrap();
    coordinator.commit_enqueue(&user, Some("user".into())).unwrap();
    enqueue(&coordinator, WorkSource::McpSendMessage, "background");

    let ReconcileDecision::Claim(user_batch) = coordinator.next("lead-1") else {
        panic!("user work must run first");
    };
    coordinator.complete_batch(&user_batch);
    let ReconcileDecision::Claim(background_batch) = coordinator.next("lead-1") else {
        panic!("background work must remain runnable");
    };
    assert_eq!(background_batch.mailbox_message_ids, vec!["background"]);
    assert!(background_batch.team_run_ids.is_empty());
}

#[test]
fn late_terminal_after_cancel_is_rejected() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    let user = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "lead-1".into(),
            role: TeamRunTargetRole::Lead,
            source: WorkSource::UserMessage,
            binding: CausalBinding::UserVisible,
        })
        .unwrap();
    coordinator.commit_enqueue(&user, Some("m1".into())).unwrap();
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("user work must be claimable");
    };
    coordinator.cancel_run("run-1");

    assert_eq!(coordinator.complete_batch(&batch), CommitResult::StaleOwner);
}
