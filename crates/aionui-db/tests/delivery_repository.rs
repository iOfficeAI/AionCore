use aionui_db::models::{DevelopmentCiCheckRow, DevelopmentDeliveryRow};
use aionui_db::{IDevelopmentRepository, SqliteDevelopmentRepository, init_database_memory};

async fn setup() -> (SqliteDevelopmentRepository, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO projects (id, user_id, name, local_path, project_type, created_at, updated_at) \
         VALUES ('project-delivery', 'system_default_user', 'Delivery', '/tmp/delivery', 'single', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO development_runs \
         (id, user_id, project_id, execution_mode, status, request_summary, acceptance_criteria, created_at, updated_at) \
         VALUES ('run-delivery', 'system_default_user', 'project-delivery', 'single', 'reviewing', 'Ship', '[]', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    (SqliteDevelopmentRepository::new(db.pool().clone()), db)
}

fn delivery() -> DevelopmentDeliveryRow {
    DevelopmentDeliveryRow {
        id: "delivery-1".into(),
        run_id: "run-delivery".into(),
        project_id: "project-delivery".into(),
        user_id: "system_default_user".into(),
        provider: "github".into(),
        repository: Some("example/aion".into()),
        branch: "aion/run/run-delivery/delivery".into(),
        base_branch: "main".into(),
        commit_sha: Some("abc123".into()),
        status: "prepared".into(),
        push_status: "pending".into(),
        pr_number: None,
        pr_url: None,
        pr_status: "not_created".into(),
        ci_status: "not_started".into(),
        review_status: "pending".into(),
        merge_status: "blocked".into(),
        report_json: "{}".into(),
        last_error: None,
        created_at: 10,
        updated_at: 10,
    }
}

#[tokio::test]
async fn delivery_is_idempotent_per_run_and_owner_scoped() {
    let (repo, _db) = setup().await;
    repo.upsert_delivery(&delivery()).await.unwrap();
    repo.upsert_delivery(&delivery()).await.unwrap();

    assert!(repo.get_delivery("other-user", "run-delivery").await.unwrap().is_none());
    let stored = repo
        .get_delivery("system_default_user", "run-delivery")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.commit_sha.as_deref(), Some("abc123"));
}

#[tokio::test]
async fn ci_checks_upsert_and_keep_rework_link() {
    let (repo, _db) = setup().await;
    repo.upsert_delivery(&delivery()).await.unwrap();
    let mut check = DevelopmentCiCheckRow {
        id: "check-1".into(),
        delivery_id: "delivery-1".into(),
        provider_check_id: "provider-1".into(),
        name: "test".into(),
        status: "failed".into(),
        details_url: Some("https://example.invalid/check/1".into()),
        summary: Some("unit tests failed".into()),
        rework_task_id: Some("task-rework".into()),
        started_at: Some(10),
        completed_at: Some(20),
        created_at: 10,
        updated_at: 20,
    };
    repo.upsert_ci_check(&check).await.unwrap();
    check.status = "passed".into();
    check.summary = Some("retry passed".into());
    check.updated_at = 30;
    repo.upsert_ci_check(&check).await.unwrap();

    let rows = repo.list_ci_checks("delivery-1").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "passed");
    assert_eq!(rows[0].rework_task_id.as_deref(), Some("task-rework"));
}
