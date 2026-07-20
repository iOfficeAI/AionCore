use std::sync::Arc;

use aionui_db::models::{
    ProjectCommandProfileRow, ProjectKnowledgeContextRow, ProjectKnowledgeFactRow, ProjectKnowledgeIndexRow,
    ProjectRepositoryFactsRow, ProjectRow, ProjectRuntimeProfileRow,
};
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

#[tokio::test]
async fn repository_facts_roundtrip_without_exposing_secret_values() {
    let (repo, _db) = repo().await;
    repo.create(&project("p1", "/tmp/p1")).await.unwrap();
    repo.upsert_repository_facts(&ProjectRepositoryFactsRow {
        project_id: "p1".into(),
        repository_url: Some("ssh://git@example.test/org/repo.git".into()),
        default_branch: Some("main".into()),
        baseline_commit: Some("abc123".into()),
        repository_dirty: true,
        dirty_worktree_choice: "snapshot".into(),
        dirty_snapshot_ref: Some("/managed/_snapshots/one".into()),
        credential_reference: Some("vault:github-production".into()),
        detected_languages_json: r#"["rust"]"#.into(),
        detected_package_managers_json: r#"["cargo"]"#.into(),
        detected_rules_files_json: r#"["AGENTS.md"]"#.into(),
        monorepo_packages_json: r#"["crates/demo"]"#.into(),
        submodules_json: "[]".into(),
        lfs_detected: true,
        detected_at: 300,
    })
    .await
    .unwrap();

    let stored = repo.get_repository_facts("p1", USER).await.unwrap().unwrap();
    assert_eq!(stored.baseline_commit.as_deref(), Some("abc123"));
    assert_eq!(stored.credential_reference.as_deref(), Some("vault:github-production"));
    assert!(!serde_json::to_string(&stored).unwrap().contains("private-token"));
    assert!(repo.get_repository_facts("p1", "other-user").await.unwrap().is_none());
}

#[tokio::test]
async fn knowledge_generation_and_context_are_atomic_and_owner_scoped() {
    let (repo, _db) = repo().await;
    repo.create(&project("p1", "/tmp/p1")).await.unwrap();
    let index = ProjectKnowledgeIndexRow {
        project_id: "p1".into(),
        provider: "codebase-memory".into(),
        provider_project_name: "aionui-project-p1".into(),
        provider_version: Some("0.9.0".into()),
        status: "healthy".into(),
        generation: 1,
        source_commit: Some("abc123".into()),
        indexed_at: Some(400),
        changed_paths_json: r#"["src/lib.rs"]"#.into(),
        error_category: None,
        updated_at: 400,
    };
    let facts = vec![ProjectKnowledgeFactRow {
        id: "fact-1".into(),
        project_id: "p1".into(),
        generation: 1,
        kind: "symbol".into(),
        name: "run".into(),
        qualified_name: Some("app.run".into()),
        source_path: "src/lib.rs".into(),
        source_line: Some(9),
        indexed_at: 400,
    }];
    repo.commit_knowledge_generation(&index, &facts).await.unwrap();

    assert_eq!(repo.get_knowledge_index("p1", USER).await.unwrap(), Some(index));
    assert_eq!(repo.list_knowledge_facts("p1", USER).await.unwrap(), facts);
    assert!(repo.get_knowledge_index("p1", "other-user").await.unwrap().is_none());
    assert!(repo.list_knowledge_facts("p1", "other-user").await.unwrap().is_empty());

    let context = ProjectKnowledgeContextRow {
        id: "context-1".into(),
        project_id: "p1".into(),
        provider_project_name: "aionui-project-p1".into(),
        generation: 1,
        query: "change run".into(),
        symbols_json: r#"["app.run"]"#.into(),
        callers_json: "[]".into(),
        tests_json: r#"["tests/run.rs"]"#.into(),
        routes_json: "[]".into(),
        data_entities_json: "[]".into(),
        created_at: 500,
    };
    repo.insert_knowledge_context(&context).await.unwrap();
    assert_eq!(
        repo.get_knowledge_context("context-1", USER).await.unwrap(),
        Some(context)
    );
    assert!(
        repo.get_knowledge_context("context-1", "other-user")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn cron_and_channel_ownership_follow_their_bound_conversation_owner() {
    let (repo, db) = repo().await;
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
         VALUES ('conversation-owned', 'system_default_user', 'Owned', 'acp', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO cron_jobs \
         (id, name, schedule_kind, schedule_value, payload_message, execution_mode, conversation_id, created_by, \
          created_at, updated_at) \
         VALUES ('cron-owned', 'Owned cron', 'every', '60', 'run', 'existing', 'conversation-owned', 'user', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO assistant_users (id, platform_user_id, platform_type, authorized_at) \
         VALUES ('channel-user', 'telegram-user', 'telegram', 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO assistant_sessions \
         (id, user_id, agent_type, conversation_id, created_at, last_activity) \
         VALUES ('channel-owned', 'channel-user', 'acp', 'conversation-owned', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    assert!(repo.resource_is_owned(USER, "cron", "cron-owned").await.unwrap());
    assert!(repo.resource_is_owned(USER, "channel", "channel-owned").await.unwrap());
    assert!(
        !repo
            .resource_is_owned("other-user", "cron", "cron-owned")
            .await
            .unwrap()
    );
    assert!(
        !repo
            .resource_is_owned("other-user", "channel", "channel-owned")
            .await
            .unwrap()
    );
}
