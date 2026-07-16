use std::sync::Arc;

use aionui_db::models::{ProjectCommandProfileRow, ProjectRow, ProjectRuntimeProfileRow};
use aionui_db::{DbError, IProjectRepository, SqliteProjectRepository, UpdateProjectParams, init_database_memory};

const USER: &str = "system_default_user";

async fn repo() -> (Arc<dyn IProjectRepository>, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
    (repo, db)
}

fn project(id: &str, path: &str) -> ProjectRow {
    ProjectRow {
        id: id.into(),
        user_id: USER.into(),
        name: format!("Project {id}"),
        local_path: path.into(),
        repository_url: Some(format!("https://example.com/{id}.git")),
        default_branch: Some("main".into()),
        project_type: "single".into(),
        created_at: 100,
        updated_at: 100,
    }
}

#[tokio::test]
async fn project_crud_is_scoped_to_owner() {
    let (repo, _db) = repo().await;
    repo.create(&project("p1", "/tmp/p1")).await.unwrap();

    assert!(repo.get_for_user("p1", "other-user").await.unwrap().is_none());
    assert_eq!(repo.list_for_user(USER).await.unwrap().len(), 1);

    let updated = repo
        .update_for_user(
            "p1",
            USER,
            &UpdateProjectParams {
                name: Some("Renamed".into()),
                repository_url: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.name, "Renamed");
    assert_eq!(updated.repository_url, None);

    assert!(!repo.delete_for_user("p1", "other-user").await.unwrap());
    assert!(repo.delete_for_user("p1", USER).await.unwrap());
}

#[tokio::test]
async fn duplicate_local_path_for_same_owner_returns_conflict() {
    let (repo, _db) = repo().await;
    repo.create(&project("p1", "/tmp/shared")).await.unwrap();

    let error = repo.create(&project("p2", "/tmp/shared")).await.unwrap_err();
    assert!(matches!(error, DbError::Conflict(_)));
}

#[tokio::test]
async fn project_profiles_roundtrip() {
    let (repo, _db) = repo().await;
    repo.create(&project("p1", "/tmp/p1")).await.unwrap();

    repo.upsert_command_profile(&ProjectCommandProfileRow {
        project_id: "p1".into(),
        install_command: Some("bun install".into()),
        format_command: Some("bunx oxfmt".into()),
        lint_command: None,
        typecheck_command: Some("bunx tsc --noEmit".into()),
        unit_test_command: Some("bun test".into()),
        integration_test_command: None,
        e2e_command: None,
        build_command: Some("bun run build".into()),
        security_scan_command: None,
        command_timeout_seconds: 600,
        updated_at: 200,
    })
    .await
    .unwrap();
    repo.upsert_runtime_profile(&ProjectRuntimeProfileRow {
        project_id: "p1".into(),
        environment_kind: "local".into(),
        language: Some("typescript".into()),
        package_manager: Some("bun".into()),
        runtime_version: None,
        env_keys: r#"["NODE_ENV"]"#.into(),
        metadata: r#"{"monorepo":false}"#.into(),
        updated_at: 200,
    })
    .await
    .unwrap();

    let command = repo.get_command_profile("p1", USER).await.unwrap().unwrap();
    let runtime = repo.get_runtime_profile("p1", USER).await.unwrap().unwrap();
    assert_eq!(command.command_timeout_seconds, 600);
    assert_eq!(runtime.package_manager.as_deref(), Some("bun"));
}

#[tokio::test]
async fn resource_binding_replaces_the_previous_project_for_same_owner() {
    let (repo, _db) = repo().await;
    repo.create(&project("p1", "/tmp/p1")).await.unwrap();
    repo.create(&project("p2", "/tmp/p2")).await.unwrap();

    repo.bind_resource("p1", USER, "conversation", "c1").await.unwrap();
    repo.bind_resource("p2", USER, "conversation", "c1").await.unwrap();

    let linked = repo
        .get_for_resource(USER, "conversation", "c1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(linked.id, "p2");
    assert!(repo.list_resource_links("p1", USER).await.unwrap().is_empty());
    assert_eq!(repo.list_resource_links("p2", USER).await.unwrap().len(), 1);
}
