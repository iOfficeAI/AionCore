use std::fs;
use std::path::{Path, PathBuf};

use aionui_api_types::{DirtyWorktreeChoice, ProjectRepositoryFacts, RepositorySource};
use aionui_common::now_ms;
use aionui_runtime::Builder;

use crate::ProjectError;
use crate::detection::{detect_project, is_safe_relative_path};

#[derive(Debug, Clone)]
pub struct RepositoryOnboarder {
    managed_root: PathBuf,
}

impl RepositoryOnboarder {
    pub fn new(managed_root: impl Into<PathBuf>) -> Self {
        Self {
            managed_root: managed_root.into(),
        }
    }

    pub async fn onboard(
        &self,
        source: RepositorySource,
        dirty_choice: DirtyWorktreeChoice,
    ) -> Result<ProjectRepositoryFacts, ProjectError> {
        let (path, repository_url, credential_reference) = match source {
            RepositorySource::Local { path } => (canonical_directory(&path)?, None, None),
            RepositorySource::Clone {
                url,
                destination_name,
                branch,
                credential_reference,
            } => {
                Self::validate_clone_url(&url)?;
                Self::validate_destination_name(&destination_name)?;
                if let Some(value) = branch.as_deref() {
                    Self::validate_branch_name(value)?;
                }
                if let Some(value) = credential_reference.as_deref() {
                    validate_credential_reference(value)?;
                }
                let path = self
                    .clone_repository(&url, &destination_name, branch.as_deref())
                    .await?;
                (path, Some(url), credential_reference)
            }
        };

        let baseline_commit = git_optional(&path, &["rev-parse", "HEAD"]).await?;
        let default_branch = git_optional(&path, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
        let status = git(&path, &["status", "--porcelain=v1", "-z"]).await?;
        let dirty = !status.is_empty();
        let dirty_snapshot_ref = match (dirty, dirty_choice) {
            (true, DirtyWorktreeChoice::Reject) => {
                return Err(ProjectError::Conflict(
                    "repository is dirty; choose preserve or snapshot explicitly".into(),
                ));
            }
            (true, DirtyWorktreeChoice::Snapshot) => Some(self.snapshot_dirty_worktree(&path).await?),
            _ => None,
        };
        let detected = detect_project(&path)?;
        Ok(ProjectRepositoryFacts {
            local_path: path.to_string_lossy().into_owned(),
            repository_url,
            default_branch,
            baseline_commit,
            dirty,
            dirty_worktree_choice: dirty_choice,
            dirty_snapshot_ref,
            credential_reference,
            languages: detected.languages,
            package_managers: detected.package_managers,
            rules_files: detected.rules_files,
            monorepo_packages: detected.monorepo_packages,
            submodules: detected.submodules,
            lfs_detected: detected.lfs_detected,
            detected_at: now_ms(),
        })
    }

    pub fn validate_clone_url(value: &str) -> Result<(), ProjectError> {
        let value = value.trim();
        if value.is_empty() || value.contains(['\n', '\r', '\0']) {
            return Err(ProjectError::BadRequest("clone URL is invalid".into()));
        }
        let supported = value.starts_with("https://")
            || value.starts_with("ssh://")
            || value.starts_with("file://")
            || (value.starts_with("git@") && value.contains(':'));
        if !supported {
            return Err(ProjectError::BadRequest("clone URL must use HTTPS or SSH".into()));
        }
        if let Some(authority) = value.strip_prefix("https://").and_then(|rest| rest.split('/').next())
            && authority.contains('@')
        {
            return Err(ProjectError::BadRequest(
                "clone URL must not contain embedded credentials; select a credential reference".into(),
            ));
        }
        if value.contains('?') || value.contains('#') {
            return Err(ProjectError::BadRequest(
                "clone URL must not contain query or fragment data".into(),
            ));
        }
        Ok(())
    }

    pub fn validate_destination_name(value: &str) -> Result<(), ProjectError> {
        validate_relative_path(value, "clone destination")?;
        if Path::new(value).components().count() != 1 {
            return Err(ProjectError::BadRequest(
                "clone destination must be a single directory name".into(),
            ));
        }
        Ok(())
    }

    pub fn validate_branch_name(value: &str) -> Result<(), ProjectError> {
        let valid = !value.is_empty()
            && !value.starts_with('-')
            && !value.starts_with('/')
            && !value.ends_with('/')
            && !value.ends_with('.')
            && !value.contains("..")
            && !value.contains("@{")
            && !value
                .chars()
                .any(|ch| ch.is_control() || matches!(ch, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'));
        if valid {
            Ok(())
        } else {
            Err(ProjectError::BadRequest("branch name is invalid".into()))
        }
    }

    pub fn validate_submodule_path(value: &str) -> Result<(), ProjectError> {
        validate_relative_path(value, "submodule path")
    }

    async fn clone_repository(
        &self,
        url: &str,
        destination_name: &str,
        branch: Option<&str>,
    ) -> Result<PathBuf, ProjectError> {
        fs::create_dir_all(&self.managed_root)
            .map_err(|error| ProjectError::Internal(format!("failed to create managed project root: {error}")))?;
        let root = self
            .managed_root
            .canonicalize()
            .map_err(|error| ProjectError::Internal(format!("managed project root cannot be resolved: {error}")))?;
        let destination = root.join(destination_name);
        if destination.exists() {
            return Err(ProjectError::Conflict(format!(
                "clone destination already exists: {destination_name}"
            )));
        }
        let mut command = Builder::clean_cli("git");
        command.arg("clone").arg("--no-tags");
        if let Some(branch) = branch {
            command.args(["--branch", branch, "--single-branch"]);
        }
        command.arg("--").arg(url).arg(&destination);
        let output = command
            .output()
            .await
            .map_err(|error| ProjectError::Internal(format!("git clone could not start: {error}")))?;
        if !output.status.success() {
            let _ = fs::remove_dir_all(&destination);
            return Err(ProjectError::BadRequest(format!(
                "git clone failed: {}",
                safe_git_error(&output.stderr)
            )));
        }
        let canonical = destination
            .canonicalize()
            .map_err(|error| ProjectError::Internal(format!("cloned repository cannot be resolved: {error}")))?;
        if !canonical.starts_with(&root) || canonical.parent() != Some(root.as_path()) {
            let _ = fs::remove_dir_all(&canonical);
            return Err(ProjectError::BadRequest(
                "clone destination escaped the managed project root".into(),
            ));
        }
        Ok(canonical)
    }

    async fn snapshot_dirty_worktree(&self, repository: &Path) -> Result<String, ProjectError> {
        let snapshot_root = self.managed_root.join("_snapshots");
        fs::create_dir_all(&snapshot_root)
            .map_err(|error| ProjectError::Internal(format!("failed to create snapshot root: {error}")))?;
        let snapshot = snapshot_root.join(uuid::Uuid::now_v7().to_string());
        let untracked_root = snapshot.join("untracked");
        fs::create_dir_all(&untracked_root)
            .map_err(|error| ProjectError::Internal(format!("failed to create snapshot: {error}")))?;
        let patch = git(repository, &["diff", "--binary", "--no-ext-diff", "HEAD"]).await?;
        fs::write(snapshot.join("changes.patch"), patch)
            .map_err(|error| ProjectError::Internal(format!("failed to write repository snapshot: {error}")))?;
        let untracked = git(repository, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
        for relative in untracked.split(|byte| *byte == 0).filter(|item| !item.is_empty()) {
            let relative = std::str::from_utf8(relative)
                .map_err(|_| ProjectError::BadRequest("untracked file path is not valid UTF-8".into()))?;
            validate_relative_path(relative, "untracked file")?;
            let source = repository.join(relative);
            let metadata = fs::symlink_metadata(&source)
                .map_err(|error| ProjectError::Internal(format!("failed to inspect untracked file: {error}")))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(ProjectError::BadRequest(format!(
                    "snapshot does not accept symlink: {relative}"
                )));
            }
            if metadata.len() > 50 * 1024 * 1024 {
                return Err(ProjectError::BadRequest(format!(
                    "untracked snapshot file is too large: {relative}"
                )));
            }
            let target = untracked_root.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| ProjectError::Internal(format!("failed to create snapshot directory: {error}")))?;
            }
            fs::copy(source, target)
                .map_err(|error| ProjectError::Internal(format!("failed to snapshot untracked file: {error}")))?;
        }
        snapshot
            .canonicalize()
            .map(|value| value.to_string_lossy().into_owned())
            .map_err(|error| ProjectError::Internal(format!("snapshot cannot be resolved: {error}")))
    }
}

pub(crate) fn validate_relative_path(value: &str, field: &str) -> Result<(), ProjectError> {
    if is_safe_relative_path(value) {
        Ok(())
    } else {
        Err(ProjectError::BadRequest(format!(
            "{field} must stay inside its repository boundary"
        )))
    }
}

fn validate_credential_reference(value: &str) -> Result<(), ProjectError> {
    let valid = (3..=128).contains(&value.len())
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'))
        && value.contains(':');
    if valid {
        Ok(())
    } else {
        Err(ProjectError::BadRequest("credential reference is invalid".into()))
    }
}

fn canonical_directory(value: &str) -> Result<PathBuf, ProjectError> {
    let canonical = Path::new(value)
        .canonicalize()
        .map_err(|error| ProjectError::BadRequest(format!("repository path cannot be resolved: {error}")))?;
    if !canonical.is_dir() {
        return Err(ProjectError::BadRequest("repository path is not a directory".into()));
    }
    Ok(canonical)
}

async fn git(repository: &Path, args: &[&str]) -> Result<Vec<u8>, ProjectError> {
    let mut command = Builder::clean_cli("git");
    command.args(args).current_dir(repository);
    let output = command
        .output()
        .await
        .map_err(|error| ProjectError::Internal(format!("git could not start: {error}")))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(ProjectError::BadRequest(format!(
            "git command failed: {}",
            safe_git_error(&output.stderr)
        )))
    }
}

async fn git_optional(repository: &Path, args: &[&str]) -> Result<Option<String>, ProjectError> {
    let mut command = Builder::clean_cli("git");
    command.args(args).current_dir(repository);
    let output = command
        .output()
        .await
        .map_err(|error| ProjectError::Internal(format!("git could not start: {error}")))?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!value.is_empty()).then_some(value))
}

fn safe_git_error(stderr: &[u8]) -> String {
    let value = String::from_utf8_lossy(stderr);
    let line = value.lines().last().unwrap_or("unknown Git error").trim();
    let mut safe = line.replace(['\r', '\n'], " ");
    if safe.len() > 300 {
        safe.truncate(300);
    }
    safe
}
