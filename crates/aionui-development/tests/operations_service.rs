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
    DevelopmentOperationsService, DevelopmentPolicyInput, RecoveryDecisionInput, redact_sensitive,
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
        max_duration_ms: 60_000,
        max_parallel_agents: 2,
        max_retries: 1,
        max_cost_microunits: 1_000,
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
