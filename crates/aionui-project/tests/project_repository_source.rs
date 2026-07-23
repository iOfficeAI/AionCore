use std::path::Path;

use std::sync::Arc;

use aionui_db::{
    IConversationRepository, IProjectRepository, ITeamRepository, SqliteConversationRepository,
    SqliteProjectRepository, SqliteTeamRepository, init_database_memory,
};
use aionui_project::{
    AgentCapabilitySnapshot, DirtyWorktreeChoice, ProjectAgentCapabilityPort, ProjectError,
    ProjectRepositoryOnboardingInput, ProjectService, RepositoryOnboarder, RepositorySource,
};

struct NoAgents;

#[async_trait::async_trait]
impl ProjectAgentCapabilityPort for NoAgents {
    async fn snapshot(&self, _id: &str, _refresh: bool) -> Result<Option<AgentCapabilitySnapshot>, ProjectError> {
        Ok(None)
    }
}

fn commit_file(root: &Path, name: &str, contents: &str) {
    let repository = git2::Repository::init(root).unwrap();
    std::fs::write(root.join(name), contents).unwrap();
    let mut index = repository.index().unwrap();
    index.add_path(Path::new(name)).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repository.find_tree(tree_id).unwrap();
    let signature = git2::Signature::now("Aion test", "aion@example.test").unwrap();
    repository
        .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
        .unwrap();
}

#[tokio::test]
async fn local_registration_detects_repository_and_project_facts() {
    let temp = tempfile::tempdir().unwrap();
    let repository = temp.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    commit_file(&repository, "Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    std::fs::create_dir_all(repository.join("crates/demo/src")).unwrap();
    std::fs::write(
        repository.join("crates/demo/Cargo.toml"),
        "[package]\nname = \"demo\"\n",
    )
    .unwrap();
    std::fs::write(repository.join("crates/demo/src/lib.rs"), "pub fn demo() {}\n").unwrap();
    std::fs::write(repository.join("AGENTS.md"), "# Project rules\n").unwrap();
    std::fs::write(
        repository.join("asset.bin"),
        "version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 3\n",
    )
    .unwrap();
    std::fs::write(
        repository.join(".gitmodules"),
        "[submodule \"vendor/demo\"]\n\tpath = vendor/demo\n\turl = https://example.test/demo.git\n",
    )
    .unwrap();

    let managed_root = temp.path().join("managed");
    let onboarder = RepositoryOnboarder::new(managed_root);
    let facts = onboarder
        .onboard(
            RepositorySource::Local {
                path: repository.join(".").to_string_lossy().into_owned(),
            },
            DirtyWorktreeChoice::Preserve,
        )
        .await
        .unwrap();

    assert_eq!(facts.local_path, repository.canonicalize().unwrap().to_string_lossy());
    assert!(facts.baseline_commit.is_some());
    assert!(facts.dirty);
    assert!(facts.languages.contains(&"rust".to_owned()));
    assert!(facts.package_managers.contains(&"cargo".to_owned()));
    assert!(facts.rules_files.contains(&"AGENTS.md".to_owned()));
    assert!(facts.monorepo_packages.contains(&"crates/demo".to_owned()));
    assert!(facts.lfs_detected);
    assert_eq!(facts.submodules[0].path, "vendor/demo");
}

#[tokio::test]
async fn dirty_choices_reject_preserve_or_capture_a_non_destructive_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let repository = temp.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    commit_file(&repository, "README.md", "baseline\n");
    std::fs::write(repository.join("README.md"), "changed\n").unwrap();
    std::fs::write(repository.join("new.txt"), "untracked\n").unwrap();
    let onboarder = RepositoryOnboarder::new(temp.path().join("managed"));

    let rejected = onboarder
        .onboard(
            RepositorySource::Local {
                path: repository.to_string_lossy().into_owned(),
            },
            DirtyWorktreeChoice::Reject,
        )
        .await
        .unwrap_err();
    assert!(rejected.to_string().contains("dirty"));

    let preserved = onboarder
        .onboard(
            RepositorySource::Local {
                path: repository.to_string_lossy().into_owned(),
            },
            DirtyWorktreeChoice::Preserve,
        )
        .await
        .unwrap();
    assert!(preserved.dirty);
    assert!(preserved.dirty_snapshot_ref.is_none());

    let snapshotted = onboarder
        .onboard(
            RepositorySource::Local {
                path: repository.to_string_lossy().into_owned(),
            },
            DirtyWorktreeChoice::Snapshot,
        )
        .await
        .unwrap();
    let snapshot = snapshotted.dirty_snapshot_ref.unwrap();
    assert!(Path::new(&snapshot).join("changes.patch").is_file());
    assert!(Path::new(&snapshot).join("untracked/new.txt").is_file());
    assert_eq!(
        std::fs::read_to_string(repository.join("README.md")).unwrap(),
        "changed\n"
    );
    assert!(repository.join("new.txt").is_file());
}

#[tokio::test]
async fn clone_uses_a_canonical_managed_child_and_validates_branch_and_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    commit_file(&source, "README.md", "clone me\n");
    let managed_root = temp.path().join("managed");
    let onboarder = RepositoryOnboarder::new(&managed_root);

    let facts = onboarder
        .onboard(
            RepositorySource::Clone {
                url: format!("file://{}", source.to_string_lossy()),
                destination_name: "safe-child".into(),
                branch: None,
                credential_reference: Some("vault:github-production".into()),
            },
            DirtyWorktreeChoice::Reject,
        )
        .await
        .unwrap();

    let canonical_root = managed_root.canonicalize().unwrap();
    assert!(Path::new(&facts.local_path).starts_with(&canonical_root));
    assert_eq!(facts.credential_reference.as_deref(), Some("vault:github-production"));
    let serialized = serde_json::to_string(&facts).unwrap();
    assert!(!serialized.contains("token"));
    assert!(!serialized.contains("password"));

    let missing_branch = onboarder
        .onboard(
            RepositorySource::Clone {
                url: format!("file://{}", source.to_string_lossy()),
                destination_name: "missing-branch".into(),
                branch: Some("does-not-exist".into()),
                credential_reference: None,
            },
            DirtyWorktreeChoice::Reject,
        )
        .await
        .unwrap_err();
    assert!(missing_branch.to_string().contains("clone"));

    for url in [
        "https://example.test/org/repo.git",
        "ssh://git@example.test/org/repo.git",
        "git@example.test:org/repo.git",
    ] {
        RepositoryOnboarder::validate_clone_url(url).unwrap();
    }
    assert!(RepositoryOnboarder::validate_clone_url("https://token@example.test/repo.git").is_err());
    assert!(RepositoryOnboarder::validate_destination_name("../escape").is_err());
    assert!(RepositoryOnboarder::validate_branch_name("../../escape").is_err());
    assert!(RepositoryOnboarder::validate_submodule_path("../outside").is_err());
}

#[tokio::test]
async fn project_service_persists_repository_facts_for_owner_scoped_reads() {
    let temp = tempfile::tempdir().unwrap();
    let repository = temp.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    commit_file(&repository, "main.rs", "fn main() {}\n");
    let database = init_database_memory().await.unwrap();
    let pool = database.pool().clone();
    let project_repo: Arc<dyn IProjectRepository> = Arc::new(SqliteProjectRepository::new(pool.clone()));
    let conversation_repo: Arc<dyn IConversationRepository> = Arc::new(SqliteConversationRepository::new(pool.clone()));
    let team_repo: Arc<dyn ITeamRepository> = Arc::new(SqliteTeamRepository::new(pool));
    let service = ProjectService::new(project_repo, conversation_repo, team_repo, Arc::new(NoAgents))
        .with_managed_project_root(temp.path().join("managed"));

    let created = service
        .onboard(
            "system_default_user",
            ProjectRepositoryOnboardingInput {
                name: "Persisted repository".into(),
                source: RepositorySource::Local {
                    path: repository.to_string_lossy().into_owned(),
                },
                dirty_worktree_choice: DirtyWorktreeChoice::Reject,
                project_type: "single".into(),
            },
        )
        .await
        .unwrap();
    let persisted = service
        .get_repository_facts("system_default_user", &created.project.id)
        .await
        .unwrap();

    assert_eq!(persisted.baseline_commit, created.repository.baseline_commit);
    assert_eq!(persisted.languages, vec!["rust"]);
    assert!(
        service
            .get_repository_facts("another-user", &created.project.id)
            .await
            .is_err()
    );
}
