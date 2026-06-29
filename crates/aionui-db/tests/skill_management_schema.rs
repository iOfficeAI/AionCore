use aionui_db::init_database_memory;

#[tokio::test]
async fn migration_creates_skill_management_tables() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();

    let skill_columns: Vec<(String,)> = sqlx::query_as("SELECT name FROM pragma_table_info('skills') ORDER BY cid")
        .fetch_all(pool)
        .await
        .unwrap();
    let skill_columns: Vec<String> = skill_columns.into_iter().map(|row| row.0).collect();
    assert_eq!(
        skill_columns,
        vec![
            "id",
            "name",
            "description",
            "path",
            "source",
            "enabled",
            "deleted_at",
            "created_at",
            "updated_at",
        ]
    );

    let import_columns: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM pragma_table_info('skill_import_records') ORDER BY cid")
            .fetch_all(pool)
            .await
            .unwrap();
    let import_columns: Vec<String> = import_columns.into_iter().map(|row| row.0).collect();
    assert_eq!(
        import_columns,
        vec![
            "id",
            "operation_id",
            "source_label",
            "source_path",
            "source_name",
            "skill_id",
            "skill_name",
            "status",
            "error_code",
            "error_path",
            "actual_bytes",
            "limit_bytes",
            "line",
            "column",
            "created_at",
        ]
    );
}

#[tokio::test]
async fn migration_allows_known_skill_sources_and_rejects_unknown_sources() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();

    for source in ["user", "builtin", "extension", "cron"] {
        sqlx::query(
            "INSERT INTO skills (id, name, description, path, source, enabled, created_at, updated_at)
             VALUES (?, ?, NULL, ?, ?, 1, 1, 1)",
        )
        .bind(format!("skill-{source}"))
        .bind(format!("skill-{source}"))
        .bind(format!("/tmp/{source}"))
        .bind(source)
        .execute(pool)
        .await
        .unwrap();
    }

    let rejected = sqlx::query(
        "INSERT INTO skills (id, name, description, path, source, enabled, created_at, updated_at)
         VALUES ('skill-invalid', 'skill-invalid', NULL, '/tmp/invalid', 'invalid', 1, 1, 1)",
    )
    .execute(pool)
    .await;

    assert!(rejected.is_err());
}
