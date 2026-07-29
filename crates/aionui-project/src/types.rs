use aionui_common::TimestampMs;
use aionui_db::{DbError, FolderRow, ProjectExplorerRow, ProjectRow};
use serde::Serialize;

/// Filesystem operation intent carried into containment resolution. Kept
/// distinct so future access rules (e.g. write-only guards) can branch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOp {
    Read,
    Write,
    Remove,
    Rename,
    Browse,
}

/// Result of a folder/project resolution: the reused-or-created project, its
/// folder, and the workspace explorer entry binding the two.
#[derive(Debug, Clone)]
pub struct ResolveOutput {
    pub project: ProjectRow,
    pub folder: FolderRow,
    pub project_explorer: ProjectExplorerRow,
}

/// Runtime availability of a folder's resource root, computed at read time
/// (never persisted). `file:` provider yields the first three; `disconnected`
/// is reserved for remote providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatus {
    Available,
    Missing,
    PermissionDenied,
    Disconnected,
}

impl RuntimeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeStatus::Available => "available",
            RuntimeStatus::Missing => "missing",
            RuntimeStatus::PermissionDenied => "permission_denied",
            RuntimeStatus::Disconnected => "disconnected",
        }
    }
}

/// API-facing folder view. Excludes scheme/authority/path (parsed on demand,
/// not stored); `default_display_name` and `runtime_status` are derived.
#[derive(Debug, Clone, Serialize)]
pub struct FolderDto {
    pub folder_id: String,
    pub resource_uri: String,
    pub resource_canonical: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_display_name: Option<String>,
    pub runtime_status: RuntimeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_error: Option<String>,
}

/// One Project Explorer entry with its joined folder view.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectExplorerEntry {
    pub pe_id: String,
    pub project_id: String,
    pub folder_id: String,
    pub role: String,
    pub display_name: Option<String>,
    pub order_index: i64,
    pub folder: FolderDto,
}

/// Explorer view aggregated onto a project.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectExplorerView {
    pub workspace_pe_id: String,
    pub entries: Vec<ProjectExplorerEntry>,
}

/// Aggregated project detail returned by `get_project`.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectDetail {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub explorer: ProjectExplorerView,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Attach an additional (non-workspace) folder to a project.
#[derive(Debug, Clone)]
pub struct AttachInput {
    pub project_id: String,
    pub uri: String,
    pub display_name: Option<String>,
}

/// Resolve a `pe_id + relative_path` reference to a concrete resource.
#[derive(Debug, Clone)]
pub struct ReferenceInput {
    pub pe_id: String,
    pub relative_path: String,
    pub op: FileOp,
}

/// A reference resolved to a concrete child resource within a folder root.
/// Identity + containment only — no IO is performed to produce it.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedResource {
    pub project_id: String,
    pub pe_id: String,
    pub folder_id: String,
    pub root_resource_uri: String,
    pub root_resource_canonical: String,
    pub relative_path: String,
    pub resource_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absolute_path: Option<String>,
}

/// Creation-binding-chain errors, stable to UI-consumable codes (see
/// `service-contract.md` error table). Each variant carries structured context
/// rather than relying on message parsing.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("folder not found: {path}")]
    FolderNotFound { path: String },

    #[error("path is not a directory: {path}")]
    FolderNotDirectory { path: String },

    #[error("permission denied: {path}")]
    FolderPermissionDenied { path: String },

    #[error("failed to canonicalize resource uri: {uri}")]
    FolderCanonicalizeFailed { uri: String },

    #[error("temp directory already exists: {path}")]
    TempDirExists { path: String },

    #[error("workspace path missing, cannot backfill")]
    WorkspaceMissing,

    #[error("owner folder {folder_id} does not match project {project_id} workspace folder")]
    WorkspaceFolderMismatch { project_id: String, folder_id: String },

    #[error("multiple standard projects found for workspace folder {folder_id}")]
    StandardProjectConflict { folder_id: String },

    #[error("project {project_id} already references folder {folder_id}")]
    ProjectExplorerDuplicate { project_id: String, folder_id: String },

    #[error("folder overlaps an existing explorer entry in project {project_id}")]
    ProjectExplorerOverlap { project_id: String },

    #[error("project not found: {project_id}")]
    ProjectNotFound { project_id: String },

    #[error("project_explorer entry not found: {pe_id}")]
    ProjectExplorerNotFound { pe_id: String },

    #[error("workspace entry is immutable: {pe_id}")]
    WorkspaceEntryImmutable { pe_id: String },

    #[error("invalid relative path: {relative_path}")]
    InvalidRelativePath { relative_path: String },

    #[error("resource escapes folder root: {relative_path}")]
    ResourceOutsideFolder { relative_path: String },

    #[error("unsupported resource scheme: {scheme}")]
    UnsupportedResourceScheme { scheme: String },

    #[error(transparent)]
    Database(#[from] DbError),
}

impl ProjectError {
    /// Stable, UI-consumable error code.
    pub fn code(&self) -> &'static str {
        match self {
            ProjectError::FolderNotFound { .. } => "folder_not_found",
            ProjectError::FolderNotDirectory { .. } => "folder_not_directory",
            ProjectError::FolderPermissionDenied { .. } => "folder_permission_denied",
            ProjectError::FolderCanonicalizeFailed { .. } => "folder_canonicalize_failed",
            ProjectError::TempDirExists { .. } => "temp_dir_exists",
            ProjectError::WorkspaceMissing => "workspace_missing",
            ProjectError::WorkspaceFolderMismatch { .. } => "workspace_folder_mismatch",
            ProjectError::StandardProjectConflict { .. } => "standard_project_conflict",
            ProjectError::ProjectNotFound { .. } => "project_not_found",
            ProjectError::ProjectExplorerDuplicate { .. } => "project_explorer_duplicate",
            ProjectError::ProjectExplorerOverlap { .. } => "project_explorer_overlap",
            ProjectError::ProjectExplorerNotFound { .. } => "project_explorer_not_found",
            ProjectError::WorkspaceEntryImmutable { .. } => "workspace_entry_immutable",
            ProjectError::InvalidRelativePath { .. } => "invalid_relative_path",
            ProjectError::ResourceOutsideFolder { .. } => "resource_outside_folder",
            ProjectError::UnsupportedResourceScheme { .. } => "unsupported_resource_scheme",
            ProjectError::Database(_) => "internal_db_error",
        }
    }
}
