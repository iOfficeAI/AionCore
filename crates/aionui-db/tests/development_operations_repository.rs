use aionui_db::models::{
    DevelopmentAlertRow, DevelopmentAuditEventRow, DevelopmentPolicyRow, DevelopmentRecoveryRecordRow,
    DevelopmentUsageEventRow,
};
use aionui_db::{IDevelopmentOperationsRepository, SqliteDevelopmentOperationsRepository, init_database_memory};

async fn setup() -> (SqliteDevelopmentOperationsRepository, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO projects (id, user_id, name, local_path, project_type, created_at, updated_at) \
         VALUES ('project-ops', 'system_default_user', 'Operations', '/tmp/operations', 'single', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    (SqliteDevelopmentOperationsRepository::new(db.pool().clone()), db)
}

fn policy() -> DevelopmentPolicyRow {
    DevelopmentPolicyRow {
        id: "policy-1".into(),
        user_id: "system_default_user".into(),
        project_id: "project-ops".into(),
        isolation_mode: "docker".into(),
        container_image: Some("node:24-alpine".into()),
        devcontainer_config_path: None,
        container_cpu_millis: 1000,
        container_memory_mb: 2048,
        container_pids_limit: 256,
        network_mode: "none".into(),
        allowed_secret_keys_json: "[\"NPM_TOKEN\"]".into(),
        allowed_commands_json: "[\"cargo\"]".into(),
        protected_paths_json: "[\".env\"]".into(),
        allowed_network_hosts_json: "[]".into(),
        protected_branches_json: "[\"main\"]".into(),
        dangerous_confirmation_count: 2,
        max_duration_ms: 14_400_000,
        max_parallel_agents: 4,
        max_retries: 3,
        max_cost_microunits: 10_000,
        max_total_tokens: 0,
        fallback_model: None,
        alert_percent: 80,
        over_limit_action: "pause".into(),
        created_at: 1,
        updated_at: 1,
    }
}

#[tokio::test]
async fn policy_is_upserted_and_owner_scoped() {
    let (repo, _db) = setup().await;
    repo.upsert_policy(&policy()).await.unwrap();
    let mut changed = policy();
    changed.isolation_mode = "host".into();
    changed.updated_at = 2;
    repo.upsert_policy(&changed).await.unwrap();

    assert!(repo.get_policy("other", "project-ops").await.unwrap().is_none());
    let stored = repo
        .get_policy("system_default_user", "project-ops")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.id, "policy-1");
    assert_eq!(stored.isolation_mode, "host");
    assert_eq!(stored.created_at, 1);
}

#[tokio::test]
async fn usage_is_append_only_and_summarized_by_owner_and_run() {
    let (repo, db) = setup().await;
    sqlx::query(
        "INSERT INTO development_runs \
         (id, user_id, project_id, execution_mode, status, request_summary, acceptance_criteria, created_at, updated_at) \
         VALUES ('run-ops', 'system_default_user', 'project-ops', 'single', 'running', 'Operate', '[]', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    for (id, duration, cost, retries) in [("usage-1", 120_i64, 50_i64, 0_i64), ("usage-2", 80, 25, 1)] {
        repo.append_usage(&DevelopmentUsageEventRow {
            id: id.into(),
            user_id: "system_default_user".into(),
            project_id: "project-ops".into(),
            run_id: Some("run-ops".into()),
            task_id: None,
            usage_type: "quality_gate".into(),
            source: "platform".into(),
            confidence: "measured".into(),
            input_tokens: 0,
            output_tokens: 0,
            cost_microunits: cost,
            duration_ms: duration,
            retry_count: retries,
            metadata_json: "{}".into(),
            created_at: duration,
        })
        .await
        .unwrap();
    }

    let summary = repo
        .summarize_usage("system_default_user", "project-ops", Some("run-ops"))
        .await
        .unwrap();
    assert_eq!(summary.event_count, 2);
    assert_eq!(summary.duration_ms, 200);
    assert_eq!(summary.cost_microunits, 75);
    assert_eq!(summary.retry_count, 1);

    let mutation = sqlx::query("UPDATE development_usage_events SET cost_microunits = 0 WHERE id = 'usage-1'")
        .execute(db.pool())
        .await;
    assert!(mutation.is_err(), "usage ledger must reject updates");
}

#[tokio::test]
async fn audit_alert_and_recovery_records_are_deterministic() {
    let (repo, _db) = setup().await;
    repo.append_audit(&DevelopmentAuditEventRow {
        id: "audit-1".into(),
        user_id: "system_default_user".into(),
        actor_type: "user".into(),
        actor_id: "system_default_user".into(),
        action: "policy.update".into(),
        target_type: "project".into(),
        target_id: "project-ops".into(),
        project_id: "project-ops".into(),
        run_id: None,
        task_id: None,
        result: "success".into(),
        redacted_payload_json: "{\"token\":\"[REDACTED]\"}".into(),
        created_at: 1,
    })
    .await
    .unwrap();

    let mut alert = DevelopmentAlertRow {
        id: "alert-1".into(),
        user_id: "system_default_user".into(),
        project_id: "project-ops".into(),
        run_id: None,
        alert_type: "budget".into(),
        severity: "warning".into(),
        status: "open".into(),
        message: "80% consumed".into(),
        dedupe_key: "budget:project-ops".into(),
        created_at: 1,
        updated_at: 1,
        resolved_at: None,
    };
    repo.upsert_alert(&alert).await.unwrap();
    alert.id = "alert-ignored".into();
    alert.message = "100% consumed".into();
    alert.severity = "critical".into();
    alert.updated_at = 2;
    repo.upsert_alert(&alert).await.unwrap();

    let alerts = repo
        .list_alerts("system_default_user", "project-ops", None, true)
        .await
        .unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].id, "alert-1");
    assert_eq!(alerts[0].message, "100% consumed");

    repo.append_recovery(&DevelopmentRecoveryRecordRow {
        id: "recovery-1".into(),
        user_id: "system_default_user".into(),
        project_id: "project-ops".into(),
        run_id: None,
        recovery_key: "project:project-ops:git".into(),
        finding: "git repository unavailable".into(),
        decision: "manual_required".into(),
        status_before: None,
        status_after: None,
        details_json: "{}".into(),
        created_at: 3,
    })
    .await
    .unwrap();
    repo.append_recovery(&DevelopmentRecoveryRecordRow {
        id: "recovery-2".into(),
        ..repo
            .list_recovery("system_default_user", "project-ops", None, 10)
            .await
            .unwrap()[0]
            .clone()
    })
    .await
    .unwrap();
    assert_eq!(
        repo.list_recovery("system_default_user", "project-ops", None, 10)
            .await
            .unwrap()
            .len(),
        1,
        "recovery_key must be idempotent",
    );
}
