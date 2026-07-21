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

#[derive(Default)]
struct RecordingController {
    calls: Mutex<Vec<String>>,
}

struct FailOnceController {
    calls: Mutex<Vec<String>>,
    failed: Mutex<bool>,
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
        let row: ExecutionResourceLeaseRow = coordinator.create(input("service", &id, 30)).await.unwrap();
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
