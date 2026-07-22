use std::sync::{Arc, Mutex};

use aionui_common::now_ms;
use aionui_db::{
    ExecutionResourceLeaseRow, IDevelopmentOperationsRepository, SqliteDevelopmentOperationsRepository,
    SqliteProjectRepository, init_database_memory,
};
use aionui_development::{
    CleanupTarget, CommandExecutionInput, DevelopmentResourceController, DevelopmentRunner, ResourceLeaseCoordinator,
    ResourceLeaseInput, RunnerContext, SecretAccessContext, SecretCreateInput, SecretGrantInput,
    SecretReferenceRequest, SecretService, SystemDevelopmentResourceController, default_policy,
};
use async_trait::async_trait;
use tokio::sync::Notify;

#[derive(Default)]
struct RecordingController {
    calls: Mutex<Vec<String>>,
}

struct FailOnceController {
    calls: Mutex<Vec<String>>,
    failed: Mutex<bool>,
}

struct BlockingController {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl RecordingController {
    fn record(&self, value: impl Into<String>) {
        self.calls.lock().unwrap().push(value.into());
    }
}

#[async_trait]
impl DevelopmentResourceController for RecordingController {
    async fn signal_agent(&self, run_id: &str) -> Result<(), String> {
        self.record(format!("signal:{run_id}"));
        Ok(())
    }

    async fn cleanup(&self, target: CleanupTarget<'_>) -> Result<(), String> {
        self.record(format!("{}:{}", target.resource_kind, target.resource_identifier));
        Ok(())
    }
}

#[async_trait]
impl DevelopmentResourceController for FailOnceController {
    async fn signal_agent(&self, run_id: &str) -> Result<(), String> {
        self.calls.lock().unwrap().push(format!("signal:{run_id}"));
        Ok(())
    }

    async fn cleanup(&self, target: CleanupTarget<'_>) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{}:{}", target.resource_kind, target.resource_identifier));
        let mut failed = self.failed.lock().unwrap();
        if !*failed {
            *failed = true;
            return Err("injected cleanup failure".into());
        }
        Ok(())
    }
}

#[async_trait]
impl DevelopmentResourceController for BlockingController {
    async fn signal_agent(&self, _run_id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn cleanup(&self, _target: CleanupTarget<'_>) -> Result<(), String> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

async fn setup() -> (
    ResourceLeaseCoordinator,
    Arc<SqliteDevelopmentOperationsRepository>,
    aionui_db::Database,
) {
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO projects (id, user_id, name, local_path, project_type, created_at, updated_at) \
         VALUES ('project-runner', 'system_default_user', 'Runner', '/tmp/runner', 'single', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO development_runs \
         (id, user_id, project_id, execution_mode, status, request_summary, acceptance_criteria, created_at, updated_at) \
         VALUES ('run-runner', 'system_default_user', 'project-runner', 'single', 'running', 'Run', '[]', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let repo = Arc::new(SqliteDevelopmentOperationsRepository::new(db.pool().clone()));
    (ResourceLeaseCoordinator::new(repo.clone(), "instance-a"), repo, db)
}

fn input(kind: &str, identifier: &str, cleanup_order: i64) -> ResourceLeaseInput {
    ResourceLeaseInput {
        user_id: "system_default_user".into(),
        project_id: "project-runner".into(),
        run_id: "run-runner".into(),
        task_id: Some("task-1".into()),
        turn_id: Some("turn-1".into()),
        gate_id: Some("gate-1".into()),
        environment_id: "host:local".into(),
        environment_kind: "host".into(),
        resource_kind: kind.into(),
        resource_identifier: identifier.into(),
        cleanup_order,
        ttl_ms: 60_000,
    }
}

#[tokio::test]
async fn managed_host_execution_persists_environment_and_terminal_process_evidence() {
    let (coordinator, repo, db) = setup().await;
    let runner = DevelopmentRunner::new(repo.clone(), coordinator, Arc::new(RecordingController::default()));
    let workspace = tempfile::tempdir().unwrap();
    let policy = default_policy("system_default_user", "project-runner");
    let output = runner
        .execute(
            CommandExecutionInput {
                execution_id: "gate-runner",
                run_id: "run-runner",
                command: "printf runner-ok",
                working_directory: workspace.path(),
                timeout_seconds: 5,
                policy: &policy,
                runtime_profile: None,
                environment: Default::default(),
            },
            &RunnerContext {
                user_id: "system_default_user".into(),
                project_id: "project-runner".into(),
                run_id: "run-runner".into(),
                task_id: None,
                turn_id: Some("turn-runner".into()),
                gate_id: Some("gate-runner".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(output.status, "passed");
    assert_eq!(output.stdout, "runner-ok");

    let binding_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM execution_environment_bindings WHERE environment_id = 'host:local'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(binding_count, 3);
    let leases = repo
        .list_resource_leases("system_default_user", "run-runner", false)
        .await
        .unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].resource_kind, "process");
    assert_eq!(leases[0].status, "released");
    assert_eq!(leases[0].cleanup_result.as_deref(), Some("passed"));
}

#[tokio::test]
async fn runner_materializes_opaque_secret_references_only_for_the_leased_child() {
    let (coordinator, repo, db) = setup().await;
    let secrets = SecretService::new(
        repo.clone(),
        Arc::new(SqliteProjectRepository::new(db.pool().clone())),
        Arc::new([9_u8; 32]),
    );
    let secret = secrets
        .create(
            "system_default_user",
            "project-runner",
            SecretCreateInput {
                name: "Runner token".into(),
                value: "runner-plaintext-secret".into(),
                expires_at: None,
            },
        )
        .await
        .unwrap();
    secrets
        .grant(
            "system_default_user",
            SecretGrantInput {
                secret_id: secret.id.clone(),
                scope_type: "run".into(),
                scope_id: "run-runner".into(),
                environment_key: "RUNNER_TOKEN".into(),
                expires_at: None,
            },
        )
        .await
        .unwrap();
    let runner =
        DevelopmentRunner::new(repo, coordinator, Arc::new(RecordingController::default())).with_secrets(secrets);
    let workspace = tempfile::tempdir().unwrap();
    let mut policy = default_policy("system_default_user", "project-runner");
    policy.allowed_secret_keys_json = "[\"RUNNER_TOKEN\"]".into();

    let output = runner
        .execute_with_secret_references(
            CommandExecutionInput {
                execution_id: "gate-secret",
                run_id: "run-runner",
                command: "printf %s \"$RUNNER_TOKEN\"",
                working_directory: workspace.path(),
                timeout_seconds: 5,
                policy: &policy,
                runtime_profile: None,
                environment: Default::default(),
            },
            &RunnerContext {
                user_id: "system_default_user".into(),
                project_id: "project-runner".into(),
                run_id: "run-runner".into(),
                task_id: None,
                turn_id: Some("turn-secret".into()),
                gate_id: Some("gate-secret".into()),
            },
            &SecretAccessContext {
                project_id: "project-runner".into(),
                run_id: Some("run-runner".into()),
                agent_id: None,
            },
            &[SecretReferenceRequest {
                secret_id: secret.id,
                environment_key: "RUNNER_TOKEN".into(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(output.status, "passed");
    assert_eq!(output.stdout, "[REDACTED]");
    let serialized = serde_json::to_string(&output).unwrap();
    assert!(!serialized.contains("runner-plaintext-secret"));
}

#[tokio::test]
async fn cancellation_uses_fixed_cleanup_order_and_is_idempotent() {
    let (coordinator, repo, _db) = setup().await;
    for item in [
        input("workspace", "workspace-1", 60),
        input("port", "4173", 50),
        input("service", "preview-1", 30),
        input("container", "container-1", 40),
        input("process", "4321", 20),
    ] {
        coordinator.create(item).await.unwrap();
    }
    let controller = RecordingController::default();

    coordinator
        .cancel_run("system_default_user", "run-runner", &controller)
        .await
        .unwrap();
    coordinator
        .cancel_run("system_default_user", "run-runner", &controller)
        .await
        .unwrap();

    assert_eq!(
        controller.calls.lock().unwrap().as_slice(),
        [
            "signal:run-runner",
            "process:4321",
            "service:preview-1",
            "container:container-1",
            "port:4173",
            "workspace:workspace-1",
        ]
    );
    let leases = repo
        .list_resource_leases("system_default_user", "run-runner", false)
        .await
        .unwrap();
    assert_eq!(leases.len(), 5);
    assert!(
        leases
            .iter()
            .all(|lease| lease.status == "released" && lease.accepts_work == 0)
    );
}

#[tokio::test]
async fn cancellation_retries_only_resources_whose_cleanup_failed() {
    let (coordinator, repo, _db) = setup().await;
    coordinator.create(input("process", "4321", 20)).await.unwrap();
    let controller = FailOnceController {
        calls: Mutex::new(Vec::new()),
        failed: Mutex::new(false),
    };

    assert!(
        coordinator
            .cancel_run("system_default_user", "run-runner", &controller)
            .await
            .is_err()
    );
    coordinator
        .cancel_run("system_default_user", "run-runner", &controller)
        .await
        .unwrap();

    assert_eq!(
        controller.calls.lock().unwrap().as_slice(),
        ["signal:run-runner", "process:4321", "signal:run-runner", "process:4321",]
    );
    let leases = repo
        .list_resource_leases("system_default_user", "run-runner", false)
        .await
        .unwrap();
    assert_eq!(leases[0].status, "released");
    assert_eq!(leases[0].cleanup_status.as_deref(), Some("released"));
}

#[tokio::test]
async fn stale_cleanup_cannot_terminalize_a_lease_after_takeover() {
    let (coordinator, repo, _db) = setup().await;
    let mut lease = coordinator
        .create(input("service", "blocked-cleanup", 30))
        .await
        .unwrap();
    lease.heartbeat_at = 1;
    lease.expires_at = 2;
    lease.updated_at = now_ms();
    repo.upsert_resource_lease(&lease).await.unwrap();

    let controller = Arc::new(BlockingController {
        started: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    });
    let cancel_coordinator = coordinator.clone();
    let cancel_controller = controller.clone();
    let cancellation = tokio::spawn(async move {
        cancel_coordinator
            .cancel_run("system_default_user", "run-runner", cancel_controller.as_ref())
            .await
    });
    controller.started.notified().await;

    let stopping = repo.get_resource_lease(&lease.id).await.unwrap().unwrap();
    assert_eq!(stopping.status, "stopping");
    assert!(coordinator.complete(&lease, "late normal completion").await.is_err());
    let orphaned = coordinator.reconcile_stale(stopping.expires_at + 1).await.unwrap();
    assert_eq!(orphaned.len(), 1);
    assert_eq!(orphaned[0].status, "orphaned");

    let new_owner = ResourceLeaseCoordinator::new(repo.clone(), "instance-takeover");
    let taken_over = new_owner.record_recovery_decision(&lease.id, "takeover").await.unwrap();
    assert_eq!(taken_over.status, "active");
    assert_eq!(taken_over.owner_instance_id, "instance-takeover");

    controller.release.notify_one();
    assert!(cancellation.await.unwrap().is_err());
    let persisted = repo.get_resource_lease(&lease.id).await.unwrap().unwrap();
    assert_eq!(persisted.owner_instance_id, "instance-takeover");
    assert_eq!(persisted.status, "active");
    assert_eq!(persisted.accepts_work, 1);
    assert!(persisted.terminal_at.is_none());
}

#[tokio::test]
async fn cancellation_does_not_succeed_while_an_unclaimed_cleanup_is_in_progress() {
    let (coordinator, repo, _db) = setup().await;
    let mut lease = coordinator
        .create(input("service", "already-stopping", 30))
        .await
        .unwrap();
    lease.status = "stopping".into();
    lease.accepts_work = 0;
    lease.updated_at = now_ms();
    repo.upsert_resource_lease(&lease).await.unwrap();

    let result = coordinator
        .cancel_run("system_default_user", "run-runner", &RecordingController::default())
        .await;
    assert!(result.is_err());
    let persisted = repo.get_resource_lease(&lease.id).await.unwrap().unwrap();
    assert_eq!(persisted.status, "stopping");
    assert!(persisted.terminal_at.is_none());
}

#[tokio::test]
async fn stale_reconciliation_and_recovery_decisions_are_persisted_without_sensitive_payloads() {
    let (coordinator, repo, _db) = setup().await;
    let mut lease = coordinator.create(input("process", "9876", 20)).await.unwrap();
    lease.heartbeat_at = now_ms() - 120_000;
    lease.expires_at = now_ms() - 60_000;
    repo.upsert_resource_lease(&lease).await.unwrap();

    let stale = coordinator.reconcile_stale(now_ms()).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].status, "orphaned");

    for decision in ["retry", "rollback", "takeover", "terminate"] {
        let id = format!("decision-{decision}");
        let mut row: ExecutionResourceLeaseRow = coordinator.create(input("service", &id, 30)).await.unwrap();
        if decision == "takeover" {
            row.heartbeat_at = now_ms() - 120_000;
            row.expires_at = now_ms() - 60_000;
            repo.upsert_resource_lease(&row).await.unwrap();
            coordinator.reconcile_stale(now_ms()).await.unwrap();
        }
        let first = coordinator.record_recovery_decision(&row.id, decision).await.unwrap();
        let second = coordinator.record_recovery_decision(&row.id, decision).await.unwrap();
        assert_eq!(first.recovery_decision.as_deref(), Some(decision));
        assert_eq!(first, second);
    }

    let serialized = serde_json::to_string(
        &repo
            .list_resource_leases("system_default_user", "run-runner", false)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(!serialized.contains("command"));
    assert!(!serialized.contains("secret"));
}

#[tokio::test]
async fn takeover_rejects_terminal_leases() {
    let (coordinator, repo, _db) = setup().await;
    let takeover = ResourceLeaseCoordinator::new(repo.clone(), "instance-takeover");
    for previously_taken_over in [false, true] {
        let identifier = format!("terminal-takeover-{previously_taken_over}");
        let mut lease = coordinator.create(input("service", &identifier, 30)).await.unwrap();
        if previously_taken_over {
            lease.heartbeat_at = now_ms() - 120_000;
            lease.expires_at = now_ms() - 60_000;
            repo.upsert_resource_lease(&lease).await.unwrap();
            coordinator.reconcile_stale(now_ms()).await.unwrap();
            lease = takeover.record_recovery_decision(&lease.id, "takeover").await.unwrap();
            takeover.complete(&lease, "done").await.unwrap();
        } else {
            coordinator.complete(&lease, "done").await.unwrap();
        }

        let next_owner = ResourceLeaseCoordinator::new(repo.clone(), "instance-next");
        assert!(
            next_owner
                .record_recovery_decision(&lease.id, "takeover")
                .await
                .is_err()
        );
        let persisted = repo.get_resource_lease(&lease.id).await.unwrap().unwrap();
        assert_eq!(persisted.status, "released");
        assert_eq!(persisted.accepts_work, 0);
        assert!(persisted.terminal_at.is_some());
    }
}

#[tokio::test]
async fn conflicting_resource_recovery_decisions_are_atomic_and_preserve_the_winner() {
    let (coordinator, repo, _db) = setup().await;
    let row = coordinator.create(input("service", "decision-race", 30)).await.unwrap();
    let takeover = ResourceLeaseCoordinator::new(repo.clone(), "instance-takeover");

    let (rollback_result, takeover_result) = tokio::join!(
        coordinator.record_recovery_decision(&row.id, "rollback"),
        takeover.record_recovery_decision(&row.id, "takeover")
    );
    assert_ne!(rollback_result.is_ok(), takeover_result.is_ok());

    let persisted = repo.get_resource_lease(&row.id).await.unwrap().unwrap();
    match persisted.recovery_decision.as_deref() {
        Some("rollback") => assert_eq!(persisted.owner_instance_id, "instance-a"),
        Some("takeover") => assert_eq!(persisted.owner_instance_id, "instance-takeover"),
        decision => panic!("unexpected recovery decision: {decision:?}"),
    }
}

#[tokio::test]
async fn concurrent_takeovers_allow_only_the_persisted_owner_to_succeed() {
    let (original_owner, repo, _db) = setup().await;
    let mut lease = original_owner
        .create(input("service", "concurrent-takeover", 30))
        .await
        .unwrap();
    lease.heartbeat_at = now_ms() - 120_000;
    lease.expires_at = now_ms() - 60_000;
    repo.upsert_resource_lease(&lease).await.unwrap();
    original_owner.reconcile_stale(now_ms()).await.unwrap();

    let owner_b = ResourceLeaseCoordinator::new(repo.clone(), "instance-b");
    let owner_c = ResourceLeaseCoordinator::new(repo.clone(), "instance-c");
    let (result_b, result_c) = tokio::join!(
        owner_b.record_recovery_decision(&lease.id, "takeover"),
        owner_c.record_recovery_decision(&lease.id, "takeover")
    );
    assert_ne!(result_b.is_ok(), result_c.is_ok());
    let persisted = repo.get_resource_lease(&lease.id).await.unwrap().unwrap();
    assert_eq!(
        persisted.owner_instance_id,
        if result_b.is_ok() { "instance-b" } else { "instance-c" }
    );
    assert_eq!(persisted.status, "active");
    assert_eq!(persisted.accepts_work, 1);
}

#[tokio::test]
async fn a_new_orphan_epoch_accepts_a_different_recovery_decision() {
    let (original_owner, repo, _db) = setup().await;
    let mut lease = original_owner
        .create(input("service", "recovery-epoch", 30))
        .await
        .unwrap();
    lease.heartbeat_at = now_ms() - 120_000;
    lease.expires_at = now_ms() - 60_000;
    repo.upsert_resource_lease(&lease).await.unwrap();
    original_owner.reconcile_stale(now_ms()).await.unwrap();

    let takeover = ResourceLeaseCoordinator::new(repo.clone(), "instance-takeover");
    let mut active = takeover.record_recovery_decision(&lease.id, "takeover").await.unwrap();
    assert_eq!(active.recovery_decision.as_deref(), Some("takeover"));
    active.heartbeat_at = now_ms() - 120_000;
    active.expires_at = now_ms() - 60_000;
    repo.upsert_resource_lease(&active).await.unwrap();
    let orphaned = takeover.reconcile_stale(now_ms()).await.unwrap();
    assert_eq!(orphaned.len(), 1);
    assert_eq!(orphaned[0].status, "orphaned");
    assert_eq!(orphaned[0].recovery_decision, None);

    let terminated = takeover.record_recovery_decision(&lease.id, "terminate").await.unwrap();
    assert_eq!(terminated.recovery_decision.as_deref(), Some("terminate"));
}

#[tokio::test]
async fn takeover_reactivates_an_orphaned_lease_and_fences_the_previous_owner() {
    let (original_owner, repo, _db) = setup().await;
    let mut lease = original_owner
        .create(input("service", "takeover-fencing", 30))
        .await
        .unwrap();
    lease.heartbeat_at = now_ms() - 120_000;
    lease.expires_at = now_ms() - 60_000;
    repo.upsert_resource_lease(&lease).await.unwrap();
    original_owner.reconcile_stale(now_ms()).await.unwrap();

    let new_owner = ResourceLeaseCoordinator::new(repo.clone(), "instance-takeover");
    let mut taken_over = new_owner.record_recovery_decision(&lease.id, "takeover").await.unwrap();
    assert_eq!(taken_over.owner_instance_id, "instance-takeover");
    assert_eq!(taken_over.status, "active");
    assert_eq!(taken_over.accepts_work, 1);
    assert!(taken_over.expires_at > taken_over.heartbeat_at);

    assert!(original_owner.heartbeat(&mut lease, 60_000).await.is_err());
    assert!(new_owner.heartbeat(&mut taken_over, 60_000).await.is_ok());

    let mut second_loss = repo.get_resource_lease(&lease.id).await.unwrap().unwrap();
    second_loss.heartbeat_at = now_ms() - 120_000;
    second_loss.expires_at = now_ms() - 60_000;
    repo.upsert_resource_lease(&second_loss).await.unwrap();
    new_owner.reconcile_stale(now_ms()).await.unwrap();
    let third_owner = ResourceLeaseCoordinator::new(repo.clone(), "instance-third");
    let retaken = third_owner
        .record_recovery_decision(&lease.id, "takeover")
        .await
        .unwrap();
    assert_eq!(retaken.owner_instance_id, "instance-third");
    assert_eq!(retaken.status, "active");
    assert_eq!(retaken.accepts_work, 1);

    assert!(
        repo.heartbeat_resource_lease(
            &lease.id,
            "instance-takeover",
            taken_over.updated_at,
            now_ms(),
            now_ms() + 60_000,
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        !repo
            .complete_resource_lease(
                &lease.id,
                "instance-takeover",
                taken_over.updated_at,
                "stale in-flight owner",
                now_ms(),
            )
            .await
            .unwrap()
    );
    assert!(original_owner.complete(&lease, "stale owner").await.is_err());
    assert!(new_owner.complete(&taken_over, "previous owner").await.is_err());
    assert_eq!(
        third_owner.complete(&retaken, "new owner").await.unwrap().status,
        "released"
    );
}

#[tokio::test]
async fn same_instance_takeover_fences_the_old_execution_epoch() {
    let (coordinator, repo, _db) = setup().await;
    let mut old_epoch = coordinator
        .create(input("service", "same-instance-epoch", 30))
        .await
        .unwrap();
    old_epoch.heartbeat_at = now_ms() - 120_000;
    old_epoch.expires_at = now_ms() - 60_000;
    repo.upsert_resource_lease(&old_epoch).await.unwrap();
    coordinator.reconcile_stale(now_ms()).await.unwrap();

    let mut new_epoch = coordinator
        .record_recovery_decision(&old_epoch.id, "takeover")
        .await
        .unwrap();
    assert_eq!(new_epoch.owner_instance_id, old_epoch.owner_instance_id);
    assert_ne!(new_epoch.updated_at, old_epoch.updated_at);

    assert!(coordinator.heartbeat(&mut old_epoch, 60_000).await.is_err());
    assert!(coordinator.complete(&old_epoch, "stale completion").await.is_err());
    assert!(coordinator.heartbeat(&mut new_epoch, 60_000).await.is_ok());
    assert_eq!(
        coordinator
            .complete(&new_epoch, "current completion")
            .await
            .unwrap()
            .status,
        "released"
    );
}

async fn docker_container_ids(filter: &str) -> Vec<String> {
    let mut command = aionui_runtime::Builder::clean_cli("docker");
    command.args(["ps", "--all", "--quiet", "--filter", filter]);
    let output = command
        .output()
        .await
        .expect("Docker must be installed for live acceptance");
    assert!(
        output.status.success(),
        "Docker query failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
#[ignore = "requires a live Docker daemon and the alpine:3.20 image"]
async fn live_docker_runner_executes_redacts_and_removes_timed_out_containers() {
    let (coordinator, repo, _db) = setup().await;
    let runner = DevelopmentRunner::new(repo.clone(), coordinator, Arc::new(SystemDevelopmentResourceController));
    let workspace = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(workspace.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    }
    let mut policy = default_policy("system_default_user", "project-runner");
    policy.isolation_mode = "docker".into();
    policy.container_image = Some("alpine:3.20".into());
    policy.allowed_secret_keys_json = "[\"LIVE_SECRET\"]".into();
    let secret = "live-docker-secret-must-not-leak";

    let output = runner
        .execute(
            CommandExecutionInput {
                execution_id: "live-docker-success",
                run_id: "run-runner",
                command: "printf 'container-ok:%s' \"$LIVE_SECRET\"; printf artifact > result.txt",
                working_directory: workspace.path(),
                timeout_seconds: 15,
                policy: &policy,
                runtime_profile: None,
                environment: [("LIVE_SECRET".into(), secret.into())].into(),
            },
            &RunnerContext {
                user_id: "system_default_user".into(),
                project_id: "project-runner".into(),
                run_id: "run-runner".into(),
                task_id: Some("task-live-docker".into()),
                turn_id: Some("turn-live-docker".into()),
                gate_id: Some("gate-live-docker".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(output.status, "passed", "Docker runner stderr: {}", output.stderr);
    assert_eq!(output.stdout, "container-ok:[REDACTED]");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
        "artifact"
    );
    assert!(!serde_json::to_string(&output).unwrap().contains(secret));
    assert!(docker_container_ids("name=aion-live-docker-success").await.is_empty());

    let timed_out = runner
        .execute(
            CommandExecutionInput {
                execution_id: "live-docker-timeout",
                run_id: "run-runner",
                command: "sleep 30",
                working_directory: workspace.path(),
                timeout_seconds: 1,
                policy: &policy,
                runtime_profile: None,
                environment: Default::default(),
            },
            &RunnerContext {
                user_id: "system_default_user".into(),
                project_id: "project-runner".into(),
                run_id: "run-runner".into(),
                task_id: Some("task-live-timeout".into()),
                turn_id: Some("turn-live-timeout".into()),
                gate_id: Some("gate-live-timeout".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(timed_out.status, "timed_out");
    assert!(docker_container_ids("name=aion-live-docker-timeout").await.is_empty());

    let leases = repo
        .list_resource_leases("system_default_user", "run-runner", false)
        .await
        .unwrap();
    assert!(leases.iter().all(|lease| lease.status == "released"));
}

#[tokio::test]
#[ignore = "requires a live Docker daemon and the Dev Container CLI"]
async fn live_devcontainer_runner_executes_then_cancellation_removes_the_service() {
    let (coordinator, repo, _db) = setup().await;
    let runner = DevelopmentRunner::new(
        repo.clone(),
        coordinator.clone(),
        Arc::new(SystemDevelopmentResourceController),
    );
    let workspace = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(workspace.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    }
    let config_dir = workspace.path().join(".devcontainer");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("devcontainer.json"),
        r#"{"image":"alpine:3.20","remoteUser":"root"}"#,
    )
    .unwrap();
    let mut policy = default_policy("system_default_user", "project-runner");
    policy.isolation_mode = "devcontainer".into();
    policy.devcontainer_config_path = Some(".devcontainer/devcontainer.json".into());

    let output = runner
        .execute(
            CommandExecutionInput {
                execution_id: "live-devcontainer",
                run_id: "run-runner",
                command: "printf devcontainer-ok; printf artifact > result.txt",
                working_directory: workspace.path(),
                timeout_seconds: 60,
                policy: &policy,
                runtime_profile: None,
                environment: Default::default(),
            },
            &RunnerContext {
                user_id: "system_default_user".into(),
                project_id: "project-runner".into(),
                run_id: "run-runner".into(),
                task_id: Some("task-live-devcontainer".into()),
                turn_id: Some("turn-live-devcontainer".into()),
                gate_id: Some("gate-live-devcontainer".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(output.status, "passed");
    assert!(output.stdout.ends_with("devcontainer-ok"));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
        "artifact"
    );

    let folder_filter = format!("label=devcontainer.local_folder={}", workspace.path().display());
    assert_eq!(docker_container_ids(&folder_filter).await.len(), 1);
    coordinator
        .cancel_run(
            "system_default_user",
            "run-runner",
            &SystemDevelopmentResourceController,
        )
        .await
        .unwrap();
    assert!(docker_container_ids(&folder_filter).await.is_empty());

    let leases = repo
        .list_resource_leases("system_default_user", "run-runner", false)
        .await
        .unwrap();
    assert!(leases.iter().all(|lease| lease.status == "released"));
}
