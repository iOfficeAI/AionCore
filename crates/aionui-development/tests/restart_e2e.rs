use std::sync::Arc;

use aionui_common::now_ms;
use aionui_db::{IDevelopmentOperationsRepository, SqliteDevelopmentOperationsRepository, init_database};
use aionui_development::{ResourceLeaseCoordinator, ResourceLeaseInput};

fn lease_input(kind: &str, identifier: &str, cleanup_order: i64) -> ResourceLeaseInput {
    ResourceLeaseInput {
        user_id: "system_default_user".into(),
        project_id: "project-restart".into(),
        run_id: "run-restart".into(),
        task_id: Some("task-restart".into()),
        turn_id: Some("turn-restart".into()),
        gate_id: Some("gate-restart".into()),
        environment_id: "host:local".into(),
        environment_kind: "host".into(),
        resource_kind: kind.into(),
        resource_identifier: identifier.into(),
        cleanup_order,
        ttl_ms: 60_000,
    }
}

#[tokio::test]
async fn restored_instance_reconciles_once_and_persists_each_recovery_decision() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("restart.sqlite3");

    let database = init_database(&database_path).await.unwrap();
    sqlx::query(
        "INSERT INTO projects (id, user_id, name, local_path, project_type, created_at, updated_at) \
         VALUES ('project-restart', 'system_default_user', 'Restart', '/tmp/restart', 'single', 1, 1)",
    )
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO development_runs \
         (id, user_id, project_id, execution_mode, status, request_summary, acceptance_criteria, created_at, updated_at) \
         VALUES ('run-restart', 'system_default_user', 'project-restart', 'single', 'running', 'Restart', '[]', 1, 1)",
    )
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO team_tasks \
         (id, team_id, run_id, subject, status, blocked_by, blocks, acceptance_criteria, task_type, risk_level, \
          review_status, verification_status, created_at, updated_at) \
         VALUES ('task-restart', 'team-restart', 'run-restart', 'Restart task', 'in_progress', '[]', '[]', '[]', \
                 'implementation', 'medium', 'pending', 'running', 1, 1)",
    )
    .execute(database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO quality_gate_runs \
         (id, run_id, task_id, gate_type, command, working_directory, status, required, started_at, created_at) \
         VALUES ('gate-restart', 'run-restart', 'task-restart', 'test', 'redacted', '/tmp/restart', 'running', 1, 1, 1)",
    )
    .execute(database.pool())
    .await
    .unwrap();

    let first_repo = Arc::new(SqliteDevelopmentOperationsRepository::new(database.pool().clone()));
    let first_instance = ResourceLeaseCoordinator::new(first_repo.clone(), "instance-before-restart");
    let mut lease_ids = Vec::new();
    for decision in ["retry", "rollback", "takeover", "terminate"] {
        let mut lease = first_instance
            .create(lease_input("process", &format!("child-{decision}"), 20))
            .await
            .unwrap();
        lease.heartbeat_at = now_ms() - 120_000;
        lease.expires_at = now_ms() - 60_000;
        first_repo.upsert_resource_lease(&lease).await.unwrap();
        lease_ids.push((lease.id, decision));
    }
    drop(first_instance);
    drop(first_repo);
    database.close().await;

    // Simulate a fresh application process: reopen the same SQLite file and
    // recreate all repository/coordinator state with a new instance identity.
    let restored_database = init_database(&database_path).await.unwrap();
    let restored_repo = Arc::new(SqliteDevelopmentOperationsRepository::new(
        restored_database.pool().clone(),
    ));
    let restored_instance = ResourceLeaseCoordinator::new(restored_repo.clone(), "instance-after-restart");

    let first_scan = restored_instance.reconcile_stale(now_ms()).await.unwrap();
    assert_eq!(first_scan.len(), 4);
    assert!(first_scan.iter().all(|lease| lease.status == "orphaned"));
    assert!(restored_instance.reconcile_stale(now_ms()).await.unwrap().is_empty());

    for (lease_id, decision) in lease_ids {
        let first = restored_instance
            .record_recovery_decision(&lease_id, decision)
            .await
            .unwrap();
        let duplicate = restored_instance
            .record_recovery_decision(&lease_id, decision)
            .await
            .unwrap();
        assert_eq!(first, duplicate);
        assert_eq!(first.recovery_decision.as_deref(), Some(decision));
    }

    let (run_count, task_count, gate_count): (i64, i64, i64) = (
        sqlx::query_scalar("SELECT COUNT(*) FROM development_runs WHERE id = 'run-restart'")
            .fetch_one(restored_database.pool())
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT COUNT(*) FROM team_tasks WHERE id = 'task-restart'")
            .fetch_one(restored_database.pool())
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT COUNT(*) FROM quality_gate_runs WHERE id = 'gate-restart'")
            .fetch_one(restored_database.pool())
            .await
            .unwrap(),
    );
    assert_eq!((run_count, task_count, gate_count), (1, 1, 1));

    let leases = restored_repo
        .list_resource_leases("system_default_user", "run-restart", false)
        .await
        .unwrap();
    assert_eq!(leases.len(), 4);
    assert!(leases.iter().all(|lease| lease.recovery_decision.is_some()));
    let takeover = leases
        .iter()
        .find(|lease| lease.recovery_decision.as_deref() == Some("takeover"))
        .expect("takeover decision persisted");
    assert_eq!(takeover.owner_instance_id, "instance-after-restart");

    restored_database.close().await;
}
