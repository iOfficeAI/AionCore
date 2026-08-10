//! Project Explorer control-plane HTTP DTOs.
//!
//! The wire contract the explorer frontend consumes to fetch a project's
//! roots (`GET /api/projects/{id}`) and mutate its attached folders
//! (`POST`/`DELETE .../folders`). The filesystem *content* of each root is
//! carried separately over the `fs/*` WebSocket protocol, keyed by `pe_id` —
//! these DTOs only describe the project shell and its root list.
//!
//! Deliberately excludes absolute paths / canonical URIs: the frontend
//! identifies resources purely by `{pe_id, relative_path}`. `display_path`
//! is a human-facing, read-only rendering of the folder location.

use serde::{Deserialize, Serialize};

use crate::chat_file::ChatFileRef;

/// Aggregated project detail — everything the explorer needs in one call,
/// so the frontend never fans out one request per root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDetailResponse {
    pub project_id: String,
    /// Project display name (explorer header).
    pub name: String,
    pub explorer: ProjectExplorer,
}

/// The explorer view of a project: its pinned workspace root plus every
/// attached root, in display order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectExplorer {
    /// The `pe_id` of the immutable workspace root (pinned first, not
    /// removable). The frontend uses it to pin + lock that row.
    pub workspace_pe_id: String,
    /// Roots ordered by `order_index` ascending (backend-sorted).
    pub entries: Vec<ProjectEntry>,
}

/// One explorer root. Also returned singly by `attach_folder` so the
/// frontend can splice it into the tree without re-fetching the project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    /// Stable root identity (= frontend `PeKey` prefix). Content subscriptions
    /// key off this.
    pub pe_id: String,
    /// `"workspace"` (pinned, immutable) or `"attached"` (removable).
    pub role: String,
    /// Optional user-assigned label; `null` → frontend falls back to the
    /// `display_path` basename.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Human-facing path rendering of the folder location (read-only).
    pub display_path: String,
    /// Position among roots (ascending).
    pub order_index: i64,
    /// Folder availability: `available` | `missing` | `permission_denied` |
    /// `disconnected`. Drives the greyed-out / stale root indicator.
    pub runtime_status: String,
}

/// `POST /api/projects/{project_id}/folders` body — attach an additional
/// (non-workspace) folder. `uri` is a `file://` URI (same form as project
/// creation).
#[derive(Debug, Clone, Deserialize)]
pub struct AttachFolderRequest {
    pub uri: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// `POST /api/projects/{project_id}/resolve-ref` body — a [`ChatFileRef`] to
/// re-express in its strongest form for this project.
///
/// Exists because the same file reached from different entry points produces
/// different refs (the explorer yields `Project`, a chat link yields `Local`),
/// and callers that key on the ref need one answer per file.
#[derive(Debug, Clone, Deserialize)]
pub struct ResolveRefRequest {
    pub file: ChatFileRef,
}

/// `POST /api/projects/{project_id}/resolve-ref` response.
///
/// `file` is the upgraded ref when the path turned out to live under one of the
/// project's roots, and the request's ref unchanged otherwise — the caller
/// always gets something addressable, so there is no failure case to branch on.
/// Absolute paths stay on the backend: an upgraded ref carries only
/// `{pe_id, relative_path}`, and a ref that could not be upgraded is echoed back
/// exactly as the caller sent it.
#[derive(Debug, Clone, Serialize)]
pub struct ResolveRefResponse {
    pub file: ChatFileRef,
    /// Whether `file` differs from what was sent. Lets a caller skip a state
    /// write when nothing changed, without comparing the refs itself.
    pub upgraded: bool,
}
