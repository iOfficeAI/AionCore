//! `ProjectService::with_registration_roots`: with roots configured, a folder can
//! only become a project workspace or an attached folder inside them. Without
//! them (the default), any existing directory is accepted.

use std::sync::Arc;

use aionui_db::{Database, IProjectStore, SqliteProjectStore, init_database_memory};
use aionui_project::ProjectService;
use aionui_project::canonical::to_file_uri;
use aionui_project::types::{AttachInput, ProjectError};

const USER: &str = "system_default_user";

async fn service(roots: Option<Vec<std::path::PathBuf>>) -> (ProjectService, Database) {
    let db = init_database_memory().await.unwrap();
    let store: Arc<dyn IProjectStore> = Arc::new(SqliteProjectStore::new(db.pool().clone()));
    let mut svc = ProjectService::new(store, std::env::temp_dir());
    if let Some(roots) = roots {
        svc = svc.with_registration_roots(roots);
    }
    (svc, db)
}

fn uri(p: &std::path::Path) -> String {
    to_file_uri(p).unwrap()
}

#[tokio::test]
async fn standard_project_outside_the_roots_is_refused() {
    let sandbox = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let inside = sandbox.path().join("proj");
    std::fs::create_dir(&inside).unwrap();
    let (svc, _db) = service(Some(vec![sandbox.path().to_path_buf()])).await;

    let err = svc.create_standard(USER, uri(outside.path())).await.unwrap_err();
    assert!(matches!(err, ProjectError::FolderPermissionDenied { .. }), "{err:?}");
    let err = svc.resolve_existing(USER, uri(outside.path())).await.unwrap_err();
    assert!(matches!(err, ProjectError::FolderPermissionDenied { .. }), "{err:?}");

    svc.create_standard(USER, uri(&inside)).await.unwrap();
}

#[tokio::test]
async fn attaching_a_folder_outside_the_roots_is_refused() {
    let sandbox = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let (svc, _db) = service(Some(vec![sandbox.path().to_path_buf()])).await;
    let project = svc.create_standard(USER, uri(sandbox.path())).await.unwrap().project;

    let err = svc
        .attach_folder(
            USER,
            AttachInput {
                project_id: project.project_id.clone(),
                uri: uri(outside.path()),
                display_name: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ProjectError::FolderPermissionDenied { .. }), "{err:?}");
}

#[tokio::test]
async fn without_roots_any_existing_directory_is_accepted() {
    let anywhere = tempfile::tempdir().unwrap();
    let (svc, _db) = service(None).await;
    svc.create_standard(USER, uri(anywhere.path())).await.unwrap();
}
