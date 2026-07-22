use std::sync::Arc;

use aionui_common::now_ms;
use aionui_db::models::{DevelopmentRunRow, ProjectRow};
use aionui_db::{
    IDevelopmentOperationsRepository, IDevelopmentRepository, IProjectRepository, SqliteAgentWorkspaceLeaseRepository,
    SqliteDevelopmentOperationsRepository, SqliteDevelopmentRepository, SqliteProjectRepository, UsageDimension,
    init_database_memory,
};
use aionui_development::{
    DevelopmentOperationsService, DevelopmentPolicyInput, DevelopmentUsageIngestor, ObservedAgentTurnUsage,
    PricingService,
};
use serde_json::json;

#[tokio::test]
async fn observed_agent_turn_is_idempotent_and_pauses_the_bound_run() {
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('usage-user', 'usage-user', '', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let workspace = tempfile::tempdir().unwrap();
    let project_repo = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
    project_repo
        .create(&ProjectRow {
            id: "usage-project".into(),
            user_id: "usage-user".into(),
            name: "Usage".into(),
            local_path: workspace.path().to_string_lossy().into_owned(),
            repository_url: None,
            default_branch: Some("main".into()),
            project_type: "single".into(),
            created_at: 1,
            updated_at: 1,
        })
        .await
        .unwrap();
    project_repo
        .bind_resource("usage-project", "usage-user", "conversation", "usage-conversation")
        .await
        .unwrap();

    let development_repo = Arc::new(SqliteDevelopmentRepository::new(db.pool().clone()));
    let started_at = now_ms();
    development_repo
        .create_run(&DevelopmentRunRow {
            id: "usage-run".into(),
            user_id: "usage-user".into(),
            project_id: "usage-project".into(),
            team_id: None,
            source_channel: None,
            source_user_id: None,
            execution_mode: "single".into(),
            status: "running".into(),
            request_summary: "observe usage".into(),
            acceptance_criteria: "[]".into(),
            baseline_commit: None,
            integration_branch: None,
            started_at: Some(started_at),
            finished_at: None,
            created_at: started_at,
            updated_at: started_at,
        })
        .await
        .unwrap();

    let operations_repo = Arc::new(SqliteDevelopmentOperationsRepository::new(db.pool().clone()));
    let operations = DevelopmentOperationsService::new(
        operations_repo.clone(),
        development_repo.clone(),
        project_repo.clone(),
        Arc::new(SqliteAgentWorkspaceLeaseRepository::new(db.pool().clone())),
    );
    operations
        .upsert_policy(
            "usage-user",
            "usage-project",
            DevelopmentPolicyInput {
                isolation_mode: "host".into(),
                container_image: None,
                devcontainer_config_path: None,
                container_cpu_millis: 1000,
                container_memory_mb: 1024,
                container_pids_limit: 128,
                network_mode: "none".into(),
                allowed_secret_keys: vec![],
                allowed_commands: vec![],
                protected_paths: vec![],
                allowed_network_hosts: vec![],
                protected_branches: vec!["main".into()],
                dangerous_confirmation_count: 2,
                max_duration_ms: 60_000,
                max_parallel_agents: 1,
                max_retries: 1,
                max_cost_microunits: 0,
                max_total_tokens: 100,
                fallback_model: None,
                alert_percent: 80,
                over_limit_action: "pause".into(),
            },
        )
        .await
        .unwrap();

    let ingestor = DevelopmentUsageIngestor::new(
        project_repo,
        development_repo.clone(),
        operations_repo.clone(),
        operations.clone(),
        PricingService::new(operations_repo.clone()).with_budget(operations),
    );
    let event = ObservedAgentTurnUsage {
        user_id: "usage-user".into(),
        conversation_id: "usage-conversation".into(),
        turn_id: "turn-budget-1".into(),
        agent_id: Some("codex-agent".into()),
        provider: "openai".into(),
        model: "gpt-test".into(),
        team_id: None,
        slot_id: None,
        usage: Some(json!({
            "input_tokens": 120,
            "output_tokens": 5,
            "size": 258400,
            "used": 120,
            "cost": {"amount": 0.25, "currency": "USD"}
        })),
        duration_ms: 20,
        retry_count: 0,
        occurred_at: now_ms(),
    };

    let first = ingestor.record(event.clone()).await.unwrap().unwrap();
    assert!(first.inserted);
    assert_eq!(first.row.input_tokens, 120);
    assert_eq!(first.row.cost_microunits, 250_000);
    assert_eq!(first.budget.as_ref().unwrap().action, "pause");
    assert!(!first.budget.as_ref().unwrap().reasons.is_empty());
    assert_eq!(
        development_repo
            .get_run("usage-run", "usage-user")
            .await
            .unwrap()
            .unwrap()
            .status,
        "paused"
    );
    let admission = ingestor
        .admit("usage-user", "usage-conversation", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(admission.run_id, "usage-run");
    assert_eq!(admission.run_status, "paused");
    assert!(!admission.evaluation.reasons.is_empty());

    let duplicate = ingestor.record(event).await.unwrap().unwrap();
    assert!(!duplicate.inserted);
    assert_eq!(duplicate.budget.as_ref().unwrap().action, "pause");
    assert!(!duplicate.budget.as_ref().unwrap().reasons.is_empty());
    let summary = operations_repo
        .summarize_usage_dimension("usage-user", &UsageDimension::Conversation("usage-conversation".into()))
        .await
        .unwrap();
    assert_eq!(summary.event_count, 1);
    assert_eq!(summary.input_tokens, 120);
    assert_eq!(summary.output_tokens, 5);
}

#[tokio::test]
async fn unbound_conversation_usage_is_not_assigned_to_an_unrelated_project() {
    let db = init_database_memory().await.unwrap();
    let project_repo = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
    let development_repo = Arc::new(SqliteDevelopmentRepository::new(db.pool().clone()));
    let operations_repo = Arc::new(SqliteDevelopmentOperationsRepository::new(db.pool().clone()));
    let operations = DevelopmentOperationsService::new(
        operations_repo.clone(),
        development_repo.clone(),
        project_repo.clone(),
        Arc::new(SqliteAgentWorkspaceLeaseRepository::new(db.pool().clone())),
    );
    let ingestor = DevelopmentUsageIngestor::new(
        project_repo,
        development_repo,
        operations_repo.clone(),
        operations,
        PricingService::new(operations_repo),
    );
    let result = ingestor
        .record(ObservedAgentTurnUsage {
            user_id: "nobody".into(),
            conversation_id: "unbound".into(),
            turn_id: "turn-unbound".into(),
            agent_id: None,
            provider: "unknown".into(),
            model: "unknown".into(),
            team_id: None,
            slot_id: None,
            usage: Some(json!({"used": 10})),
            duration_ms: 1,
            retry_count: 0,
            occurred_at: now_ms(),
        })
        .await
        .unwrap();
    assert!(result.is_none());
    assert!(ingestor.admit("nobody", "unbound", None).await.unwrap().is_none());
}
