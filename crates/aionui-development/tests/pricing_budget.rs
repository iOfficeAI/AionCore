use std::sync::Arc;

use aionui_common::now_ms;
use aionui_db::models::ProjectRow;
use aionui_db::{
    IDevelopmentOperationsRepository, IProjectRepository, SqliteDevelopmentOperationsRepository,
    SqliteProjectRepository, init_database_memory,
};
use aionui_development::{ModelPriceInput, PricingService, UsageDimension, UsageMeasurement};

async fn setup() -> (
    PricingService,
    Arc<SqliteDevelopmentOperationsRepository>,
    tempfile::TempDir,
) {
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES ('user-price', 'pricing', '', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let projects = SqliteProjectRepository::new(db.pool().clone());
    projects
        .create(&ProjectRow {
            id: "project-price".into(),
            user_id: "user-price".into(),
            name: "Pricing".into(),
            local_path: workspace.path().to_string_lossy().into_owned(),
            repository_url: None,
            default_branch: Some("main".into()),
            project_type: "single".into(),
            created_at: 1,
            updated_at: 1,
        })
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO teams (id, user_id, name, workspace, created_at, updated_at) \
         VALUES ('team-1', 'user-price', 'Pricing Team', ?, 1, 1)",
    )
    .bind(workspace.path().to_string_lossy().as_ref())
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO development_runs \
         (id, user_id, project_id, team_id, execution_mode, status, request_summary, created_at, updated_at) \
         VALUES ('run-price', 'user-price', 'project-price', 'team-1', 'team', 'running', 'price', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO team_tasks (id, team_id, run_id, subject, created_at, updated_at) \
         VALUES ('task-price', 'team-1', 'run-price', 'Measure pricing', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let repo = Arc::new(SqliteDevelopmentOperationsRepository::new(db.pool().clone()));
    (PricingService::new(repo.clone()), repo, workspace)
}

fn measurement(model: &str) -> UsageMeasurement {
    UsageMeasurement {
        user_id: "user-price".into(),
        project_id: "project-price".into(),
        conversation_id: Some("conversation-1".into()),
        agent_id: Some("agent-1".into()),
        task_id: Some("task-price".into()),
        run_id: Some("run-price".into()),
        team_id: Some("team-1".into()),
        provider: "anthropic".into(),
        model: model.into(),
        input_tokens: 1_000_000,
        output_tokens: 2_000_000,
        cache_read_tokens: 1_000_000,
        cache_write_tokens: 1_000_000,
        duration_ms: 500,
        retry_count: 0,
        provider_reported_cost_microunits: None,
        occurred_at: now_ms(),
    }
}

#[tokio::test]
async fn versioned_pricing_records_all_token_classes_and_cost_provenance() {
    let (service, repo, _workspace) = setup().await;
    service
        .upsert_price(ModelPriceInput {
            provider: "anthropic".into(),
            model: "claude-test".into(),
            input_per_million_microunits: 1_000,
            output_per_million_microunits: 2_000,
            cache_read_per_million_microunits: 100,
            cache_write_per_million_microunits: 200,
            source_id: "operator-catalog".into(),
            version: "2026-07-20".into(),
            effective_at: now_ms() - 1_000,
        })
        .await
        .unwrap();
    let event = service.record(measurement("claude-test")).await.unwrap();
    assert_eq!(event.cost_microunits, 5_300);
    assert_eq!(event.cost_status, "known");
    assert_eq!(event.cost_origin, "platform_estimated");
    assert_eq!(event.price_source_id.as_deref(), Some("operator-catalog"));
    assert_eq!(event.price_version.as_deref(), Some("2026-07-20"));

    for scope in [
        UsageDimension::Project("project-price".into()),
        UsageDimension::Run("run-price".into()),
        UsageDimension::Task("task-price".into()),
        UsageDimension::Conversation("conversation-1".into()),
        UsageDimension::Agent("agent-1".into()),
        UsageDimension::Team("team-1".into()),
    ] {
        let summary = repo.summarize_usage_dimension("user-price", &scope).await.unwrap();
        assert_eq!(summary.event_count, 1);
        assert_eq!(summary.cache_read_tokens, 1_000_000);
        assert_eq!(summary.cache_write_tokens, 1_000_000);
        assert_eq!(summary.known_cost_microunits, 5_300);
        assert_eq!(summary.unknown_cost_events, 0);
    }
}

#[tokio::test]
async fn missing_prices_remain_unknown_and_provider_reported_cost_wins() {
    let (service, _repo, _workspace) = setup().await;
    let unknown = service.record(measurement("unpriced-model")).await.unwrap();
    assert_eq!(unknown.cost_status, "unknown");
    assert_eq!(unknown.cost_microunits, 0);
    assert_eq!(unknown.cost_origin, "unknown");
    assert!(unknown.price_source_id.is_none());

    let mut reported = measurement("unpriced-model");
    reported.provider_reported_cost_microunits = Some(9_999);
    let reported = service.record(reported).await.unwrap();
    assert_eq!(reported.cost_status, "known");
    assert_eq!(reported.cost_microunits, 9_999);
    assert_eq!(reported.cost_origin, "provider_reported");
}
