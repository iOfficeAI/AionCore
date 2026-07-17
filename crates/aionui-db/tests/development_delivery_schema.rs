use aionui_db::init_database_memory;

#[tokio::test]
async fn phase_five_migration_creates_delivery_and_ci_tables() {
    let db = init_database_memory().await.unwrap();
    for table in ["development_deliveries", "development_ci_checks"] {
        let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?")
            .bind(table)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(exists, 1, "missing {table}");
    }
}

#[tokio::test]
async fn delivery_is_unique_per_run_and_ci_check_per_provider_id() {
    let db = init_database_memory().await.unwrap();
    let delivery_unique: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_index_list('development_deliveries') WHERE \"unique\" = 1")
            .fetch_one(db.pool())
            .await
            .unwrap();
    let checks_unique: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_index_list('development_ci_checks') WHERE \"unique\" = 1")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(delivery_unique >= 1);
    assert!(checks_unique >= 1);
}
