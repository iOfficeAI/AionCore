use aionui_db::init_database_memory;

#[tokio::test]
async fn project_migration_creates_all_phase_one_tables() {
    let db = init_database_memory().await.unwrap();
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN (\
         'projects', 'project_command_profiles', 'project_runtime_profiles', 'project_resource_links', \
         'project_repository_facts') \
         ORDER BY name",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();

    assert_eq!(
        names,
        vec![
            "project_command_profiles",
            "project_repository_facts",
            "project_resource_links",
            "project_runtime_profiles",
            "projects",
        ]
    );
}

#[tokio::test]
async fn project_knowledge_migration_creates_indexes_facts_and_contexts() {
    let database = init_database_memory().await.unwrap();
    for table in [
        "project_knowledge_indexes",
        "project_knowledge_facts",
        "project_knowledge_contexts",
    ] {
        let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(table)
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(exists, 1, "missing table {table}");
    }
}

#[tokio::test]
async fn project_schema_enforces_owner_path_uniqueness_and_resource_types() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();

    sqlx::query(
        "INSERT INTO projects (id, user_id, name, local_path, project_type, created_at, updated_at) \
         VALUES ('p1', 'system_default_user', 'One', '/tmp/project', 'unknown', 1, 1)",
    )
    .execute(pool)
    .await
    .unwrap();

    let duplicate = sqlx::query(
        "INSERT INTO projects (id, user_id, name, local_path, project_type, created_at, updated_at) \
         VALUES ('p2', 'system_default_user', 'Two', '/tmp/project', 'unknown', 1, 1)",
    )
    .execute(pool)
    .await;
    assert!(duplicate.is_err());

    for (index, resource_type) in ["conversation", "team", "cron", "channel"].into_iter().enumerate() {
        sqlx::query(
            "INSERT INTO project_resource_links (project_id, user_id, resource_type, resource_id, created_at) \
             VALUES ('p1', 'system_default_user', ?, ?, 1)",
        )
        .bind(resource_type)
        .bind(format!("resource-{index}"))
        .execute(pool)
        .await
        .unwrap();
    }
    let invalid_link = sqlx::query(
        "INSERT INTO project_resource_links (project_id, user_id, resource_type, resource_id, created_at) \
         VALUES ('p1', 'system_default_user', 'unsupported', 'resource-invalid', 1)",
    )
    .execute(pool)
    .await;
    assert!(invalid_link.is_err());
}
