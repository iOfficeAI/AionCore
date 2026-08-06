#![allow(clippy::disallowed_types)]

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Extension, Json, Multipart, Query, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::services::ServeFile;

use aionui_api_types::{
    ApiResponse, ContentMetadataRequest, CopyFilesRequest, CopyFilesResponse, DirOrFileResponse,
    FetchRemoteImageRequest, FileChangeInfoResponse, FileMetadataResponse, GetFileMetadataRequest,
    GetFilesByDirRequest, GetImageBase64Request, ListWorkspaceFilesRequest, OpenSystemFileRequest, ReadContentRequest,
    ReadFileRequest, RevealItemRequest, SnapshotBaselineRequest, SnapshotCompareResponse, SnapshotDiscardRequest,
    SnapshotInfoResponse, SnapshotStageRequest, SnapshotWorkspaceRequest, StreamQuery, WorkspaceFlatFileResponse,
    WriteContentRequest, WriteFileRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use aionui_common::constants::UPLOAD_MAX_SIZE;

use crate::error::FileError;
use crate::traits::{FileServiceRef, ItemRevealerRef, SnapshotServiceRef, SystemFileOpenerRef};

/// Request-body cap for `PUT /api/fs/content`, aligned with the 256 MB read cap
/// so large files can be saved (the 10 MB global limit would otherwise 413).
const CONTENT_MAX_SIZE: usize = 256 * 1024 * 1024;
use crate::types::{
    CompareResult, CopyResult, DirOrFile, FileChangeInfo, FileMetadata, SnapshotInfo, SnapshotMode, WorkspaceFlatFile,
};

impl From<FileError> for ApiError {
    fn from(error: FileError) -> Self {
        match error {
            FileError::BadRequest(message) => ApiError::BadRequest(message),
            FileError::Forbidden(message) => ApiError::Forbidden(message),
            FileError::PathOutsideSandbox {
                message,
                field,
                operation,
            } => ApiError::PathOutsideSandbox {
                message,
                field,
                operation,
            },
            FileError::NotFound(message) => ApiError::NotFound(message),
            FileError::Internal(message) => ApiError::Internal(message),
            // The cause was logged where it arose; it is not forwarded because it
            // comes from the shell layer and can quote subprocess stderr or a path.
            // The client keys off `REVEAL_FAILED` and supplies its own wording.
            FileError::RevealFailed(_) => ApiError::coded(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "REVEAL_FAILED",
                "Could not open the system file manager.",
                None::<serde_json::Value>,
            ),
            // Identity-addressed not-found: a stable code and a path-free message,
            // since the resolved absolute path is server-side only.
            FileError::TargetNotFound => ApiError::coded(
                axum::http::StatusCode::NOT_FOUND,
                "FILE_NOT_FOUND",
                "The requested file no longer exists.",
                None::<serde_json::Value>,
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Router state
// ---------------------------------------------------------------------------

/// Shared state for all file-related route handlers.
#[derive(Clone)]
pub struct FileRouterState {
    pub file_service: FileServiceRef,
    pub snapshot_service: SnapshotServiceRef,
    /// Resolves pe-addressed copy/reveal targets (`/api/fs/copy`,
    /// `/api/fs/reveal`) to absolute paths.
    pub project: Arc<aionui_project::ProjectService>,
    /// Reveals a resolved absolute path in the OS file manager
    /// (`/api/fs/reveal`). Injected by composition over the shell service.
    pub revealer: ItemRevealerRef,
    /// Opens a resolved absolute path with the OS default application
    /// (`/api/fs/open-system`). Injected by composition over the shell service.
    pub system_opener: SystemFileOpenerRef,
    pub allowed_roots: Vec<std::path::PathBuf>,
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the file router with all `/api/fs/*` routes.
///
/// All routes require authentication (applied by the caller).
pub fn file_routes(state: FileRouterState) -> Router {
    // Upload route carries its own body-size limit (UPLOAD_MAX_SIZE, 30 MB).
    // We first disable the global `DefaultBodyLimit` that `aionui-app`
    // installs (otherwise the `Multipart` extractor would cap the body at
    // `BODY_LIMIT`), then apply `RequestBodyLimitLayer` as the sole hard
    // cap. The layers are added in outer->inner order via `.layer()`.
    let upload_router = Router::new()
        .route("/api/fs/upload", post(upload_file))
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(UPLOAD_MAX_SIZE))
        .with_state(state.clone());

    // Content endpoint (ChatFileRef identity). PUT carries the full file body,
    // so — like upload — it disables the 10 MB global `DefaultBodyLimit` and
    // applies its own `CONTENT_MAX_SIZE` cap aligned with the 256 MB read cap
    // (otherwise saving a large file would 413 before reaching the handler).
    // POST (read) shares the sub-router; its body is a tiny ChatFileRef so the
    // wider limit is harmless.
    let content_router = Router::new()
        .route("/api/fs/content", post(read_content).put(write_content))
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(CONTENT_MAX_SIZE))
        .with_state(state.clone());

    Router::new()
        // A. Core file operations
        .route("/api/fs/content/metadata", post(content_metadata))
        .route("/api/fs/stream", get(stream_file))
        .route("/api/fs/dir", post(get_files_by_dir))
        .route("/api/fs/list", post(list_workspace_files))
        .route("/api/fs/metadata", post(get_file_metadata))
        .route("/api/fs/read", post(read_file))
        .route("/api/fs/write", post(write_file))
        .route("/api/fs/copy", post(copy_files))
        .route("/api/fs/reveal", post(reveal_item))
        .route("/api/fs/open-system", post(open_system_file))
        .route("/api/fs/image-base64", post(get_image_base64))
        .route("/api/fs/fetch-remote-image", post(fetch_remote_image))
        // B. Workspace snapshot
        .route("/api/fs/snapshot/init", post(snapshot_init))
        .route("/api/fs/snapshot/info", post(snapshot_info))
        .route("/api/fs/snapshot/compare", post(snapshot_compare))
        .route("/api/fs/snapshot/baseline", post(snapshot_baseline))
        .route("/api/fs/snapshot/stage", post(snapshot_stage_file))
        .route("/api/fs/snapshot/stage-all", post(snapshot_stage_all))
        .route("/api/fs/snapshot/unstage", post(snapshot_unstage_file))
        .route("/api/fs/snapshot/unstage-all", post(snapshot_unstage_all))
        .route("/api/fs/snapshot/discard", post(snapshot_discard))
        .route("/api/fs/snapshot/reset", post(snapshot_reset))
        .route("/api/fs/snapshot/branches", post(snapshot_branches))
        .route("/api/fs/snapshot/dispose", post(snapshot_dispose))
        .with_state(state)
        .merge(upload_router)
        .merge(content_router)
}

// ---------------------------------------------------------------------------
// A. Core file operations — handlers
// ---------------------------------------------------------------------------

async fn get_files_by_dir(
    State(state): State<FileRouterState>,
    body: Result<Json<GetFilesByDirRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Vec<DirOrFileResponse>>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let items = state.file_service.get_files_by_dir(&req.dir, &req.root).await?;
    let response: Vec<DirOrFileResponse> = items.into_iter().map(to_dir_or_file_response).collect();
    Ok(Json(ApiResponse::ok(response)))
}

async fn list_workspace_files(
    State(state): State<FileRouterState>,
    body: Result<Json<ListWorkspaceFilesRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Vec<WorkspaceFlatFileResponse>>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let root = req.root.trim();
    if root.is_empty() {
        return Err(ApiError::BadRequest("root is required".to_owned()));
    }
    let items = state
        .file_service
        .list_workspace_files_with_extra_root(root, Some(Path::new(root)))
        .await?;

    let response: Vec<WorkspaceFlatFileResponse> = items.into_iter().map(to_flat_file_response).collect();
    Ok(Json(ApiResponse::ok(response)))
}

async fn get_file_metadata(
    State(state): State<FileRouterState>,
    body: Result<Json<GetFileMetadataRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<FileMetadataResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let meta = state
        .file_service
        .get_file_metadata(&req.path, req.workspace.as_deref().map(Path::new))
        .await?;
    Ok(Json(ApiResponse::ok(to_metadata_response(meta))))
}

async fn read_file(
    State(state): State<FileRouterState>,
    body: Result<Json<ReadFileRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Option<String>>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let content = state
        .file_service
        .read_file(&req.path, req.workspace.as_deref().map(Path::new))
        .await?;
    Ok(Json(ApiResponse::ok(content)))
}

async fn write_file(
    State(state): State<FileRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<WriteFileRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<bool>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let workspace = req.workspace.unwrap_or_else(|| {
        std::path::Path::new(&req.path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let ok = state
        .file_service
        .write_file_for_user(&user.id, &req.path, req.data.as_bytes(), &workspace)
        .await?;
    Ok(Json(ApiResponse::ok(ok)))
}

async fn copy_files(
    State(state): State<FileRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CopyFilesRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CopyFilesResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    // Resolve the pe-addressed target to an absolute directory (containment +
    // identity via the project service); device file paths are copied into it.
    let resolved = state
        .project
        .resolve_reference(
            &user.id,
            aionui_project::ReferenceInput {
                pe_id: req.target.pe_id,
                relative_path: req.target.relative_path,
                op: aionui_project::FileOp::Write,
            },
        )
        .await
        .map_err(ApiError::from)?;
    let dir = resolved
        .absolute_path
        .ok_or_else(|| ApiError::BadRequest("copy target is not a local path".to_owned()))?;
    let result = state
        .file_service
        .copy_files_to_workspace(&req.file_paths, &dir, req.source_root.as_deref())
        .await?;
    Ok(Json(ApiResponse::ok(to_copy_response(result))))
}

/// `POST /api/fs/reveal` — reveal a pe-addressed file/dir in the OS file manager
/// ("open enclosing folder"). Resolves the identity to an absolute path
/// (containment-checked, op = Read) then hands it to the reveal capability.
async fn reveal_item(
    State(state): State<FileRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<RevealItemRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let resolved = state
        .project
        .resolve_reference(
            &user.id,
            aionui_project::ReferenceInput {
                pe_id: req.pe_id,
                relative_path: req.relative_path,
                op: aionui_project::FileOp::Read,
            },
        )
        .await
        .map_err(ApiError::from)?;
    reveal_resolved(state.revealer.as_ref(), resolved.absolute_path).await?;
    Ok(Json(ApiResponse::success()))
}

/// Reveal the resolved absolute path via the revealer port. Split from the
/// handler so the resolve → reveal wiring (and its no-local-path / reveal-failed
/// error mapping) is unit-testable with a mock revealer, independent of the
/// project service (`resolve_reference` is covered in `aionui-project`).
async fn reveal_resolved(
    revealer: &dyn crate::traits::IItemRevealer,
    absolute_path: Option<String>,
) -> Result<(), FileError> {
    let abs = absolute_path.ok_or_else(|| FileError::BadRequest("reveal target is not a local path".to_owned()))?;
    revealer.reveal(&abs).await
}

/// Collapse a `ChatFileRef` resolution failure into a path-free API error.
///
/// Shared by every `ChatFileRef`-addressed endpoint (content read/write, metadata,
/// stream, open-system). The shared `From<ProjectError> for ApiError` mapping is
/// **not** usable for these: it renders the error's `Display` as the public message
/// *and* repeats the path under `details.path`, while several variants
/// (`ChatFileMissing`, `LocalPathNotReadable`, `UploadPathOutsideRoot`) carry the
/// absolute path. Both outlets have to be closed — sealing only the message leaves
/// `details` as a second channel.
///
/// These callers address files by identity, so the absolute path is resolved
/// server-side and the client has never seen it; disclosing it in an error would be
/// telling the client something it had no way to know. Endpoints keyed on
/// client-supplied paths are a different case (echoing back what the caller sent
/// reveals nothing) and keep using the shared mapping.
///
/// Resolution failures collapse to one code deliberately: from the client's side
/// "we could not resolve what you named" is a single outcome, and splitting it
/// further would start signalling *why* — which is where path detail creeps back
/// in. Internal failures stay `INTERNAL_ERROR`, whose public message is already
/// fixed.
fn chat_file_resolve_error(err: aionui_project::ProjectError) -> ApiError {
    let code = err.code();
    tracing::warn!(target: "chat_file", error = %err, code, "could not resolve chat file reference");
    match err {
        aionui_project::ProjectError::Database(_) => ApiError::Internal("failed to resolve target".to_owned()),
        _ => ApiError::coded(
            axum::http::StatusCode::NOT_FOUND,
            "FILE_NOT_FOUND",
            "The requested file no longer exists.",
            None::<serde_json::Value>,
        ),
    }
}

/// `POST /api/fs/open-system` — open a `ChatFileRef`-addressed file with the OS
/// default application ("open in system editor"). Preview surfaces this as the
/// escape hatch for files it declines to render (oversized or unsupported
/// formats), so it accepts all three preview sources rather than only project
/// files the way `/api/fs/reveal` does.
///
/// # INV-OPEN (invariant — do not weaken)
///
/// This endpoint's sole effect is invoking the system opener **on the backend
/// host**. It must never return the resolved absolute path to the client in any
/// form:
///
/// - success → empty body;
/// - failure → a stable error code plus a message that says nothing about the
///   path (no `message`, no `details`, no code carrying a path fragment).
///
/// The client addressed this by identity and has no absolute path of its own; the
/// one resolved here is server-side knowledge. Both failure sources are therefore
/// narrowed on purpose: [`chat_file_resolve_error`] discards the resolver's
/// path-bearing context, and the opener adapter logs its cause instead of
/// returning it ([`FileError::TargetNotFound`] has no payload to fill). The
/// earlier reveal implementation threaded a shell error's path through
/// `NotFound(String)` into the response body; leaving nothing to forward is what
/// stops that recurring.
async fn open_system_file(
    State(state): State<FileRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<OpenSystemFileRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let abs = state
        .project
        .resolve_chat_file_ref(
            &user.id,
            &req.file,
            &content_upload_root(),
            aionui_project::FileOp::Read,
        )
        .await
        .map_err(chat_file_resolve_error)?;
    state.system_opener.open(&abs).await?;
    Ok(Json(ApiResponse::success()))
}

// ---------------------------------------------------------------------------
// Content endpoint (ChatFileRef identity) — handlers
// ---------------------------------------------------------------------------

/// Managed upload directory (`<tmp>/aionui`) used to validate `Upload`
/// ChatFileRef variants — mirrors the chat send-boundary convention.
fn content_upload_root() -> PathBuf {
    std::env::temp_dir().join("aionui")
}

/// Parse the optional `If-Match` header as a last-modified-millisecond stamp.
fn parse_if_match(headers: &axum::http::HeaderMap) -> Option<i64> {
    headers
        .get(axum::http::header::IF_MATCH)?
        .to_str()
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()
}

/// `POST /api/fs/content` — read a file addressed by `ChatFileRef` identity.
/// Collapses the old `read` + `image-base64`: body carries the ref plus an
/// `encoding` (utf8|base64|dataurl). Resolves per-variant (op = Read) then reads
/// the trusted absolute path.
async fn read_content(
    State(state): State<FileRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<ReadContentRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let abs = state
        .project
        .resolve_chat_file_ref(
            &user.id,
            &req.file,
            &content_upload_root(),
            aionui_project::FileOp::Read,
        )
        .await
        .map_err(chat_file_resolve_error)?;
    let content = state
        .file_service
        .read_resolved_content(Path::new(&abs), req.encoding)
        .await?;
    Ok(Json(ApiResponse::ok(content)))
}

/// `PUT /api/fs/content` — write a file addressed by `ChatFileRef` identity
/// (op = Write for the Project arm). Optimistic concurrency: when the client
/// sends `If-Match: <last-modified ms>`, a mismatch against the current on-disk
/// mtime returns 409 Conflict (guards the "external change silently overwritten"
/// case). Body cap is `CONTENT_MAX_SIZE` (see the router builder).
async fn write_content(
    State(state): State<FileRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: axum::http::HeaderMap,
    body: Result<Json<WriteContentRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<bool>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let abs = state
        .project
        .resolve_chat_file_ref(
            &user.id,
            &req.file,
            &content_upload_root(),
            aionui_project::FileOp::Write,
        )
        .await
        .map_err(chat_file_resolve_error)?;
    let path = Path::new(&abs);

    if let Some(expected) = parse_if_match(&headers) {
        let current = state.file_service.resolved_metadata(path).await?.last_modified;
        if current != expected {
            return Err(ApiError::Conflict(format!(
                "file changed on disk since last read (expected mtime {expected}, found {current})"
            )));
        }
    }

    state
        .file_service
        .write_resolved_content(path, req.data.as_bytes())
        .await?;
    Ok(Json(ApiResponse::ok(true)))
}

/// `POST /api/fs/content/metadata` — metadata for a `ChatFileRef`-addressed file.
async fn content_metadata(
    State(state): State<FileRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<ContentMetadataRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<FileMetadataResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let abs = state
        .project
        .resolve_chat_file_ref(
            &user.id,
            &req.file,
            &content_upload_root(),
            aionui_project::FileOp::Read,
        )
        .await
        .map_err(chat_file_resolve_error)?;
    let meta = state.file_service.resolved_metadata(Path::new(&abs)).await?;
    Ok(Json(ApiResponse::ok(to_metadata_response(meta))))
}

/// `GET /api/fs/stream` — raw byte range server for a `ChatFileRef`-addressed
/// file, for `<webview src>` / `<embed>` consumers (pdf) that can only GET.
///
/// The identity is a flattened [`StreamQuery`] in the query string (webview src
/// has no request body). Resolves per-variant (op = Read) to a trusted absolute
/// path, then hands the request to `tower_http`'s `ServeFile`, which supplies
/// `Content-Type` (from the extension), `Accept-Ranges`, and full `Range` /
/// `If-Range` handling (206 Partial Content) — including large-file byte ranges.
async fn stream_file(
    State(state): State<FileRouterState>,
    Extension(user): Extension<CurrentUser>,
    request: Request,
) -> Result<Response, ApiError> {
    let Query(params) =
        Query::<StreamQuery>::try_from_uri(request.uri()).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let file_ref = params
        .to_chat_file_ref()
        .map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    let abs = state
        .project
        .resolve_chat_file_ref(
            &user.id,
            &file_ref,
            &content_upload_root(),
            aionui_project::FileOp::Read,
        )
        .await
        .map_err(chat_file_resolve_error)?;
    // ServeFile owns Range/If-Range/Content-Type; the path is already
    // containment-checked by resolve_chat_file_ref, so no re-sandbox here.
    let response = ServeFile::new(&abs)
        .oneshot(request)
        .await
        .map_err(|e| ApiError::Internal(format!("stream task failed: {e}")))?;
    Ok(response.into_response())
}

/// Fields extracted from a `/api/fs/upload` multipart request.
struct UploadMultipartFields {
    file_data: Vec<u8>,
    file_name: Option<String>,
    dispo_file_name: Option<String>,
    conversation_id: Option<String>,
}

/// Strip any directory component from a file name and reject empty results.
/// The returned name is guaranteed not to contain path separators; deeper
/// traversal validation happens in [`IFileService::create_upload_file`].
fn sanitize_upload_filename(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let last = trimmed.rsplit(['/', '\\']).next().unwrap_or("");
    let last = last.trim();
    if last.is_empty() { None } else { Some(last.to_owned()) }
}

async fn extract_upload_multipart(mut multipart: Multipart) -> Result<UploadMultipartFields, ApiError> {
    let mut file_data: Option<Vec<u8>> = None;
    let mut file_name: Option<String> = None;
    let mut dispo_file_name: Option<String> = None;
    let mut conversation_id: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("multipart error: {e}")))?
    {
        let name = field.name().unwrap_or("").to_owned();
        match name.as_str() {
            "file" => {
                // Capture the Content-Disposition filename (if any) before
                // consuming the field body — `field.file_name()` is only
                // available on the field metadata, not on the Bytes below.
                dispo_file_name = field.file_name().and_then(sanitize_upload_filename);
                file_data = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| ApiError::BadRequest(format!("failed to read file: {e}")))?
                        .to_vec(),
                );
            }
            "file_name" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| ApiError::BadRequest(format!("failed to read file_name: {e}")))?;
                if let Some(name) = sanitize_upload_filename(&text) {
                    file_name = Some(name);
                }
            }
            "conversation_id" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| ApiError::BadRequest(format!("failed to read conversation_id: {e}")))?;
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    conversation_id = Some(trimmed.to_owned());
                }
            }
            _ => {}
        }
    }

    let file_data = file_data.ok_or_else(|| ApiError::BadRequest("missing 'file' field".to_owned()))?;

    Ok(UploadMultipartFields {
        file_data,
        file_name,
        dispo_file_name,
        conversation_id,
    })
}

async fn upload_file(
    State(state): State<FileRouterState>,
    multipart: Multipart,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    let fields = extract_upload_multipart(multipart).await?;

    let file_name = fields.file_name.or(fields.dispo_file_name).ok_or_else(|| {
        ApiError::BadRequest("missing file name: provide 'file_name' or a multipart filename".to_owned())
    })?;

    let path = state
        .file_service
        .create_upload_file(&file_name, &fields.file_data, fields.conversation_id.as_deref())
        .await?;
    Ok(Json(ApiResponse::ok(path)))
}

async fn get_image_base64(
    State(state): State<FileRouterState>,
    body: Result<Json<GetImageBase64Request>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let data_url = state
        .file_service
        .get_image_base64(&req.path, req.workspace.as_deref().map(Path::new))
        .await?;
    Ok(Json(ApiResponse::ok(data_url)))
}

async fn fetch_remote_image(
    State(state): State<FileRouterState>,
    body: Result<Json<FetchRemoteImageRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let data_url = state.file_service.fetch_remote_image(&req.url).await;
    Ok(Json(ApiResponse::ok(data_url)))
}

// ---------------------------------------------------------------------------
// B. Workspace snapshot — handlers
// ---------------------------------------------------------------------------

async fn snapshot_init(
    State(state): State<FileRouterState>,
    body: Result<Json<SnapshotWorkspaceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<SnapshotInfoResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let info = state.snapshot_service.init(&req.workspace).await?;
    Ok(Json(ApiResponse::ok(to_snapshot_info_response(info))))
}

async fn snapshot_info(
    State(state): State<FileRouterState>,
    body: Result<Json<SnapshotWorkspaceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<SnapshotInfoResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let info = state.snapshot_service.get_info(&req.workspace).await?;
    Ok(Json(ApiResponse::ok(to_snapshot_info_response(info))))
}

async fn snapshot_compare(
    State(state): State<FileRouterState>,
    body: Result<Json<SnapshotWorkspaceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<SnapshotCompareResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state.snapshot_service.compare(&req.workspace).await?;
    Ok(Json(ApiResponse::ok(to_compare_response(result))))
}

async fn snapshot_baseline(
    State(state): State<FileRouterState>,
    body: Result<Json<SnapshotBaselineRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Option<String>>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let content = state
        .snapshot_service
        .get_baseline_content(&req.workspace, &req.file_path)
        .await?;
    Ok(Json(ApiResponse::ok(content)))
}

async fn snapshot_stage_file(
    State(state): State<FileRouterState>,
    body: Result<Json<SnapshotStageRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .snapshot_service
        .stage_file(&req.workspace, &req.file_path)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn snapshot_stage_all(
    State(state): State<FileRouterState>,
    body: Result<Json<SnapshotWorkspaceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state.snapshot_service.stage_all(&req.workspace).await?;
    Ok(Json(ApiResponse::success()))
}

async fn snapshot_unstage_file(
    State(state): State<FileRouterState>,
    body: Result<Json<SnapshotStageRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .snapshot_service
        .unstage_file(&req.workspace, &req.file_path)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn snapshot_unstage_all(
    State(state): State<FileRouterState>,
    body: Result<Json<SnapshotWorkspaceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state.snapshot_service.unstage_all(&req.workspace).await?;
    Ok(Json(ApiResponse::success()))
}

async fn snapshot_discard(
    State(state): State<FileRouterState>,
    body: Result<Json<SnapshotDiscardRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .snapshot_service
        .discard_file(&req.workspace, &req.file_path, req.operation)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn snapshot_reset(
    State(state): State<FileRouterState>,
    body: Result<Json<SnapshotDiscardRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .snapshot_service
        .reset_file(&req.workspace, &req.file_path, req.operation)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn snapshot_branches(
    State(state): State<FileRouterState>,
    body: Result<Json<SnapshotWorkspaceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Vec<String>>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let branches = state.snapshot_service.get_branches(&req.workspace).await?;
    Ok(Json(ApiResponse::ok(branches)))
}

async fn snapshot_dispose(
    State(state): State<FileRouterState>,
    body: Result<Json<SnapshotWorkspaceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state.snapshot_service.dispose(&req.workspace).await?;
    Ok(Json(ApiResponse::success()))
}

// ---------------------------------------------------------------------------
// Domain → DTO conversions
// ---------------------------------------------------------------------------

fn to_dir_or_file_response(d: DirOrFile) -> DirOrFileResponse {
    let children = if d.is_dir {
        Some(d.children.into_iter().map(to_dir_or_file_response).collect())
    } else {
        None
    };
    DirOrFileResponse {
        name: d.name,
        full_path: d.full_path,
        relative_path: d.relative_path,
        is_dir: d.is_dir,
        is_file: !d.is_dir,
        children,
    }
}

fn to_flat_file_response(f: WorkspaceFlatFile) -> WorkspaceFlatFileResponse {
    WorkspaceFlatFileResponse {
        name: f.name,
        full_path: f.full_path,
        relative_path: f.relative_path,
    }
}

fn to_metadata_response(m: FileMetadata) -> FileMetadataResponse {
    FileMetadataResponse {
        name: m.name,
        path: m.path,
        size: m.size,
        mime_type: m.mime_type,
        last_modified: m.last_modified,
        is_directory: if m.is_directory { Some(true) } else { None },
    }
}

fn to_copy_response(r: CopyResult) -> CopyFilesResponse {
    CopyFilesResponse {
        copied_files: r.copied_files,
        failed_files: r.failed_files,
    }
}

fn to_snapshot_info_response(info: SnapshotInfo) -> SnapshotInfoResponse {
    let mode = match info.mode {
        SnapshotMode::GitRepo => aionui_api_types::SnapshotMode::GitRepo,
        SnapshotMode::Snapshot => aionui_api_types::SnapshotMode::Snapshot,
    };
    SnapshotInfoResponse {
        mode,
        branch: info.branch,
    }
}

fn to_file_change_response(c: FileChangeInfo) -> FileChangeInfoResponse {
    FileChangeInfoResponse {
        file_path: c.file_path,
        relative_path: c.relative_path,
        operation: c.operation,
    }
}

fn to_compare_response(r: CompareResult) -> SnapshotCompareResponse {
    SnapshotCompareResponse {
        staged: r.staged.into_iter().map(to_file_change_response).collect(),
        unstaged: r.unstaged.into_iter().map(to_file_change_response).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_path_outside_sandbox_maps_to_explicit_api_code() {
        let api_err = ApiError::from(FileError::PathOutsideSandbox {
            message: "path '/tmp/x' is outside the allowed sandbox".into(),
            field: Some("path"),
            operation: Some("access"),
        });
        assert_eq!(api_err.error_code(), "PATH_OUTSIDE_SANDBOX");
        assert_eq!(api_err.error_details().unwrap()["field"], "path");
        assert_eq!(api_err.error_details().unwrap()["operation"], "access");
    }

    #[test]
    fn reveal_failed_maps_to_stable_code() {
        // Contract for the frontend: distinct from NOT_FOUND so it can tell
        // "couldn't open the file manager" from "item gone".
        let api_err = ApiError::from(FileError::RevealFailed("gdbus not available".into()));
        assert_eq!(api_err.error_code(), "REVEAL_FAILED");
        assert_eq!(api_err.status_code(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// The cause carried by `RevealFailed` is for logs only. It originates in the
    /// shell layer, where it can quote subprocess stderr or an absolute path, so the
    /// boundary must substitute its own wording rather than pass it through.
    #[test]
    fn reveal_failed_does_not_publish_its_cause() {
        let api_err = ApiError::from(FileError::RevealFailed(
            "gdbus failed for /Users/someone/secret-dir/file.txt".into(),
        ));
        let public = api_err.public_message();
        assert!(
            !public.contains("secret-dir") && !public.contains("gdbus"),
            "internal cause must not reach the client, got {public:?}"
        );
    }

    /// INV-OPEN at the HTTP boundary: an identity-addressed target that is gone
    /// yields a stable code and a path-free message. `TargetNotFound` is
    /// payload-free by construction, so this pins the code/status/message contract
    /// the frontend keys off.
    #[test]
    fn target_not_found_maps_to_path_free_stable_code() {
        let api_err = ApiError::from(FileError::TargetNotFound);
        assert_eq!(api_err.error_code(), "FILE_NOT_FOUND");
        assert_eq!(api_err.status_code(), axum::http::StatusCode::NOT_FOUND);
        assert_eq!(api_err.public_message(), "The requested file no longer exists.");
        assert!(api_err.error_details().is_none(), "details must not carry path context");
    }

    // -- open_system_file: the system-open port seam ---------------------------

    /// Records the absolute paths handed to `open`, and optionally fails the way
    /// the real adapter does (classification only, never the path).
    struct MockSystemOpener {
        calls: std::sync::Mutex<Vec<String>>,
        fail: bool,
    }
    impl MockSystemOpener {
        fn new(fail: bool) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                fail,
            }
        }
    }
    #[async_trait::async_trait]
    impl crate::traits::ISystemFileOpener for MockSystemOpener {
        async fn open(&self, absolute_path: &str) -> Result<(), FileError> {
            self.calls.lock().unwrap().push(absolute_path.to_owned());
            if self.fail {
                Err(FileError::TargetNotFound)
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn system_opener_receives_the_resolved_absolute_path() {
        // The port takes the trusted absolute path resolved from the identity — the
        // handler must not hand it anything client-derived.
        let mock = MockSystemOpener::new(false);
        let result = crate::traits::ISystemFileOpener::open(&mock, "/abs/target.docx").await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert_eq!(*mock.calls.lock().unwrap(), vec!["/abs/target.docx".to_owned()]);
    }

    /// INV-OPEN end to end at the seam: a failure carrying a path-shaped input must
    /// still render a response with no trace of it.
    #[tokio::test]
    async fn system_open_failure_response_is_path_free() {
        let mock = MockSystemOpener::new(true);
        let err = crate::traits::ISystemFileOpener::open(&mock, "/Users/someone/secret-dir/private.docx")
            .await
            .expect_err("must fail");

        let api_err = ApiError::from(err);
        assert_eq!(api_err.error_code(), "FILE_NOT_FOUND");
        let public = api_err.public_message();
        let details = format!("{:?}", api_err.error_details());
        for haystack in [&public, &details] {
            assert!(
                !haystack.contains("secret-dir") && !haystack.contains("private.docx"),
                "response must not disclose the resolved path, got {haystack:?}"
            );
        }
    }

    // -- reveal_resolved: resolve → reveal wiring (mock revealer seam) ---------

    /// Records the absolute paths handed to `reveal`, and optionally fails.
    struct MockRevealer {
        calls: std::sync::Mutex<Vec<String>>,
        fail: bool,
    }
    impl MockRevealer {
        fn new(fail: bool) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                fail,
            }
        }
    }
    #[async_trait::async_trait]
    impl crate::traits::IItemRevealer for MockRevealer {
        async fn reveal(&self, absolute_path: &str) -> Result<(), FileError> {
            self.calls.lock().unwrap().push(absolute_path.to_owned());
            if self.fail {
                Err(FileError::RevealFailed("mock reveal failed".to_owned()))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn reveal_resolved_passes_absolute_path_to_revealer() {
        let mock = MockRevealer::new(false);
        let result = reveal_resolved(&mock, Some("/abs/target.txt".to_owned())).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert_eq!(
            *mock.calls.lock().unwrap(),
            vec!["/abs/target.txt".to_owned()],
            "revealer must receive the resolved absolute path"
        );
    }

    #[tokio::test]
    async fn reveal_resolved_without_local_path_is_bad_request_and_skips_reveal() {
        let mock = MockRevealer::new(false);
        let result = reveal_resolved(&mock, None).await;
        assert!(
            matches!(result, Err(FileError::BadRequest(_))),
            "non-local target must be BadRequest, got {result:?}"
        );
        assert!(mock.calls.lock().unwrap().is_empty(), "revealer must not be called");
    }

    #[tokio::test]
    async fn reveal_resolved_propagates_reveal_failure() {
        let mock = MockRevealer::new(true);
        let result = reveal_resolved(&mock, Some("/abs/x".to_owned())).await;
        assert!(
            matches!(result, Err(FileError::RevealFailed(_))),
            "reveal failure must propagate, got {result:?}"
        );
    }

    /// The resolver seam shared by every `ChatFileRef`-addressed endpoint — the leak
    /// the shared `From<ProjectError>` mapping introduces. Verified against it
    /// directly: that mapping renders `ChatFileMissing`'s `Display` as the public
    /// message ("attached file does not exist: /Users/…/private.docx") *and* repeats
    /// the path under `details.path`. Both outlets are asserted, because sealing only
    /// the message would leave `details` as a second channel.
    #[test]
    fn chat_file_resolve_error_is_path_free_for_path_bearing_variants() {
        let secret = "/Users/someone/secret-dir/private.docx";
        let variants = [
            aionui_project::ProjectError::ChatFileMissing {
                path: secret.to_owned(),
            },
            aionui_project::ProjectError::LocalPathNotReadable {
                path: secret.to_owned(),
            },
            aionui_project::ProjectError::UploadPathOutsideRoot {
                path: secret.to_owned(),
            },
        ];

        for err in variants {
            let api_err = chat_file_resolve_error(err);
            assert_eq!(api_err.error_code(), "FILE_NOT_FOUND");
            assert_eq!(api_err.status_code(), axum::http::StatusCode::NOT_FOUND);

            let public = api_err.public_message();
            let details = format!("{:?}", api_err.error_details());
            for haystack in [&public, &details] {
                assert!(
                    !haystack.contains("secret-dir") && !haystack.contains("private.docx"),
                    "resolve failure must not disclose the path, got {haystack:?}"
                );
            }
        }
    }

    /// Every `ChatFileRef`-addressed handler must route its resolver failure through
    /// [`chat_file_resolve_error`], not the shared `From<ProjectError>` mapping.
    ///
    /// Asserted against the source text because the alternative — spinning up five
    /// authenticated handlers with a real `ProjectService` — would not actually pin
    /// this: the wiring is a single `map_err` per handler, and a future edit swapping
    /// one back to `ApiError::from` is exactly the regression worth catching. The
    /// count guards against a sixth such endpoint being added without a decision:
    /// bump it deliberately, having checked the new one addresses files by identity.
    #[test]
    fn every_chat_file_ref_endpoint_uses_the_sealed_resolver_mapping() {
        // Scan handler code only. This test module mentions both needles in its own
        // assertions, and counting those would inflate the totals.
        let source = include_str!("routes.rs");
        let handlers = source
            .split_once("\n#[cfg(test)]")
            .map(|(before, _)| before)
            .expect("routes.rs has a #[cfg(test)] module");

        let resolve_calls = handlers.matches(".resolve_chat_file_ref(").count();
        let sealed = handlers.matches(".map_err(chat_file_resolve_error)?").count();

        assert_eq!(
            resolve_calls, 5,
            "expected 5 ChatFileRef-addressed endpoints (content read/write, metadata, stream, \
             open-system); found {resolve_calls} — a new one must be checked for identity \
             addressing and sealed before bumping this"
        );
        assert_eq!(
            sealed, resolve_calls,
            "every resolve_chat_file_ref call must map its error through \
             chat_file_resolve_error; {sealed} of {resolve_calls} do. A handler using \
             ApiError::from leaks the resolved absolute path in message and details."
        );
    }

    /// The `Database` arm (reachable through `resolve_reference`) is deliberately
    /// not covered here: constructing a `DbError` would mean adding `aionui-db` as a
    /// dependency of this crate purely for a test. Its correctness rests on
    /// `ApiError::Internal`'s public message being a fixed string, which
    /// `aionui-common` already tests.
    #[test]
    fn dir_or_file_response_conversion_file() {
        let d = DirOrFile {
            name: "test.txt".into(),
            full_path: "/ws/test.txt".into(),
            relative_path: "test.txt".into(),
            is_dir: false,
            children: vec![],
        };
        let r = to_dir_or_file_response(d);
        assert_eq!(r.name, "test.txt");
        assert!(!r.is_dir);
        assert!(r.is_file);
        assert!(r.children.is_none());
    }

    #[test]
    fn dir_or_file_response_conversion_dir_with_children() {
        let d = DirOrFile {
            name: "src".into(),
            full_path: "/ws/src".into(),
            relative_path: "src".into(),
            is_dir: true,
            children: vec![DirOrFile {
                name: "main.rs".into(),
                full_path: "/ws/src/main.rs".into(),
                relative_path: "src/main.rs".into(),
                is_dir: false,
                children: vec![],
            }],
        };
        let r = to_dir_or_file_response(d);
        assert!(r.is_dir);
        assert!(!r.is_file);
        let children = r.children.unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "main.rs");
    }

    #[test]
    fn flat_file_response_conversion() {
        let f = WorkspaceFlatFile {
            name: "lib.rs".into(),
            full_path: "/ws/src/lib.rs".into(),
            relative_path: "src/lib.rs".into(),
        };
        let r = to_flat_file_response(f);
        assert_eq!(r.name, "lib.rs");
        assert_eq!(r.full_path, "/ws/src/lib.rs");
        assert_eq!(r.relative_path, "src/lib.rs");
    }

    #[test]
    fn metadata_response_conversion_file() {
        let m = FileMetadata {
            name: "readme.md".into(),
            path: "/ws/readme.md".into(),
            size: 1024,
            mime_type: "text/markdown".into(),
            last_modified: 1700000000000,
            is_directory: false,
        };
        let r = to_metadata_response(m);
        assert_eq!(r.name, "readme.md");
        assert_eq!(r.size, 1024);
        assert!(r.is_directory.is_none());
    }

    #[test]
    fn metadata_response_conversion_directory() {
        let m = FileMetadata {
            name: "src".into(),
            path: "/ws/src".into(),
            size: 0,
            mime_type: "".into(),
            last_modified: 1700000000000,
            is_directory: true,
        };
        let r = to_metadata_response(m);
        assert_eq!(r.is_directory, Some(true));
    }

    #[test]
    fn snapshot_info_response_git_repo() {
        let info = SnapshotInfo {
            mode: SnapshotMode::GitRepo,
            branch: Some("main".into()),
        };
        let r = to_snapshot_info_response(info);
        assert_eq!(r.mode, aionui_api_types::SnapshotMode::GitRepo);
        assert_eq!(r.branch, Some("main".into()));
    }

    #[test]
    fn snapshot_info_response_snapshot_mode() {
        let info = SnapshotInfo {
            mode: SnapshotMode::Snapshot,
            branch: None,
        };
        let r = to_snapshot_info_response(info);
        assert_eq!(r.mode, aionui_api_types::SnapshotMode::Snapshot);
        assert!(r.branch.is_none());
    }

    #[test]
    fn compare_response_conversion() {
        use aionui_common::FileChangeOperation;
        let result = CompareResult {
            staged: vec![FileChangeInfo {
                file_path: "/ws/a.txt".into(),
                relative_path: "a.txt".into(),
                operation: FileChangeOperation::Create,
            }],
            unstaged: vec![FileChangeInfo {
                file_path: "/ws/b.txt".into(),
                relative_path: "b.txt".into(),
                operation: FileChangeOperation::Modify,
            }],
        };
        let r = to_compare_response(result);
        assert_eq!(r.staged.len(), 1);
        assert_eq!(r.staged[0].file_path, "/ws/a.txt");
        assert_eq!(r.staged[0].operation, FileChangeOperation::Create);
        assert_eq!(r.unstaged.len(), 1);
        assert_eq!(r.unstaged[0].operation, FileChangeOperation::Modify);
    }

    // ---- sanitize_upload_filename -----------------------------------------

    #[test]
    fn sanitize_upload_filename_strips_directory_components() {
        assert_eq!(sanitize_upload_filename("a/b/c.png").as_deref(), Some("c.png"));
        assert_eq!(sanitize_upload_filename("C:\\tmp\\d.jpg").as_deref(), Some("d.jpg"));
        assert_eq!(
            sanitize_upload_filename("  spaced.txt  ").as_deref(),
            Some("spaced.txt")
        );
    }

    #[test]
    fn sanitize_upload_filename_rejects_empty() {
        assert_eq!(sanitize_upload_filename(""), None);
        assert_eq!(sanitize_upload_filename("   "), None);
        assert_eq!(sanitize_upload_filename("/"), None);
        assert_eq!(sanitize_upload_filename("a/b/"), None);
    }

    #[test]
    fn sanitize_upload_filename_plain_passthrough() {
        assert_eq!(sanitize_upload_filename("image.png").as_deref(), Some("image.png"));
    }
}
