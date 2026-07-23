use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aionui_db::{
    IConversationRepository, IProjectRepository, ITeamRepository, SqliteConversationRepository,
    SqliteProjectRepository, SqliteTeamRepository, init_database_memory,
};
use aionui_project::{
    AgentCapabilitySnapshot, CodebaseMemoryCliProvider, CreateProjectInput, KnowledgeProviderError,
    ProjectAgentCapabilityPort, ProjectError, ProjectKnowledgeFact, ProjectKnowledgeProvider,
    ProjectKnowledgeProviderHealth, ProjectKnowledgeProviderRequest, ProjectKnowledgeProviderResult, ProjectService,
    ProjectTaskContext,
};

struct NoAgents;

#[async_trait::async_trait]
impl ProjectAgentCapabilityPort for NoAgents {
    async fn snapshot(&self, _id: &str, _refresh: bool) -> Result<Option<AgentCapabilitySnapshot>, ProjectError> {
        Ok(None)
    }
}

#[derive(Default)]
struct FakeKnowledgeProvider {
    health_error: Mutex<Option<KnowledgeProviderError>>,
    result_error: Mutex<Option<KnowledgeProviderError>>,
    index_calls: AtomicUsize,
    update_calls: AtomicUsize,
}

impl FakeKnowledgeProvider {
    fn unavailable() -> Self {
        Self {
            health_error: Mutex::new(Some(KnowledgeProviderError::Unavailable)),
            ..Default::default()
        }
    }

    fn malformed() -> Self {
        Self {
            result_error: Mutex::new(Some(KnowledgeProviderError::MalformedOutput)),
            ..Default::default()
        }
    }

    fn result(&self, request: &ProjectKnowledgeProviderRequest) -> ProjectKnowledgeProviderResult {
        ProjectKnowledgeProviderResult {
            provider_project_name: request.provider_project_name.clone(),
            source_commit: request.source_commit.clone(),
            changed_paths: request.changed_paths.clone(),
            facts: vec![ProjectKnowledgeFact {
                kind: "symbol".into(),
                name: "run_task".into(),
                qualified_name: Some("app.runner.run_task".into()),
                source_path: "src/runner.rs".into(),
                source_line: Some(42),
                indexed_at: 0,
            }],
        }
    }
}

#[async_trait::async_trait]
impl ProjectKnowledgeProvider for FakeKnowledgeProvider {
    async fn health(&self) -> Result<ProjectKnowledgeProviderHealth, KnowledgeProviderError> {
        if let Some(error) = self.health_error.lock().unwrap().clone() {
            return Err(error);
        }
        Ok(ProjectKnowledgeProviderHealth {
            provider: "codebase-memory".into(),
            version: Some("test".into()),
        })
    }

    async fn index(
        &self,
        request: &ProjectKnowledgeProviderRequest,
    ) -> Result<ProjectKnowledgeProviderResult, KnowledgeProviderError> {
        self.index_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.result_error.lock().unwrap().clone() {
            return Err(error);
        }
        Ok(self.result(request))
    }

    async fn update(
        &self,
        request: &ProjectKnowledgeProviderRequest,
    ) -> Result<ProjectKnowledgeProviderResult, KnowledgeProviderError> {
        self.update_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.result(request))
    }

    async fn architecture(
        &self,
        _provider_project_name: &str,
    ) -> Result<Vec<ProjectKnowledgeFact>, KnowledgeProviderError> {
        Ok(vec![ProjectKnowledgeFact {
            kind: "route".into(),
            name: "POST /api/run".into(),
            qualified_name: None,
            source_path: "src/routes.rs".into(),
            source_line: Some(18),
            indexed_at: 0,
        }])
    }

    async fn search(
        &self,
        _provider_project_name: &str,
        _query: &str,
    ) -> Result<Vec<ProjectKnowledgeFact>, KnowledgeProviderError> {
        Ok(Vec::new())
    }

    async fn trace(
        &self,
        _provider_project_name: &str,
        _function_name: &str,
    ) -> Result<Vec<ProjectKnowledgeFact>, KnowledgeProviderError> {
        Ok(Vec::new())
    }

    async fn task_context(
        &self,
        provider_project_name: &str,
        query: &str,
        generation: i64,
    ) -> Result<ProjectTaskContext, KnowledgeProviderError> {
        Ok(ProjectTaskContext {
            id: String::new(),
            project_id: String::new(),
            provider_project_name: provider_project_name.into(),
            generation,
            query: query.into(),
            symbols: vec!["app.runner.run_task".into()],
            callers: vec!["app.routes.start_run".into()],
            tests: vec!["tests/run_task.rs".into()],
            routes: vec!["POST /api/run".into()],
            data_entities: vec!["development_runs".into()],
            created_at: 0,
        })
    }
}

fn commit_file(root: &Path, name: &str, contents: &str, message: &str) -> String {
    let repository = git2::Repository::open(root)
        .or_else(|_| git2::Repository::init(root))
        .unwrap();
    std::fs::write(root.join(name), contents).unwrap();
    let mut index = repository.index().unwrap();
    index.add_path(Path::new(name)).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repository.find_tree(tree_id).unwrap();
    let signature = git2::Signature::now("Aion test", "aion@example.test").unwrap();
    let parents = repository
        .head()
        .ok()
        .and_then(|head| head.target())
        .and_then(|id| repository.find_commit(id).ok())
        .into_iter()
        .collect::<Vec<_>>();
    let parent_refs = parents.iter().collect::<Vec<_>>();
    repository
        .commit(Some("HEAD"), &signature, &signature, message, &tree, &parent_refs)
        .unwrap()
        .to_string()
}

async fn service(provider: Arc<FakeKnowledgeProvider>) -> (ProjectService, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let project_repo: Arc<dyn IProjectRepository> = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
    let conversation_repo: Arc<dyn IConversationRepository> =
        Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let team_repo: Arc<dyn ITeamRepository> = Arc::new(SqliteTeamRepository::new(db.pool().clone()));
    (
        ProjectService::new(project_repo, conversation_repo, team_repo, Arc::new(NoAgents))
            .with_knowledge_provider(provider),
        db,
    )
}

async fn create_project(service: &ProjectService, root: &Path) -> String {
    service
        .create(
            "system_default_user",
            CreateProjectInput {
                name: "Knowledge project".into(),
                local_path: root.to_string_lossy().into_owned(),
                repository_url: None,
                default_branch: Some("main".into()),
                project_type: "single".into(),
            },
        )
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn initial_index_unchanged_noop_and_changed_file_incremental_update() {
    let temp = tempfile::tempdir().unwrap();
    commit_file(temp.path(), "src.rs", "fn first() {}\n", "initial");
    let provider = Arc::new(FakeKnowledgeProvider::default());
    let (service, _db) = service(provider.clone()).await;
    let project_id = create_project(&service, temp.path()).await;

    let initial = service
        .refresh_knowledge("system_default_user", &project_id)
        .await
        .unwrap();
    assert_eq!(initial.status, "healthy");
    assert_eq!(initial.generation, 1);
    assert_eq!(provider.index_calls.load(Ordering::SeqCst), 1);

    let unchanged = service
        .refresh_knowledge("system_default_user", &project_id)
        .await
        .unwrap();
    assert_eq!(unchanged.generation, 1);
    assert_eq!(provider.index_calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.update_calls.load(Ordering::SeqCst), 0);

    std::fs::write(temp.path().join("src.rs"), "fn changed() {}\n").unwrap();
    let stale = service
        .get_knowledge_status("system_default_user", &project_id)
        .await
        .unwrap();
    assert_eq!(stale.status, "stale");
    assert_eq!(stale.changed_paths, vec!["src.rs"]);

    let updated = service
        .refresh_knowledge("system_default_user", &project_id)
        .await
        .unwrap();
    assert_eq!(updated.status, "healthy");
    assert_eq!(updated.generation, 2);
    assert_eq!(provider.update_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unavailable_and_malformed_provider_states_are_explicit_and_redacted() {
    for (provider, expected) in [
        (Arc::new(FakeKnowledgeProvider::unavailable()), "unavailable"),
        (Arc::new(FakeKnowledgeProvider::malformed()), "failed"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        commit_file(temp.path(), "lib.rs", "pub fn demo() {}\n", "initial");
        let (service, _db) = service(provider).await;
        let project_id = create_project(&service, temp.path()).await;

        let error = service
            .refresh_knowledge("system_default_user", &project_id)
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("stderr"));
        let status = service
            .get_knowledge_status("system_default_user", &project_id)
            .await
            .unwrap();
        assert_eq!(status.status, expected);
        assert!(matches!(
            status.error_category.as_deref(),
            Some("unavailable" | "malformed_output")
        ));
    }
}

#[tokio::test]
async fn facts_keep_source_and_index_time_and_reads_are_owner_scoped() {
    let temp = tempfile::tempdir().unwrap();
    commit_file(temp.path(), "main.rs", "fn main() {}\n", "initial");
    let provider = Arc::new(FakeKnowledgeProvider::default());
    let (service, _db) = service(provider).await;
    let project_id = create_project(&service, temp.path()).await;
    let indexed = service
        .refresh_knowledge("system_default_user", &project_id)
        .await
        .unwrap();

    let facts = service
        .list_knowledge_facts("system_default_user", &project_id)
        .await
        .unwrap();
    assert_eq!(facts.len(), 2);
    assert!(facts.iter().all(|fact| !fact.source_path.is_empty()));
    assert!(facts.iter().all(|fact| fact.indexed_at == indexed.indexed_at.unwrap()));
    assert!(service.list_knowledge_facts("other-user", &project_id).await.is_err());

    let context = service
        .task_context("system_default_user", &project_id, "change run task")
        .await
        .unwrap();
    assert_eq!(context.generation, indexed.generation);
    assert_eq!(context.symbols, vec!["app.runner.run_task"]);
    assert_eq!(context.callers, vec!["app.routes.start_run"]);
    assert_eq!(context.tests, vec!["tests/run_task.rs"]);
    assert_eq!(context.routes, vec!["POST /api/run"]);
    assert_eq!(context.data_entities, vec!["development_runs"]);
    assert!(service.task_context("other-user", &project_id, "secret").await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn cli_adapter_rejects_malformed_stdout_without_exposing_it() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let executable = temp.path().join("fake-codebase-memory");
    std::fs::write(
        &executable,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'codebase-memory-mcp test'; else echo 'private stderr payload' >&2; echo 'not-json'; fi\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions).unwrap();
    let provider = CodebaseMemoryCliProvider::new(executable.to_string_lossy());

    assert_eq!(provider.health().await.unwrap().provider, "codebase-memory");
    let error = provider
        .index(&ProjectKnowledgeProviderRequest {
            project_path: temp.path().to_string_lossy().into_owned(),
            provider_project_name: "test-project".into(),
            source_commit: None,
            changed_paths: Vec::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(error, KnowledgeProviderError::MalformedOutput);
    assert!(!error.to_string().contains("private stderr payload"));
}
