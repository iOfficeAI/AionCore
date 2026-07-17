use std::path::Path;
use std::sync::{Arc, Mutex};

use aionui_db::{IDevelopmentRepository, IProjectRepository, SqliteDevelopmentRepository, SqliteProjectRepository};
use aionui_development::{
    CreatePullRequestInput, DeliveryProvider, DeliveryProviderSnapshot, DeliveryService, PrepareDeliveryInput,
    ProviderCiCheck, ProviderPullRequest,
};
use async_trait::async_trait;

#[derive(Default)]
struct FakeProvider {
    pushes: Mutex<usize>,
    pull_requests: Mutex<usize>,
    merges: Mutex<usize>,
    snapshot: Mutex<DeliveryProviderSnapshot>,
    preflight_error: Mutex<Option<String>>,
}

#[async_trait]
impl DeliveryProvider for FakeProvider {
    async fn preflight(&self, _repository: &Path) -> Result<(), String> {
        match self.preflight_error.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn push(&self, _repository: &Path, _branch: &str) -> Result<(), String> {
        *self.pushes.lock().unwrap() += 1;
        Ok(())
    }

    async fn ensure_pull_request(
        &self,
        _repository: &Path,
        _head: &str,
        _base: &str,
        _title: &str,
        _body: &str,
    ) -> Result<ProviderPullRequest, String> {
        *self.pull_requests.lock().unwrap() += 1;
        Ok(ProviderPullRequest {
            number: 7,
            url: "https://github.example/pr/7".into(),
            status: "open".into(),
            review_status: "pending".into(),
        })
    }

    async fn synchronize(&self, _repository: &Path, _number: i64) -> Result<DeliveryProviderSnapshot, String> {
        Ok(self.snapshot.lock().unwrap().clone())
    }

    async fn merge(&self, _repository: &Path, _number: i64) -> Result<(), String> {
        *self.merges.lock().unwrap() += 1;
        Ok(())
    }
}

fn git(repository: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

async fn setup() -> (
    DeliveryService,
    Arc<FakeProvider>,
    Arc<SqliteDevelopmentRepository>,
    tempfile::TempDir,
    aionui_db::Database,
) {
    let workspace = tempfile::tempdir().unwrap();
    git(workspace.path(), &["init", "-b", "main"]);
    git(workspace.path(), &["config", "user.email", "delivery@example.com"]);
    git(workspace.path(), &["config", "user.name", "Delivery Test"]);
    std::fs::write(workspace.path().join("README.md"), "baseline\n").unwrap();
    git(workspace.path(), &["add", "."]);
    git(workspace.path(), &["commit", "-m", "baseline"]);
    let baseline = git(workspace.path(), &["rev-parse", "HEAD"]);

    let db = aionui_db::init_database_memory().await.unwrap();
    let project_repo = Arc::new(SqliteProjectRepository::new(db.pool().clone()));
    project_repo
        .create(&aionui_db::models::ProjectRow {
            id: "project-delivery".into(),
            user_id: "system_default_user".into(),
            name: "Delivery".into(),
            local_path: workspace.path().to_string_lossy().into_owned(),
            repository_url: Some("https://github.com/example/delivery.git".into()),
            default_branch: Some("main".into()),
            project_type: "single".into(),
            created_at: 1,
            updated_at: 1,
        })
        .await
        .unwrap();
    let repo = Arc::new(SqliteDevelopmentRepository::new(db.pool().clone()));
    repo.create_run(&aionui_db::models::DevelopmentRunRow {
        id: "run-delivery".into(),
        user_id: "system_default_user".into(),
        project_id: "project-delivery".into(),
        team_id: None,
        source_channel: Some("webui".into()),
        source_user_id: None,
        execution_mode: "single".into(),
        status: "reviewing".into(),
        request_summary: "Ship delivery".into(),
        acceptance_criteria: r#"["tests pass"]"#.into(),
        baseline_commit: Some(baseline),
        integration_branch: None,
        started_at: Some(1),
        finished_at: None,
        created_at: 1,
        updated_at: 1,
    })
    .await
    .unwrap();
    repo.create_task(&aionui_db::models::DevelopmentTaskRow {
        id: "task-complete".into(),
        team_id: "run-delivery".into(),
        run_id: Some("run-delivery".into()),
        subject: "Implement".into(),
        description: None,
        status: "completed".into(),
        owner: Some("agent".into()),
        blocked_by: "[]".into(),
        blocks: "[]".into(),
        metadata: None,
        acceptance_criteria: r#"["tests pass"]"#.into(),
        task_type: "implementation".into(),
        risk_level: "medium".into(),
        assigned_workspace_lease_id: None,
        review_status: "approved".into(),
        verification_status: "passed".into(),
        created_at: 1,
        updated_at: 1,
    })
    .await
    .unwrap();
    repo.create_gate(&aionui_db::models::QualityGateRunRow {
        id: "gate-pass".into(),
        run_id: "run-delivery".into(),
        task_id: None,
        gate_type: "unit_test".into(),
        command: "cargo test".into(),
        working_directory: workspace.path().to_string_lossy().into_owned(),
        exit_code: Some(0),
        status: "passed".into(),
        stdout_artifact_id: None,
        stderr_artifact_id: None,
        duration_ms: Some(1),
        isolation_mode: "host".into(),
        execution_id: Some("gate-failed".into()),
        required: true,
        started_at: Some(1),
        finished_at: Some(2),
        created_at: 1,
    })
    .await
    .unwrap();

    let provider = Arc::new(FakeProvider::default());
    let service = DeliveryService::new(repo.clone(), project_repo, provider.clone());
    (service, provider, repo, workspace, db)
}

#[tokio::test]
async fn prepare_creates_non_protected_branch_and_candidate_commit_idempotently() {
    let (service, _provider, _repo, workspace, _db) = setup().await;
    std::fs::write(workspace.path().join("README.md"), "delivered\n").unwrap();

    let first = service
        .prepare(
            "system_default_user",
            "run-delivery",
            PrepareDeliveryInput {
                message: Some("feat: deliver".into()),
            },
        )
        .await
        .unwrap();
    let second = service
        .prepare(
            "system_default_user",
            "run-delivery",
            PrepareDeliveryInput { message: None },
        )
        .await
        .unwrap();

    assert_ne!(first.branch, "main");
    assert!(first.branch.starts_with("aion/run/"));
    assert_eq!(first.commit_sha, second.commit_sha);
    assert_eq!(git(workspace.path(), &["branch", "--show-current"]), first.branch);
}

#[tokio::test]
async fn remote_flow_is_retry_safe_and_failed_ci_creates_one_rework_task() {
    let (service, provider, repo, workspace, _db) = setup().await;
    std::fs::write(workspace.path().join("README.md"), "delivered\n").unwrap();
    service
        .prepare(
            "system_default_user",
            "run-delivery",
            PrepareDeliveryInput { message: None },
        )
        .await
        .unwrap();
    service.push("system_default_user", "run-delivery", true).await.unwrap();
    service.push("system_default_user", "run-delivery", true).await.unwrap();
    assert_eq!(*provider.pushes.lock().unwrap(), 1);

    service
        .create_pull_request(
            "system_default_user",
            "run-delivery",
            CreatePullRequestInput {
                title: Some("Ship delivery".into()),
                confirmed: true,
            },
        )
        .await
        .unwrap();
    service
        .create_pull_request(
            "system_default_user",
            "run-delivery",
            CreatePullRequestInput {
                title: None,
                confirmed: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(*provider.pull_requests.lock().unwrap(), 1);

    *provider.snapshot.lock().unwrap() = DeliveryProviderSnapshot {
        pull_request: ProviderPullRequest {
            number: 7,
            url: "https://github.example/pr/7".into(),
            status: "open".into(),
            review_status: "approved".into(),
        },
        checks: vec![ProviderCiCheck {
            id: "check-unit".into(),
            name: "unit".into(),
            status: "failed".into(),
            details_url: None,
            summary: Some("tests failed".into()),
        }],
    };
    let failed = service.sync("system_default_user", "run-delivery").await.unwrap();
    assert_eq!(failed.ci_status, "failed");
    assert!(
        service
            .merge("system_default_user", "run-delivery", true)
            .await
            .is_err()
    );
    let rework = repo
        .list_tasks("run-delivery")
        .await
        .unwrap()
        .into_iter()
        .filter(|task| task.task_type == "rework")
        .collect::<Vec<_>>();
    assert_eq!(rework.len(), 1);
    service.sync("system_default_user", "run-delivery").await.unwrap();
    assert_eq!(
        repo.list_tasks("run-delivery")
            .await
            .unwrap()
            .into_iter()
            .filter(|task| task.task_type == "rework")
            .count(),
        1
    );

    *provider.snapshot.lock().unwrap() = DeliveryProviderSnapshot {
        pull_request: ProviderPullRequest {
            number: 7,
            url: "https://github.example/pr/7".into(),
            status: "open".into(),
            review_status: "approved".into(),
        },
        checks: vec![ProviderCiCheck {
            id: "check-unit".into(),
            name: "unit".into(),
            status: "passed".into(),
            details_url: None,
            summary: Some("tests passed".into()),
        }],
    };
    let passed = service.sync("system_default_user", "run-delivery").await.unwrap();
    assert_eq!(passed.ci_status, "passed");
    assert_eq!(passed.review_status, "approved");
    let rework_task = repo
        .list_tasks("run-delivery")
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.task_type == "rework")
        .unwrap();
    repo.update_task_state("run-delivery", &rework_task.id, "completed", "approved", "passed")
        .await
        .unwrap();
    let ready = service.sync("system_default_user", "run-delivery").await.unwrap();
    assert_eq!(ready.status, "merge_ready");
    let merged = service
        .merge("system_default_user", "run-delivery", true)
        .await
        .unwrap();
    assert_eq!(merged.status, "merged");
    assert_eq!(*provider.merges.lock().unwrap(), 1);
}

#[tokio::test]
async fn prepare_blocks_latest_failed_gate_and_protected_integration_branch() {
    let (service, _provider, repo, workspace, db) = setup().await;
    std::fs::write(workspace.path().join("README.md"), "delivered\n").unwrap();
    repo.create_gate(&aionui_db::models::QualityGateRunRow {
        id: "gate-latest-failed".into(),
        run_id: "run-delivery".into(),
        task_id: None,
        gate_type: "unit_test".into(),
        command: "cargo test".into(),
        working_directory: workspace.path().to_string_lossy().into_owned(),
        exit_code: Some(1),
        status: "failed".into(),
        stdout_artifact_id: None,
        stderr_artifact_id: None,
        duration_ms: Some(1),
        isolation_mode: "host".into(),
        execution_id: Some("gate-pass".into()),
        required: true,
        started_at: Some(2),
        finished_at: Some(3),
        created_at: 2,
    })
    .await
    .unwrap();
    assert!(
        service
            .prepare(
                "system_default_user",
                "run-delivery",
                PrepareDeliveryInput { message: None },
            )
            .await
            .is_err()
    );

    sqlx::query("DELETE FROM quality_gate_runs WHERE id = 'gate-latest-failed'")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE development_runs SET integration_branch = 'main' WHERE id = 'run-delivery'")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        service
            .prepare(
                "system_default_user",
                "run-delivery",
                PrepareDeliveryInput { message: None },
            )
            .await
            .is_err()
    );
    assert_eq!(git(workspace.path(), &["branch", "--show-current"]), "main");
}

#[tokio::test]
async fn no_change_requires_explicit_artifact_and_cannot_be_pushed() {
    let (service, _provider, repo, _workspace, _db) = setup().await;
    assert!(
        service
            .prepare(
                "system_default_user",
                "run-delivery",
                PrepareDeliveryInput { message: None },
            )
            .await
            .is_err()
    );
    repo.create_artifact(&aionui_db::models::TaskArtifactRow {
        id: "accepted-no-change".into(),
        run_id: "run-delivery".into(),
        task_id: Some("task-complete".into()),
        artifact_type: "no_code_change".into(),
        path_or_uri: "review-approved".into(),
        checksum: "review-approved".into(),
        producer_agent_id: Some("reviewer".into()),
        metadata: Some(r#"{"reason":"documentation-only acceptance"}"#.into()),
        created_at: 2,
    })
    .await
    .unwrap();
    let delivery = service
        .prepare(
            "system_default_user",
            "run-delivery",
            PrepareDeliveryInput { message: None },
        )
        .await
        .unwrap();
    assert_eq!(delivery.status, "no_change");
    assert!(delivery.commit_sha.is_none());
    assert!(service.push("system_default_user", "run-delivery", true).await.is_err());
}

#[tokio::test]
async fn provider_failures_are_persisted_without_credentials() {
    let (service, provider, _repo, workspace, _db) = setup().await;
    std::fs::write(workspace.path().join("README.md"), "delivered\n").unwrap();
    service
        .prepare(
            "system_default_user",
            "run-delivery",
            PrepareDeliveryInput { message: None },
        )
        .await
        .unwrap();
    *provider.preflight_error.lock().unwrap() =
        Some("authorization: bearer-secret token=ghp_sensitive password=hunter2".into());
    assert!(service.push("system_default_user", "run-delivery", true).await.is_err());
    let delivery = service.get("system_default_user", "run-delivery").await.unwrap();
    let error = delivery.last_error.unwrap();
    assert!(error.contains("[REDACTED]"));
    assert!(!error.contains("bearer-secret"));
    assert!(!error.contains("ghp_sensitive"));
    assert!(!error.contains("hunter2"));
    assert!(!delivery.report_json.contains("ghp_sensitive"));
}
