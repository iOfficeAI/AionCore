use aionui_db::init_database_memory;

#[tokio::test]
async fn phase_six_migration_creates_operations_tables_and_constraints() {
    let db = init_database_memory().await.unwrap();
    for table in [
        "development_policies",
        "development_usage_events",
        "development_audit_events",
        "development_alerts",
        "development_recovery_records",
    ] {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?")
            .bind(table)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count, 1, "missing table {table}");
    }
    for column in ["isolation_mode", "execution_id"] {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info('quality_gate_runs') WHERE name = ?")
                .bind(column)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(count, 1, "missing quality gate column {column}");
    }

    seed_project(&db).await;
    let invalid_mode = sqlx::query(
        "INSERT INTO development_policies \
         (id, user_id, project_id, isolation_mode, created_at, updated_at) \
         VALUES ('policy-invalid', 'system_default_user', 'project-ops', 'virtual-machine', 1, 1)",
    )
    .execute(db.pool())
    .await;
    assert!(invalid_mode.is_err());

    let invalid_secret_payload = sqlx::query(
        "INSERT INTO development_policies \
         (id, user_id, project_id, allowed_secret_keys_json, created_at, updated_at) \
         VALUES ('policy-secret', 'system_default_user', 'project-ops', '{\"TOKEN\":\"secret\"}', 1, 1)",
    )
    .execute(db.pool())
    .await;
    assert!(invalid_secret_payload.is_err());
}

async fn seed_project(db: &aionui_db::Database) {
    sqlx::query(
        "INSERT INTO projects (id, user_id, name, local_path, project_type, created_at, updated_at) \
         VALUES ('project-ops', 'system_default_user', 'Operations', '/tmp/operations', 'single', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn alert_dedupe_and_usage_dimensions_are_enforced() {
    let db = init_database_memory().await.unwrap();
    seed_project(&db).await;

    sqlx::query(
        "INSERT INTO development_alerts \
         (id, user_id, project_id, alert_type, severity, status, message, dedupe_key, created_at, updated_at) \
         VALUES ('alert-1', 'system_default_user', 'project-ops', 'budget', 'warning', 'open', 'near limit', 'budget:project-ops', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let duplicate = sqlx::query(
        "INSERT INTO development_alerts \
         (id, user_id, project_id, alert_type, severity, status, message, dedupe_key, created_at, updated_at) \
         VALUES ('alert-2', 'system_default_user', 'project-ops', 'budget', 'warning', 'open', 'near limit', 'budget:project-ops', 2, 2)",
    )
    .execute(db.pool())
    .await;
    assert!(duplicate.is_err());

    let negative_usage = sqlx::query(
        "INSERT INTO development_usage_events \
         (id, user_id, project_id, usage_type, source, confidence, duration_ms, created_at) \
         VALUES ('usage-invalid', 'system_default_user', 'project-ops', 'quality_gate', 'platform', 'measured', -1, 1)",
    )
    .execute(db.pool())
    .await;
    assert!(negative_usage.is_err());
}
