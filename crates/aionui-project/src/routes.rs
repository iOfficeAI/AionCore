// `ApiError` is the intended error type at this HTTP boundary (routes map the
// crate-owned `ProjectError` to it here), so the disallowed_types lint that
// steers service code away from `ApiError` does not apply to this module.
#![allow(clippy::disallowed_types)]

//! Project Explorer control-plane HTTP routes.
//!
//! Read a project's roots (`GET /api/projects/{id}`) and mutate its attached
//! folders (`POST`/`DELETE .../folders`). Filesystem content is served
//! separately over the `fs/*` WebSocket protocol; these routes only expose the
//! project shell + root list the explorer needs to open subscriptions.
//!
//! Handlers do request/response transformation only; all business logic lives
//! in [`ProjectService`]. Domain [`ProjectError`]s map to `ApiError` with
//! stable, machine-readable codes (`project_explorer_duplicate`,
//! `project_explorer_overlap`, …) so the frontend can branch without parsing
//! human messages.

use std::sync::Arc;

use aionui_api_types::{
    ApiResponse, AttachFolderRequest, ProjectDetailResponse, ProjectEntry, ProjectExplorer, ResolveRefRequest,
    ResolveRefResponse,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Extension, Router};
use serde_json::json;

use crate::canonical;
use crate::service::ProjectService;
use crate::types::{AttachInput, ProjectDetail, ProjectError, ProjectExplorerEntry};

/// Shared state for project route handlers.
#[derive(Clone)]
pub struct ProjectRouterState {
    pub project: Arc<ProjectService>,
}

/// Build the project control-plane router (`/api/projects/*`).
///
/// All routes require authentication (applied by the caller).
pub fn project_routes(state: ProjectRouterState) -> Router {
    Router::new()
        .route("/api/projects/{project_id}", get(get_project))
        .route("/api/projects/{project_id}/folders", post(attach_folder))
        .route("/api/projects/{project_id}/folders/{pe_id}", delete(remove_folder))
        .route("/api/projects/{project_id}/resolve-ref", post(resolve_ref))
        .with_state(state)
}

/// `GET /api/projects/{project_id}` — full project detail + all roots in one
/// call (frontend does not fan out per root).
async fn get_project(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
) -> Result<Json<ApiResponse<ProjectDetailResponse>>, ApiError> {
    let detail = state.project.get_project(&user.id, &project_id).await?;
    Ok(Json(ApiResponse::ok(to_detail_response(detail))))
}

/// `POST /api/projects/{project_id}/resolve-ref` — re-express a `ChatFileRef` in
/// its strongest form for this project.
///
/// The explorer and a chat link produce different refs for the same file
/// (`Project` vs `Local`), so callers that key on the ref — tab identity, the
/// `fs` change signal — would otherwise treat one file as two. This resolves a
/// `Local` path that lives under one of the project's roots into the `Project`
/// form; `Project` and `Upload` come back untouched.
///
/// Always succeeds with a usable ref: a path outside every root, or one that does
/// not exist, is echoed back unchanged rather than raising. "Not upgradeable" is
/// an ordinary answer here, and a caller mid-way through opening a missing file
/// still needs its ref to render that state.
///
/// The judgement stays server-side because case folding is a compile-time
/// platform fork (`canonical::IGNORE_PATH_CASING`); a client comparing path
/// strings would miss matches on macOS and merge distinct files on Linux.
async fn resolve_ref(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
    body: Result<Json<ResolveRefRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ResolveRefResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let file = state
        .project
        .upgrade_chat_file_ref(&user.id, &project_id, &req.file)
        .await?;
    let upgraded = file != req.file;
    Ok(Json(ApiResponse::ok(ResolveRefResponse { file, upgraded })))
}

/// `POST /api/projects/{project_id}/folders` — attach a folder. Returns the
/// single new (or focused-existing) entry so the frontend can splice it in
/// without re-fetching the project.
async fn attach_folder(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
    body: Result<Json<AttachFolderRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ProjectEntry>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let row = state
        .project
        .attach_folder(
            &user.id,
            AttachInput {
                project_id: project_id.clone(),
                uri: req.uri,
                display_name: req.display_name,
            },
        )
        .await?;

    // `attach_folder` returns the bare explorer row (no folder metadata). Re-read
    // the project to build the fully-shaped entry (display_path + runtime_status).
    // A descendant-attach focuses an existing entry, so match on the returned
    // pe_id rather than assuming the last row.
    let detail = state.project.get_project(&user.id, &project_id).await?;
    let entry = detail
        .explorer
        .entries
        .into_iter()
        .find(|e| e.pe_id == row.pe_id)
        .ok_or_else(|| ApiError::Internal("attached entry missing after insert".to_owned()))?;
    Ok(Json(ApiResponse::ok(to_entry(entry))))
}

/// `DELETE /api/projects/{project_id}/folders/{pe_id}` — detach an attached
/// folder. The workspace root is immutable (`workspace_entry_immutable`).
async fn remove_folder(
    State(state): State<ProjectRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((_project_id, pe_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    state.project.remove_attached(&user.id, &pe_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── mapping: domain → wire DTO ───────────────────────────────────────────────

fn to_detail_response(detail: ProjectDetail) -> ProjectDetailResponse {
    ProjectDetailResponse {
        project_id: detail.id,
        name: detail.name,
        explorer: ProjectExplorer {
            workspace_pe_id: detail.explorer.workspace_pe_id,
            // `get_project` yields entries ordered by order_index ASC.
            entries: detail.explorer.entries.into_iter().map(to_entry).collect(),
        },
    }
}

fn to_entry(entry: ProjectExplorerEntry) -> ProjectEntry {
    // `display_path` is a human-facing rendering of the folder's original
    // resource_uri; fall back to the raw uri if it is not a decodable path.
    let display_path = canonical::uri_to_path(&entry.folder.resource_uri)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| entry.folder.resource_uri.clone());
    ProjectEntry {
        pe_id: entry.pe_id,
        role: entry.role,
        display_name: entry.display_name,
        display_path,
        order_index: entry.order_index,
        runtime_status: entry.folder.runtime_status.as_str().to_owned(),
    }
}

// ── error mapping: ProjectError → ApiError (stable domain codes) ─────────────

impl From<ProjectError> for ApiError {
    fn from(err: ProjectError) -> Self {
        let (status, code, details) = match &err {
            ProjectError::ProjectNotFound { project_id } => (
                StatusCode::NOT_FOUND,
                "project_not_found",
                Some(json!({ "project_id": project_id })),
            ),
            ProjectError::ProjectExplorerNotFound { pe_id } => (
                StatusCode::NOT_FOUND,
                "project_explorer_not_found",
                Some(json!({ "pe_id": pe_id })),
            ),
            ProjectError::ProjectExplorerDuplicate { project_id, folder_id } => (
                StatusCode::CONFLICT,
                "project_explorer_duplicate",
                Some(json!({ "project_id": project_id, "folder_id": folder_id })),
            ),
            ProjectError::ProjectExplorerOverlap { project_id } => (
                StatusCode::CONFLICT,
                "project_explorer_overlap",
                Some(json!({ "project_id": project_id })),
            ),
            ProjectError::WorkspaceEntryImmutable { pe_id } => (
                StatusCode::CONFLICT,
                "workspace_entry_immutable",
                Some(json!({ "pe_id": pe_id })),
            ),
            ProjectError::StandardProjectConflict { folder_id } => (
                StatusCode::CONFLICT,
                "standard_project_conflict",
                Some(json!({ "folder_id": folder_id })),
            ),
            ProjectError::WorkspaceFolderMismatch { project_id, folder_id } => (
                StatusCode::CONFLICT,
                "workspace_folder_mismatch",
                Some(json!({ "project_id": project_id, "folder_id": folder_id })),
            ),
            ProjectError::FolderNotFound { .. } => (StatusCode::NOT_FOUND, "folder_not_found", None),
            ProjectError::FolderNotDirectory { .. } => (StatusCode::BAD_REQUEST, "folder_not_directory", None),
            ProjectError::FolderPermissionDenied { .. } => (StatusCode::FORBIDDEN, "folder_permission_denied", None),
            ProjectError::FolderCanonicalizeFailed { .. }
            | ProjectError::UnsupportedResourceScheme { .. }
            | ProjectError::InvalidRelativePath { .. }
            | ProjectError::ResourceOutsideFolder { .. } => (StatusCode::BAD_REQUEST, "invalid_resource", None),
            ProjectError::TempDirExists { .. } | ProjectError::WorkspaceMissing => {
                (StatusCode::BAD_REQUEST, "invalid_request", None)
            }
            ProjectError::UploadPathOutsideRoot { path } => (
                StatusCode::BAD_REQUEST,
                "upload_path_outside_root",
                Some(json!({ "path": path })),
            ),
            ProjectError::ChatFileMissing { path } => (
                StatusCode::NOT_FOUND,
                "chat_file_missing",
                Some(json!({ "path": path })),
            ),
            ProjectError::LocalPathNotReadable { path } => (
                StatusCode::BAD_REQUEST,
                "local_path_not_readable",
                Some(json!({ "path": path })),
            ),
            ProjectError::Database(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error", None),
        };
        // Never leak internal DB detail to clients (Security: no internal leakage).
        let message = match &err {
            ProjectError::Database(_) => "internal error".to_owned(),
            other => other.to_string(),
        };
        ApiError::coded(status, code, message, details)
    }
}

#[cfg(test)]
#[path = "routes_test.rs"]
mod routes_test;
