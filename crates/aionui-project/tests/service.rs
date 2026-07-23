use std::path::PathBuf;
use std::sync::Arc;

use aionui_db::{Database, IProjectStore, SqliteProjectStore, init_database_memory};
use aionui_project::ProjectService;
use aionui_project::canonical::{canonicalize, to_file_uri};
use aionui_project::types::{AttachInput, FileOp, ReferenceInput};

async fn harness(temp_root: PathBuf) -> (ProjectService, Arc<dyn IProjectStore>, Database) {
    let db = init_database_memory().await.unwrap();
    let store: Arc<dyn IProjectStore> = Arc::new(SqliteProjectStore::new(db.pool().clone()));
    let service = ProjectService::new(Arc::clone(&store), temp_root);
    (service, store, db)
}

async fn service() -> (ProjectService, Database) {
    let (service, _store, db) = harness(std::env::temp_dir()).await;
    (service, db)
}

fn uri_of(path: &std::path::Path) -> String {
    to_file_uri(path).unwrap()
}

// ── happy paths ────────────────────────────────────────────────────────

#[tokio::test]
async fn create_standard_creates_then_reuses_by_canonical() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;

    let a = svc.create_standard(uri_of(dir.path())).await.unwrap();
    assert_eq!(a.project.kind, "standard");
    assert_eq!(a.project_explorer.role, "workspace");

    let b = svc.create_standard(uri_of(dir.path())).await.unwrap();
    assert_eq!(a.project.project_id, b.project.project_id);
    assert_eq!(a.folder.folder_id, b.folder.folder_id);
    assert_eq!(a.project_explorer.pe_id, b.project_explorer.pe_id);
}

#[tokio::test]
async fn create_temp_builds_temp_project_and_conflicts_on_reused_basename() {
    let temp_root = tempfile::tempdir().unwrap();
    let (svc, _store, _db) = harness(temp_root.path().to_path_buf()).await;

    let a = svc.create_temp(Some("myconv".to_owned())).await.unwrap();
    assert_eq!(a.project.kind, "temp");
    assert_eq!(a.project.name, "myconv");

    let err = svc.create_temp(Some("myconv".to_owned())).await.unwrap_err();
    assert_eq!(err.code(), "temp_dir_exists");
}

#[tokio::test]
async fn create_temp_auto_uuid_yields_distinct_projects() {
    let temp_root = tempfile::tempdir().unwrap();
    let (svc, _store, _db) = harness(temp_root.path().to_path_buf()).await;

    let a = svc.create_temp(None).await.unwrap();
    let b = svc.create_temp(None).await.unwrap();
    assert_eq!(a.project.kind, "temp");
    assert_ne!(a.folder.folder_id, b.folder.folder_id);
    assert_ne!(a.project.project_id, b.project.project_id);
}

#[tokio::test]
async fn resolve_existing_classifies_temp_vs_standard_by_temp_root() {
    let temp_root = tempfile::tempdir().unwrap();
    let (svc, _store, _db) = harness(temp_root.path().to_path_buf()).await;

    let under = temp_root.path().join("under-temp");
    std::fs::create_dir_all(&under).unwrap();
    let temp = svc.resolve_existing(uri_of(&under)).await.unwrap();
    assert_eq!(temp.project.kind, "temp");

    let outside = tempfile::tempdir().unwrap();
    let standard = svc.resolve_existing(uri_of(outside.path())).await.unwrap();
    assert_eq!(standard.project.kind, "standard");
}

#[tokio::test]
async fn get_project_aggregates_explorer_view() {
    let ws = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc.create_standard(uri_of(ws.path())).await.unwrap();

    let detail = svc.get_project(&out.project.project_id).await.unwrap();
    assert_eq!(detail.kind, "standard");
    assert_eq!(detail.explorer.workspace_pe_id, out.project_explorer.pe_id);
    assert_eq!(detail.explorer.entries.len(), 1);
    assert_eq!(detail.explorer.entries[0].folder.runtime_status.as_str(), "available");
}

#[tokio::test]
async fn resolve_reference_returns_child_resource() {
    let ws = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc.create_standard(uri_of(ws.path())).await.unwrap();

    let resolved = svc
        .resolve_reference(ReferenceInput {
            pe_id: out.project_explorer.pe_id.clone(),
            relative_path: "src/main.rs".to_owned(),
            op: FileOp::Read,
        })
        .await
        .unwrap();
    assert_eq!(resolved.relative_path, "src/main.rs");
    assert!(resolved.resource_uri.ends_with("/src/main.rs"));
    assert_eq!(resolved.folder_id, out.folder.folder_id);
}

#[tokio::test]
async fn validate_workspace_match_accepts_matching_folder() {
    let ws = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc.create_standard(uri_of(ws.path())).await.unwrap();

    svc.validate_workspace_match(&out.project.project_id, &out.folder.folder_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn attach_then_remove_attached_entry() {
    let ws = tempfile::tempdir().unwrap();
    let att = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc.create_standard(uri_of(ws.path())).await.unwrap();

    let entry = svc
        .attach_folder(AttachInput {
            project_id: out.project.project_id.clone(),
            uri: uri_of(att.path()),
            display_name: Some("Docs".to_owned()),
        })
        .await
        .unwrap();
    assert_eq!(entry.role, "attached");
    assert_eq!(entry.display_name.as_deref(), Some("Docs"));

    svc.remove_attached(&entry.pe_id).await.unwrap();
    let detail = svc.get_project(&out.project.project_id).await.unwrap();
    assert_eq!(detail.explorer.entries.len(), 1); // only workspace remains
}

#[tokio::test]
async fn attach_child_of_existing_focuses_existing_entry() {
    let ws = tempfile::tempdir().unwrap();
    let att = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc.create_standard(uri_of(ws.path())).await.unwrap();
    let entry = svc
        .attach_folder(AttachInput {
            project_id: out.project.project_id.clone(),
            uri: uri_of(att.path()),
            display_name: None,
        })
        .await
        .unwrap();

    let child = att.path().join("sub");
    std::fs::create_dir_all(&child).unwrap();
    let focused = svc
        .attach_folder(AttachInput {
            project_id: out.project.project_id.clone(),
            uri: uri_of(&child),
            display_name: None,
        })
        .await
        .unwrap();
    assert_eq!(focused.pe_id, entry.pe_id);
}

#[tokio::test]
async fn orphan_folder_is_adopted_on_next_create() {
    let dir = tempfile::tempdir().unwrap();
    let uri = uri_of(dir.path());
    let canonical = canonicalize(&uri).unwrap();
    let (svc, store, _db) = harness(std::env::temp_dir()).await;

    // Simulate a benign orphan: folder row exists with no project/entry.
    let orphan = store.upsert_folder(canonical.as_str(), &uri).await.unwrap();

    let out = svc.create_standard(uri).await.unwrap();
    assert_eq!(out.folder.folder_id, orphan.folder_id); // reused, not re-created
    assert_eq!(out.project.kind, "standard");
}

// ── bad paths (assert specific codes) ────────────────────────────────────

#[tokio::test]
async fn create_standard_missing_dir_is_folder_not_found() {
    let (svc, _db) = service().await;
    let missing = uri_of(std::path::Path::new("/nonexistent-aionui-xyz-8f3a2b1c"));
    let err = svc.create_standard(missing).await.unwrap_err();
    assert_eq!(err.code(), "folder_not_found");
}

#[tokio::test]
async fn create_standard_on_a_file_is_folder_not_directory() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let (svc, _db) = service().await;
    let err = svc.create_standard(uri_of(file.path())).await.unwrap_err();
    assert_eq!(err.code(), "folder_not_directory");
}

#[tokio::test]
async fn attach_duplicate_folder_is_rejected() {
    let ws = tempfile::tempdir().unwrap();
    let att = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc.create_standard(uri_of(ws.path())).await.unwrap();
    svc.attach_folder(AttachInput {
        project_id: out.project.project_id.clone(),
        uri: uri_of(att.path()),
        display_name: None,
    })
    .await
    .unwrap();

    let err = svc
        .attach_folder(AttachInput {
            project_id: out.project.project_id.clone(),
            uri: uri_of(att.path()),
            display_name: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), "project_explorer_duplicate");
}

#[tokio::test]
async fn attach_ancestor_of_existing_is_overlap() {
    let ws = tempfile::tempdir().unwrap();
    let att = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc.create_standard(uri_of(ws.path())).await.unwrap();
    svc.attach_folder(AttachInput {
        project_id: out.project.project_id.clone(),
        uri: uri_of(att.path()),
        display_name: None,
    })
    .await
    .unwrap();

    // Parent of the attached folder overlaps it (and the workspace).
    let parent = att.path().parent().unwrap().to_path_buf();
    let err = svc
        .attach_folder(AttachInput {
            project_id: out.project.project_id.clone(),
            uri: uri_of(&parent),
            display_name: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), "project_explorer_overlap");
}

#[tokio::test]
async fn remove_workspace_entry_is_immutable() {
    let ws = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc.create_standard(uri_of(ws.path())).await.unwrap();

    let err = svc.remove_attached(&out.project_explorer.pe_id).await.unwrap_err();
    assert_eq!(err.code(), "workspace_entry_immutable");
}

#[tokio::test]
async fn validate_workspace_match_rejects_mismatched_folder() {
    let ws = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc.create_standard(uri_of(ws.path())).await.unwrap();

    let err = svc
        .validate_workspace_match(&out.project.project_id, "some-other-folder-id")
        .await
        .unwrap_err();
    assert_eq!(err.code(), "workspace_folder_mismatch");
}

#[tokio::test]
async fn resolve_reference_rejects_dot_dot_escape() {
    let ws = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc.create_standard(uri_of(ws.path())).await.unwrap();

    let err = svc
        .resolve_reference(ReferenceInput {
            pe_id: out.project_explorer.pe_id.clone(),
            relative_path: "../escape".to_owned(),
            op: FileOp::Read,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_relative_path");
}

#[tokio::test]
async fn resolve_reference_missing_pe_is_not_found() {
    let (svc, _db) = service().await;
    let err = svc
        .resolve_reference(ReferenceInput {
            pe_id: "nope".to_owned(),
            relative_path: "a".to_owned(),
            op: FileOp::Read,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), "project_explorer_not_found");
}

#[tokio::test]
async fn get_missing_project_is_project_not_found() {
    let (svc, _db) = service().await;
    let err = svc.get_project("nope").await.unwrap_err();
    assert_eq!(err.code(), "project_not_found");
}
