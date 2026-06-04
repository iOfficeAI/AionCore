//! Git-based workspace snapshot service.
//!
//! Supports two modes:
//! - **git-repo**: directory already has `.git` — uses it directly.
//! - **snapshot**: no `.git` — creates a temporary git repo that tracks the
//!   workspace via a separate worktree.

mod helpers;

use aionui_common::FileChangeOperation;

use crate::error::FileError;
use dashmap::DashMap;
use git2::Repository;

use crate::types::{CompareResult, SnapshotInfo, SnapshotMode};

use helpers::{
    SNAPSHOT_DIR_PREFIX, WorkspaceState, build_info, discard_single_file, init_snapshot_repo, list_branches, open_repo,
    parse_statuses, read_baseline, reset_single_file, resolve_workspace, stage_all_with_deletions, stage_single_file,
    temp_repo_path, unstage_all_files, unstage_single_file,
};

// ---------------------------------------------------------------------------
// SnapshotService
// ---------------------------------------------------------------------------

/// Git-based workspace snapshot service.
pub struct SnapshotService {
    workspaces: DashMap<String, WorkspaceState>,
}

impl Default for SnapshotService {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotService {
    pub fn new() -> Self {
        Self {
            workspaces: DashMap::new(),
        }
    }

    /// Remove leftover `aionui-snapshot-*` directories from the system temp
    /// dir. Call once at application startup.
    pub fn cleanup_stale_snapshots() {
        let temp_dir = std::env::temp_dir();
        let entries = match std::fs::read_dir(&temp_dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to read temp dir for snapshot cleanup"
                );
                return;
            }
        };
        for entry in entries.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if name.starts_with(SNAPSHOT_DIR_PREFIX) {
                let path = entry.path();
                if let Err(e) = std::fs::remove_dir_all(&path) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Failed to clean up stale snapshot directory"
                    );
                } else {
                    tracing::info!(
                        path = %path.display(),
                        "Cleaned up stale snapshot directory"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: get workspace state or return error
// ---------------------------------------------------------------------------

fn get_state(workspaces: &DashMap<String, WorkspaceState>, workspace: &str) -> Result<WorkspaceState, FileError> {
    workspaces
        .get(workspace)
        .map(|r| r.clone())
        .ok_or_else(|| FileError::BadRequest(format!("Workspace not initialized: {}", workspace)))
}

// ---------------------------------------------------------------------------
// ISnapshotService implementation
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl crate::traits::ISnapshotService for SnapshotService {
    async fn init(&self, workspace: &str) -> Result<SnapshotInfo, FileError> {
        let ws = workspace.to_owned();

        // Check if already initialized
        if let Some(state) = self.workspaces.get(&ws) {
            let st = state.clone();
            return tokio::task::spawn_blocking(move || {
                let repo = open_repo(&st)?;
                Ok(build_info(st.mode, &repo))
            })
            .await
            .map_err(|e| FileError::Internal(format!("Blocking task failed: {}", e)))?;
        }

        let ws_clone = ws.clone();
        let result = tokio::task::spawn_blocking(move || {
            let canonical = resolve_workspace(&ws_clone)?;
            let canonical_str = canonical.to_string_lossy().to_string();

            let git_dir = canonical.join(".git");
            let (mode, repo_path) = if git_dir.exists() {
                (SnapshotMode::GitRepo, canonical.clone())
            } else {
                let temp = temp_repo_path(&canonical_str);
                init_snapshot_repo(&canonical, &temp)?;
                (SnapshotMode::Snapshot, temp)
            };

            let state = WorkspaceState {
                mode,
                repo_path: repo_path.clone(),
                workspace_path: canonical,
            };

            let repo = Repository::open(&repo_path)
                .map_err(|e| FileError::Internal(format!("Failed to open repo after init: {}", e)))?;
            let info = build_info(mode, &repo);

            Ok::<(WorkspaceState, SnapshotInfo), FileError>((state, info))
        })
        .await
        .map_err(|e| FileError::Internal(format!("Blocking task failed: {}", e)))??;

        let (state, info) = result;
        self.workspaces.insert(ws, state);
        Ok(info)
    }

    async fn get_info(&self, workspace: &str) -> Result<SnapshotInfo, FileError> {
        let state = get_state(&self.workspaces, workspace)?;

        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&state)?;
            Ok(build_info(state.mode, &repo))
        })
        .await
        .map_err(|e| FileError::Internal(format!("Blocking task failed: {}", e)))?
    }

    async fn compare(&self, workspace: &str) -> Result<CompareResult, FileError> {
        let state = get_state(&self.workspaces, workspace)?;

        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&state)?;
            parse_statuses(&repo, &state.workspace_path)
        })
        .await
        .map_err(|e| FileError::Internal(format!("Blocking task failed: {}", e)))?
    }

    async fn get_baseline_content(&self, workspace: &str, file_path: &str) -> Result<Option<String>, FileError> {
        let state = get_state(&self.workspaces, workspace)?;
        let rel = file_path.to_owned();

        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&state)?;
            read_baseline(&repo, &rel)
        })
        .await
        .map_err(|e| FileError::Internal(format!("Blocking task failed: {}", e)))?
    }

    async fn stage_file(&self, workspace: &str, file_path: &str) -> Result<(), FileError> {
        let state = get_state(&self.workspaces, workspace)?;
        let fp = file_path.to_owned();

        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&state)?;
            stage_single_file(&repo, &fp)
        })
        .await
        .map_err(|e| FileError::Internal(format!("Blocking task failed: {}", e)))?
    }

    async fn stage_all(&self, workspace: &str) -> Result<(), FileError> {
        let state = get_state(&self.workspaces, workspace)?;

        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&state)?;
            stage_all_with_deletions(&repo)
        })
        .await
        .map_err(|e| FileError::Internal(format!("Blocking task failed: {}", e)))?
    }

    async fn unstage_file(&self, workspace: &str, file_path: &str) -> Result<(), FileError> {
        let state = get_state(&self.workspaces, workspace)?;
        let fp = file_path.to_owned();

        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&state)?;
            unstage_single_file(&repo, &fp)
        })
        .await
        .map_err(|e| FileError::Internal(format!("Blocking task failed: {}", e)))?
    }

    async fn unstage_all(&self, workspace: &str) -> Result<(), FileError> {
        let state = get_state(&self.workspaces, workspace)?;

        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&state)?;
            unstage_all_files(&repo)
        })
        .await
        .map_err(|e| FileError::Internal(format!("Blocking task failed: {}", e)))?
    }

    async fn discard_file(
        &self,
        workspace: &str,
        file_path: &str,
        operation: FileChangeOperation,
    ) -> Result<(), FileError> {
        let state = get_state(&self.workspaces, workspace)?;
        let fp = file_path.to_owned();

        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&state)?;
            discard_single_file(&repo, &state.workspace_path, &fp, operation)
        })
        .await
        .map_err(|e| FileError::Internal(format!("Blocking task failed: {}", e)))?
    }

    async fn reset_file(
        &self,
        workspace: &str,
        file_path: &str,
        operation: FileChangeOperation,
    ) -> Result<(), FileError> {
        let state = get_state(&self.workspaces, workspace)?;
        let fp = file_path.to_owned();

        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&state)?;
            reset_single_file(&repo, &state.workspace_path, &fp, operation)
        })
        .await
        .map_err(|e| FileError::Internal(format!("Blocking task failed: {}", e)))?
    }

    async fn get_branches(&self, workspace: &str) -> Result<Vec<String>, FileError> {
        let state = get_state(&self.workspaces, workspace)?;

        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&state)?;
            list_branches(&repo)
        })
        .await
        .map_err(|e| FileError::Internal(format!("Blocking task failed: {}", e)))?
    }

    async fn dispose(&self, workspace: &str) -> Result<(), FileError> {
        let state = match self.workspaces.remove(workspace) {
            Some((_, s)) => s,
            // Already disposed or never initialized -- idempotent
            None => return Ok(()),
        };

        if state.mode == SnapshotMode::Snapshot {
            let repo_path = state.repo_path.clone();
            tokio::task::spawn_blocking(move || {
                if repo_path.exists() {
                    std::fs::remove_dir_all(&repo_path).map_err(|e| {
                        FileError::Internal(format!("Failed to remove snapshot dir {}: {}", repo_path.display(), e))
                    })?;
                }
                Ok(())
            })
            .await
            .map_err(|e| FileError::Internal(format!("Blocking task failed: {}", e)))?
        } else {
            // git-repo mode: nothing to clean up
            Ok(())
        }
    }
}
