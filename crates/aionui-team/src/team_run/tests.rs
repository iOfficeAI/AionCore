use std::sync::Arc;

use aionui_api_types::{TeamRunSource, TeamRunStatus, TeamRunTargetRole};

use crate::events::TeamEventEmitter;
use crate::team_run::TeamRunManager;
use crate::test_utils::workspace_harness::RecordingBroadcaster;
use crate::work_coordinator::{
    CausalBinding, EnqueueRequest, ReconcileDecision, RuntimeConstraint, SlotWorkCoordinator,
};
use crate::work_source::WorkSource;

fn coordinator_and_manager() -> (Arc<SlotWorkCoordinator>, Arc<TeamRunManager>) {
    let broadcaster = Arc::new(RecordingBroadcaster::new());
    let emitter = Arc::new(TeamEventEmitter::new("team-1".into(), broadcaster));
    let manager = Arc::new(TeamRunManager::new("team-1".into(), emitter));
    let coordinator = Arc::new(SlotWorkCoordinator::new(
        "team-1".into(),
        "generation-1".into(),
        manager.clone(),
    ));
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
    (coordinator, manager)
}

fn acquire(coordinator: &SlotWorkCoordinator, binding: CausalBinding) -> crate::work_coordinator::EnqueueLease {
    coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "lead-1".into(),
            role: TeamRunTargetRole::Lead,
            source: WorkSource::UserMessage,
            binding,
        })
        .unwrap()
}

#[test]
fn background_work_does_not_create_or_block_a_run() {
    let (coordinator, manager) = coordinator_and_manager();
    let lease = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "lead-1".into(),
            role: TeamRunTargetRole::Lead,
            source: WorkSource::RecoveryDrain,
            binding: CausalBinding::Background,
        })
        .unwrap();
    coordinator.commit_enqueue(&lease, Some("m1".into())).unwrap();
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("background intent must be claimable");
    };
    assert_eq!(manager.current_active_run_id(), None);
    coordinator.complete_batch(&batch);
    assert_eq!(manager.current_active_run_id(), None);
}

#[test]
fn enqueue_lease_blocks_completion() {
    let (coordinator, manager) = coordinator_and_manager();
    let lease = acquire(&coordinator, CausalBinding::UserVisible);
    let run_id = lease.team_run_id.clone().unwrap();

    let payload = manager.current_payload(&coordinator.snapshot()).unwrap();
    assert_eq!(payload.team_run_id, run_id);
    assert_eq!(payload.active_enqueue_lease_count, 1);
    assert_eq!(payload.status, TeamRunStatus::Accepted);
}

#[test]
fn run_completes_after_its_last_intent_is_terminal() {
    let (coordinator, manager) = coordinator_and_manager();
    let lease = acquire(&coordinator, CausalBinding::UserVisible);
    coordinator.commit_enqueue(&lease, Some("m1".into())).unwrap();
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("user intent must be claimable");
    };
    coordinator.complete_batch(&batch);

    let payload = manager.current_payload(&coordinator.snapshot()).unwrap();
    assert_eq!(payload.status, TeamRunStatus::Completed);
    assert_eq!(manager.current_active_run_id(), None);
}

#[test]
fn runtime_failure_fails_the_related_run() {
    let (coordinator, manager) = coordinator_and_manager();
    let lease = acquire(&coordinator, CausalBinding::UserVisible);
    coordinator.commit_enqueue(&lease, Some("m1".into())).unwrap();
    coordinator.set_runtime_constraint(
        "lead-1",
        RuntimeConstraint::Failed {
            operation_id: 7,
            classification: "attach_failed",
        },
    );

    let payload = manager.current_payload(&coordinator.snapshot()).unwrap();
    assert_eq!(payload.status, TeamRunStatus::Failed);
    assert_eq!(manager.current_active_run_id(), None);
}

#[test]
fn aborted_first_enqueue_removes_the_empty_run() {
    let (coordinator, manager) = coordinator_and_manager();
    let lease = acquire(&coordinator, CausalBinding::UserVisible);
    coordinator.abort_enqueue(&lease, "mailbox_write_failed");

    assert_eq!(manager.current_active_run_id(), None);
    assert!(manager.current_payload(&coordinator.snapshot()).is_none());
}

#[test]
fn aborting_the_creator_lease_preserves_a_concurrent_committed_enqueue() {
    let (coordinator, manager) = coordinator_and_manager();
    let creator = acquire(&coordinator, CausalBinding::UserVisible);
    let concurrent = acquire(&coordinator, CausalBinding::UserVisible);
    let run_id = creator.team_run_id.clone().unwrap();
    assert_eq!(concurrent.team_run_id.as_deref(), Some(run_id.as_str()));

    coordinator
        .commit_enqueue(&concurrent, Some("m-concurrent".into()))
        .unwrap();
    coordinator.abort_enqueue(&creator, "mailbox_write_failed");

    let payload = manager
        .current_payload(&coordinator.snapshot())
        .expect("the concurrent enqueue must retain its run");
    assert_eq!(payload.team_run_id, run_id);
    assert_eq!(payload.queued_intent_count, 1);
    assert_eq!(manager.current_active_run_id(), Some(payload.team_run_id));
}

#[test]
fn accepted_event_is_emitted_only_after_the_durable_enqueue_commits() {
    let broadcaster = Arc::new(RecordingBroadcaster::new());
    let emitter = Arc::new(TeamEventEmitter::new("team-1".into(), broadcaster.clone()));
    let manager = Arc::new(TeamRunManager::new("team-1".into(), emitter));
    let coordinator = SlotWorkCoordinator::new("team-1".into(), "generation-1".into(), manager);
    coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);

    let lease = acquire(&coordinator, CausalBinding::UserVisible);
    assert!(broadcaster.events_by_name("team.runAccepted").is_empty());

    coordinator.commit_enqueue(&lease, Some("m1".into())).unwrap();
    assert_eq!(broadcaster.events_by_name("team.runAccepted").len(), 1);
}

#[test]
fn mcp_message_inherits_the_callers_running_batch_causality() {
    let (coordinator, manager) = coordinator_and_manager();
    coordinator.set_runtime_constraint("worker-1", RuntimeConstraint::Ready);
    coordinator.set_runtime_constraint("worker-2", RuntimeConstraint::Ready);

    let user = acquire(&coordinator, CausalBinding::UserVisible);
    let run_id = user.team_run_id.clone().unwrap();
    coordinator.commit_enqueue(&user, Some("m-user".into())).unwrap();
    let ReconcileDecision::Claim(user_batch) = coordinator.next("lead-1") else {
        panic!("user batch must be claimed");
    };
    coordinator.mark_started(&user_batch, "turn-user");
    let inherited = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "worker-1".into(),
            role: TeamRunTargetRole::Teammate,
            source: WorkSource::McpSendMessage,
            binding: CausalBinding::InheritRunningBatch {
                caller_slot_id: "lead-1".into(),
            },
        })
        .unwrap();
    assert_eq!(inherited.team_run_id.as_deref(), Some(run_id.as_str()));
    coordinator.abort_enqueue(&inherited, "test_complete");

    coordinator.complete_batch(&user_batch);
    let background = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "worker-1".into(),
            role: TeamRunTargetRole::Teammate,
            source: WorkSource::RecoveryDrain,
            binding: CausalBinding::Background,
        })
        .unwrap();
    coordinator
        .commit_enqueue(&background, Some("m-background".into()))
        .unwrap();
    let ReconcileDecision::Claim(background_batch) = coordinator.next("worker-1") else {
        panic!("background batch must be claimed");
    };
    coordinator.mark_started(&background_batch, "turn-background");

    let unrelated_user = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "worker-2".into(),
            role: TeamRunTargetRole::Teammate,
            source: WorkSource::UserMessage,
            binding: CausalBinding::UserVisible,
        })
        .unwrap();
    assert!(unrelated_user.team_run_id.is_some());
    let background_child = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "lead-1".into(),
            role: TeamRunTargetRole::Lead,
            source: WorkSource::McpSendMessage,
            binding: CausalBinding::InheritRunningBatch {
                caller_slot_id: "worker-1".into(),
            },
        })
        .unwrap();
    // Post-fix: an InheritRunningBatch wake with no inheritable run no longer
    // goes run-less; bind_system_enqueue attaches it to the active run.
    assert_eq!(background_child.team_run_id, unrelated_user.team_run_id);

    coordinator.abort_enqueue(&background_child, "test_complete");
    coordinator.abort_enqueue(&unrelated_user, "test_complete");
    coordinator.complete_batch(&background_batch);
    assert!(manager.current_active_run_id().is_none());
}

#[test]
fn published_dynamic_attach_failure_blocks_only_related_work_and_preserves_healthy_runtimes() {
    let (coordinator, manager) = coordinator_and_manager();
    coordinator.set_runtime_constraint("worker-1", RuntimeConstraint::Ready);
    let user = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "worker-1".into(),
            role: TeamRunTargetRole::Teammate,
            source: WorkSource::UserMessage,
            binding: CausalBinding::UserVisible,
        })
        .unwrap();
    coordinator.commit_enqueue(&user, Some("m-user".into())).unwrap();
    let healthy = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "lead-1".into(),
            role: TeamRunTargetRole::Lead,
            source: WorkSource::RecoveryDrain,
            binding: CausalBinding::Background,
        })
        .unwrap();
    coordinator.commit_enqueue(&healthy, Some("m-healthy".into())).unwrap();

    coordinator.set_runtime_constraint(
        "worker-1",
        RuntimeConstraint::Failed {
            operation_id: 9,
            classification: "attach_failed",
        },
    );

    let run = manager.current_payload(&coordinator.snapshot()).unwrap();
    assert_eq!(run.status, TeamRunStatus::Failed);
    let healthy = coordinator.slot_snapshot("lead-1").unwrap();
    assert_eq!(healthy.runtime_constraint, RuntimeConstraint::Ready);
    assert_eq!(healthy.queued_background_count, 1);
}

#[test]
fn manual_or_leader_add_during_run_inherits_run_causality() {
    let (coordinator, _manager) = coordinator_and_manager();
    coordinator.set_runtime_constraint("worker-1", RuntimeConstraint::Ready);
    coordinator.set_runtime_constraint("worker-2", RuntimeConstraint::Ready);
    let user = acquire(&coordinator, CausalBinding::UserVisible);
    let run_id = user.team_run_id.clone().unwrap();
    coordinator.commit_enqueue(&user, Some("m-user".into())).unwrap();
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("lead batch must be claimed");
    };
    coordinator.mark_started(&batch, "turn-lead");

    let manual = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "worker-1".into(),
            role: TeamRunTargetRole::Teammate,
            source: WorkSource::SpawnWelcome,
            binding: CausalBinding::ActiveRunOrBackground,
        })
        .unwrap();
    let leader_spawn = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "worker-2".into(),
            role: TeamRunTargetRole::Teammate,
            source: WorkSource::SpawnWelcome,
            binding: CausalBinding::InheritRunningBatch {
                caller_slot_id: "lead-1".into(),
            },
        })
        .unwrap();

    assert_eq!(manual.team_run_id.as_deref(), Some(run_id.as_str()));
    assert_eq!(leader_spawn.team_run_id.as_deref(), Some(run_id.as_str()));
}

fn acquire_system(
    coordinator: &SlotWorkCoordinator,
    slot_id: &str,
    role: TeamRunTargetRole,
    source: WorkSource,
    inherit_from: Option<String>,
) -> crate::work_coordinator::EnqueueLease {
    coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: slot_id.into(),
            role,
            source,
            binding: CausalBinding::SystemInitiated { inherit_from },
        })
        .unwrap()
}

#[test]
fn system_initiated_without_active_run_opens_system_lifecycle_run() {
    let (coordinator, manager) = coordinator_and_manager();
    let lease = acquire_system(
        &coordinator,
        "lead-1",
        TeamRunTargetRole::Lead,
        WorkSource::SpawnWelcome,
        None,
    );
    assert!(lease.team_run_id.is_some(), "a system wake must open a run when idle");
    coordinator.commit_enqueue(&lease, Some("m1".into())).unwrap();
    let ReconcileDecision::Claim(_batch) = coordinator.next("lead-1") else {
        panic!("system intent must be claimable");
    };

    assert!(manager.current_active_run_id().is_some());
    let payload = manager.current_payload(&coordinator.snapshot()).unwrap();
    assert_eq!(payload.source, TeamRunSource::SystemLifecycle);
    assert!(
        !payload.has_user_intervention,
        "a system run must not be flagged as user-intervened"
    );
}

#[test]
fn inherit_running_batch_without_active_run_opens_system_lifecycle_run() {
    // A run-scoped wake (team_send_message / team_shutdown_agent both build
    // InheritRunningBatch) issued while the caller has no inheritable active
    // run must now open a SystemLifecycle run instead of going run-less.
    let (coordinator, manager) = coordinator_and_manager();
    coordinator.set_runtime_constraint("worker-1", RuntimeConstraint::Ready);

    // "lead-1" has no active batch/run: nothing to inherit.
    let lease = coordinator
        .acquire_enqueue(EnqueueRequest {
            slot_id: "worker-1".into(),
            role: TeamRunTargetRole::Teammate,
            source: WorkSource::McpSendMessage,
            binding: CausalBinding::InheritRunningBatch {
                caller_slot_id: "lead-1".into(),
            },
        })
        .unwrap();
    assert!(
        lease.team_run_id.is_some(),
        "run-scoped wake with no inheritable run must ensure a run"
    );

    coordinator.commit_enqueue(&lease, Some("m1".into())).unwrap();
    let ReconcileDecision::Claim(_batch) = coordinator.next("worker-1") else {
        panic!("wake intent must be claimable");
    };

    assert!(manager.current_active_run_id().is_some());
    let payload = manager.current_payload(&coordinator.snapshot()).unwrap();
    assert_eq!(payload.source, TeamRunSource::SystemLifecycle);
    assert!(!payload.has_user_intervention);
}

#[test]
fn system_initiated_attaches_to_existing_user_run() {
    let (coordinator, manager) = coordinator_and_manager();
    let user = acquire(&coordinator, CausalBinding::UserVisible);
    let run_id = user.team_run_id.clone().unwrap();
    coordinator.commit_enqueue(&user, Some("m-user".into())).unwrap();
    assert_eq!(manager.current_active_run_id().as_deref(), Some(run_id.as_str()));

    let system = acquire_system(
        &coordinator,
        "lead-1",
        TeamRunTargetRole::Lead,
        WorkSource::TeamMembershipChanged,
        None,
    );
    assert_eq!(
        system.team_run_id.as_deref(),
        Some(run_id.as_str()),
        "a system wake must attach to the active user run"
    );
    coordinator.commit_enqueue(&system, Some("m-system".into())).unwrap();

    assert_eq!(
        manager.current_active_run_id().as_deref(),
        Some(run_id.as_str()),
        "attaching must not open a second run"
    );
    let payload = manager.current_payload(&coordinator.snapshot()).unwrap();
    assert_eq!(
        payload.source,
        TeamRunSource::UserMessage,
        "attaching keeps the original user run"
    );
    assert!(
        !payload.has_user_intervention,
        "a system attach must not flag user intervention"
    );
}

#[test]
fn system_initiated_inherits_callers_running_run() {
    let (coordinator, manager) = coordinator_and_manager();
    coordinator.set_runtime_constraint("worker-1", RuntimeConstraint::Ready);
    let user = acquire(&coordinator, CausalBinding::UserVisible);
    let run_id = user.team_run_id.clone().unwrap();
    coordinator.commit_enqueue(&user, Some("m-user".into())).unwrap();
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("caller batch must be claimable");
    };
    coordinator.mark_started(&batch, "turn-lead");

    let inherited = acquire_system(
        &coordinator,
        "worker-1",
        TeamRunTargetRole::Teammate,
        WorkSource::SpawnWelcome,
        Some("lead-1".into()),
    );
    assert_eq!(
        inherited.team_run_id.as_deref(),
        Some(run_id.as_str()),
        "a system wake must inherit the caller's running run"
    );
    assert_eq!(
        manager.current_active_run_id().as_deref(),
        Some(run_id.as_str()),
        "inheriting must not open a second run"
    );
    coordinator.abort_enqueue(&inherited, "test_complete");
}

#[test]
fn system_initiated_inherit_miss_falls_back_to_new_system_run() {
    let (coordinator, manager) = coordinator_and_manager();
    // Caller slot has no active batch and the team has no active run, so the
    // inherit lookup misses and we open a fresh system run.
    let lease = acquire_system(
        &coordinator,
        "lead-1",
        TeamRunTargetRole::Lead,
        WorkSource::SpawnWelcome,
        Some("worker-unknown".into()),
    );
    assert!(lease.team_run_id.is_some());
    coordinator.commit_enqueue(&lease, Some("m1".into())).unwrap();

    let payload = manager.current_payload(&coordinator.snapshot()).unwrap();
    assert_eq!(payload.source, TeamRunSource::SystemLifecycle);
    assert!(!payload.has_user_intervention);
}

#[test]
fn system_run_completes_after_single_batch_no_dangling() {
    let (coordinator, manager) = coordinator_and_manager();
    let lease = acquire_system(
        &coordinator,
        "lead-1",
        TeamRunTargetRole::Lead,
        WorkSource::SpawnWelcome,
        None,
    );
    coordinator.commit_enqueue(&lease, Some("m1".into())).unwrap();
    let ReconcileDecision::Claim(batch) = coordinator.next("lead-1") else {
        panic!("system intent must be claimable");
    };
    coordinator.complete_batch(&batch);

    let payload = manager.current_payload(&coordinator.snapshot()).unwrap();
    assert_eq!(payload.status, TeamRunStatus::Completed);
    assert_eq!(
        manager.current_active_run_id(),
        None,
        "an idle-settle system run must not dangle after its only batch completes"
    );
}

#[test]
fn recovery_drain_multiple_messages_reuse_single_system_run() {
    let (coordinator, manager) = coordinator_and_manager();
    let first = acquire_system(
        &coordinator,
        "lead-1",
        TeamRunTargetRole::Lead,
        WorkSource::RecoveryDrain,
        None,
    );
    let run_id = first.team_run_id.clone().expect("the first drain opens a system run");
    coordinator.commit_enqueue(&first, Some("m1".into())).unwrap();

    let second = acquire_system(
        &coordinator,
        "lead-1",
        TeamRunTargetRole::Lead,
        WorkSource::RecoveryDrain,
        None,
    );
    coordinator.commit_enqueue(&second, Some("m2".into())).unwrap();

    assert_eq!(
        second.team_run_id.as_deref(),
        Some(run_id.as_str()),
        "subsequent drains must attach to the single system run, not open new ones"
    );
    assert_eq!(manager.current_active_run_id().as_deref(), Some(run_id.as_str()));
}
