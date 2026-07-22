use std::sync::Arc;

use aionui_common::now_ms;
use aionui_db::models::{
    DevelopmentRunRoleRow, DevelopmentRunRow, DevelopmentUsageEventRow, ProjectRow, QualityGateRunRow,
};
use aionui_db::{
    IDevelopmentOperationsRepository, IDevelopmentRepository, IProjectRepository, SqliteAgentWorkspaceLeaseRepository,
    SqliteDevelopmentOperationsRepository, SqliteDevelopmentRepository, SqliteProjectRepository, init_database_memory,
};
use aionui_development::{
    DevelopmentOperationsService, DevelopmentPolicyInput, PolicyDecision, PolicyOperation, RecoveryDecisionInput,
    ResourceLeaseCoordinator, ResourceLeaseInput, SystemDevelopmentResourceController, redact_sensitive,
};

struct Fixture {
    service: DevelopmentOperationsService,
    operations_repo: Arc<SqliteDevelopmentOperationsRepository>,
    development_repo: Arc<SqliteDevelopmentRepository>,
    _project: tempfile::TempDir,
}

async fn setup(run_started_at: i64) -> Fixture {
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('user-ops', 'operations', '', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let project = tempfile::tempdir().unwrap();
    git2::Repository::init(project.path()).unwrap();
    let project_repo = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
    project_repo
        .create(&ProjectRow {
            id: "project-ops".into(),
            user_id: "user-ops".into(),
            name: "Operations".into(),
            local_path: project.path().to_string_lossy().into_owned(),
            repository_url: None,
            default_branch: Some("main".into()),
            project_type: "single".into(),
            created_at: 1,
            updated_at: 1,
        })
        .await
        .unwrap();
    let development_repo = Arc::new(SqliteDevelopmentRepository::new(db.pool().clone()));
    development_repo
        .create_run(&DevelopmentRunRow {
            id: "run-ops".into(),
            user_id: "user-ops".into(),
            project_id: "project-ops".into(),
            team_id: Some("team-ops".into()),
            source_channel: None,
            source_user_id: None,
            execution_mode: "team".into(),
            status: "running".into(),
            request_summary: "Operate".into(),
            acceptance_criteria: "[]".into(),
            baseline_commit: None,
            integration_branch: Some("aion/run/ops/integration".into()),
            started_at: Some(run_started_at),
            finished_at: None,
            created_at: run_started_at,
            updated_at: run_started_at,
        })
        .await
        .unwrap();
    let operations_repo = Arc::new(SqliteDevelopmentOperationsRepository::new(db.pool().clone()));
    let service = DevelopmentOperationsService::new(
        operations_repo.clone(),
        development_repo.clone(),
        project_repo,
        Arc::new(SqliteAgentWorkspaceLeaseRepository::new(db.pool().clone())),
    )
    .with_resources(
        ResourceLeaseCoordinator::new(operations_repo.clone(), "instance-ops"),
        Arc::new(SystemDevelopmentResourceController),
    );
    Fixture {
        service,
        operations_repo,
        development_repo,
        _project: project,
    }
}

fn policy() -> DevelopmentPolicyInput {
    DevelopmentPolicyInput {
        isolation_mode: "docker".into(),
        container_image: Some("node:24-alpine".into()),
        devcontainer_config_path: None,
        container_cpu_millis: 750,
        container_memory_mb: 1024,
        container_pids_limit: 128,
        network_mode: "none".into(),
        allowed_secret_keys: vec!["NPM_TOKEN".into()],
        allowed_commands: vec![],
        protected_paths: vec![],
        allowed_network_hosts: vec![],
        protected_branches: vec!["main".into()],
        dangerous_confirmation_count: 2,
        max_duration_ms: 60_000,
        max_parallel_agents: 2,
        max_retries: 1,
        max_cost_microunits: 1_000,
        max_total_tokens: 0,
        fallback_model: None,
        alert_percent: 80,
        over_limit_action: "pause".into(),
    }
}

#[tokio::test]
async fn default_policy_is_safe_and_updates_store_secret_names_only() {
    let fixture = setup(now_ms()).await;
    let default = fixture.service.get_policy("user-ops", "project-ops").await.unwrap();
    assert_eq!(default.isolation_mode, "host");
    assert_eq!(default.over_limit_action, "pause");

    let stored = fixture
        .service
        .upsert_policy("user-ops", "project-ops", policy())
        .await
        .unwrap();
    assert_eq!(stored.allowed_secret_keys_json, "[\"NPM_TOKEN\"]");
    assert!(!stored.allowed_secret_keys_json.contains("secret"));

    let mut invalid = policy();
    invalid.allowed_secret_keys = vec!["NPM_TOKEN=secret".into()];
    assert!(
        fixture
            .service
            .upsert_policy("user-ops", "project-ops", invalid)
            .await
            .is_err()
    );
    assert!(fixture.service.get_policy("other", "project-ops").await.is_err());
}

#[tokio::test]
async fn budget_blocks_before_side_effect_and_deduplicates_alerts() {
    let fixture = setup(now_ms()).await;
    fixture
        .service
        .upsert_policy("user-ops", "project-ops", policy())
        .await
        .unwrap();
    fixture
        .operations_repo
        .append_usage(&DevelopmentUsageEventRow {
            id: "usage-over".into(),
            user_id: "user-ops".into(),
            project_id: "project-ops".into(),
            run_id: Some("run-ops".into()),
            task_id: None,
            usage_type: "agent_turn".into(),
            source: "provider".into(),
            confidence: "reported".into(),
            input_tokens: 100,
            output_tokens: 50,
            cost_microunits: 1_001,
            duration_ms: 100,
            retry_count: 0,
            metadata_json: "{}".into(),
            created_at: now_ms(),
        })
        .await
        .unwrap();

    let first = fixture
        .service
        .evaluate_budget("user-ops", "run-ops", "quality_gate", 0)
        .await
        .unwrap();
    let second = fixture
        .service
        .evaluate_budget("user-ops", "run-ops", "quality_gate", 0)
        .await
        .unwrap();
    assert!(!first.allowed);
    assert_eq!(first.action, "pause");
    assert!(first.reasons.iter().any(|reason| reason.contains("cost")));
    assert_eq!(second.reasons, first.reasons);

    let snapshot = fixture
        .service
        .snapshot("user-ops", "project-ops", Some("run-ops"))
        .await
        .unwrap();
    assert_eq!(snapshot.alerts.len(), 1);
    assert!(snapshot.audit.iter().any(|event| event.result == "denied"));
}

#[tokio::test]
async fn concurrency_and_retry_limits_are_evaluated_from_persisted_state() {
    let fixture = setup(now_ms()).await;
    let mut configured = policy();
    configured.max_parallel_agents = 1;
    configured.max_retries = 0;
    configured.max_cost_microunits = 0;
    fixture
        .service
        .upsert_policy("user-ops", "project-ops", configured)
        .await
        .unwrap();
    fixture
        .development_repo
        .assign_role(&DevelopmentRunRoleRow {
            run_id: "run-ops".into(),
            slot_id: "slot-1".into(),
            role: "implementer".into(),
            assigned_at: 1,
        })
        .await
        .unwrap();

    let parallel = fixture
        .service
        .evaluate_budget("user-ops", "run-ops", "assign_role", 0)
        .await
        .unwrap();
    let retry = fixture
        .service
        .evaluate_budget("user-ops", "run-ops", "quality_gate", 1)
        .await
        .unwrap();
    assert!(!parallel.allowed);
    assert!(parallel.reasons.iter().any(|reason| reason.contains("parallel")));
    assert!(!retry.allowed);
    assert!(retry.reasons.iter().any(|reason| reason.contains("retry")));
}

#[tokio::test]
async fn token_budget_returns_an_audited_model_downgrade_decision() {
    let fixture = setup(now_ms()).await;
    let mut configured = policy();
    configured.max_cost_microunits = 0;
    configured.max_total_tokens = 100;
    configured.over_limit_action = "downgrade_model".into();
    configured.fallback_model = Some("claude-haiku".into());
    configured.allowed_commands = vec!["cargo".into(), "bun".into()];
    configured.protected_paths = vec![".github/workflows".into()];
    configured.allowed_network_hosts = vec!["api.github.com".into()];
    configured.protected_branches = vec!["main".into()];
    configured.dangerous_confirmation_count = 2;
    fixture
        .service
        .upsert_policy("user-ops", "project-ops", configured)
        .await
        .unwrap();
    fixture
        .operations_repo
        .append_usage(&DevelopmentUsageEventRow {
            id: "usage-token-over".into(),
            user_id: "user-ops".into(),
            project_id: "project-ops".into(),
            run_id: Some("run-ops".into()),
            task_id: None,
            usage_type: "agent_turn".into(),
            source: "provider".into(),
            confidence: "reported".into(),
            input_tokens: 101,
            output_tokens: 1,
            cost_microunits: 0,
            duration_ms: 1,
            retry_count: 0,
            metadata_json: "{}".into(),
            created_at: now_ms(),
        })
        .await
        .unwrap();

    let decision = fixture
        .service
        .evaluate_budget("user-ops", "run-ops", "agent_turn", 0)
        .await
        .unwrap();
    assert!(!decision.allowed);
    assert_eq!(decision.action, "downgrade_model");
    assert_eq!(decision.replacement_model.as_deref(), Some("claude-haiku"));
    assert!(decision.reasons.iter().any(|reason| reason.contains("token")));

    let snapshot = fixture
        .service
        .snapshot("user-ops", "project-ops", Some("run-ops"))
        .await
        .unwrap();
    let audit = snapshot
        .audit
        .iter()
        .find(|event| event.action == "budget.agent_turn")
        .unwrap();
    assert!(audit.redacted_payload_json.contains("claude-haiku"));
    assert!(audit.redacted_payload_json.contains("downgrade_model"));
}

#[tokio::test]
async fn persisted_policy_decisions_are_owner_scoped_and_audited() {
    let fixture = setup(now_ms()).await;
    let mut configured = policy();
    configured.allowed_commands = vec!["cargo".into()];
    configured.allowed_network_hosts = vec!["api.github.com".into()];
    configured.protected_paths = vec![".env".into()];
    fixture
        .service
        .upsert_policy("user-ops", "project-ops", configured)
        .await
        .unwrap();

    let allowed = fixture
        .service
        .evaluate_policy(
            "user-ops",
            "project-ops",
            "run-ops",
            &PolicyOperation::Command {
                program: "cargo".into(),
            },
            0,
        )
        .await
        .unwrap();
    assert_eq!(allowed, PolicyDecision::Allowed);
    let denied = fixture
        .service
        .evaluate_policy(
            "user-ops",
            "project-ops",
            "run-ops",
            &PolicyOperation::Network {
                host: "evil.example".into(),
            },
            0,
        )
        .await
        .unwrap();
    assert!(matches!(denied, PolicyDecision::Denied { .. }));
    assert!(
        fixture
            .service
            .evaluate_policy(
                "other",
                "project-ops",
                "run-ops",
                &PolicyOperation::Command {
                    program: "cargo".into(),
                },
                0,
            )
            .await
            .is_err()
    );

    let snapshot = fixture
        .service
        .snapshot("user-ops", "project-ops", Some("run-ops"))
        .await
        .unwrap();
    assert!(
        snapshot
            .audit
            .iter()
            .any(|event| { event.action == "policy.command" && event.result == "success" })
    );
    assert!(
        snapshot
            .audit
            .iter()
            .any(|event| { event.action == "policy.network" && event.result == "denied" })
    );
}

#[tokio::test]
async fn pause_and_terminate_budget_actions_change_run_state() {
    for (action, expected_status) in [("pause", "paused"), ("terminate", "cancelled")] {
        let fixture = setup(now_ms()).await;
        let mut configured = policy();
        configured.max_cost_microunits = 1;
        configured.over_limit_action = action.into();
        fixture
            .service
            .upsert_policy("user-ops", "project-ops", configured)
            .await
            .unwrap();
        fixture
            .service
            .record_usage(DevelopmentUsageEventRow {
                id: format!("usage-{action}"),
                user_id: "user-ops".into(),
                project_id: "project-ops".into(),
                run_id: Some("run-ops".into()),
                task_id: None,
                usage_type: "agent_turn".into(),
                source: "provider".into(),
                confidence: "reported".into(),
                input_tokens: 1,
                output_tokens: 1,
                cost_microunits: 2,
                duration_ms: 1,
                retry_count: 0,
                metadata_json: "{}".into(),
                created_at: now_ms(),
            })
            .await
            .unwrap();
        let run = fixture
            .development_repo
            .get_run("run-ops", "user-ops")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, expected_status);
        assert_eq!(run.finished_at.is_some(), action == "terminate");
    }
}

#[tokio::test]
async fn notify_budget_action_warns_without_blocking_or_changing_run_state() {
    let fixture = setup(now_ms()).await;
    let mut configured = policy();
    configured.max_cost_microunits = 1;
    configured.over_limit_action = "notify".into();
    fixture
        .service
        .upsert_policy("user-ops", "project-ops", configured)
        .await
        .unwrap();
    fixture
        .operations_repo
        .append_usage(&DevelopmentUsageEventRow {
            id: "usage-notify".into(),
            user_id: "user-ops".into(),
            project_id: "project-ops".into(),
            run_id: Some("run-ops".into()),
            task_id: None,
            usage_type: "agent_turn".into(),
            source: "provider".into(),
            confidence: "reported".into(),
            input_tokens: 1,
            output_tokens: 1,
            cost_microunits: 2,
            duration_ms: 1,
            retry_count: 0,
            metadata_json: "{}".into(),
            created_at: now_ms(),
        })
        .await
        .unwrap();

    let evaluation = fixture
        .service
        .evaluate_budget("user-ops", "run-ops", "agent_turn", 0)
        .await
        .unwrap();
    assert!(evaluation.allowed);
    assert_eq!(evaluation.action, "notify");
    assert_eq!(
        fixture
            .development_repo
            .get_run("run-ops", "user-ops")
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
}

#[test]
fn shared_redaction_removes_named_secrets_and_common_credentials() {
    let redacted = redact_sensitive(
        "Authorization bearer abc123 NPM_TOKEN=s3cr3t https://user:pass@example.com ghp_abcdefghijklmnop",
        &["s3cr3t".into()],
    );
    assert!(!redacted.contains("abc123"));
    assert!(!redacted.contains("s3cr3t"));
    assert!(!redacted.contains("user:pass"));
    assert!(!redacted.contains("ghp_abcdefghijklmnop"));
    assert!(redacted.contains("[REDACTED]"));
}

#[tokio::test]
async fn recovery_scan_and_decisions_are_idempotent_and_audited() {
    let fixture = setup(1).await;
    fixture
        .development_repo
        .create_gate(&QualityGateRunRow {
            id: "gate-interrupted".into(),
            run_id: "run-ops".into(),
            task_id: None,
            gate_type: "unit_test".into(),
            command: "sleep 100".into(),
            working_directory: fixture._project.path().to_string_lossy().into_owned(),
            exit_code: None,
            status: "running".into(),
            stdout_artifact_id: None,
            stderr_artifact_id: None,
            duration_ms: None,
            isolation_mode: "host".into(),
            execution_id: Some("gate-interrupted".into()),
            required: true,
            started_at: Some(1),
            finished_at: None,
            created_at: 1,
        })
        .await
        .unwrap();
    let first = fixture.service.reconcile_stale_runs(10).await.unwrap();
    let second = fixture.service.reconcile_stale_runs(10).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(first[0].id, second[0].id);
    assert!(first[0].finding.contains("workspace lease"));
    assert_eq!(
        fixture.development_repo.list_gates("run-ops", None).await.unwrap()[0].status,
        "interrupted"
    );

    let resumed = fixture
        .service
        .decide_recovery(
            "user-ops",
            "run-ops",
            RecoveryDecisionInput {
                action: "resume".into(),
            },
        )
        .await
        .unwrap();
    let resumed_again = fixture
        .service
        .decide_recovery(
            "user-ops",
            "run-ops",
            RecoveryDecisionInput {
                action: "resume".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(resumed.status_after.as_deref(), Some("running"));
    assert_eq!(resumed.id, resumed_again.id);

    let snapshot = fixture
        .service
        .snapshot("user-ops", "project-ops", Some("run-ops"))
        .await
        .unwrap();
    assert_eq!(snapshot.recovery.len(), 1);
    assert!(snapshot.audit.iter().any(|event| event.action == "recovery.resume"));
}

#[tokio::test]
async fn recovery_rejects_healthy_and_succeeded_runs_but_allows_paused_runs() {
    let fixture = setup(now_ms()).await;
    let retry = RecoveryDecisionInput { action: "retry".into() };

    assert!(
        fixture
            .service
            .decide_recovery("user-ops", "run-ops", retry.clone())
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .development_repo
            .get_run("run-ops", "user-ops")
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );

    fixture
        .development_repo
        .update_run_status("run-ops", "user-ops", "succeeded", Some(now_ms()))
        .await
        .unwrap();
    assert!(
        fixture
            .service
            .decide_recovery("user-ops", "run-ops", retry.clone())
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .development_repo
            .get_run("run-ops", "user-ops")
            .await
            .unwrap()
            .unwrap()
            .status,
        "succeeded"
    );

    fixture
        .development_repo
        .update_run_status("run-ops", "user-ops", "paused", None)
        .await
        .unwrap();
    let recovered = fixture
        .service
        .decide_recovery("user-ops", "run-ops", retry)
        .await
        .unwrap();
    assert_eq!(recovered.status_before.as_deref(), Some("paused"));
    assert_eq!(recovered.status_after.as_deref(), Some("running"));
}

#[tokio::test]
async fn conflicting_run_recovery_decisions_are_atomic_and_preserve_the_winner() {
    let fixture = setup(now_ms()).await;
    fixture
        .development_repo
        .update_run_status("run-ops", "user-ops", "paused", None)
        .await
        .unwrap();

    let (retry_result, rollback_result) = tokio::join!(
        fixture
            .service
            .decide_recovery("user-ops", "run-ops", RecoveryDecisionInput { action: "retry".into() },),
        fixture.service.decide_recovery(
            "user-ops",
            "run-ops",
            RecoveryDecisionInput {
                action: "rollback".into(),
            },
        )
    );
    assert_ne!(retry_result.is_ok(), rollback_result.is_ok());

    let run = fixture
        .development_repo
        .get_run("run-ops", "user-ops")
        .await
        .unwrap()
        .unwrap();
    let recovery = fixture
        .service
        .snapshot("user-ops", "project-ops", Some("run-ops"))
        .await
        .unwrap()
        .recovery
        .into_iter()
        .find(|row| row.recovery_key == "run:run-ops:stale")
        .unwrap();
    match recovery.decision.as_str() {
        "retry" => assert_eq!(run.status, "running"),
        "rollback" => assert_eq!(run.status, "cancelled"),
        decision => panic!("unexpected recovery decision: {decision}"),
    }
}

#[tokio::test]
async fn failed_takeover_keeps_the_run_paused_and_same_action_can_retry() {
    let fixture = setup(now_ms()).await;
    fixture
        .development_repo
        .update_run_status("run-ops", "user-ops", "paused", None)
        .await
        .unwrap();
    let coordinator = ResourceLeaseCoordinator::new(fixture.operations_repo.clone(), "instance-ops");
    let mut lease = coordinator
        .create(ResourceLeaseInput {
            user_id: "user-ops".into(),
            project_id: "project-ops".into(),
            run_id: "run-ops".into(),
            task_id: None,
            turn_id: None,
            gate_id: None,
            environment_id: "host:local".into(),
            environment_kind: "host".into(),
            resource_kind: "service".into(),
            resource_identifier: "recovery-service".into(),
            cleanup_order: 30,
            ttl_ms: 60_000,
        })
        .await
        .unwrap();

    let decision = RecoveryDecisionInput {
        action: "takeover".into(),
    };
    assert!(
        fixture
            .service
            .decide_recovery("user-ops", "run-ops", decision.clone())
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .development_repo
            .get_run("run-ops", "user-ops")
            .await
            .unwrap()
            .unwrap()
            .status,
        "paused"
    );

    lease.heartbeat_at = now_ms() - 120_000;
    lease.expires_at = now_ms() - 60_000;
    fixture.operations_repo.upsert_resource_lease(&lease).await.unwrap();
    coordinator.reconcile_stale(now_ms()).await.unwrap();
    let recovered = fixture
        .service
        .decide_recovery("user-ops", "run-ops", decision)
        .await
        .unwrap();
    assert_eq!(recovered.status_after.as_deref(), Some("running"));
    assert_eq!(
        fixture
            .development_repo
            .get_run("run-ops", "user-ops")
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
}
