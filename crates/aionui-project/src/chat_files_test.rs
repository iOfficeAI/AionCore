use std::sync::Arc;

use aionui_common::constants::AIONUI_FILES_MARKER;
use aionui_db::{IProjectStore, SqliteProjectStore, init_database_memory};
use tempfile::TempDir;

use crate::ProjectService;
use crate::canonical::to_file_uri;
use crate::types::ProjectError;
use aionui_api_types::ChatFileRef;

/// Build a service with a tempdir standard project. Returns (service, pe_id,
/// workspace dir, upload_root dir).
async fn setup() -> (Arc<ProjectService>, String, TempDir, TempDir) {
    let db = init_database_memory().await.unwrap();
    let store: Arc<dyn IProjectStore> = Arc::new(SqliteProjectStore::new(db.pool().clone()));
    let service = Arc::new(ProjectService::new(Arc::clone(&store), std::env::temp_dir()));
    let dir = tempfile::tempdir().unwrap();
    let created = service.create_standard(to_file_uri(dir.path()).unwrap()).await.unwrap();
    let upload_root = tempfile::tempdir().unwrap();
    (service, created.project_explorer.pe_id, dir, upload_root)
}

#[tokio::test]
async fn resolves_project_file_and_inlines_marker() {
    let (service, pe_id, dir, upload_root) = setup().await;
    std::fs::write(dir.path().join("note.txt"), b"hi").unwrap();

    let out = service
        .resolve_chat_message(
            "please review",
            &[ChatFileRef::Project {
                pe_id: pe_id.clone(),
                relative_path: "note.txt".into(),
            }],
            upload_root.path(),
        )
        .await
        .unwrap();

    // The resolved path is the canonicalized absolute path (case-folded on
    // case-insensitive platforms), so assert on shape/resolution rather than a
    // byte-equal path, and that content re-inlines exactly that path.
    assert_eq!(out.files.len(), 1);
    let abs = &out.files[0];
    assert!(std::path::Path::new(abs).is_file());
    assert!(abs.ends_with("note.txt"));
    assert_eq!(out.content, format!("please review\n\n{AIONUI_FILES_MARKER}\n{abs}"));
}

#[tokio::test]
async fn resolves_project_directory_ref() {
    let (service, pe_id, dir, upload_root) = setup().await;
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    // A folder attachment (tree right-click on a directory) must resolve, not
    // be rejected as a missing file.
    let out = service
        .resolve_chat_message(
            "look here",
            &[ChatFileRef::Project {
                pe_id,
                relative_path: "sub".into(),
            }],
            upload_root.path(),
        )
        .await
        .unwrap();
    assert_eq!(out.files.len(), 1);
    assert!(std::path::Path::new(&out.files[0]).is_dir());
}

#[tokio::test]
async fn empty_files_leaves_content_unchanged() {
    let (service, _pe, _dir, upload_root) = setup().await;
    let out = service
        .resolve_chat_message("hi", &[], upload_root.path())
        .await
        .unwrap();
    assert_eq!(out.content, "hi");
    assert!(out.files.is_empty());
}

#[tokio::test]
async fn missing_project_file_is_atomic_error() {
    let (service, pe_id, _dir, upload_root) = setup().await;
    let err = service
        .resolve_chat_message(
            "x",
            &[ChatFileRef::Project {
                pe_id,
                relative_path: "nope.txt".into(),
            }],
            upload_root.path(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ProjectError::ChatFileMissing { .. }), "got {err:?}");
}

#[tokio::test]
async fn upload_under_root_is_accepted() {
    let (service, _pe, _dir, upload_root) = setup().await;
    let up = upload_root.path().join("u.png");
    std::fs::write(&up, b"x").unwrap();
    let path = up.to_string_lossy().into_owned();

    let out = service
        .resolve_chat_message("", &[ChatFileRef::Upload { path: path.clone() }], upload_root.path())
        .await
        .unwrap();
    assert_eq!(out.files, vec![path]);
}

#[tokio::test]
async fn upload_outside_root_is_rejected() {
    let (service, _pe, _dir, upload_root) = setup().await;
    // A real file, but outside the managed upload dir.
    let outside = tempfile::tempdir().unwrap();
    let ext = outside.path().join("secret.txt");
    std::fs::write(&ext, b"x").unwrap();

    let err = service
        .resolve_chat_message(
            "x",
            &[ChatFileRef::Upload {
                path: ext.to_string_lossy().into_owned(),
            }],
            upload_root.path(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ProjectError::UploadPathOutsideRoot { .. }), "got {err:?}");
}
