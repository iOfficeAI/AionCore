use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use aionui_common::{generate_id, now_ms};
use aionui_db::models::AgentWorkspaceLeaseRow;
use aionui_db::{AgentWorkspaceLeaseUpdate, IAgentWorkspaceLeaseRepository};
use async_trait::async_trait;
use tokio::process::Command;
use tracing::{info, warn};

use crate::TeamError;

pub const INTEGRATION_SLOT_ID: &str = "__integration__";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceAgentSpec {
    pub slot_id: String,
    pub name: String,
}

impl WorkspaceAgentSpec {
    pub fn new(slot_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            slot_id: slot_id.into(),
            name: name.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedTeamWorkspaces {
    pub repository_path: String,
    pub base_commit: String,
    pub integration: AgentWorkspaceLeaseRow,
    pub agent_leases: Vec<AgentWorkspaceLeaseRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceCleanupDisposition {
    Removed,
    BranchRetained,
    DirtyPreserved,
    MissingPreserved,
    AlreadyReleased,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceCleanupResult {
    pub lease_id: String,
    pub slot_id: String,
    pub disposition: WorkspaceCleanupDisposition,
}

#[async_trait]
pub trait TeamWorkspaceManager: Send + Sync {
    async fn prepare_team(
        &self,
        user_id: &str,
        team_id: &str,
        repository_path: &str,
        agents: &[WorkspaceAgentSpec],
    ) -> Result<PreparedTeamWorkspaces, TeamError>;

    async fn allocate_agent(
        &self,
        user_id: &str,
        team_id: &str,
        slot: &WorkspaceAgentSpec,
    ) -> Result<AgentWorkspaceLeaseRow, TeamError>;

    async fn list_team_leases(&self, team_id: &str) -> Result<Vec<AgentWorkspaceLeaseRow>, TeamError>;

    async fn release_slot(&self, team_id: &str, slot_id: &str) -> Result<WorkspaceCleanupResult, TeamError>;

    async fn release_team(&self, team_id: &str) -> Result<Vec<WorkspaceCleanupResult>, TeamError>;

    async fn reconcile_all(&self) -> Result<(), TeamError>;

    async fn validate_owned_path(&self, team_id: &str, slot_id: &str, path: &Path) -> Result<PathBuf, TeamError>;
}

#[derive(Clone)]
pub struct GitTeamWorkspaceManager {
    leases: Arc<dyn IAgentWorkspaceLeaseRepository>,
    managed_root: PathBuf,
}

impl GitTeamWorkspaceManager {
    pub fn new(leases: Arc<dyn IAgentWorkspaceLeaseRepository>, managed_root: PathBuf) -> Self {
        Self { leases, managed_root }
    }

    pub fn integration_branch_name(team_id: &str) -> String {
        format!("aion/team/{}/integration", safe_component(team_id, 24, "team"))
    }

    fn agent_branch_name(team_id: &str, slot: &WorkspaceAgentSpec) -> String {
        format!(
            "aion/team/{}/agent/{}-{}",
            safe_component(team_id, 24, "team"),
            safe_component(&slot.slot_id, 18, "slot"),
            safe_component(&slot.name, 24, "agent")
        )
    }

    fn team_root(&self, team_id: &str) -> PathBuf {
        self.managed_root.join(safe_component(team_id, 40, "team"))
    }

    async fn inspect_repository(&self, repository_path: &str) -> Result<(String, String), TeamError> {
        let requested = Path::new(repository_path);
        if !requested.is_dir() {
            return Err(workspace_error(format!(
                "repository directory is unavailable: {}",
                requested.display()
            )));
        }
        let root = git_output(requested, &["rev-parse", "--show-toplevel"]).await?;
        let root = PathBuf::from(root)
            .canonicalize()
            .map_err(|error| workspace_error(format!("failed to canonicalize repository: {error}")))?;
        let status = git_output(&root, &["status", "--porcelain", "--untracked-files=all"]).await?;
        if !status.trim().is_empty() {
            return Err(workspace_error(
                "repository is dirty; commit, stash, snapshot, or cancel before isolated Team creation".into(),
            ));
        }
        let base_commit = git_output(&root, &["rev-parse", "HEAD"]).await?;
        Ok((root.to_string_lossy().into_owned(), base_commit))
    }

    async fn branch_must_be_absent(&self, repository: &Path, branch: &str) -> Result<(), TeamError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["show-ref", "--verify", "--quiet"])
            .arg(format!("refs/heads/{branch}"))
            .output()
            .await
            .map_err(|error| workspace_error(format!("failed to inspect branch {branch}: {error}")))?;
        match output.status.code() {
            Some(1) => Ok(()),
            Some(0) => Err(workspace_error(format!("branch already exists: {branch}"))),
            _ => Err(workspace_error(format!(
                "failed to inspect branch {branch}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
        }
    }

    fn lease_row(
        &self,
        user_id: &str,
        team_id: &str,
        slot_id: &str,
        repository_path: &str,
        worktree_path: &Path,
        branch_name: String,
        base_commit: &str,
    ) -> AgentWorkspaceLeaseRow {
        let now = now_ms();
        AgentWorkspaceLeaseRow {
            id: generate_id(),
            team_id: team_id.to_owned(),
            user_id: user_id.to_owned(),
            slot_id: slot_id.to_owned(),
            workspace_mode: "isolated_worktree".into(),
            repository_path: repository_path.to_owned(),
            worktree_path: worktree_path.to_string_lossy().into_owned(),
            branch_name,
            base_commit: base_commit.to_owned(),
            allowed_paths: r#"["."]"#.into(),
            lease_status: "provisioning".into(),
            cleanup_status: "none".into(),
            conflict_files: "[]".into(),
            last_error: None,
            created_at: now,
            updated_at: now,
            released_at: None,
        }
    }

    async fn create_worktree(&self, mut lease: AgentWorkspaceLeaseRow) -> Result<AgentWorkspaceLeaseRow, TeamError> {
        let repository = Path::new(&lease.repository_path);
        let worktree = Path::new(&lease.worktree_path);
        self.branch_must_be_absent(repository, &lease.branch_name).await?;
        if worktree.exists() {
            return Err(workspace_error(format!(
                "managed worktree path already exists: {}",
                worktree.display()
            )));
        }
        if let Some(parent) = worktree.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| workspace_error(format!("failed to create managed worktree directory: {error}")))?;
        }
        self.leases.create(&lease).await?;

        let result = git_output(
            repository,
            &[
                "worktree",
                "add",
                "-b",
                &lease.branch_name,
                &lease.worktree_path,
                &lease.base_commit,
            ],
        )
        .await;
        if let Err(error) = result {
            let message = error.to_string();
            self.leases
                .update(
                    &lease.id,
                    &AgentWorkspaceLeaseUpdate {
                        lease_status: Some("conflict".into()),
                        cleanup_status: Some("provision_failed".into()),
                        last_error: Some(Some(message)),
                        ..Default::default()
                    },
                )
                .await?;
            return Err(error);
        }
        self.leases
            .update(
                &lease.id,
                &AgentWorkspaceLeaseUpdate {
                    lease_status: Some("active".into()),
                    ..Default::default()
                },
            )
            .await?;
        lease.lease_status = "active".into();
        Ok(lease)
    }

    async fn cleanup_fresh_lease(&self, lease: &AgentWorkspaceLeaseRow) {
        let worktree = Path::new(&lease.worktree_path);
        if worktree.exists() {
            let _ = git_output(
                Path::new(&lease.repository_path),
                &["worktree", "remove", "--force", &lease.worktree_path],
            )
            .await;
        }
        let _ = git_output(Path::new(&lease.repository_path), &["branch", "-D", &lease.branch_name]).await;
        let _ = self
            .leases
            .update(
                &lease.id,
                &AgentWorkspaceLeaseUpdate {
                    lease_status: Some("released".into()),
                    cleanup_status: Some("provision_rolled_back".into()),
                    released_at: Some(Some(now_ms())),
                    ..Default::default()
                },
            )
            .await;
    }

    async fn source_lease_for_team(&self, team_id: &str) -> Result<AgentWorkspaceLeaseRow, TeamError> {
        self.leases
            .get_for_team_slot(team_id, INTEGRATION_SLOT_ID)
            .await?
            .ok_or_else(|| workspace_error(format!("integration workspace lease is missing for Team {team_id}")))
    }
}

#[async_trait]
impl TeamWorkspaceManager for GitTeamWorkspaceManager {
    async fn prepare_team(
        &self,
        user_id: &str,
        team_id: &str,
        repository_path: &str,
        agents: &[WorkspaceAgentSpec],
    ) -> Result<PreparedTeamWorkspaces, TeamError> {
        if agents.is_empty() {
            return Err(workspace_error("at least one workspace agent is required".into()));
        }
        let mut slots = std::collections::HashSet::new();
        if agents.iter().any(|agent| !slots.insert(agent.slot_id.as_str())) {
            return Err(workspace_error("workspace agent slot IDs must be unique".into()));
        }
        let (repository_path, base_commit) = self.inspect_repository(repository_path).await?;
        let repository = Path::new(&repository_path);
        let team_root = self.team_root(team_id);
        let integration_branch = Self::integration_branch_name(team_id);
        let integration_path = team_root.join("integration");

        self.branch_must_be_absent(repository, &integration_branch).await?;
        for slot in agents {
            self.branch_must_be_absent(repository, &Self::agent_branch_name(team_id, slot))
                .await?;
        }
        if team_root.exists() {
            return Err(workspace_error(format!(
                "managed Team workspace already exists: {}",
                team_root.display()
            )));
        }

        let integration_row = self.lease_row(
            user_id,
            team_id,
            INTEGRATION_SLOT_ID,
            &repository_path,
            &integration_path,
            integration_branch,
            &base_commit,
        );
        let integration = self.create_worktree(integration_row).await?;
        let mut created = vec![integration.clone()];
        let mut agent_leases = Vec::with_capacity(agents.len());
        for slot in agents {
            let path = team_root.join(format!(
                "{}-{}",
                safe_component(&slot.slot_id, 24, "slot"),
                safe_component(&slot.name, 30, "agent")
            ));
            let row = self.lease_row(
                user_id,
                team_id,
                &slot.slot_id,
                &repository_path,
                &path,
                Self::agent_branch_name(team_id, slot),
                &base_commit,
            );
            match self.create_worktree(row).await {
                Ok(lease) => {
                    created.push(lease.clone());
                    agent_leases.push(lease);
                }
                Err(error) => {
                    for lease in created.iter().rev() {
                        self.cleanup_fresh_lease(lease).await;
                    }
                    return Err(error);
                }
            }
        }
        info!(
            team_id,
            count = agent_leases.len(),
            base_commit,
            "isolated Team worktrees created"
        );
        Ok(PreparedTeamWorkspaces {
            repository_path,
            base_commit,
            integration,
            agent_leases,
        })
    }

    async fn allocate_agent(
        &self,
        user_id: &str,
        team_id: &str,
        slot: &WorkspaceAgentSpec,
    ) -> Result<AgentWorkspaceLeaseRow, TeamError> {
        let source = self.source_lease_for_team(team_id).await?;
        if source.user_id != user_id {
            return Err(TeamError::Forbidden(format!(
                "Team {team_id} workspace is not owned by current user"
            )));
        }
        let path = self.team_root(team_id).join(format!(
            "{}-{}",
            safe_component(&slot.slot_id, 24, "slot"),
            safe_component(&slot.name, 30, "agent")
        ));
        let row = self.lease_row(
            user_id,
            team_id,
            &slot.slot_id,
            &source.repository_path,
            &path,
            Self::agent_branch_name(team_id, slot),
            &source.base_commit,
        );
        self.create_worktree(row).await
    }

    async fn list_team_leases(&self, team_id: &str) -> Result<Vec<AgentWorkspaceLeaseRow>, TeamError> {
        Ok(self.leases.list_for_team(team_id).await?)
    }

    async fn release_slot(&self, team_id: &str, slot_id: &str) -> Result<WorkspaceCleanupResult, TeamError> {
        let lease =
            self.leases.get_for_team_slot(team_id, slot_id).await?.ok_or_else(|| {
                workspace_error(format!("workspace lease not found for Team {team_id} slot {slot_id}"))
            })?;
        if lease.lease_status == "released" {
            return Ok(WorkspaceCleanupResult {
                lease_id: lease.id,
                slot_id: lease.slot_id,
                disposition: WorkspaceCleanupDisposition::AlreadyReleased,
            });
        }
        let worktree = Path::new(&lease.worktree_path);
        if !worktree.is_dir() {
            self.leases
                .update(
                    &lease.id,
                    &AgentWorkspaceLeaseUpdate {
                        lease_status: Some("conflict".into()),
                        cleanup_status: Some("missing_worktree".into()),
                        last_error: Some(Some("managed worktree path is missing".into())),
                        ..Default::default()
                    },
                )
                .await?;
            return Ok(WorkspaceCleanupResult {
                lease_id: lease.id,
                slot_id: lease.slot_id,
                disposition: WorkspaceCleanupDisposition::MissingPreserved,
            });
        }
        let status = git_output(worktree, &["status", "--porcelain", "--untracked-files=all"]).await?;
        if !status.trim().is_empty() {
            self.leases
                .update(
                    &lease.id,
                    &AgentWorkspaceLeaseUpdate {
                        lease_status: Some("cleanup_pending".into()),
                        cleanup_status: Some("dirty_preserved".into()),
                        last_error: Some(Some("uncommitted changes preserved".into())),
                        ..Default::default()
                    },
                )
                .await?;
            return Ok(WorkspaceCleanupResult {
                lease_id: lease.id,
                slot_id: lease.slot_id,
                disposition: WorkspaceCleanupDisposition::DirtyPreserved,
            });
        }

        let head = git_output(worktree, &["rev-parse", "HEAD"]).await?;
        git_output(
            Path::new(&lease.repository_path),
            &["worktree", "remove", &lease.worktree_path],
        )
        .await?;
        let (cleanup_status, disposition) = if head == lease.base_commit {
            git_output(Path::new(&lease.repository_path), &["branch", "-D", &lease.branch_name]).await?;
            ("removed", WorkspaceCleanupDisposition::Removed)
        } else {
            ("branch_retained", WorkspaceCleanupDisposition::BranchRetained)
        };
        self.leases
            .update(
                &lease.id,
                &AgentWorkspaceLeaseUpdate {
                    lease_status: Some("released".into()),
                    cleanup_status: Some(cleanup_status.into()),
                    last_error: Some(None),
                    released_at: Some(Some(now_ms())),
                    ..Default::default()
                },
            )
            .await?;
        Ok(WorkspaceCleanupResult {
            lease_id: lease.id,
            slot_id: lease.slot_id,
            disposition,
        })
    }

    async fn release_team(&self, team_id: &str) -> Result<Vec<WorkspaceCleanupResult>, TeamError> {
        let mut leases = self.leases.list_for_team(team_id).await?;
        leases.sort_by_key(|lease| lease.slot_id == INTEGRATION_SLOT_ID);
        let mut results = Vec::with_capacity(leases.len());
        for lease in leases {
            results.push(self.release_slot(team_id, &lease.slot_id).await?);
        }
        Ok(results)
    }

    async fn reconcile_all(&self) -> Result<(), TeamError> {
        for lease in self.leases.list_reconcile_candidates().await? {
            let exists = Path::new(&lease.worktree_path).is_dir();
            if !exists {
                self.leases
                    .update(
                        &lease.id,
                        &AgentWorkspaceLeaseUpdate {
                            lease_status: Some("conflict".into()),
                            cleanup_status: Some("missing_worktree".into()),
                            last_error: Some(Some("managed worktree path is missing during reconciliation".into())),
                            ..Default::default()
                        },
                    )
                    .await?;
                continue;
            }
            if lease.lease_status == "provisioning" {
                self.leases
                    .update(
                        &lease.id,
                        &AgentWorkspaceLeaseUpdate {
                            lease_status: Some("active".into()),
                            cleanup_status: Some("recovered".into()),
                            last_error: Some(None),
                            ..Default::default()
                        },
                    )
                    .await?;
            } else if lease.lease_status == "cleanup_pending" {
                let status = git_output(
                    Path::new(&lease.worktree_path),
                    &["status", "--porcelain", "--untracked-files=all"],
                )
                .await?;
                if status.trim().is_empty()
                    && let Err(error) = self.release_slot(&lease.team_id, &lease.slot_id).await
                {
                    warn!(lease_id = %lease.id, error = %error, "workspace reconciliation cleanup failed");
                }
            }
        }
        Ok(())
    }

    async fn validate_owned_path(&self, team_id: &str, slot_id: &str, path: &Path) -> Result<PathBuf, TeamError> {
        let lease =
            self.leases.get_for_team_slot(team_id, slot_id).await?.ok_or_else(|| {
                workspace_error(format!("workspace lease not found for Team {team_id} slot {slot_id}"))
            })?;
        if slot_id == INTEGRATION_SLOT_ID {
            return Err(workspace_error(
                "integration workspace is not writable by an Agent".into(),
            ));
        }
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(workspace_error("path is outside the Agent workspace lease".into()));
        }
        let root = PathBuf::from(&lease.worktree_path)
            .canonicalize()
            .map_err(|error| workspace_error(format!("workspace lease path is unavailable: {error}")))?;
        let mut candidate = root.clone();
        for component in path.components() {
            candidate.push(component.as_os_str());
            if candidate.exists() {
                candidate = candidate
                    .canonicalize()
                    .map_err(|error| workspace_error(format!("failed to resolve workspace path: {error}")))?;
                if !candidate.starts_with(&root) {
                    return Err(workspace_error("path is outside the Agent workspace lease".into()));
                }
            }
        }
        Ok(candidate)
    }
}

fn safe_component(value: &str, max_len: usize, fallback: &str) -> String {
    let mut result = String::with_capacity(value.len().min(max_len));
    let mut last_dash = false;
    for ch in value.chars() {
        if result.len() >= max_len {
            break;
        }
        let normalized = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if normalized == '-' {
            if result.is_empty() || last_dash {
                continue;
            }
            last_dash = true;
        } else {
            last_dash = false;
        }
        result.push(normalized);
    }
    while result.ends_with('-') {
        result.pop();
    }
    if result.is_empty() { fallback.to_owned() } else { result }
}

async fn git_output(repository: &Path, args: &[&str]) -> Result<String, TeamError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .await
        .map_err(|error| workspace_error(format!("failed to start git: {error}")))?;
    if !output.status.success() {
        return Err(workspace_error(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn workspace_error(message: String) -> TeamError {
    TeamError::WorkspaceOperation(message)
}

#[cfg(test)]
mod tests {
    use super::safe_component;

    #[test]
    fn branch_components_are_lowercase_bounded_and_safe() {
        assert_eq!(safe_component("Hello / World!", 40, "x"), "hello-world");
        assert_eq!(safe_component("---", 40, "fallback"), "fallback");
        assert_eq!(safe_component("ABCDEFGHIJ", 5, "x"), "abcde");
    }
}
