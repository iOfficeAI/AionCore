//! Integration tests for file management operations (task 7.5).
//!
//! Covers `copy_files_to_workspace` through the `IFileService` trait,
//! including path validation.

use std::fs;
use std::sync::Arc;

use aionui_api_types::WebSocketMessage;
use aionui_file::{FileService, IFileService};
use aionui_realtime::EventBroadcaster;

// -----------------------------------------------------------------------
// Test helpers (shared with file_read_write.rs pattern)
// -----------------------------------------------------------------------

struct NoopBroadcaster;

impl EventBroadcaster for NoopBroadcaster {
    fn broadcast(&self, _event: WebSocketMessage<serde_json::Value>) {}
}

fn make_service(root: &std::path::Path) -> FileService {
    FileService::new(Arc::new(NoopBroadcaster), vec![root.to_path_buf()])
}

// -----------------------------------------------------------------------
// copyFilesToWorkspace
// -----------------------------------------------------------------------

#[tokio::test]
async fn copy_files_single_file() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = dir.path().join("src");
    let ws_dir = dir.path().join("ws");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(src_dir.join("a.txt"), "hello").unwrap();

    let svc = make_service(dir.path());
    let paths = vec![src_dir.join("a.txt").to_string_lossy().into_owned()];
    let result = svc
        .copy_files_to_workspace(&paths, ws_dir.to_str().unwrap(), None)
        .await
        .unwrap();

    assert_eq!(result.copied_files.len(), 1);
    assert!(result.failed_files.is_empty());
    // Without source_root, file should be at workspace root
    assert_eq!(fs::read_to_string(ws_dir.join("a.txt")).unwrap(), "hello");
}

#[tokio::test]
async fn copy_files_with_source_root_preserves_structure() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = dir.path().join("project");
    let ws_dir = dir.path().join("ws");
    fs::create_dir_all(src_dir.join("utils")).unwrap();
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(src_dir.join("utils/helper.ts"), "export {}").unwrap();
    fs::write(src_dir.join("index.ts"), "import {}").unwrap();

    let svc = make_service(dir.path());
    let paths = vec![
        src_dir.join("utils/helper.ts").to_string_lossy().into_owned(),
        src_dir.join("index.ts").to_string_lossy().into_owned(),
    ];

    let result = svc
        .copy_files_to_workspace(&paths, ws_dir.to_str().unwrap(), Some(src_dir.to_str().unwrap()))
        .await
        .unwrap();

    assert_eq!(result.copied_files.len(), 2);
    assert!(result.failed_files.is_empty());
    // Directory structure preserved relative to source_root
    assert_eq!(fs::read_to_string(ws_dir.join("utils/helper.ts")).unwrap(), "export {}");
    assert_eq!(fs::read_to_string(ws_dir.join("index.ts")).unwrap(), "import {}");
}

#[tokio::test]
async fn copy_files_partial_failure() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = dir.path().join("src");
    let ws_dir = dir.path().join("ws");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(src_dir.join("good.txt"), "ok").unwrap();

    let svc = make_service(dir.path());
    let paths = vec![
        src_dir.join("good.txt").to_string_lossy().into_owned(),
        src_dir.join("missing.txt").to_string_lossy().into_owned(),
    ];

    let result = svc
        .copy_files_to_workspace(&paths, ws_dir.to_str().unwrap(), None)
        .await
        .unwrap();

    assert_eq!(result.copied_files.len(), 1);
    assert_eq!(result.failed_files.len(), 1);
    assert!(result.failed_files[0].path.contains("missing.txt"));
}

#[tokio::test]
async fn copy_files_empty_list() {
    let dir = tempfile::tempdir().unwrap();
    let ws_dir = dir.path().join("ws");
    fs::create_dir_all(&ws_dir).unwrap();

    let svc = make_service(dir.path());
    let result = svc
        .copy_files_to_workspace(&[], ws_dir.to_str().unwrap(), None)
        .await
        .unwrap();

    assert!(result.copied_files.is_empty());
    assert!(result.failed_files.is_empty());
}

#[tokio::test]
async fn copy_files_directory_in_list_is_copied_recursively() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("subdir");
    let ws = dir.path().join("ws");
    fs::create_dir_all(sub.join("inner")).unwrap();
    fs::write(sub.join("top.txt"), "top").unwrap();
    fs::write(sub.join("inner/leaf.txt"), "leaf").unwrap();
    fs::create_dir_all(&ws).unwrap();

    let svc = make_service(dir.path());
    let paths = vec![sub.to_string_lossy().into_owned()];
    let result = svc
        .copy_files_to_workspace(&paths, ws.to_str().unwrap(), None)
        .await
        .unwrap();

    // Directories are now copied recursively (OS-external folder drop).
    assert_eq!(result.copied_files.len(), 1);
    assert!(result.failed_files.is_empty());
    assert_eq!(fs::read_to_string(ws.join("subdir/top.txt")).unwrap(), "top");
    assert_eq!(fs::read_to_string(ws.join("subdir/inner/leaf.txt")).unwrap(), "leaf");
}

#[tokio::test]
async fn copy_files_accepts_source_and_workspace_outside_sandbox() {
    let sandbox = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let ws = workspace.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::write(source.path().join("picked.txt"), "picked").unwrap();

    let svc = make_service(sandbox.path());
    let paths = vec![source.path().join("picked.txt").to_string_lossy().into_owned()];

    let result = svc
        .copy_files_to_workspace(&paths, ws.to_str().unwrap(), None)
        .await
        .unwrap();

    assert_eq!(result.copied_files.len(), 1);
    assert!(result.failed_files.is_empty());
    assert_eq!(fs::read_to_string(ws.join("picked.txt")).unwrap(), "picked");
}

#[tokio::test]
async fn copy_files_rejects_source_outside_explicit_source_root() {
    let sandbox = tempfile::tempdir().unwrap();
    let source_root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "secret").unwrap();

    let svc = make_service(sandbox.path());
    let paths = vec![outside.path().join("secret.txt").to_string_lossy().into_owned()];

    let result = svc
        .copy_files_to_workspace(
            &paths,
            workspace.path().to_str().unwrap(),
            Some(source_root.path().to_str().unwrap()),
        )
        .await
        .unwrap();

    assert!(result.copied_files.is_empty());
    assert_eq!(result.failed_files.len(), 1);
    assert!(!workspace.path().join("secret.txt").exists());
}
