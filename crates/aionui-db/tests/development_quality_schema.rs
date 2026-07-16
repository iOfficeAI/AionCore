use aionui_db::init_database_memory;

#[tokio::test]
async fn phase_three_migration_creates_evidence_and_gate_tables() {
    let db = init_database_memory().await.unwrap();
    for table in [
        "development_runs",
        "task_artifacts",
        "quality_gate_runs",
        "review_findings",
    ] {
        let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(table)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(exists, 1, "missing table {table}");
    }
}

#[tokio::test]
async fn team_tasks_accept_expanded_states_and_quality_columns() {
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO team_tasks \
         (id, team_id, status, subject, blocked_by, blocks, acceptance_criteria, task_type, risk_level, \
          review_status, verification_status, created_at, updated_at) \
         VALUES ('task-1', 'team-1', 'review', 'Review me', '[]', '[]', '[\"criterion\"]', \
                 'implementation', 'high', 'in_review', 'passed', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let row: (String, String, String, String) = sqlx::query_as(
        "SELECT status, acceptance_criteria, review_status, verification_status FROM team_tasks WHERE id = 'task-1'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(row.0, "review");
    assert_eq!(row.1, "[\"criterion\"]");
    assert_eq!(row.2, "in_review");
    assert_eq!(row.3, "passed");

    let invalid = sqlx::query(
        "INSERT INTO team_tasks \
         (id, team_id, status, subject, blocked_by, blocks, created_at, updated_at) \
         VALUES ('task-bad', 'team-1', 'invented', 'Bad', '[]', '[]', 1, 1)",
    )
    .execute(db.pool())
    .await;
    assert!(invalid.is_err());
}
