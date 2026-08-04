use aionui_common::FileChangeOperation;
use serde::{Deserialize, Serialize};

use crate::chat_file::ChatFileRef;

// ---------------------------------------------------------------------------
// Content endpoint (ChatFileRef identity) — Request DTOs
// ---------------------------------------------------------------------------

/// How `POST /api/fs/content` encodes the returned file content.
///
/// Mirrors the WS `fs/read` encoding split: `utf8` for text, `base64`/`dataurl`
/// for binary (dataurl prepends a guessed `data:<mime>;base64,` prefix).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentEncoding {
    /// UTF-8 text (fails on non-UTF-8 input); returned as the raw string.
    #[default]
    Utf8,
    /// Raw bytes, base64-encoded, no data-URL prefix.
    Base64,
    /// Base64 data URL with a guessed MIME type: `data:<mime>;base64,<...>`.
    DataUrl,
}

/// Request body for `POST /api/fs/content` — read a file addressed by
/// [`ChatFileRef`] identity (collapses the old `read` + `image-base64`).
#[derive(Debug, Deserialize)]
pub struct ReadContentRequest {
    pub file: ChatFileRef,
    #[serde(default)]
    pub encoding: ContentEncoding,
}

/// Request body for `PUT /api/fs/content` — write a file addressed by
/// [`ChatFileRef`] identity. Optimistic-concurrency `If-Match` (last-modified
/// ms) travels in the request header, not this body.
#[derive(Debug, Deserialize)]
pub struct WriteContentRequest {
    pub file: ChatFileRef,
    pub data: String,
}

/// Request body for `POST /api/fs/content/metadata` — metadata for a file
/// addressed by [`ChatFileRef`] identity.
#[derive(Debug, Deserialize)]
pub struct ContentMetadataRequest {
    pub file: ChatFileRef,
}

/// Query parameters for `GET /api/fs/stream` — a flattened [`ChatFileRef`].
///
/// The stream endpoint is a raw byte range server for `<webview src>` / `<embed>`
/// (pdf), which can only issue a GET with no body, so the identity travels in the
/// query string. `kind` selects the variant; the other fields carry its payload.
#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    pub kind: String,
    #[serde(default)]
    pub pe_id: Option<String>,
    #[serde(default)]
    pub relative_path: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

impl StreamQuery {
    /// Rebuild the [`ChatFileRef`] from the flattened query, or return a message
    /// naming the missing/invalid field.
    pub fn to_chat_file_ref(&self) -> Result<ChatFileRef, &'static str> {
        match self.kind.as_str() {
            "project" => match (self.pe_id.clone(), self.relative_path.clone()) {
                (Some(pe_id), Some(relative_path)) => Ok(ChatFileRef::Project { pe_id, relative_path }),
                _ => Err("project stream requires pe_id and relative_path"),
            },
            "upload" => self
                .path
                .clone()
                .map(|path| ChatFileRef::Upload { path })
                .ok_or("upload stream requires path"),
            "local" => self
                .path
                .clone()
                .map(|path| ChatFileRef::Local { path })
                .ok_or("local stream requires path"),
            _ => Err("unknown stream kind (expected project|upload|local)"),
        }
    }
}

// ---------------------------------------------------------------------------
// A. Core file operations — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /api/fs/dir` — get files by directory.
#[derive(Debug, Deserialize)]
pub struct GetFilesByDirRequest {
    pub dir: String,
    pub root: String,
}

/// Request body for `POST /api/fs/list` — list workspace files.
#[derive(Debug, Deserialize)]
pub struct ListWorkspaceFilesRequest {
    pub root: String,
}

/// Request body for `POST /api/fs/metadata` — get file metadata.
#[derive(Debug, Deserialize)]
pub struct GetFileMetadataRequest {
    pub path: String,
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Request body for `POST /api/fs/read` — read file.
#[derive(Debug, Deserialize)]
pub struct ReadFileRequest {
    pub path: String,
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Request body for `POST /api/fs/write` — write file.
#[derive(Debug, Deserialize)]
pub struct WriteFileRequest {
    pub path: String,
    pub data: String,
    /// Workspace root, used to compute `relativePath` in the
    /// `fileStream.contentUpdate` event.  Falls back to the file's
    /// parent directory when absent.
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Copy destination, addressed by explorer identity (a project folder + a
/// relative subdirectory). The backend resolves it to an absolute directory
/// via `resolve_reference`; device file paths are copied into it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CopyTarget {
    pub pe_id: String,
    /// Relative directory under the pe's folder root (`""` = the root itself).
    pub relative_path: String,
}

/// Request body for `POST /api/fs/copy` — copy device files into a project
/// folder (add-to-chat "paste into workspace", pe-addressed).
#[derive(Debug, Deserialize)]
pub struct CopyFilesRequest {
    /// Absolute device paths of external OS files to copy in.
    pub file_paths: Vec<String>,
    pub target: CopyTarget,
    #[serde(default)]
    pub source_root: Option<String>,
}

/// Request body for `POST /api/fs/reveal` — reveal a pe-addressed file/dir in
/// the OS file manager ("open enclosing folder"). The backend resolves the
/// identity to an absolute path via `resolve_reference`, then hands it to the
/// shell reveal capability.
#[derive(Debug, Deserialize)]
pub struct RevealItemRequest {
    pub pe_id: String,
    /// Relative path under the pe's folder root (`""` = the root itself).
    pub relative_path: String,
}

/// Request body for `POST /api/fs/open-system` — open a `ChatFileRef`-addressed
/// file with the OS default application ("open in system editor"). Preview offers
/// this as the escape hatch for files it will not render itself (oversized or
/// unsupported formats).
///
/// Uses `ChatFileRef` rather than `{pe_id, relative_path}` so all three preview
/// sources are covered — project files, uploads, and host-picked local files —
/// whereas [`RevealItemRequest`] serves the project-only Explorer tree.
///
/// The response carries no body: the backend resolves the identity to an absolute
/// path, opens it locally, and never returns that path (see INV-OPEN on the
/// handler).
#[derive(Debug, Deserialize)]
pub struct OpenSystemFileRequest {
    pub file: ChatFileRef,
}

/// Request body for `POST /api/fs/image-base64` — get image as base64.
#[derive(Debug, Deserialize)]
pub struct GetImageBase64Request {
    pub path: String,
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Request body for `POST /api/fs/fetch-remote-image` — fetch remote image.
#[derive(Debug, Deserialize)]
pub struct FetchRemoteImageRequest {
    pub url: String,
}

// ---------------------------------------------------------------------------
// A. Core file operations — Response DTOs
// ---------------------------------------------------------------------------

/// A node in the directory tree returned by `getFilesByDir`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DirOrFileResponse {
    pub name: String,
    pub full_path: String,
    pub relative_path: String,
    pub is_dir: bool,
    pub is_file: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<DirOrFileResponse>>,
}

/// A flat file entry returned by `listWorkspaceFiles`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceFlatFileResponse {
    pub name: String,
    pub full_path: String,
    pub relative_path: String,
}

/// File metadata returned by `getFileMetadata`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadataResponse {
    pub name: String,
    pub path: String,
    pub size: u64,
    #[serde(rename = "type")]
    pub mime_type: String,
    pub last_modified: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_directory: Option<bool>,
}

/// One file that could not be copied, with a human-readable reason
/// (e.g. name collision — the backend never silently overwrites).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyFailure {
    pub path: String,
    pub reason: String,
}

/// Result of a batch copy operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyFilesResponse {
    pub copied_files: Vec<String>,
    pub failed_files: Vec<CopyFailure>,
}

// ---------------------------------------------------------------------------
// B. Workspace snapshot — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for snapshot init / getInfo / compare / stageAll / unstageAll / dispose.
#[derive(Debug, Deserialize)]
pub struct SnapshotWorkspaceRequest {
    pub workspace: String,
}

/// Request body for snapshot getBaselineContent.
#[derive(Debug, Deserialize)]
pub struct SnapshotBaselineRequest {
    pub workspace: String,
    pub file_path: String,
}

/// Request body for snapshot stageFile / unstageFile.
#[derive(Debug, Deserialize)]
pub struct SnapshotStageRequest {
    pub workspace: String,
    pub file_path: String,
}

/// Request body for snapshot discardFile / resetFile.
#[derive(Debug, Deserialize)]
pub struct SnapshotDiscardRequest {
    pub workspace: String,
    pub file_path: String,
    pub operation: FileChangeOperation,
}

// ---------------------------------------------------------------------------
// B. Workspace snapshot — Response DTOs
// ---------------------------------------------------------------------------

/// Snapshot mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotMode {
    GitRepo,
    Snapshot,
}

/// Information about a workspace snapshot.
///
/// API Spec: `branch: string | null` — always present in JSON output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotInfoResponse {
    pub mode: SnapshotMode,
    pub branch: Option<String>,
}

/// A single file change entry in a compare result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileChangeInfoResponse {
    pub file_path: String,
    pub relative_path: String,
    pub operation: FileChangeOperation,
}

/// Result of comparing workspace changes (staged vs unstaged).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotCompareResponse {
    pub staged: Vec<FileChangeInfoResponse>,
    pub unstaged: Vec<FileChangeInfoResponse>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- Request deserialization tests --

    #[test]
    fn get_files_by_dir_request_deserialization() {
        let raw = r#"{"dir":"/home/user/project","root":"/home/user"}"#;
        let req: GetFilesByDirRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.dir, "/home/user/project");
        assert_eq!(req.root, "/home/user");
    }

    #[test]
    fn list_workspace_files_request_requires_root() {
        let req: ListWorkspaceFilesRequest = serde_json::from_str(r#"{"root":"/workspace"}"#).unwrap();
        assert_eq!(req.root, "/workspace");

        let missing_root = serde_json::from_str::<ListWorkspaceFilesRequest>(r#"{}"#);
        assert!(missing_root.is_err());
    }

    #[test]
    fn copy_files_request_snake_case() {
        let raw = json!({
            "file_paths": ["/a.txt", "/b.txt"],
            "target": { "pe_id": "pe1", "relative_path": "sub" },
            "source_root": "/src"
        });
        let req: CopyFilesRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.file_paths, vec!["/a.txt", "/b.txt"]);
        assert_eq!(req.target.pe_id, "pe1");
        assert_eq!(req.target.relative_path, "sub");
        assert_eq!(req.source_root.as_deref(), Some("/src"));
    }

    #[test]
    fn copy_files_request_optional_source_root() {
        let raw = json!({
            "file_paths": ["/a.txt"],
            "target": { "pe_id": "pe1", "relative_path": "" }
        });
        let req: CopyFilesRequest = serde_json::from_value(raw).unwrap();
        assert!(req.source_root.is_none());
    }

    #[test]
    fn snapshot_discard_request_deserialization() {
        let raw = json!({
            "workspace": "/ws",
            "file_path": "src/main.rs",
            "operation": "modify"
        });
        let req: SnapshotDiscardRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.workspace, "/ws");
        assert_eq!(req.file_path, "src/main.rs");
        assert_eq!(req.operation, FileChangeOperation::Modify);
    }

    // -- Response serialization tests --

    #[test]
    fn dir_or_file_response_serialization() {
        let resp = DirOrFileResponse {
            name: "src".into(),
            full_path: "/project/src".into(),
            relative_path: "src".into(),
            is_dir: true,
            is_file: false,
            children: Some(vec![DirOrFileResponse {
                name: "main.rs".into(),
                full_path: "/project/src/main.rs".into(),
                relative_path: "src/main.rs".into(),
                is_dir: false,
                is_file: true,
                children: None,
            }]),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["name"], "src");
        assert_eq!(json["full_path"], "/project/src");
        assert_eq!(json["relative_path"], "src");
        assert_eq!(json["is_dir"], true);
        assert_eq!(json["is_file"], false);
        assert_eq!(json["children"][0]["name"], "main.rs");
    }

    #[test]
    fn dir_or_file_response_no_children_omitted() {
        let resp = DirOrFileResponse {
            name: "file.txt".into(),
            full_path: "/file.txt".into(),
            relative_path: "file.txt".into(),
            is_dir: false,
            is_file: true,
            children: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("children").is_none());
    }

    #[test]
    fn workspace_flat_file_response_serialization() {
        let resp = WorkspaceFlatFileResponse {
            name: "lib.rs".into(),
            full_path: "/project/src/lib.rs".into(),
            relative_path: "src/lib.rs".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["name"], "lib.rs");
        assert_eq!(json["full_path"], "/project/src/lib.rs");
        assert_eq!(json["relative_path"], "src/lib.rs");
    }

    #[test]
    fn file_metadata_response_serialization() {
        let resp = FileMetadataResponse {
            name: "readme.md".into(),
            path: "/project/readme.md".into(),
            size: 1024,
            mime_type: "text/markdown".into(),
            last_modified: 1700000000000,
            is_directory: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["name"], "readme.md");
        assert_eq!(json["path"], "/project/readme.md");
        assert_eq!(json["size"], 1024);
        assert_eq!(json["type"], "text/markdown");
        assert_eq!(json["last_modified"], 1700000000000_i64);
        assert!(json.get("is_directory").is_none());
    }

    #[test]
    fn file_metadata_response_with_directory_flag() {
        let resp = FileMetadataResponse {
            name: "src".into(),
            path: "/project/src".into(),
            size: 0,
            mime_type: "".into(),
            last_modified: 1700000000000,
            is_directory: Some(true),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["is_directory"], true);
    }

    #[test]
    fn copy_files_response_serialization() {
        let resp = CopyFilesResponse {
            copied_files: vec!["/ws/a.txt".into()],
            failed_files: vec![CopyFailure {
                path: "/missing.txt".into(),
                reason: "not a file".into(),
            }],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["copied_files"][0], "/ws/a.txt");
        assert_eq!(json["failed_files"][0]["path"], "/missing.txt");
        assert_eq!(json["failed_files"][0]["reason"], "not a file");
    }

    #[test]
    fn snapshot_mode_serialization() {
        assert_eq!(serde_json::to_value(SnapshotMode::GitRepo).unwrap(), "git-repo");
        assert_eq!(serde_json::to_value(SnapshotMode::Snapshot).unwrap(), "snapshot");
    }

    #[test]
    fn snapshot_mode_deserialization() {
        let mode: SnapshotMode = serde_json::from_str(r#""git-repo""#).unwrap();
        assert_eq!(mode, SnapshotMode::GitRepo);
        let mode: SnapshotMode = serde_json::from_str(r#""snapshot""#).unwrap();
        assert_eq!(mode, SnapshotMode::Snapshot);
    }

    #[test]
    fn snapshot_info_response_git_repo() {
        let resp = SnapshotInfoResponse {
            mode: SnapshotMode::GitRepo,
            branch: Some("main".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["mode"], "git-repo");
        assert_eq!(json["branch"], "main");
    }

    #[test]
    fn snapshot_info_response_snapshot_mode() {
        let resp = SnapshotInfoResponse {
            mode: SnapshotMode::Snapshot,
            branch: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["mode"], "snapshot");
        // API Spec: branch is always present, null when snapshot mode
        assert!(json["branch"].is_null());
    }

    #[test]
    fn snapshot_compare_response_serialization() {
        let resp = SnapshotCompareResponse {
            staged: vec![FileChangeInfoResponse {
                file_path: "/ws/a.txt".into(),
                relative_path: "a.txt".into(),
                operation: FileChangeOperation::Create,
            }],
            unstaged: vec![FileChangeInfoResponse {
                file_path: "/ws/b.txt".into(),
                relative_path: "b.txt".into(),
                operation: FileChangeOperation::Modify,
            }],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["staged"][0]["file_path"], "/ws/a.txt");
        assert_eq!(json["staged"][0]["relative_path"], "a.txt");
        assert_eq!(json["staged"][0]["operation"], "create");
        assert_eq!(json["unstaged"][0]["operation"], "modify");
    }

    #[test]
    fn snapshot_compare_response_deserialization() {
        let raw = json!({
            "staged": [
                { "file_path": "/ws/x.rs", "relative_path": "x.rs", "operation": "delete" }
            ],
            "unstaged": []
        });
        let resp: SnapshotCompareResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.staged.len(), 1);
        assert_eq!(resp.staged[0].operation, FileChangeOperation::Delete);
        assert!(resp.unstaged.is_empty());
    }
}
