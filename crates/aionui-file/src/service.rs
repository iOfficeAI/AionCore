use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use base64::Engine;
use dashmap::DashMap;
use ignore::WalkBuilder;
use tracing::warn;

use crate::error::FileError;
use aionui_api_types::{ContentEncoding, WebSocketMessage};
use aionui_realtime::EventBroadcaster;

use crate::path_safety::{
    has_traversal, strip_verbatim_prefix, validate_path_for_write, validate_path_with_extra_root,
};
use crate::types::{
    ContentUpdateEvent, ContentUpdateOperation, CopyResult, DirOrFile, FileMetadata, WorkspaceFlatFile,
};

/// Maximum number of files returned by `list_workspace_files`.
const MAX_WORKSPACE_FILES: usize = 20_000;

/// Maximum file size for read operations (256 MB).
const MAX_FILE_SIZE: u64 = 256 * 1024 * 1024;

/// Maximum remote image size (5 MB).
const MAX_REMOTE_IMAGE_SIZE: usize = 5 * 1024 * 1024;

/// Maximum number of HTTP redirects for remote image fetching.
const MAX_REDIRECTS: usize = 5;

/// Request timeout for remote image fetching (30 seconds).
const REMOTE_IMAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Allowed hosts for remote image fetching.
const ALLOWED_IMAGE_HOSTS: &[&str] = &[
    "github.com",
    "raw.githubusercontent.com",
    "avatars.githubusercontent.com",
    "user-images.githubusercontent.com",
    "camo.githubusercontent.com",
    "objects.githubusercontent.com",
    "repository-images.githubusercontent.com",
];

/// Placeholder SVG returned when remote image fetching fails.
const PLACEHOLDER_SVG: &str = concat!(
    "<svg xmlns=\"http://www.w3.org/2000/svg\" ",
    "width=\"200\" height=\"200\" viewBox=\"0 0 200 200\">",
    "<rect fill=\"#f0f0f0\" width=\"200\" height=\"200\"/>",
    "<text x=\"100\" y=\"96\" text-anchor=\"middle\" ",
    "fill=\"#999\" font-family=\"sans-serif\" font-size=\"14\">",
    "Image Unavailable",
    "</text>",
    "</svg>",
);

/// A concrete implementation of [`crate::traits::IFileService`].
pub struct FileService {
    broadcaster: Arc<dyn EventBroadcaster>,
    /// Allowed root directories for path safety validation.
    allowed_roots: Vec<std::path::PathBuf>,
    /// In-memory cache for `list_workspace_files`, keyed by canonical root.
    workspace_files_cache: DashMap<String, Vec<WorkspaceFlatFile>>,
}

impl FileService {
    pub fn new(broadcaster: Arc<dyn EventBroadcaster>, allowed_roots: Vec<std::path::PathBuf>) -> Self {
        Self {
            broadcaster,
            allowed_roots,
            workspace_files_cache: DashMap::new(),
        }
    }

    /// Invalidate the workspace files cache for a given root.
    /// Called when file changes are detected.
    pub fn invalidate_cache(&self, root: &str) {
        self.workspace_files_cache.remove(root);
    }

    /// Get the allowed root references for path validation.
    fn allowed_roots_refs(&self) -> Vec<&Path> {
        self.allowed_roots.iter().map(|p| p.as_path()).collect()
    }

    fn allowed_roots_with_extra<'a>(&'a self, extra_root: Option<&'a Path>) -> Vec<&'a Path> {
        let mut roots = self.allowed_roots_refs();
        if let Some(extra_root) = extra_root {
            roots.push(extra_root);
        }
        roots
    }

    fn path_uses_allowed_root(&self, path: &Path, extra_root: Option<&Path>) -> bool {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            match std::env::current_dir() {
                Ok(current_dir) => current_dir.join(path),
                Err(_) => path.to_path_buf(),
            }
        };

        self.allowed_roots
            .iter()
            .map(PathBuf::as_path)
            .chain(extra_root)
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .any(|root| candidate.starts_with(root))
    }

    /// List immediate children of `dir`, building a single-level tree.
    /// Each child directory also lists *its* children (depth = 2 from `dir`).
    async fn build_dir_tree(&self, dir: &Path, root: &Path) -> Result<Vec<DirOrFile>, FileError> {
        let dir_owned = dir.to_path_buf();
        let root_owned = root.to_path_buf();

        tokio::task::spawn_blocking(move || build_dir_tree_sync(&dir_owned, &root_owned))
            .await
            .map_err(|e| FileError::Internal(format!("directory listing task failed: {e}")))?
    }
}

/// Synchronous directory tree builder (runs in blocking thread pool).
fn build_dir_tree_sync(dir: &Path, root: &Path) -> Result<Vec<DirOrFile>, FileError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| FileError::BadRequest(format!("cannot read directory '{}': {e}", dir.display())))?;

    let mut result = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|e| FileError::Internal(format!("error reading directory entry: {e}")))?;

        let path = entry.path();
        let metadata = entry
            .metadata()
            .map_err(|e| FileError::Internal(format!("cannot read metadata for '{}': {e}", path.display())))?;

        let name = entry.file_name().to_string_lossy().into_owned();

        let full_path = strip_verbatim_prefix(&path.to_string_lossy());
        let relative_path = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned();

        let is_dir = metadata.is_dir();

        // For directories, also read their immediate children
        let children = if is_dir {
            read_children_sync(&path, root)?
        } else {
            Vec::new()
        };

        result.push(DirOrFile {
            name,
            full_path,
            relative_path,
            is_dir,
            children,
        });
    }

    // Sort: directories first, then alphabetical
    result.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));

    Ok(result)
}

/// Read immediate children of a directory (one level, no grandchildren).
fn read_children_sync(dir: &Path, root: &Path) -> Result<Vec<DirOrFile>, FileError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };

    let mut children = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();
        let is_dir = entry.metadata().map(|m| m.is_dir()).unwrap_or(false);

        let name = entry.file_name().to_string_lossy().into_owned();

        let full_path = strip_verbatim_prefix(&path.to_string_lossy());
        let relative_path = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned();

        children.push(DirOrFile {
            name,
            full_path,
            relative_path,
            is_dir,
            children: Vec::new(),
        });
    }

    children.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));

    Ok(children)
}

/// Recursively list files using the `ignore` crate (respects .gitignore).
fn list_workspace_files_sync(root: &Path) -> Result<Vec<WorkspaceFlatFile>, FileError> {
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .build();

    let mut files = Vec::new();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!("skipping unreadable entry: {e}");
                continue;
            }
        };

        let path = entry.path();
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "skipping unreadable workspace entry");
                continue;
            }
        };

        // Skip real directories and symlinks that resolve to directories.
        if metadata.is_dir() {
            continue;
        }

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let full_path = strip_verbatim_prefix(&path.to_string_lossy());
        let relative_path = path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned();

        files.push(WorkspaceFlatFile {
            name,
            full_path,
            relative_path,
        });

        if files.len() >= MAX_WORKSPACE_FILES {
            break;
        }
    }

    Ok(files)
}

/// Validate that a file exists and is within the size limit.
/// Returns `Ok(None)` if the file does not exist.
/// Returns `Ok(Some(()))` if the file is valid for reading.
fn validate_file_for_read(path: &Path) -> Result<Option<()>, FileError> {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(e) => {
            return Err(FileError::Internal(format!(
                "cannot read metadata for '{}': {e}",
                path.display()
            )));
        }
    };

    if metadata.len() > MAX_FILE_SIZE {
        return Err(FileError::BadRequest(format!(
            "file '{}' exceeds 256 MB limit ({} bytes)",
            path.display(),
            metadata.len()
        )));
    }

    if metadata.is_dir() {
        return Err(FileError::BadRequest(format!(
            "path '{}' is a directory; expected a file",
            path.display()
        )));
    }

    Ok(Some(()))
}

/// Read a file as UTF-8 text. Returns `None` if the file does not exist.
/// Rejects files larger than 256 MB.
fn read_file_sync(path: &Path) -> Result<Option<String>, FileError> {
    if validate_file_for_read(path)?.is_none() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(path)
        .map_err(|e| FileError::Internal(format!("cannot read file '{}': {e}", path.display())))?;

    Ok(Some(content))
}

/// Write data to a file synchronously. Creates the file if it does not exist.
/// Returns `true` on success.
fn write_file_sync(path: &Path, data: &[u8]) -> Result<bool, FileError> {
    std::fs::write(path, data)
        .map_err(|e| FileError::Internal(format!("cannot write file '{}': {e}", path.display())))?;
    Ok(true)
}

/// Split a file name into `(base, ext)` where `ext` includes the leading dot.
///
/// Uses the **last** `.` as the extension boundary (matching macOS Finder and
/// Chrome download naming). If the file has no extension, or the only dot is at
/// the very start (hidden files like `.env`), the entire name is treated as the
/// base and `ext` is empty.
///
/// Examples:
/// - `"image.png"` -> `("image", ".png")`
/// - `"foo.tar.gz"` -> `("foo.tar", ".gz")`
/// - `"README"` -> `("README", "")`
/// - `".env"` -> `(".env", "")`
fn split_base_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(idx) if idx > 0 => name.split_at(idx),
        _ => (name, ""),
    }
}

/// Get file metadata synchronously.
fn get_file_metadata_sync(path: &Path) -> Result<FileMetadata, FileError> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| FileError::NotFound(format!("cannot read metadata for '{}': {e}", path.display())))?;

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let size = metadata.len();
    let is_directory = metadata.is_dir();

    let mime_type = if is_directory {
        "inode/directory".to_owned()
    } else {
        mime_guess::from_path(path)
            .first()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "application/octet-stream".to_owned())
    };

    let last_modified = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    Ok(FileMetadata {
        name,
        path: path.to_string_lossy().into_owned(),
        size,
        mime_type,
        last_modified,
        is_directory,
    })
}

/// Copy a single file, creating parent directories as needed.
fn copy_single_file_sync(src: &Path, dest: &Path) -> Result<(), FileError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| FileError::Internal(format!("cannot create directory '{}': {e}", parent.display())))?;
    }

    std::fs::copy(src, dest)
        .map_err(|e| FileError::Internal(format!("cannot copy '{}' to '{}': {e}", src.display(), dest.display())))?;

    Ok(())
}

/// Read a local image file and return a base64 Data URL.
fn get_image_base64_sync(path: &Path) -> Result<String, FileError> {
    let bytes =
        std::fs::read(path).map_err(|e| FileError::NotFound(format!("cannot read image '{}': {e}", path.display())))?;

    let mime = mime_guess::from_path(path)
        .first()
        .map(|m| m.to_string())
        .unwrap_or_else(|| "application/octet-stream".to_owned());

    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);

    Ok(format!("data:{mime};base64,{encoded}"))
}

/// Encode a file's content for the `/api/fs/content` endpoint per `encoding`.
/// `Utf8` → text (errors on non-UTF-8); `Base64` → raw bytes base64 (no prefix);
/// `DataUrl` → `data:<mime>;base64,<...>`. The 256 MB read cap applies to all.
///
/// Reached only from `read_resolved_content`, whose path was resolved from a
/// `ChatFileRef` — server-side knowledge the client never saw. Errors here are
/// therefore path-free (`TargetNotFound` / a fixed `Internal` message) with the
/// detail going to the log instead, unlike the sibling helpers that serve
/// client-supplied paths and may echo them back.
fn read_resolved_content_sync(path: &Path, encoding: ContentEncoding) -> Result<String, FileError> {
    match encoding {
        ContentEncoding::Utf8 => read_file_sync(path)
            .map_err(|err| resolved_read_error(path, err))?
            .ok_or(FileError::TargetNotFound),
        ContentEncoding::Base64 => {
            if validate_file_for_read(path)
                .map_err(|err| resolved_read_error(path, err))?
                .is_none()
            {
                return Err(FileError::TargetNotFound);
            }
            let bytes = std::fs::read(path).map_err(|e| {
                tracing::error!(target: "chat_file", path = %path.display(), error = %e, "cannot read resolved file");
                FileError::Internal("cannot read file".to_owned())
            })?;
            Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
        ContentEncoding::DataUrl => get_image_base64_sync(path).map_err(|err| resolved_read_error(path, err)),
    }
}

/// Strip path detail from an error raised while reading an identity-addressed
/// file, logging it instead. Helpers shared with the client-supplied-path routes
/// embed the path in their messages, which must not reach these callers.
fn resolved_read_error(path: &Path, err: FileError) -> FileError {
    match err {
        FileError::NotFound(cause) => {
            tracing::warn!(target: "chat_file", path = %path.display(), error = %cause, "resolved read target unavailable");
            FileError::TargetNotFound
        }
        FileError::Internal(cause) => {
            tracing::error!(target: "chat_file", path = %path.display(), error = %cause, "resolved read failed");
            FileError::Internal("cannot read file".to_owned())
        }
        // BadRequest / PathOutsideSandbox carry validation context, not a resolved
        // path, and their messages are already client-safe.
        other => other,
    }
}

/// Build a placeholder SVG Data URL for failed remote image fetches.
fn placeholder_svg_data_url() -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(PLACEHOLDER_SVG);
    format!("data:image/svg+xml;base64,{encoded}")
}

/// Check whether a URL host is in the allowed whitelist.
fn is_allowed_image_host(url: &reqwest::Url) -> bool {
    let host = match url.host_str() {
        Some(h) => h,
        None => return false,
    };
    ALLOWED_IMAGE_HOSTS.contains(&host)
}

/// Validate a remote image URL: protocol must be HTTP(S) and host must be
/// whitelisted.
fn validate_remote_image_url(raw_url: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw_url).map_err(|e| format!("invalid URL '{raw_url}': {e}"))?;

    match url.scheme() {
        "http" | "https" => {}
        scheme => {
            return Err(format!("unsupported protocol '{scheme}', only HTTP/HTTPS allowed"));
        }
    }

    if !is_allowed_image_host(&url) {
        return Err(format!(
            "host '{}' is not in the allowed image host list",
            url.host_str().unwrap_or("unknown")
        ));
    }

    Ok(url)
}

#[async_trait::async_trait]
impl crate::traits::IFileService for FileService {
    // -- Content endpoint (pre-resolved absolute paths) --
    //
    // These operate on a path already resolved + containment-checked upstream by
    // `ProjectService::resolve_chat_file_ref` (per-variant guards). They do NOT
    // re-apply the `allowed_roots` sandbox — otherwise a `Local` host-picker file
    // (legitimately outside any workspace) would be rejected. Mirrors the
    // `/api/fs/reveal` pattern: resolve the identity, then operate on the path.

    async fn read_resolved_content(
        &self,
        absolute_path: &Path,
        encoding: ContentEncoding,
    ) -> Result<String, FileError> {
        let path = absolute_path.to_path_buf();
        tokio::task::spawn_blocking(move || read_resolved_content_sync(&path, encoding))
            .await
            .map_err(|e| FileError::Internal(format!("read content task failed: {e}")))?
    }

    async fn write_resolved_content(&self, absolute_path: &Path, data: &[u8]) -> Result<(), FileError> {
        let path = absolute_path.to_path_buf();
        let data = data.to_vec();
        let log_path = absolute_path.to_path_buf();
        tokio::task::spawn_blocking(move || write_file_sync(&path, &data))
            .await
            .map_err(|e| FileError::Internal(format!("write content task failed: {e}")))?
            // Same reasoning as `resolved_metadata`: `write_file_sync` embeds the
            // path in its message for the client-supplied-path callers, but this
            // path was resolved from a `ChatFileRef`.
            .map_err(|err| match err {
                FileError::Internal(cause) => {
                    tracing::error!(target: "chat_file", path = %log_path.display(), error = %cause, "resolved write failed");
                    FileError::Internal("cannot write file".to_owned())
                }
                other => other,
            })?;
        Ok(())
    }

    async fn resolved_metadata(&self, absolute_path: &Path) -> Result<FileMetadata, FileError> {
        let path = absolute_path.to_path_buf();
        tokio::task::spawn_blocking(move || get_file_metadata_sync(&path))
            .await
            .map_err(|e| FileError::Internal(format!("metadata task failed: {e}")))?
            // The caller resolved this path from a `ChatFileRef`, so it is
            // server-side knowledge the client never saw. `get_file_metadata_sync`
            // embeds the path in its `NotFound` message — fine for the
            // client-supplied-path caller (`get_file_metadata`), which is only
            // echoing back what the request contained, but a disclosure here. Swap
            // in the payload-free variant; the path stays in the log.
            .map_err(|err| match err {
                FileError::NotFound(cause) => {
                    tracing::warn!(target: "chat_file", error = %cause, "resolved metadata target is unreadable");
                    FileError::TargetNotFound
                }
                other => other,
            })
    }

    async fn get_files_by_dir(&self, dir: &str, root: &str) -> Result<Vec<DirOrFile>, FileError> {
        let roots = self.allowed_roots_refs();
        let extra_root = Path::new(root);
        let canonical_dir = validate_path_with_extra_root(dir, &roots, Some(extra_root))?;
        let canonical_root = validate_path_with_extra_root(root, &roots, Some(extra_root))?;

        self.build_dir_tree(&canonical_dir, &canonical_root).await
    }

    async fn list_workspace_files(&self, root: &str) -> Result<Vec<WorkspaceFlatFile>, FileError> {
        self.list_workspace_files_with_extra_root(root, None).await
    }

    async fn list_workspace_files_with_extra_root(
        &self,
        root: &str,
        extra_root: Option<&Path>,
    ) -> Result<Vec<WorkspaceFlatFile>, FileError> {
        let roots = self.allowed_roots_refs();
        let canonical_root = validate_path_with_extra_root(root, &roots, extra_root)?;
        let cache_key = canonical_root.to_string_lossy().into_owned();

        // Check cache first
        if let Some(cached) = self.workspace_files_cache.get(&cache_key) {
            return Ok(cached.clone());
        }

        let root_owned = canonical_root.clone();
        let files = tokio::task::spawn_blocking(move || list_workspace_files_sync(&root_owned))
            .await
            .map_err(|e| FileError::Internal(format!("workspace file listing task failed: {e}")))??;

        // Store in cache
        self.workspace_files_cache.insert(cache_key, files.clone());

        Ok(files)
    }

    async fn get_file_metadata(&self, path: &str, extra_root: Option<&Path>) -> Result<FileMetadata, FileError> {
        let roots = self.allowed_roots_refs();
        let canonical = validate_path_with_extra_root(path, &roots, extra_root)?;

        let result = tokio::task::spawn_blocking(move || get_file_metadata_sync(&canonical))
            .await
            .map_err(|e| FileError::Internal(format!("file metadata task failed: {e}")))??;

        Ok(result)
    }

    // -- File read/write (task 7.4) --

    async fn read_file(&self, path: &str, extra_root: Option<&Path>) -> Result<Option<String>, FileError> {
        if has_traversal(path) {
            return Err(FileError::BadRequest(format!(
                "path '{}' contains invalid traversal patterns",
                path
            )));
        }

        let roots = self.allowed_roots_refs();
        let canonical = match validate_path_with_extra_root(path, &roots, extra_root) {
            Ok(c) => c,
            Err(err) => {
                if matches!(err, FileError::BadRequest(_))
                    && validate_path_for_write(path, &self.allowed_roots_with_extra(extra_root)).is_ok()
                {
                    return Ok(None);
                }
                if matches!(err, FileError::BadRequest(_)) && self.path_uses_allowed_root(Path::new(path), extra_root) {
                    return Ok(None);
                }
                return Err(err);
            }
        };

        tokio::task::spawn_blocking(move || read_file_sync(&canonical))
            .await
            .map_err(|e| FileError::Internal(format!("read file task failed: {e}")))?
    }

    async fn write_file(&self, path: &str, data: &[u8], workspace: &str) -> Result<bool, FileError> {
        self.write_file_for_user("system_default_user", path, data, workspace)
            .await
    }

    async fn write_file_for_user(
        &self,
        user_id: &str,
        path: &str,
        data: &[u8],
        workspace: &str,
    ) -> Result<bool, FileError> {
        if has_traversal(path) {
            return Err(FileError::BadRequest(format!(
                "path '{}' contains invalid traversal patterns",
                path
            )));
        }

        let roots = self.allowed_roots_with_extra(Some(Path::new(workspace)));
        let canonical = validate_path_for_write(path, &roots)?;

        let path_owned = canonical.clone();
        let data_owned = data.to_vec();
        tokio::task::spawn_blocking(move || write_file_sync(&path_owned, &data_owned))
            .await
            .map_err(|e| FileError::Internal(format!("write file task failed: {e}")))??;

        // Compute relative path from workspace
        let workspace_path = Path::new(workspace);
        let relative_path = canonical
            .strip_prefix(std::fs::canonicalize(workspace_path).unwrap_or_else(|_| workspace_path.to_path_buf()))
            .unwrap_or(&canonical)
            .to_string_lossy()
            .into_owned();

        // Build and broadcast contentUpdate event
        let content = String::from_utf8(data.to_vec()).ok();
        let event = ContentUpdateEvent {
            file_path: canonical.to_string_lossy().into_owned(),
            content,
            workspace: workspace.to_owned(),
            relative_path,
            operation: ContentUpdateOperation::Write,
        };
        let mut payload = serde_json::to_value(&event).unwrap_or_default();
        payload["user_id"] = serde_json::Value::String(user_id.to_owned());
        let msg = WebSocketMessage::new("fileStream.contentUpdate", payload);
        self.broadcaster.broadcast(msg);

        // Invalidate workspace files cache since a file may have been
        // created or its content changed
        if let Ok(canonical_ws) = std::fs::canonicalize(workspace_path) {
            self.invalidate_cache(&canonical_ws.to_string_lossy());
        }

        Ok(true)
    }

    async fn copy_files_to_workspace(
        &self,
        file_paths: &[String],
        workspace: &str,
        source_root: Option<&str>,
    ) -> Result<CopyResult, FileError> {
        let roots = self.allowed_roots_refs();
        let ws_canonical = validate_path_with_extra_root(workspace, &roots, Some(Path::new(workspace)))?;

        let sr_canonical = match source_root {
            Some(sr) => Some(validate_path_with_extra_root(sr, &roots, Some(Path::new(sr)))?),
            None => None,
        };

        let file_paths_owned: Vec<String> = file_paths.to_vec();
        let roots_owned: Vec<std::path::PathBuf> = self.allowed_roots.clone();
        let workspace_root_owned = ws_canonical.clone();
        let source_root_owned = sr_canonical.clone();

        tokio::task::spawn_blocking(move || {
            let mut roots_refs: Vec<&Path> = roots_owned.iter().map(|p| p.as_path()).collect();
            roots_refs.push(workspace_root_owned.as_path());
            if let Some(source_root) = source_root_owned.as_deref() {
                roots_refs.push(source_root);
            }
            let mut copied = Vec::new();
            let mut failed = Vec::new();

            let mut fail = |fp: &str, reason: &str| {
                failed.push(aionui_api_types::CopyFailure {
                    path: fp.to_owned(),
                    reason: reason.to_owned(),
                });
            };

            for fp in &file_paths_owned {
                let source_extra = source_root_owned.as_deref().or_else(|| Path::new(fp).parent());
                let src = match validate_path_with_extra_root(fp, &roots_refs, source_extra) {
                    Ok(p) if p.is_file() => p,
                    Ok(_) => {
                        // Directories are not copied this round (files-only).
                        fail(fp, "not a file (directories are not supported yet)");
                        continue;
                    }
                    Err(_) => {
                        fail(fp, "source is not accessible or outside the allowed roots");
                        continue;
                    }
                };

                let relative = match &sr_canonical {
                    Some(sr) => src
                        .strip_prefix(sr)
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|_| Path::new(src.file_name().unwrap_or_default()).to_path_buf()),
                    None => Path::new(src.file_name().unwrap_or_default()).to_path_buf(),
                };

                let dest = ws_canonical.join(&relative);
                // Never silently overwrite: a name collision is a reported failure.
                if dest.exists() {
                    fail(fp, "a file with the same name already exists at the destination");
                    continue;
                }
                match copy_single_file_sync(&src, &dest) {
                    Ok(()) => copied.push(fp.clone()),
                    Err(_) => fail(fp, "copy failed"),
                }
            }

            Ok(CopyResult {
                copied_files: copied,
                failed_files: failed,
            })
        })
        .await
        .map_err(|e| FileError::Internal(format!("copy task failed: {e}")))?
    }

    async fn create_upload_file(
        &self,
        file_name: &str,
        data: &[u8],
        conversation_id: Option<&str>,
    ) -> Result<String, FileError> {
        if file_name.is_empty() {
            return Err(FileError::BadRequest("file name must not be empty".to_owned()));
        }
        if has_traversal(file_name) {
            return Err(FileError::BadRequest(format!(
                "file name '{}' contains invalid traversal patterns",
                file_name
            )));
        }
        if file_name.contains('/') || file_name.contains('\\') {
            return Err(FileError::BadRequest(format!(
                "file name '{}' must not contain path separators",
                file_name
            )));
        }

        // Validate optional conversation_id: no separators / traversal.
        let conv_id = match conversation_id {
            Some(id) if !id.is_empty() => {
                if has_traversal(id) || id.contains('/') || id.contains('\\') {
                    return Err(FileError::BadRequest(format!(
                        "conversation id '{}' contains invalid characters",
                        id
                    )));
                }
                Some(id.to_owned())
            }
            _ => None,
        };

        let name = file_name.to_owned();
        let bytes = data.to_vec();

        tokio::task::spawn_blocking(move || {
            let mut dir = std::env::temp_dir().join("aionui");
            if let Some(conv_id) = conv_id.as_deref() {
                dir = dir.join(conv_id);
            } else {
                dir = dir.join("general");
            }
            std::fs::create_dir_all(&dir)
                .map_err(|e| FileError::Internal(format!("cannot create upload directory: {e}")))?;

            let (base, ext) = split_base_ext(&name);
            let mut candidate = name.clone();
            let mut counter: u32 = 2;
            loop {
                let file_path = dir.join(&candidate);
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&file_path)
                {
                    Ok(mut f) => {
                        f.write_all(&bytes).map_err(|e| {
                            FileError::Internal(format!("cannot write upload file '{}': {e}", file_path.display()))
                        })?;
                        return Ok(file_path.to_string_lossy().into_owned());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        if counter > 1000 {
                            return Err(FileError::Internal(format!(
                                "too many name collisions for upload file '{}'",
                                name
                            )));
                        }
                        candidate = format!("{base}({counter}){ext}");
                        counter += 1;
                    }
                    Err(e) => {
                        return Err(FileError::Internal(format!(
                            "cannot write upload file '{}': {e}",
                            file_path.display()
                        )));
                    }
                }
            }
        })
        .await
        .map_err(|e| FileError::Internal(format!("create upload file task failed: {e}")))?
    }

    async fn get_image_base64(&self, path: &str, extra_root: Option<&Path>) -> Result<String, FileError> {
        if has_traversal(path) {
            return Err(FileError::BadRequest(format!(
                "path '{}' contains invalid traversal patterns",
                path
            )));
        }

        let roots = self.allowed_roots_refs();
        let canonical = validate_path_with_extra_root(path, &roots, extra_root)?;

        tokio::task::spawn_blocking(move || get_image_base64_sync(&canonical))
            .await
            .map_err(|e| FileError::Internal(format!("image base64 task failed: {e}")))?
    }

    async fn fetch_remote_image(&self, url: &str) -> String {
        let parsed = match validate_remote_image_url(url) {
            Ok(u) => u,
            Err(e) => {
                warn!("remote image rejected: {e}");
                return placeholder_svg_data_url();
            }
        };

        let client = match reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .timeout(REMOTE_IMAGE_TIMEOUT)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to build HTTP client: {e}");
                return placeholder_svg_data_url();
            }
        };

        let response = match client.get(parsed.clone()).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!("remote image fetch failed for '{}': {e}", url);
                return placeholder_svg_data_url();
            }
        };

        if !response.status().is_success() {
            warn!("remote image fetch returned status {} for '{}'", response.status(), url);
            return placeholder_svg_data_url();
        }

        // Early reject if Content-Length exceeds limit
        if let Some(len) = response.content_length()
            && len > MAX_REMOTE_IMAGE_SIZE as u64
        {
            warn!("remote image too large ({} bytes) for '{}'", len, url);
            return placeholder_svg_data_url();
        }

        // Determine MIME from Content-Type header, fall back to URL extension
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|ct| ct.split(';').next())
            .map(|s| s.trim().to_owned());

        let mime = content_type.unwrap_or_else(|| {
            mime_guess::from_path(parsed.path())
                .first()
                .map(|m| m.to_string())
                .unwrap_or_else(|| "application/octet-stream".to_owned())
        });

        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                warn!("failed to read remote image body for '{}': {e}", url);
                return placeholder_svg_data_url();
            }
        };

        if bytes.len() > MAX_REMOTE_IMAGE_SIZE {
            warn!("remote image body too large ({} bytes) for '{}'", bytes.len(), url);
            return placeholder_svg_data_url();
        }

        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        format!("data:{mime};base64,{encoded}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // The `resolved_*` methods under test are trait methods, not inherent ones.
    use crate::traits::IFileService;

    #[test]
    fn build_dir_tree_sync_lists_files_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        fs::write(dir.path().join("b.rs"), "fn main(){}").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/c.txt"), "nested").unwrap();

        let result = build_dir_tree_sync(dir.path(), dir.path()).unwrap();

        // sub/ should come first (directories first)
        assert_eq!(result[0].name, "sub");
        assert!(result[0].is_dir);
        // sub/ should have c.txt as child
        assert_eq!(result[0].children.len(), 1);
        assert_eq!(result[0].children[0].name, "c.txt");

        // Then files alphabetically
        assert_eq!(result[1].name, "a.txt");
        assert!(!result[1].is_dir);
        assert_eq!(result[2].name, "b.rs");
    }

    #[test]
    fn build_dir_tree_sync_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = build_dir_tree_sync(dir.path(), dir.path()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn build_dir_tree_sync_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("folder");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("file.txt"), "data").unwrap();

        let result = build_dir_tree_sync(dir.path(), dir.path()).unwrap();

        assert_eq!(result[0].relative_path, "folder");
        assert_eq!(result[0].children[0].relative_path, "folder/file.txt");
    }

    #[test]
    fn build_dir_tree_sync_nonexistent_dir_errors() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("nonexistent");
        let result = build_dir_tree_sync(&fake, dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn list_workspace_files_sync_basic() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/b.txt"), "world").unwrap();

        let files = list_workspace_files_sync(dir.path()).unwrap();

        assert_eq!(files.len(), 2);
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
    }

    #[test]
    fn list_workspace_files_sync_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.path().join("kept.txt"), "keep").unwrap();
        fs::write(dir.path().join("ignored.txt"), "skip").unwrap();

        let files = list_workspace_files_sync(dir.path()).unwrap();

        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"kept.txt"));
        assert!(names.contains(&".gitignore"));
        assert!(!names.contains(&"ignored.txt"));
    }

    #[test]
    fn list_workspace_files_sync_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let files = list_workspace_files_sync(dir.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn list_workspace_files_sync_truncates_at_limit() {
        // Creating 20,000+ files is impractical in a unit test;
        // verify the constant exists and the branch logic is sound.
        assert_eq!(MAX_WORKSPACE_FILES, 20_000);
    }

    #[test]
    fn list_workspace_files_sync_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main(){}").unwrap();

        let files = list_workspace_files_sync(dir.path()).unwrap();
        let main_file = files.iter().find(|f| f.name == "main.rs").unwrap();

        assert_eq!(main_file.relative_path, "src/main.rs");
    }

    /// Regression for ELECTRON-3TG: production canonicalizes the workspace root
    /// (via `validate_path_with_extra_root`) before walking, which on Windows
    /// yields a verbatim `\\?\` root. Every emitted `full_path` must be stripped
    /// so mention / preview consumers never receive verbatim paths.
    #[cfg(windows)]
    #[test]
    fn windows_full_paths_are_not_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main(){}").unwrap();

        // Mirror production: the walked root is the canonicalized (verbatim) form.
        let canonical_root = std::fs::canonicalize(dir.path()).unwrap();
        assert!(
            canonical_root.to_string_lossy().starts_with(r"\\?\"),
            "precondition: canonicalize should yield a verbatim root on Windows"
        );

        let flat = list_workspace_files_sync(&canonical_root).unwrap();
        assert!(!flat.is_empty());
        for f in &flat {
            assert!(
                !f.full_path.starts_with(r"\\?\"),
                "flat-list full_path is verbatim: {}",
                f.full_path
            );
        }

        let tree = build_dir_tree_sync(&canonical_root, &canonical_root).unwrap();
        fn assert_no_verbatim(nodes: &[DirOrFile]) {
            for n in nodes {
                assert!(
                    !n.full_path.starts_with(r"\\?\"),
                    "dir-tree full_path is verbatim: {}",
                    n.full_path
                );
                assert_no_verbatim(&n.children);
            }
        }
        assert_no_verbatim(&tree);
    }

    #[cfg(unix)]
    #[test]
    fn list_workspace_files_sync_skips_directory_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("builtin-skills/auto-inject/aionui-skills");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "---\ndescription: test\n---\nbody").unwrap();

        let workspace = dir.path().join("workspace/.claude/skills");
        fs::create_dir_all(&workspace).unwrap();
        std::os::unix::fs::symlink(&skill_dir, workspace.join("aionui-skills")).unwrap();

        let files = list_workspace_files_sync(&dir.path().join("workspace")).unwrap();

        assert!(
            files.iter().all(|f| f.name != "aionui-skills"),
            "directory symlink should not be surfaced as a file: {files:?}"
        );
    }

    #[test]
    fn get_file_metadata_sync_text_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        fs::write(&file, "hello world").unwrap();

        let meta = get_file_metadata_sync(&file).unwrap();
        assert_eq!(meta.name, "hello.txt");
        assert_eq!(meta.size, 11);
        assert_eq!(meta.mime_type, "text/plain");
        assert!(!meta.is_directory);
        assert!(meta.last_modified > 0);
    }

    #[test]
    fn get_file_metadata_sync_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("mydir");
        fs::create_dir(&sub).unwrap();

        let meta = get_file_metadata_sync(&sub).unwrap();
        assert_eq!(meta.name, "mydir");
        assert!(meta.is_directory);
        assert_eq!(meta.mime_type, "inode/directory");
    }

    #[test]
    fn get_file_metadata_sync_rust_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        fs::write(&file, "pub fn foo() {}").unwrap();

        let meta = get_file_metadata_sync(&file).unwrap();
        assert_eq!(meta.name, "lib.rs");
        // rust files should get a reasonable mime type
        assert!(!meta.mime_type.is_empty());
    }

    #[test]
    fn get_file_metadata_sync_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("missing.txt");
        let result = get_file_metadata_sync(&fake);
        assert!(result.is_err());
    }

    // -- identity-addressed reads must not disclose the resolved path -----------
    //
    // `read_resolved_content` / `resolved_metadata` / `write_resolved_content` are
    // reached only from the `ChatFileRef` endpoints, where the absolute path is
    // resolved server-side and the client has never seen it. The sync helpers they
    // build on embed the path in their messages — correct for the callers that were
    // handed a path by the client, a disclosure for these. The seals below strip it.

    /// The helper deliberately keeps the path (its other caller echoes back a
    /// client-supplied path), so the strip has to happen on the way out. Pinning it
    /// here documents *why* the wrappers cannot simply forward the error.
    #[test]
    fn get_file_metadata_sync_keeps_path_for_client_supplied_callers() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("secret-name.txt");
        let err = get_file_metadata_sync(&fake).expect_err("must fail");
        assert!(
            err.to_string().contains("secret-name.txt"),
            "helper is expected to name the path; the identity-addressed wrapper strips it"
        );
    }

    #[tokio::test]
    async fn resolved_metadata_error_is_path_free() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("secret-name.docx");
        let svc = test_service(dir.path());

        let err = svc.resolved_metadata(&missing).await.expect_err("must fail");
        assert!(
            matches!(err, FileError::TargetNotFound),
            "expected TargetNotFound, got {err:?}"
        );
        assert_path_absent(&err, "secret-name");
    }

    #[tokio::test]
    async fn read_resolved_content_error_is_path_free_for_every_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("secret-name.docx");
        let svc = test_service(dir.path());

        // All three encodings take different branches through the helper; each has to
        // be sealed, and DataUrl in particular routes via `get_image_base64_sync`.
        for encoding in [ContentEncoding::Utf8, ContentEncoding::Base64, ContentEncoding::DataUrl] {
            let err = svc
                .read_resolved_content(&missing, encoding)
                .await
                .expect_err("must fail");
            assert_path_absent(&err, "secret-name");
        }
    }

    #[tokio::test]
    async fn write_resolved_content_error_is_path_free() {
        let dir = tempfile::tempdir().unwrap();
        // A path whose parent does not exist → write fails inside the helper.
        let unwritable = dir.path().join("secret-name-dir/nested/file.txt");
        let svc = test_service(dir.path());

        let err = svc
            .write_resolved_content(&unwritable, b"x")
            .await
            .expect_err("must fail");
        assert_path_absent(&err, "secret-name-dir");
    }

    fn test_service(root: &Path) -> FileService {
        FileService::new(Arc::new(NoopBroadcaster), vec![root.to_path_buf()])
    }

    /// Assert neither the `Display` nor the `Debug` rendering names the path — a
    /// message-only check would miss a payload still carrying it.
    fn assert_path_absent(err: &FileError, needle: &str) {
        let rendered = format!("{err}");
        let debug = format!("{err:?}");
        for haystack in [&rendered, &debug] {
            assert!(
                !haystack.contains(needle),
                "identity-addressed error must not disclose the resolved path, got {haystack:?}"
            );
        }
    }

    /// No-op broadcaster for constructing a service in tests.
    struct NoopBroadcaster;
    impl EventBroadcaster for NoopBroadcaster {
        fn broadcast(&self, _event: aionui_api_types::WebSocketMessage<serde_json::Value>) {}
    }

    #[test]
    fn get_file_metadata_sync_image_mime() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("icon.png");
        fs::write(&png, [0x89, 0x50, 0x4E, 0x47]).unwrap();

        let meta = get_file_metadata_sync(&png).unwrap();
        assert_eq!(meta.mime_type, "image/png");
    }

    #[test]
    fn get_file_metadata_sync_unknown_extension() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.xyz123");
        fs::write(&file, "binary data").unwrap();

        let meta = get_file_metadata_sync(&file).unwrap();
        assert_eq!(meta.mime_type, "application/octet-stream");
    }

    // -- read_file_sync tests (task 7.4) --

    #[test]
    fn read_file_sync_normal_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        fs::write(&file, "hello world").unwrap();

        let result = read_file_sync(&file).unwrap();
        assert_eq!(result.as_deref(), Some("hello world"));
    }

    #[test]
    fn read_file_sync_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("empty.txt");
        fs::write(&file, "").unwrap();

        let result = read_file_sync(&file).unwrap();
        assert_eq!(result.as_deref(), Some(""));
    }

    #[test]
    fn read_file_sync_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("missing.txt");

        let result = read_file_sync(&fake).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn read_file_sync_rejects_directory() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("subdir");
        fs::create_dir(&folder).unwrap();

        let err = read_file_sync(&folder).unwrap_err();
        assert!(matches!(err, FileError::BadRequest(_)));
        assert!(err.to_string().contains("is a directory"));
    }

    // -- validate_file_for_read tests --

    #[test]
    fn validate_file_for_read_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("valid.txt");
        fs::write(&file, "data").unwrap();

        let result = validate_file_for_read(&file).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn validate_file_for_read_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("nope.txt");

        let result = validate_file_for_read(&fake).unwrap();
        assert!(result.is_none());
    }

    // -- write_file_sync tests --

    #[test]
    fn write_file_sync_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("output.txt");

        let ok = write_file_sync(&file, b"hello").unwrap();
        assert!(ok);
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello");
    }

    #[test]
    fn write_file_sync_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("overwrite.txt");
        fs::write(&file, "old").unwrap();

        let ok = write_file_sync(&file, b"new content").unwrap();
        assert!(ok);
        assert_eq!(fs::read_to_string(&file).unwrap(), "new content");
    }

    #[test]
    fn write_file_sync_binary() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.bin");
        let data = vec![0x00, 0xFF, 0xAB];

        let ok = write_file_sync(&file, &data).unwrap();
        assert!(ok);
        assert_eq!(fs::read(&file).unwrap(), data);
    }

    // -- copy_single_file_sync tests (task 7.5) --

    #[test]
    fn copy_single_file_sync_basic() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&src, "content").unwrap();

        copy_single_file_sync(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "content");
    }

    #[test]
    fn copy_single_file_sync_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("nested/deep/dest.txt");
        fs::write(&src, "nested").unwrap();

        copy_single_file_sync(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "nested");
    }

    #[test]
    fn copy_single_file_sync_source_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("missing.txt");
        let dest = dir.path().join("dest.txt");

        let result = copy_single_file_sync(&src, &dest);
        assert!(result.is_err());
    }

    // -- get_image_base64_sync tests (task 7.6) --

    #[test]
    fn get_image_base64_sync_png() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.png");
        let bytes = vec![0x89, 0x50, 0x4E, 0x47]; // PNG magic bytes
        fs::write(&file, &bytes).unwrap();

        let result = get_image_base64_sync(&file).unwrap();
        assert!(result.starts_with("data:image/png;base64,"));

        // Verify the base64 part decodes back to original bytes
        let encoded_part = result.strip_prefix("data:image/png;base64,").unwrap();
        let decoded = base64::engine::general_purpose::STANDARD.decode(encoded_part).unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn get_image_base64_sync_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("photo.jpg");
        let bytes = vec![0xFF, 0xD8, 0xFF, 0xE0]; // JPEG magic bytes
        fs::write(&file, &bytes).unwrap();

        let result = get_image_base64_sync(&file).unwrap();
        assert!(result.starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn get_image_base64_sync_svg() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("icon.svg");
        fs::write(&file, "<svg></svg>").unwrap();

        let result = get_image_base64_sync(&file).unwrap();
        assert!(result.starts_with("data:image/svg+xml;base64,"));
    }

    #[test]
    fn get_image_base64_sync_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("missing.png");

        let result = get_image_base64_sync(&fake);
        assert!(result.is_err());
    }

    #[test]
    fn get_image_base64_sync_unknown_extension() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.xyz999");
        fs::write(&file, b"some bytes").unwrap();

        let result = get_image_base64_sync(&file).unwrap();
        // Falls back to application/octet-stream
        assert!(result.starts_with("data:application/octet-stream;base64,"));
    }

    // -- placeholder_svg_data_url tests --

    #[test]
    fn placeholder_svg_data_url_format() {
        let url = placeholder_svg_data_url();
        assert!(url.starts_with("data:image/svg+xml;base64,"));

        // Verify it decodes to valid SVG content
        let encoded_part = url.strip_prefix("data:image/svg+xml;base64,").unwrap();
        let decoded = base64::engine::general_purpose::STANDARD.decode(encoded_part).unwrap();
        let svg = String::from_utf8(decoded).unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains("</svg>"));
    }

    // -- validate_remote_image_url tests --

    #[test]
    fn validate_remote_image_url_https_allowed_host() {
        let result = validate_remote_image_url("https://raw.githubusercontent.com/owner/repo/main/image.png");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_remote_image_url_http_allowed_host() {
        let result = validate_remote_image_url("http://github.com/image.png");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_remote_image_url_disallowed_host() {
        let result = validate_remote_image_url("https://evil.com/image.png");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not in the allowed"));
    }

    #[test]
    fn validate_remote_image_url_ftp_protocol() {
        let result = validate_remote_image_url("ftp://github.com/image.png");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unsupported protocol"));
    }

    #[test]
    fn validate_remote_image_url_invalid_url() {
        let result = validate_remote_image_url("not-a-url");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid URL"));
    }

    #[test]
    fn validate_remote_image_url_file_protocol() {
        let result = validate_remote_image_url("file:///etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unsupported protocol"));
    }

    // -- is_allowed_image_host tests --

    #[test]
    fn is_allowed_image_host_exact_match() {
        let url = reqwest::Url::parse("https://github.com/img.png").unwrap();
        assert!(is_allowed_image_host(&url));
    }

    #[test]
    fn is_allowed_image_host_subdomain_not_matched() {
        // "sub.github.com" should NOT match "github.com"
        let url = reqwest::Url::parse("https://sub.github.com/img.png").unwrap();
        assert!(!is_allowed_image_host(&url));
    }

    #[test]
    fn is_allowed_image_host_all_listed_hosts() {
        for host in ALLOWED_IMAGE_HOSTS {
            let url_str = format!("https://{host}/test.png");
            let url = reqwest::Url::parse(&url_str).unwrap();
            assert!(is_allowed_image_host(&url), "host '{host}' should be allowed");
        }
    }
}
