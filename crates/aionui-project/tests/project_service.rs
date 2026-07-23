use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aionui_db::{
    IConversationRepository, IProjectRepository, ITeamRepository, SqliteConversationRepository,
    SqliteProjectRepository, SqliteTeamRepository, init_database_memory,
};
use aionui_project::{
    AgentCapabilitySnapshot, CreateProjectInput, ProjectAgentCapabilityPort, ProjectCommandProfileInput, ProjectError,
    ProjectService,
};

struct FakeAgents {
    snapshots: Vec<AgentCapabilitySnapshot>,
    refresh_seen: Arc<AtomicBool>,
}

impl Default for FakeAgents {
    fn default() -> Self {
        Self {
            snapshots: Vec::new(),
            refresh_seen: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait::async_trait]
impl ProjectAgentCapabilityPort for FakeAgents {
    async fn snapshot(&self, id: &str, refresh: bool) -> Result<Option<AgentCapabilitySnapshot>, ProjectError> {
        self.refresh_seen.store(refresh, Ordering::SeqCst);
        Ok(self.snapshots.iter().find(|item| item.id == id).cloned())
    }
}

async fn service(agents: FakeAgents) -> (ProjectService, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let project_repo: Arc<dyn IProjectRepository> = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
    let conversation_repo: Arc<dyn IConversationRepository> =
        Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let team_repo: Arc<dyn ITeamRepository> = Arc::new(SqliteTeamRepository::new(db.pool().clone()));
    (
        ProjectService::new(project_repo, conversation_repo, team_repo, Arc::new(agents)),
        db,
    )
}

#[tokio::test]
async fn create_canonicalizes_existing_path_and_scopes_reads_to_owner() {
    let temp = tempfile::tempdir().unwrap();
    let nested = temp.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let (service, _db) = service(FakeAgents::default()).await;

    let created = service
        .create(
            "system_default_user",
            CreateProjectInput {
                name: "Example".into(),
                local_path: nested.join("..").join("nested").to_string_lossy().into_owned(),
                repository_url: None,
                default_branch: Some("main".into()),
                project_type: "single".into(),
            },
        )
        .await
        .unwrap();

    assert_eq!(created.local_path, nested.canonicalize().unwrap().to_string_lossy());
    assert!(service.get("other-user", &created.id).await.is_err());
}

#[tokio::test]
async fn preflight_reports_dirty_git_missing_command_and_unhealthy_agent() {
    let temp = tempfile::tempdir().unwrap();
    git2::Repository::init(temp.path()).unwrap();
    std::fs::write(temp.path().join("untracked.txt"), "dirty").unwrap();
    let (service, _db) = service(FakeAgents {
        snapshots: vec![AgentCapabilitySnapshot {
            id: "codex".into(),
            agent_type: "acp".into(),
            enabled: true,
            installed: true,
            status: "offline".into(),
            last_check_status: Some("unavailable".into()),
            last_check_at: Some(aionui_common::now_ms()),
            last_success_at: None,
            agent_capabilities: None,
            available_models: None,
            available_modes: None,
            available_commands: None,
            dynamic_probe: None,
        }],
        ..Default::default()
    })
    .await;
    let project = service
        .create(
            "system_default_user",
            CreateProjectInput {
                name: "Git".into(),
                local_path: temp.path().to_string_lossy().into_owned(),
                repository_url: None,
                default_branch: Some("main".into()),
                project_type: "single".into(),
            },
        )
        .await
        .unwrap();
    service
        .upsert_command_profile(
            "system_default_user",
            &project.id,
            ProjectCommandProfileInput {
                unit_test_command: Some("missing-tool-for-aion test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let result = service
        .preflight("system_default_user", &project.id, &["codex".into()], false)
        .await
        .unwrap();

    assert_eq!(result.overall_status, "fail");
    assert!(
        result
            .checks
            .iter()
            .any(|check| check.code == "git.dirty" && check.level == "warning")
    );
    assert!(
        result
            .checks
            .iter()
            .any(|check| check.code == "command.unit_test" && check.level == "fail")
    );
    assert!(
        result
            .agents
            .iter()
            .any(|agent| agent.agent_id == "codex" && agent.level == "fail")
    );
}

#[tokio::test]
async fn explicit_agent_refresh_is_forwarded_and_healthy_snapshot_passes() {
    let temp = tempfile::tempdir().unwrap();
    let refresh_seen = Arc::new(AtomicBool::new(false));
    let (service, _db) = service(FakeAgents {
        snapshots: vec![AgentCapabilitySnapshot {
            id: "codex".into(),
            agent_type: "aionrs".into(),
            enabled: true,
            installed: true,
            status: "online".into(),
            last_check_status: Some("online".into()),
            last_check_at: Some(aionui_common::now_ms()),
            last_success_at: Some(aionui_common::now_ms()),
            agent_capabilities: Some(serde_json::json!({ "loadSession": true })),
            available_models: Some(serde_json::json!([{ "id": "gpt-5" }])),
            available_modes: None,
            available_commands: None,
            dynamic_probe: None,
        }],
        refresh_seen: refresh_seen.clone(),
    })
    .await;
    let project = service
        .create(
            "system_default_user",
            CreateProjectInput {
                name: "Healthy".into(),
                local_path: temp.path().to_string_lossy().into_owned(),
                repository_url: None,
                default_branch: None,
                project_type: "unknown".into(),
            },
        )
        .await
        .unwrap();

    let result = service
        .preflight("system_default_user", &project.id, &["codex".into()], true)
        .await
        .unwrap();

    assert!(refresh_seen.load(Ordering::SeqCst));
    assert_eq!(result.agents[0].level, "pass");
    assert!(
        result
            .checks
            .iter()
            .any(|check| check.code == "git.repository" && check.level == "warning")
    );
}

#[tokio::test]
async fn stale_healthy_agent_snapshot_requires_attention() {
    let temp = tempfile::tempdir().unwrap();
    let stale_time = aionui_common::now_ms() - 25 * 60 * 60 * 1000;
    let (service, _db) = service(FakeAgents {
        snapshots: vec![AgentCapabilitySnapshot {
            id: "claude".into(),
            agent_type: "aionrs".into(),
            enabled: true,
            installed: true,
            status: "online".into(),
            last_check_status: Some("online".into()),
            last_check_at: Some(stale_time),
            last_success_at: Some(stale_time),
            agent_capabilities: None,
            available_models: None,
            available_modes: None,
            available_commands: None,
            dynamic_probe: None,
        }],
        ..Default::default()
    })
    .await;
    let project = service
        .create(
            "system_default_user",
            CreateProjectInput {
                name: "Stale".into(),
                local_path: temp.path().to_string_lossy().into_owned(),
                repository_url: None,
                default_branch: None,
                project_type: "unknown".into(),
            },
        )
        .await
        .unwrap();

    let result = service
        .preflight("system_default_user", &project.id, &["claude".into()], false)
        .await
        .unwrap();
    assert_eq!(result.agents[0].level, "warning");
    assert!(result.agents[0].summary.contains("stale"));
}

#[tokio::test]
async fn stale_dynamic_probe_blocks_formal_project_preflight() {
    let temp = tempfile::tempdir().unwrap();
    let now = aionui_common::now_ms();
    let (service, _db) = service(FakeAgents {
        snapshots: vec![AgentCapabilitySnapshot {
            id: "codex".into(),
            agent_type: "acp".into(),
            enabled: true,
            installed: true,
            status: "online".into(),
            last_check_status: Some("online".into()),
            last_check_at: Some(now),
            last_success_at: Some(now),
            agent_capabilities: None,
            available_models: Some(serde_json::json!(["gpt-5"])),
            available_modes: None,
            available_commands: None,
            dynamic_probe: Some(aionui_api_types::AgentDynamicProbeResult {
                agent_id: "codex".into(),
                checked_at: now - 25 * 60 * 60 * 1000,
                available_models: vec!["gpt-5".into()],
                steps: vec![],
            }),
        }],
        ..Default::default()
    })
    .await;
    let project = service
        .create(
            "system_default_user",
            CreateProjectInput {
                name: "Stale dynamic probe".into(),
                local_path: temp.path().to_string_lossy().into_owned(),
                repository_url: None,
                default_branch: None,
                project_type: "unknown".into(),
            },
        )
        .await
        .unwrap();

    let result = service
        .preflight("system_default_user", &project.id, &["codex".into()], false)
        .await
        .unwrap();

    assert_eq!(result.agents[0].level, "fail");
    assert!(result.agents[0].summary.contains("dynamic probe is stale"));
}

#[tokio::test]
async fn resource_binding_rejects_a_conversation_owned_by_another_user() {
    let temp = tempfile::tempdir().unwrap();
    let (service, db) = service(FakeAgents::default()).await;
    let project = service
        .create(
            "system_default_user",
            CreateProjectInput {
                name: "Bindings".into(),
                local_path: temp.path().to_string_lossy().into_owned(),
                repository_url: None,
                default_branch: None,
                project_type: "unknown".into(),
            },
        )
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('other-user', 'other', 'hash', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at) \
         VALUES ('other-conversation', 'other-user', 'Other', 'chat', '{}', 'pending', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let error = service
        .bind_resource("system_default_user", &project.id, "conversation", "other-conversation")
        .await
        .unwrap_err();
    assert!(matches!(error, ProjectError::NotFound(_)));
}

#[tokio::test]
async fn preflight_detects_project_directory_removed_after_registration() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().to_path_buf();
    let (service, _db) = service(FakeAgents::default()).await;
    let project = service
        .create(
            "system_default_user",
            CreateProjectInput {
                name: "Removed".into(),
                local_path: path.to_string_lossy().into_owned(),
                repository_url: None,
                default_branch: None,
                project_type: "unknown".into(),
            },
        )
        .await
        .unwrap();
    drop(temp);

    let result = service
        .preflight("system_default_user", &project.id, &[], false)
        .await
        .unwrap();
    assert_eq!(result.overall_status, "fail");
    assert!(
        result
            .checks
            .iter()
            .any(|check| check.code == "path.exists" && check.level == "fail")
    );
}
