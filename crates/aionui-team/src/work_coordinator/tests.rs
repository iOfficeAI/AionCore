use std::sync::{Arc, Barrier, Mutex};

use aionui_api_types::TeamRunTargetRole;

use super::*;
use crate::{TeamError, work_source::WorkSource};

#[derive(Default)]
struct RecordingRunCausality {
    summaries: Mutex<Vec<RunWorkSummary>>,
    slot_work: Mutex<Vec<SlotWorkSnapshot>>,
    system_binds: Mutex<Vec<EnqueueRequest>>,
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

    fn bind_system_enqueue(&self, request: &EnqueueRequest) -> RunBinding {
        self.system_binds.lock().unwrap().push(request.clone());
        RunBinding {
            team_run_id: Some("system-run-1".to_owned()),
            created_new_run: true,
            user_intervention: false,
        }
    }

    fn abort_binding(&self, _binding: &RunBinding) {}

    fn apply_work_summary(&self, summary: RunWorkSummary) {
        self.summaries.lock().unwrap().push(summary);
    }

    fn publish_slot_work(&self, snapshot: SlotWorkSnapshot) {
        self.slot_work.lock().unwrap().push(snapshot);
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

fn coordinator_with_recorder() -> (SlotWorkCoordinator, Arc<RecordingRunCausality>) {
    let recorder = Arc::new(RecordingRunCausality::default());
    let coordinator = SlotWorkCoordinator::new("team-1".into(), "generation-1".into(), recorder.clone());
    (coordinator, recorder)
}

// Regression: run-less work (Background binding → no team_run_id, e.g. a leader
// self-wake draining its mailbox for a membership-change notice) produces NO run
// summaries, so the ONLY way the frontend learns the slot moved Running→Idle is a
// per-slot `team.slotWorkChanged`. Every batch-lifecycle transition must publish
// the slot's current snapshot regardless of run association, or the send-box
// spinner stays stuck until a full reconcile.
#[test]
fn run_less_batch_lifecycle_publishes_per_slot_work_snapshots() {
    let (coordinator, recorder) = coordinator_with_recorder();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::McpSendMessage, "bg-1");

    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("run-less batch must be claimable");
    };
    coordinator.mark_started(&batch, "turn-1");
    assert_eq!(coordinator.complete_batch(&batch), CommitResult::Committed);

    let snaps = recorder.slot_work.lock().unwrap();
    assert!(
        snaps
            .iter()
            .any(|snap| snap.slot_id == "lead-1" && snap.state == SlotPhase::Running),
        "run-less start must publish a Running per-slot snapshot"
    );
    let last = snaps
        .iter()
        .rev()
        .find(|snap| snap.slot_id == "lead-1")
        .expect("run-less slot transitions must publish per-slot work snapshots");
    assert_eq!(
        last.state,
        SlotPhase::Idle,
        "the final published snapshot must be Idle so the frontend clears the spinner"
    );
}

#[test]
fn priority_lanes_claim_foreground_then_control_then_directed_then_background() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::TeamMembershipChanged, "background-1");
    enqueue(&coordinator, WorkSource::McpShutdownRequest, "control-1");
    enqueue(&coordinator, WorkSource::McpSendMessage, "directed-1");
    enqueue(&coordinator, WorkSource::UserMessage, "foreground-1");
    enqueue(&coordinator, WorkSource::UserIntervention, "foreground-2");

    let ReconcileDecision::Claim(foreground) = coordinator.next("lead-1") else {
        panic!("foreground batch must be claimable");
    };
    assert_eq!(foreground.highest_priority, WorkPriority::Foreground);
    assert_eq!(
        foreground.mailbox_message_ids,
        vec!["foreground-1".to_owned(), "foreground-2".to_owned()]
    );
    assert_eq!(coordinator.complete_batch(&foreground), CommitResult::Committed);

    let ReconcileDecision::Claim(control) = coordinator.next("lead-1") else {
        panic!("control batch must follow foreground");
    };
    assert_eq!(control.highest_priority, WorkPriority::Control);
    assert_eq!(control.mailbox_message_ids, vec!["control-1"]);
    assert_eq!(coordinator.complete_batch(&control), CommitResult::Committed);

    let ReconcileDecision::Claim(directed) = coordinator.next("lead-1") else {
        panic!("directed batch must follow control");
    };
    assert_eq!(directed.highest_priority, WorkPriority::Directed);
    assert_eq!(directed.mailbox_message_ids, vec!["directed-1"]);
    assert_eq!(coordinator.complete_batch(&directed), CommitResult::Committed);

    let ReconcileDecision::Claim(background) = coordinator.next("lead-1") else {
        panic!("background batch must follow directed");
    };
    assert_eq!(background.highest_priority, WorkPriority::Background);
    assert_eq!(background.mailbox_message_ids, vec!["background-1"]);
}

/// A shutdown request must not be pushed behind teammate traffic that keeps
/// arriving. This is the case that motivated ranking Control above Directed: the
/// directed lane refills between batches, so a lower-ranked control lane could be
/// deferred round after round.
#[test]
fn a_shutdown_request_is_claimed_before_continuously_arriving_directed_messages() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::McpSendMessage, "directed-1");
    enqueue(&coordinator, WorkSource::McpShutdownRequest, "control-1");
    // More teammate traffic lands before the coordinator picks a lane.
    enqueue(&coordinator, WorkSource::McpSendMessage, "directed-2");

    let ReconcileDecision::Claim(first) = coordinator.next("lead-1") else {
        panic!("a batch must be claimable");
    };
    assert_eq!(first.highest_priority, WorkPriority::Control);
    assert_eq!(first.mailbox_message_ids, vec!["control-1"]);
    assert_eq!(coordinator.complete_batch(&first), CommitResult::Committed);

    // The directed backlog is still there, coalesced, and claimed next.
    let ReconcileDecision::Claim(second) = coordinator.next("lead-1") else {
        panic!("directed backlog must follow");
    };
    assert_eq!(second.highest_priority, WorkPriority::Directed);
    assert_eq!(
        second.mailbox_message_ids,
        vec!["directed-1".to_owned(), "directed-2".to_owned()]
    );
}

#[test]
fn directed_message_follows_a_coalesced_foreground_backlog() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::McpSendMessage, "directed-1");
    for index in 1..=5 {
        enqueue(&coordinator, WorkSource::UserMessage, &format!("foreground-{index}"));
    }

    let ReconcileDecision::Claim(foreground) = coordinator.next("lead-1") else {
        panic!("foreground backlog must be claimable");
    };
    assert_eq!(foreground.highest_priority, WorkPriority::Foreground);
    assert_eq!(foreground.mailbox_message_ids.len(), 5);
    assert_eq!(coordinator.complete_batch(&foreground), CommitResult::Committed);

    let ReconcileDecision::Claim(directed) = coordinator.next("lead-1") else {
        panic!("directed message must follow the foreground batch");
    };
    assert_eq!(directed.highest_priority, WorkPriority::Directed);
    assert_eq!(directed.mailbox_message_ids, vec!["directed-1"]);
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
fn runtime_restart_gate_atomically_rejects_active_work() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "m1");
    let ReconcileDecision::Claim(_) = coordinator.next("lead-1") else {
        panic!("message must be claimable");
    };

    assert_eq!(
        coordinator.begin_runtime_restart("lead-1"),
        Err(RuntimeRestartRejection::Busy)
    );
    assert!(matches!(
        coordinator.slot_snapshot("lead-1").unwrap().runtime_constraint,
        RuntimeConstraint::Ready
    ));
}

#[test]
fn runtime_restart_gate_rejects_queued_work() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "m1");

    assert_eq!(
        coordinator.begin_runtime_restart("lead-1"),
        Err(RuntimeRestartRejection::Busy)
    );
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("rejected restart must leave queued work claimable");
    };
    assert_eq!(batch.mailbox_message_ids, vec!["m1"]);
}

#[test]
fn mcp_refresh_defers_active_work_and_deduplicates_applied_fingerprint() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "m1");
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("message must be claimable");
    };
    assert_eq!(
        coordinator.request_mcp_refresh("lead-1", "revision-2"),
        McpRefreshDisposition::Deferred
    );
    assert_eq!(coordinator.complete_batch(&batch), CommitResult::Committed);
    assert_eq!(
        coordinator.claim_pending_mcp_refresh("lead-1").as_deref(),
        Some("revision-2")
    );
    coordinator.complete_mcp_refresh("lead-1", "revision-2");
    assert_eq!(
        coordinator.request_mcp_refresh("lead-1", "revision-2"),
        McpRefreshDisposition::Unchanged
    );
    assert!(coordinator.claim_pending_mcp_refresh("lead-1").is_none());
}

#[test]
fn mcp_restart_gate_runs_before_queued_work() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "m1");
    assert_eq!(
        coordinator.request_mcp_refresh("lead-1", "revision-2"),
        McpRefreshDisposition::Deferred
    );
    assert!(coordinator.begin_mcp_runtime_restart("lead-1").is_ok());
    assert_eq!(
        coordinator.next("lead-1"),
        ReconcileDecision::Blocked(RuntimeConstraint::Starting { operation_id: 1 })
    );
}

#[test]
fn runtime_restart_gate_and_batch_claim_have_one_atomic_winner() {
    for _ in 0..64 {
        let coordinator = Arc::new(coordinator());
        coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
        enqueue(&coordinator, WorkSource::UserMessage, "m1");
        let barrier = Arc::new(Barrier::new(3));

        let gate_coordinator = Arc::clone(&coordinator);
        let gate_barrier = Arc::clone(&barrier);
        let gate_task = std::thread::spawn(move || {
            gate_barrier.wait();
            gate_coordinator.begin_runtime_restart("lead-1")
        });
        let claim_coordinator = Arc::clone(&coordinator);
        let claim_barrier = Arc::clone(&barrier);
        let claim_task = std::thread::spawn(move || {
            claim_barrier.wait();
            claim_coordinator.next("lead-1")
        });
        barrier.wait();

        let gate = gate_task.join().unwrap();
        let claim = claim_task.join().unwrap();
        assert!(matches!(gate, Err(RuntimeRestartRejection::Busy)));
        assert!(matches!(claim, ReconcileDecision::Claim(_)));
    }
}

#[test]
fn runtime_restart_gate_rejects_removing_and_stopped_slots() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Removing { operation_id: 7 });
    assert_eq!(
        coordinator.begin_runtime_restart("lead-1"),
        Err(RuntimeRestartRejection::Removing)
    );
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::SessionStopped);
    assert_eq!(
        coordinator.begin_runtime_restart("lead-1"),
        Err(RuntimeRestartRejection::SessionStopped)
    );
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
fn failed_batch_is_reprojected_from_the_unread_mailbox() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "m1");
    let ReconcileDecision::Claim(first) = coordinator.next("lead-1") else {
        panic!("message must be claimable");
    };

    let failure = coordinator.fail_batch(&first, "turn_start_failed");
    assert_eq!(failure.commit_result, CommitResult::Committed);
    assert!(failure.exhausted_message_ids.is_empty());

    coordinator.reconcile_mailbox("lead-1", &["m1".into()], TeamRunTargetRole::Lead);
    let ReconcileDecision::Claim(retry) = coordinator.next("lead-1") else {
        panic!("unread failed delivery must be reprojected");
    };
    assert_eq!(retry.mailbox_message_ids, vec!["m1"]);
    assert_ne!(retry.intent_ids, first.intent_ids);
    assert_eq!(
        coordinator
            .intents_for_slot("lead-1")
            .into_iter()
            .find(|intent| retry.intent_ids.contains(&intent.intent_id))
            .expect("recovery intent must exist")
            .source,
        WorkSource::RecoveryDrain
    );
}

#[test]
fn delivery_failures_pause_after_three_attempts() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "m1");

    for attempt in 1..=MAX_MESSAGE_DELIVERY_FAILURES {
        let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
            panic!("attempt {attempt} must be claimable");
        };
        let failure = coordinator.fail_batch(&batch, "turn_failed");
        assert_eq!(failure.commit_result, CommitResult::Committed);
        if attempt < MAX_MESSAGE_DELIVERY_FAILURES {
            assert!(failure.exhausted_message_ids.is_empty());
            coordinator.reconcile_mailbox("lead-1", &["m1".into()], TeamRunTargetRole::Lead);
        } else {
            assert_eq!(failure.exhausted_message_ids, vec!["m1"]);
        }
    }

    assert_eq!(coordinator.slot_snapshot("lead-1").unwrap().state, SlotPhase::Paused);
    coordinator.reconcile_mailbox("lead-1", &[], TeamRunTargetRole::Lead);
    assert_eq!(coordinator.next("lead-1"), ReconcileDecision::Quiescent);
}

#[test]
fn successful_retry_clears_the_consecutive_failure_count() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "m1");
    let ReconcileDecision::Claim(first) = coordinator.next("lead-1") else {
        panic!("first attempt must be claimable");
    };
    assert!(
        coordinator
            .fail_batch(&first, "turn_failed")
            .exhausted_message_ids
            .is_empty()
    );
    coordinator.reconcile_mailbox("lead-1", &["m1".into()], TeamRunTargetRole::Lead);
    let ReconcileDecision::Claim(success) = coordinator.next("lead-1") else {
        panic!("successful retry must be claimable");
    };
    assert_eq!(coordinator.complete_batch(&success), CommitResult::Committed);

    coordinator.reconcile_mailbox("lead-1", &["m1".into()], TeamRunTargetRole::Lead);
    let ReconcileDecision::Claim(after_success) = coordinator.next("lead-1") else {
        panic!("simulated mark-read failure must be recoverable");
    };
    assert!(
        coordinator
            .fail_batch(&after_success, "turn_failed")
            .exhausted_message_ids
            .is_empty(),
        "a success between failures must reset the consecutive count"
    );
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
fn observed_messages_complete_with_the_active_batch_and_do_not_requeue() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "initial");
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("initial message must be claimable");
    };
    assert_eq!(coordinator.mark_started(&batch, "turn-1"), StartCommitResult::Accepted);
    enqueue(&coordinator, WorkSource::UserMessage, "late");

    let observed = coordinator.observe_messages("lead-1", &batch.batch_id, &["initial".into(), "late".into()]);
    assert_eq!(observed.batch_id.as_deref(), Some(batch.batch_id.as_str()));
    assert_eq!(
        observed.observed_count, 1,
        "the original batch message is already owned"
    );
    assert_eq!(
        coordinator
            .slot_snapshot("lead-1")
            .unwrap()
            .active_batch
            .unwrap()
            .observed_message_ids,
        vec!["late"]
    );

    let completion = coordinator.complete_batch_with_ack(&batch);
    assert_eq!(completion.commit_result, CommitResult::Committed);
    assert_eq!(completion.ack_message_ids, vec!["initial", "late"]);
    assert_eq!(coordinator.next("lead-1"), ReconcileDecision::Quiescent);
    assert!(
        coordinator
            .intents_for_slot("lead-1")
            .iter()
            .all(|intent| intent.state == WorkIntentState::Completed)
    );
}

#[test]
fn observed_messages_remain_queued_when_the_active_batch_fails() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "initial");
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("initial message must be claimable");
    };
    coordinator.mark_started(&batch, "turn-1");
    enqueue(&coordinator, WorkSource::UserMessage, "late");
    coordinator.observe_messages("lead-1", &batch.batch_id, &["late".into()]);

    let failure = coordinator.fail_batch(&batch, "turn_failed");
    assert_eq!(failure.commit_result, CommitResult::Committed);
    let ReconcileDecision::Claim(retry) = coordinator.next("lead-1") else {
        panic!("observed late message must remain queued after failure");
    };
    assert_eq!(retry.mailbox_message_ids, vec!["late"]);
}

#[test]
fn observed_messages_remain_queued_when_the_active_batch_is_cancelled_or_interrupted() {
    for interruption in [false, true] {
        let coordinator = coordinator();
        coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
        enqueue(&coordinator, WorkSource::UserMessage, "initial");
        let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
            panic!("initial message must be claimable");
        };
        coordinator.mark_started(&batch, "turn-1");
        enqueue(&coordinator, WorkSource::UserMessage, "late");
        coordinator.observe_messages("lead-1", &batch.batch_id, &["late".into()]);

        if interruption {
            assert_eq!(
                coordinator
                    .interrupt_batch(&batch, Some("replace".into()), "replacement".into())
                    .commit_result,
                CommitResult::Committed
            );
        } else {
            assert_eq!(
                coordinator.cancel_batch(&batch, "turn_cancelled"),
                CommitResult::Committed
            );
        }

        let ReconcileDecision::Claim(next) = coordinator.next("lead-1") else {
            panic!("observed late message must survive cancellation/interruption");
        };
        assert_eq!(next.mailbox_message_ids, vec!["late"]);
    }
}

/// P3: a `team_read_messages` call that was still in flight when its turn got
/// replaced must not bind its rows to whatever batch owns the slot now — the new
/// batch would acknowledge messages its agent never saw.
#[test]
fn observations_from_a_replaced_turn_are_dropped_instead_of_rebound() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "initial");
    let ReconcileDecision::Claim(first) = coordinator.next("lead-1") else {
        panic!("initial message must be claimable");
    };
    coordinator.mark_started(&first, "turn-1");
    enqueue(&coordinator, WorkSource::UserMessage, "late");

    // The turn the agent was reading in gets cancelled, and the next batch takes over.
    assert_eq!(
        coordinator.cancel_batch(&first, "turn_cancelled"),
        CommitResult::Committed
    );
    let ReconcileDecision::Claim(second) = coordinator.next("lead-1") else {
        panic!("late message must start the next batch");
    };
    coordinator.mark_started(&second, "turn-2");
    assert_ne!(first.batch_id, second.batch_id);

    // The in-flight tool call finally lands, still carrying the old batch id.
    let observed = coordinator.observe_messages("lead-1", &first.batch_id, &["late".into()]);
    assert_eq!(observed.batch_id, None, "a stale observation is rejected");
    assert_eq!(observed.observed_count, 0);
    assert!(
        coordinator
            .slot_snapshot("lead-1")
            .unwrap()
            .active_batch
            .unwrap()
            .observed_message_ids
            .is_empty(),
        "the replacement batch must not inherit the stale observation"
    );
}

#[test]
fn active_batch_id_tracks_turn_ownership() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    assert_eq!(coordinator.active_batch_id("lead-1"), None, "no slot, no turn");

    enqueue(&coordinator, WorkSource::UserMessage, "initial");
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("initial message must be claimable");
    };
    assert_eq!(coordinator.active_batch_id("lead-1").as_ref(), Some(&batch.batch_id));

    coordinator.complete_batch_with_ack(&batch);
    assert_eq!(
        coordinator.active_batch_id("lead-1"),
        None,
        "ownership is released with the batch"
    );
}

#[test]
fn messages_arriving_after_observation_are_left_for_the_next_batch() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "initial");
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("initial message must be claimable");
    };
    coordinator.mark_started(&batch, "turn-1");
    enqueue(&coordinator, WorkSource::UserMessage, "observed");
    coordinator.observe_messages("lead-1", &batch.batch_id, &["observed".into()]);
    enqueue(&coordinator, WorkSource::UserMessage, "after-read");

    let completion = coordinator.complete_batch_with_ack(&batch);
    assert_eq!(completion.ack_message_ids, vec!["initial", "observed"]);
    let ReconcileDecision::Claim(next) = coordinator.next("lead-1") else {
        panic!("message arriving after read must start the next batch");
    };
    assert_eq!(next.mailbox_message_ids, vec!["after-read"]);
}

#[test]
fn pause_cancels_running_batch_and_retains_queued_work() {
    let (coordinator, recorder) = coordinator_with_recorder();
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
    recorder.slot_work.lock().unwrap().clear();

    let paused = coordinator.pause_slot("lead-1");
    assert_eq!(paused.cancel_target.unwrap().batch, running);
    {
        let snapshots = recorder.slot_work.lock().unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].state, SlotPhase::Paused);
        assert_eq!(snapshots[0].active_turn_id.as_deref(), Some("turn-1"));
    }
    assert_eq!(
        coordinator.cancel_batch(&running, "slot_paused"),
        CommitResult::Committed
    );
    {
        let snapshots = recorder.slot_work.lock().unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[1].state, SlotPhase::Paused);
        assert_eq!(snapshots[1].active_turn_id, None);
    }
    assert_eq!(coordinator.next("lead-1"), ReconcileDecision::Quiescent);
    assert_eq!(coordinator.slot_snapshot("lead-1").unwrap().queued_background_count, 1);
}

#[test]
fn pause_queued_only_slot_publishes_paused_snapshot() {
    let (coordinator, recorder) = coordinator_with_recorder();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::McpSendMessage, "queued");
    recorder.slot_work.lock().unwrap().clear();

    let paused = coordinator.pause_slot("lead-1");

    assert!(paused.cancel_target.is_none());
    let snapshots = recorder.slot_work.lock().unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].state, SlotPhase::Paused);
    assert_eq!(snapshots[0].active_turn_id, None);
    assert_eq!(snapshots[0].queued_background_count, 1);
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

#[test]
fn system_initiated_inherit_uses_caller_batch_run() {
    let (coordinator, recorder) = coordinator_with_recorder();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    // The caller runs a user batch that carries a team run id.
    let caller = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "lead-1".into(),
            role: TeamRunTargetRole::Lead,
            source: WorkSource::UserMessage,
            binding: CausalBinding::UserVisible,
        })
        .unwrap();
    let run_id = caller.team_run_id.clone().expect("user enqueue must carry a run");
    coordinator.commit_enqueue(&caller, Some("m-user".into())).unwrap();
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("caller batch must be claimable");
    };
    assert_eq!(batch.team_run_ids, vec![run_id.clone()]);

    // A system wake targeting a teammate inherits the caller's running run
    // via the inline lookup, never falling back to the causality port.
    let inherited = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "worker-1".into(),
            role: TeamRunTargetRole::Teammate,
            source: WorkSource::SpawnWelcome,
            binding: CausalBinding::SystemInitiated {
                inherit_from: Some("lead-1".into()),
            },
        })
        .unwrap();
    assert_eq!(inherited.team_run_id.as_deref(), Some(run_id.as_str()));
    assert!(
        recorder.system_binds.lock().unwrap().is_empty(),
        "an inherit hit must not delegate to bind_system_enqueue"
    );
}

#[test]
fn system_initiated_none_delegates_to_port() {
    let (coordinator, recorder) = coordinator_with_recorder();
    let lease = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "lead-1".into(),
            role: TeamRunTargetRole::Lead,
            source: WorkSource::SpawnWelcome,
            binding: CausalBinding::SystemInitiated { inherit_from: None },
        })
        .unwrap();
    assert_eq!(lease.team_run_id.as_deref(), Some("system-run-1"));
    assert_eq!(
        recorder.system_binds.lock().unwrap().len(),
        1,
        "a None inherit must delegate to bind_system_enqueue"
    );
}

#[test]
fn lead_intervention_interrupts_active_batch_and_runs_before_retained_queue() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "active");
    let ReconcileDecision::Claim(active) = coordinator.next("lead-1") else {
        panic!("active work must be claimed");
    };
    assert_eq!(coordinator.mark_started(&active, "turn-1"), StartCommitResult::Accepted);
    enqueue(&coordinator, WorkSource::UserIntervention, "older-queued");
    enqueue(&coordinator, WorkSource::LeadIntervention, "replacement");

    let interrupted = coordinator.interrupt_batch(&active, Some("requirements changed".into()), "replacement".into());
    assert_eq!(interrupted.commit_result, CommitResult::Committed);
    assert_eq!(interrupted.terminal_message_ids, vec!["active"]);
    assert!(coordinator.is_batch_cancelled(&active));
    assert_eq!(
        coordinator.take_interrupt_metadata(&active.batch_id),
        Some(BatchInterruptMetadata {
            reason: Some("requirements changed".into()),
            replacement_message_id: "replacement".into(),
        })
    );

    let ReconcileDecision::Claim(replacement) = coordinator.next("lead-1") else {
        panic!("replacement must be next");
    };
    assert_eq!(replacement.mailbox_message_ids, vec!["replacement"]);
    coordinator.complete_batch(&replacement);
    let ReconcileDecision::Claim(retained) = coordinator.next("lead-1") else {
        panic!("older queued work must be retained");
    };
    assert_eq!(retained.mailbox_message_ids, vec!["older-queued"]);
}

#[test]
fn interrupt_compare_and_set_reports_completion_race() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "active");
    let ReconcileDecision::Claim(active) = coordinator.next("lead-1") else {
        panic!("active work must be claimed");
    };
    assert_eq!(coordinator.complete_batch(&active), CommitResult::Committed);

    let result = coordinator.interrupt_batch(&active, None, "replacement".into());
    assert_eq!(result.commit_result, CommitResult::StaleOwner);
    assert!(result.terminal_message_ids.is_empty());
}

#[test]
fn starting_batch_interrupt_defers_stream_cancel_until_late_start_callback() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "active");
    let ReconcileDecision::Claim(starting) = coordinator.next("lead-1") else {
        panic!("active work must be claimed");
    };
    let result = coordinator.interrupt_batch(&starting, None, "replacement".into());
    assert_eq!(result.commit_result, CommitResult::Committed);
    assert_eq!(
        coordinator.mark_started(&starting, "late-turn"),
        StartCommitResult::CancelImmediately
    );
    assert_eq!(
        coordinator.take_interrupt_metadata(&starting.batch_id),
        Some(BatchInterruptMetadata {
            reason: None,
            replacement_message_id: "replacement".into(),
        })
    );
}

#[test]
fn discard_policy_terminalizes_unclaimed_queue_but_keeps_replacement() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserIntervention, "old-1");
    enqueue(&coordinator, WorkSource::McpSendMessage, "old-2");
    enqueue(&coordinator, WorkSource::LeadIntervention, "replacement");

    let discarded = coordinator.discard_queued_except("lead-1", "replacement");
    assert_eq!(discarded.len(), 2);
    assert!(discarded.contains(&"old-1".to_owned()));
    assert!(discarded.contains(&"old-2".to_owned()));
    let ReconcileDecision::Claim(replacement) = coordinator.next("lead-1") else {
        panic!("replacement must remain queued");
    };
    assert_eq!(replacement.mailbox_message_ids, vec!["replacement"]);
}

/// I2: Discard supersedes queued *instructions*. The Control lane carries the
/// shutdown handshake, which has no retry path, so dropping it would strand the
/// protocol forever.
#[test]
fn discard_policy_exempts_control_lane_work() {
    for control_source in [WorkSource::McpShutdownRequest, WorkSource::ShutdownRejected] {
        let coordinator = coordinator();
        coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
        enqueue(&coordinator, WorkSource::UserIntervention, "instruction");
        enqueue(&coordinator, control_source, "control");
        enqueue(&coordinator, WorkSource::LeadIntervention, "replacement");

        let discarded = coordinator.discard_queued_except("lead-1", "replacement");
        assert_eq!(
            discarded,
            vec!["instruction".to_owned()],
            "{control_source:?} must survive a discard"
        );

        // The replacement runs first (Foreground), then the retained control work.
        let ReconcileDecision::Claim(replacement) = coordinator.next("lead-1") else {
            panic!("replacement must remain queued");
        };
        assert_eq!(replacement.mailbox_message_ids, vec!["replacement"]);
        coordinator.complete_batch(&replacement);

        let ReconcileDecision::Claim(control) = coordinator.next("lead-1") else {
            panic!("{control_source:?} must still be claimable after the discard");
        };
        assert_eq!(control.mailbox_message_ids, vec!["control"]);
    }
}

// ── ELECTRON-3RN: recognized slash commands batch alone (FIFO, no preempt) ──

// AC3: when a command shares the queue with other unread messages, it is claimed
// as a single-message batch (`is_command`), never merged with them — otherwise
// the native op would swallow the rest of the turn (the flaw that ruled out B).
#[test]
fn command_intent_is_claimed_alone_not_merged() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserCommand, "c1");
    enqueue(&coordinator, WorkSource::UserMessage, "m2");
    enqueue(&coordinator, WorkSource::UserMessage, "m3");
    coordinator.reconcile_mailbox(
        "lead-1",
        &["c1".into(), "m2".into(), "m3".into()],
        TeamRunTargetRole::Lead,
    );

    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("command must be claimable");
    };
    assert!(batch.is_command, "a command batch must be flagged is_command");
    assert_eq!(
        batch.mailbox_message_ids,
        vec!["c1"],
        "command must not merge later messages"
    );
    assert_eq!(coordinator.mark_started(&batch, "turn-1"), StartCommitResult::Accepted);
    assert_eq!(coordinator.complete_batch(&batch), CommitResult::Committed);

    // The trailing ordinary messages merge as usual once the command completes.
    let ReconcileDecision::Claim(rest) = coordinator.next("lead-1") else {
        panic!("ordinary messages must follow the command");
    };
    assert!(!rest.is_command);
    assert_eq!(rest.mailbox_message_ids, vec!["m2", "m3"]);
}

// AC3: an ordinary batch merges the leading run of plain messages but STOPS at
// the first command, so the command is never folded into a plain batch.
#[test]
fn plain_batch_merge_stops_at_command() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "m1");
    enqueue(&coordinator, WorkSource::UserMessage, "m2");
    enqueue(&coordinator, WorkSource::UserCommand, "c3");
    enqueue(&coordinator, WorkSource::UserMessage, "m4");
    coordinator.reconcile_mailbox(
        "lead-1",
        &["m1".into(), "m2".into(), "c3".into(), "m4".into()],
        TeamRunTargetRole::Lead,
    );

    let ReconcileDecision::Claim(first) = coordinator.next("lead-1") else {
        panic!("plain messages must be claimable");
    };
    assert!(!first.is_command);
    assert_eq!(
        first.mailbox_message_ids,
        vec!["m1", "m2"],
        "merge must stop before the command"
    );
    assert_eq!(coordinator.mark_started(&first, "turn-1"), StartCommitResult::Accepted);
    assert_eq!(coordinator.complete_batch(&first), CommitResult::Committed);

    let ReconcileDecision::Claim(command) = coordinator.next("lead-1") else {
        panic!("command must be claimed next, alone");
    };
    assert!(command.is_command);
    assert_eq!(command.mailbox_message_ids, vec!["c3"]);
}

// AC3: a command sent while a batch is running queues in FIFO order and does not
// preempt the active batch.
#[test]
fn command_during_running_batch_queues_without_preemption() {
    let coordinator = coordinator();
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    enqueue(&coordinator, WorkSource::UserMessage, "m1");
    let ReconcileDecision::Claim(first) = coordinator.next("lead-1") else {
        panic!("first message must be claimable");
    };
    assert!(!first.is_command);
    assert_eq!(coordinator.mark_started(&first, "turn-1"), StartCommitResult::Accepted);

    // Command arrives mid-turn: it must NOT preempt the running batch.
    enqueue(&coordinator, WorkSource::UserCommand, "c2");
    let active = coordinator.slot_snapshot("lead-1").unwrap();
    assert_eq!(active.active_batch.unwrap().mailbox_message_ids, vec!["m1"]);
    assert_eq!(coordinator.next("lead-1"), ReconcileDecision::WaitingForCompletion);

    // Once the active batch completes, the command is claimed alone.
    assert_eq!(coordinator.complete_batch(&first), CommitResult::Committed);
    let ReconcileDecision::Claim(second) = coordinator.next("lead-1") else {
        panic!("queued command must follow the active batch");
    };
    assert!(second.is_command);
    assert_eq!(second.mailbox_message_ids, vec!["c2"]);
}
