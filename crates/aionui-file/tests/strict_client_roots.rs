//! Strict client roots (`FileService::with_client_roots_trusted(false)`): a
//! request-supplied `root` / `workspace` must never widen the sandbox. These
//! mirror the default-mode tests (for example
//! `get_files_by_dir_accepts_requested_root_outside_sandbox`) with the opposite
//! expectation, and check that paths inside the sandbox keep working.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use aionui_api_types::WebSocketMessage;
use aionui_file::{FileError, FileService, IFileService};
use aionui_realtime::EventBroadcaster;

struct NoopBroadcaster;

impl EventBroadcaster for NoopBroadcaster {
    fn broadcast(&self, _event: WebSocketMessage<serde_json::Value>) {}
}

fn strict(root: &Path) -> FileService {
    FileService::new(Arc::new(NoopBroadcaster), vec![root.to_path_buf()]).with_client_roots_trusted(false)
}

fn trusted(root: &Path) -> FileService {
    FileService::new(Arc::new(NoopBroadcaster), vec![root.to_path_buf()])
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn assert_outside<T: std::fmt::Debug>(result: Result<T, FileError>) {
    match result {
        Err(FileError::PathOutsideSandbox { .. }) => {}
        other => panic!("expected PathOutsideSandbox, got {other:?}"),
    }
}

/// A sandbox and an outside dir holding a secret file.
fn fixture() -> (tempfile::TempDir, tempfile::TempDir) {
    let sandbox = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(sandbox.path().join("inside.txt"), "inside").unwrap();
    fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    (sandbox, outside)
}

#[tokio::test]
async fn get_files_by_dir_rejects_requested_root_outside_sandbox() {
    let (sandbox, outside) = fixture();
    let svc = strict(sandbox.path());
    assert_outside(svc.get_files_by_dir(s(outside.path()), s(outside.path())).await);
    // Inside the sandbox, root = dir still works (the picker's pattern).
    let items = svc
        .get_files_by_dir(s(sandbox.path()), s(sandbox.path()))
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
}

#[tokio::test]
async fn list_workspace_files_rejects_requested_root_outside_sandbox() {
    let (sandbox, outside) = fixture();
    let svc = strict(sandbox.path());
    assert_outside(
        svc.list_workspace_files_with_extra_root(s(outside.path()), Some(outside.path()))
            .await,
    );
    assert_eq!(
        svc.list_workspace_files_with_extra_root(s(sandbox.path()), Some(sandbox.path()))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn read_metadata_and_image_ignore_the_client_workspace() {
    let (sandbox, outside) = fixture();
    let svc = strict(sandbox.path());
    let secret = outside.path().join("secret.txt");
    assert_outside(svc.read_file(s(&secret), Some(outside.path())).await);
    assert_outside(svc.get_file_metadata(s(&secret), Some(outside.path())).await);
    assert_outside(svc.get_image_base64(s(&secret), Some(outside.path())).await);

    let inside = sandbox.path().join("inside.txt");
    assert_eq!(
        svc.read_file(s(&inside), Some(sandbox.path()))
            .await
            .unwrap()
            .as_deref(),
        Some("inside")
    );
}

#[tokio::test]
async fn read_of_a_missing_file_outside_the_sandbox_is_not_reported_as_absent() {
    // In trusted mode a missing file under the client root reads as `None`;
    // strict mode must not use the client root for that decision either.
    let (sandbox, outside) = fixture();
    let missing = outside.path().join("missing.txt");
    assert!(
        trusted(sandbox.path())
            .read_file(s(&missing), Some(outside.path()))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        strict(sandbox.path())
            .read_file(s(&missing), Some(outside.path()))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn write_rejects_a_workspace_outside_the_sandbox() {
    let (sandbox, outside) = fixture();
    let svc = strict(sandbox.path());
    let target = outside.path().join("planted.txt");
    assert_outside(svc.write_file(s(&target), b"x", s(outside.path())).await);
    assert!(!target.exists());

    let ok = sandbox.path().join("new.txt");
    assert!(svc.write_file(s(&ok), b"ok", s(sandbox.path())).await.unwrap());
    assert_eq!(fs::read_to_string(ok).unwrap(), "ok");
}

#[tokio::test]
async fn copy_does_not_take_sources_from_outside_the_sandbox() {
    let (sandbox, outside) = fixture();
    let ws = sandbox.path().join("ws");
    fs::create_dir(&ws).unwrap();
    let secret = outside.path().join("secret.txt").to_string_lossy().into_owned();

    // No source_root: the source's own parent must not count as a root.
    let svc = strict(sandbox.path());
    let result = svc
        .copy_files_to_workspace(std::slice::from_ref(&secret), s(&ws), None)
        .await
        .unwrap();
    assert!(result.copied_files.is_empty());
    assert_eq!(result.failed_files.len(), 1);
    assert!(!ws.join("secret.txt").exists());

    // An explicit source_root outside the sandbox is rejected outright.
    assert_outside(
        svc.copy_files_to_workspace(std::slice::from_ref(&secret), s(&ws), Some(s(outside.path())))
            .await,
    );
    // So is a destination workspace outside it.
    assert_outside(svc.copy_files_to_workspace(&[], s(outside.path()), None).await);

    // Trusted mode keeps the upstream behaviour (copy from the picked file's folder).
    let result = trusted(sandbox.path())
        .copy_files_to_workspace(std::slice::from_ref(&secret), s(&ws), None)
        .await
        .unwrap();
    assert_eq!(result.copied_files.len(), 1);
}

#[tokio::test]
async fn ensure_in_sandbox_checks_only_in_strict_mode() {
    let (sandbox, outside) = fixture();
    assert_outside(strict(sandbox.path()).ensure_in_sandbox(s(outside.path())));
    strict(sandbox.path()).ensure_in_sandbox(s(sandbox.path())).unwrap();
    trusted(sandbox.path()).ensure_in_sandbox(s(outside.path())).unwrap();
}

#[tokio::test]
async fn symlink_out_of_the_sandbox_is_rejected() {
    // A link inside the sandbox that points outside must not become a way out.
    #[cfg(unix)]
    {
        let (sandbox, outside) = fixture();
        let link = sandbox.path().join("escape");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let svc = strict(sandbox.path());
        assert_outside(svc.get_files_by_dir(s(&link), s(&link)).await);
        assert_outside(svc.read_file(s(&link.join("secret.txt")), Some(&link)).await);
    }
}
