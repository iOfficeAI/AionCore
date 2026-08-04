use std::path::PathBuf;
use std::sync::Arc;

use aionui_api_types::ChatFileRef;
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

    let a = svc
        .create_standard("system_default_user", uri_of(dir.path()))
        .await
        .unwrap();
    assert_eq!(a.project.kind, "standard");
    assert_eq!(a.project_explorer.role, "workspace");

    let b = svc
        .create_standard("system_default_user", uri_of(dir.path()))
        .await
        .unwrap();
    assert_eq!(a.project.project_id, b.project.project_id);
    assert_eq!(a.folder.folder_id, b.folder.folder_id);
    assert_eq!(a.project_explorer.pe_id, b.project_explorer.pe_id);
}

#[tokio::test]
async fn create_temp_builds_temp_project_and_conflicts_on_reused_basename() {
    let temp_root = tempfile::tempdir().unwrap();
    let (svc, _store, _db) = harness(temp_root.path().to_path_buf()).await;

    let a = svc
        .create_temp("system_default_user", Some("myconv".to_owned()))
        .await
        .unwrap();
    assert_eq!(a.project.kind, "temp");
    assert_eq!(a.project.name, "myconv");

    let err = svc
        .create_temp("system_default_user", Some("myconv".to_owned()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "temp_dir_exists");
}

#[tokio::test]
async fn create_temp_auto_uuid_yields_distinct_projects() {
    let temp_root = tempfile::tempdir().unwrap();
    let (svc, _store, _db) = harness(temp_root.path().to_path_buf()).await;

    let a = svc.create_temp("system_default_user", None).await.unwrap();
    let b = svc.create_temp("system_default_user", None).await.unwrap();
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
    let temp = svc
        .resolve_existing("system_default_user", uri_of(&under))
        .await
        .unwrap();
    assert_eq!(temp.project.kind, "temp");

    let outside = tempfile::tempdir().unwrap();
    let standard = svc
        .resolve_existing("system_default_user", uri_of(outside.path()))
        .await
        .unwrap();
    assert_eq!(standard.project.kind, "standard");
}

#[tokio::test]
async fn get_project_aggregates_explorer_view() {
    let ws = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc
        .create_standard("system_default_user", uri_of(ws.path()))
        .await
        .unwrap();

    let detail = svc
        .get_project("system_default_user", &out.project.project_id)
        .await
        .unwrap();
    assert_eq!(detail.kind, "standard");
    assert_eq!(detail.explorer.workspace_pe_id, out.project_explorer.pe_id);
    assert_eq!(detail.explorer.entries.len(), 1);
    assert_eq!(detail.explorer.entries[0].folder.runtime_status.as_str(), "available");
}

#[tokio::test]
async fn resolve_reference_returns_child_resource() {
    let ws = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc
        .create_standard("system_default_user", uri_of(ws.path()))
        .await
        .unwrap();

    let resolved = svc
        .resolve_reference(
            "system_default_user",
            ReferenceInput {
                pe_id: out.project_explorer.pe_id.clone(),
                relative_path: "src/main.rs".to_owned(),
                op: FileOp::Read,
            },
        )
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
    let out = svc
        .create_standard("system_default_user", uri_of(ws.path()))
        .await
        .unwrap();

    svc.validate_workspace_match("system_default_user", &out.project.project_id, &out.folder.folder_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn attach_then_remove_attached_entry() {
    let ws = tempfile::tempdir().unwrap();
    let att = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc
        .create_standard("system_default_user", uri_of(ws.path()))
        .await
        .unwrap();

    let entry = svc
        .attach_folder(
            "system_default_user",
            AttachInput {
                project_id: out.project.project_id.clone(),
                uri: uri_of(att.path()),
                display_name: Some("Docs".to_owned()),
            },
        )
        .await
        .unwrap();
    assert_eq!(entry.role, "attached");
    assert_eq!(entry.display_name.as_deref(), Some("Docs"));

    svc.remove_attached("system_default_user", &entry.pe_id).await.unwrap();
    let detail = svc
        .get_project("system_default_user", &out.project.project_id)
        .await
        .unwrap();
    assert_eq!(detail.explorer.entries.len(), 1); // only workspace remains
}

#[tokio::test]
async fn attach_child_of_existing_focuses_existing_entry() {
    let ws = tempfile::tempdir().unwrap();
    let att = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc
        .create_standard("system_default_user", uri_of(ws.path()))
        .await
        .unwrap();
    let entry = svc
        .attach_folder(
            "system_default_user",
            AttachInput {
                project_id: out.project.project_id.clone(),
                uri: uri_of(att.path()),
                display_name: None,
            },
        )
        .await
        .unwrap();

    let child = att.path().join("sub");
    std::fs::create_dir_all(&child).unwrap();
    let focused = svc
        .attach_folder(
            "system_default_user",
            AttachInput {
                project_id: out.project.project_id.clone(),
                uri: uri_of(&child),
                display_name: None,
            },
        )
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

    let out = svc.create_standard("system_default_user", uri).await.unwrap();
    assert_eq!(out.folder.folder_id, orphan.folder_id); // reused, not re-created
    assert_eq!(out.project.kind, "standard");
}

// ── bad paths (assert specific codes) ────────────────────────────────────

#[tokio::test]
async fn create_standard_missing_dir_is_folder_not_found() {
    let (svc, _db) = service().await;
    let missing = uri_of(std::path::Path::new("/nonexistent-aionui-xyz-8f3a2b1c"));
    let err = svc.create_standard("system_default_user", missing).await.unwrap_err();
    assert_eq!(err.code(), "folder_not_found");
}

#[tokio::test]
async fn create_standard_on_a_file_is_folder_not_directory() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let (svc, _db) = service().await;
    let err = svc
        .create_standard("system_default_user", uri_of(file.path()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "folder_not_directory");
}

#[tokio::test]
async fn attach_duplicate_folder_is_rejected() {
    let ws = tempfile::tempdir().unwrap();
    let att = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc
        .create_standard("system_default_user", uri_of(ws.path()))
        .await
        .unwrap();
    svc.attach_folder(
        "system_default_user",
        AttachInput {
            project_id: out.project.project_id.clone(),
            uri: uri_of(att.path()),
            display_name: None,
        },
    )
    .await
    .unwrap();

    let err = svc
        .attach_folder(
            "system_default_user",
            AttachInput {
                project_id: out.project.project_id.clone(),
                uri: uri_of(att.path()),
                display_name: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "project_explorer_duplicate");
}

#[tokio::test]
async fn attach_ancestor_of_existing_is_overlap() {
    let ws = tempfile::tempdir().unwrap();
    let att = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc
        .create_standard("system_default_user", uri_of(ws.path()))
        .await
        .unwrap();
    svc.attach_folder(
        "system_default_user",
        AttachInput {
            project_id: out.project.project_id.clone(),
            uri: uri_of(att.path()),
            display_name: None,
        },
    )
    .await
    .unwrap();

    // Parent of the attached folder overlaps it (and the workspace).
    let parent = att.path().parent().unwrap().to_path_buf();
    let err = svc
        .attach_folder(
            "system_default_user",
            AttachInput {
                project_id: out.project.project_id.clone(),
                uri: uri_of(&parent),
                display_name: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "project_explorer_overlap");
}

#[tokio::test]
async fn remove_workspace_entry_is_immutable() {
    let ws = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc
        .create_standard("system_default_user", uri_of(ws.path()))
        .await
        .unwrap();

    let err = svc
        .remove_attached("system_default_user", &out.project_explorer.pe_id)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "workspace_entry_immutable");
}

#[tokio::test]
async fn validate_workspace_match_rejects_mismatched_folder() {
    let ws = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc
        .create_standard("system_default_user", uri_of(ws.path()))
        .await
        .unwrap();

    let err = svc
        .validate_workspace_match("system_default_user", &out.project.project_id, "some-other-folder-id")
        .await
        .unwrap_err();
    assert_eq!(err.code(), "workspace_folder_mismatch");
}

#[tokio::test]
async fn resolve_reference_rejects_dot_dot_escape() {
    let ws = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let out = svc
        .create_standard("system_default_user", uri_of(ws.path()))
        .await
        .unwrap();

    let err = svc
        .resolve_reference(
            "system_default_user",
            ReferenceInput {
                pe_id: out.project_explorer.pe_id.clone(),
                relative_path: "../escape".to_owned(),
                op: FileOp::Read,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_relative_path");
}

#[tokio::test]
async fn resolve_reference_missing_pe_is_not_found() {
    let (svc, _db) = service().await;
    let err = svc
        .resolve_reference(
            "system_default_user",
            ReferenceInput {
                pe_id: "nope".to_owned(),
                relative_path: "a".to_owned(),
                op: FileOp::Read,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "project_explorer_not_found");
}

#[tokio::test]
async fn get_missing_project_is_project_not_found() {
    let (svc, _db) = service().await;
    let err = svc.get_project("system_default_user", "nope").await.unwrap_err();
    assert_eq!(err.code(), "project_not_found");
}

// ── user-scope isolation ───────────────────────────────────────────────

async fn seed_user(db: &Database, username: &str) -> String {
    use aionui_db::IUserRepository;
    let repo = aionui_db::SqliteUserRepository::new(db.pool().clone());
    repo.create_user(username, "hash").await.unwrap().id
}

#[tokio::test]
async fn same_folder_yields_one_project_per_user() {
    let (svc, db) = service().await;
    let user_b = seed_user(&db, "user_b").await;
    let dir = tempfile::tempdir().unwrap();

    let a = svc
        .create_standard("system_default_user", uri_of(dir.path()))
        .await
        .unwrap();
    let b = svc.create_standard(user_b.as_str(), uri_of(dir.path())).await.unwrap();

    // Distinct per-user projects over the same (shared) folder row.
    assert_ne!(a.project.project_id, b.project.project_id);
    assert_eq!(a.folder.folder_id, b.folder.folder_id);

    // Re-resolving reuses each owner's own project.
    let a2 = svc
        .create_standard("system_default_user", uri_of(dir.path()))
        .await
        .unwrap();
    assert_eq!(a2.project.project_id, a.project.project_id);
}

#[tokio::test]
async fn cross_user_get_project_is_not_found() {
    let (svc, db) = service().await;
    let user_b = seed_user(&db, "user_b").await;
    let dir = tempfile::tempdir().unwrap();
    let a = svc
        .create_standard("system_default_user", uri_of(dir.path()))
        .await
        .unwrap();

    let err = svc.get_project(&user_b, &a.project.project_id).await.unwrap_err();
    assert_eq!(err.code(), "project_not_found");
}

#[tokio::test]
async fn cross_user_attach_folder_is_not_found() {
    let (svc, db) = service().await;
    let user_b = seed_user(&db, "user_b").await;
    let ws = tempfile::tempdir().unwrap();
    let att = tempfile::tempdir().unwrap();
    let a = svc
        .create_standard("system_default_user", uri_of(ws.path()))
        .await
        .unwrap();

    let err = svc
        .attach_folder(
            &user_b,
            AttachInput {
                project_id: a.project.project_id.clone(),
                uri: uri_of(att.path()),
                display_name: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "project_not_found");
}

#[tokio::test]
async fn cross_user_remove_and_rename_entry_are_not_found() {
    let (svc, db) = service().await;
    let user_b = seed_user(&db, "user_b").await;
    let ws = tempfile::tempdir().unwrap();
    let att = tempfile::tempdir().unwrap();
    let a = svc
        .create_standard("system_default_user", uri_of(ws.path()))
        .await
        .unwrap();
    let entry = svc
        .attach_folder(
            "system_default_user",
            AttachInput {
                project_id: a.project.project_id.clone(),
                uri: uri_of(att.path()),
                display_name: None,
            },
        )
        .await
        .unwrap();

    let err = svc.remove_attached(&user_b, &entry.pe_id).await.unwrap_err();
    assert_eq!(err.code(), "project_explorer_not_found");

    let err = svc
        .rename_entry(&user_b, &entry.pe_id, Some("stolen".to_owned()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "project_explorer_not_found");
}

// ── resolve_chat_file_ref: the Project variant ─────────────────────────
//
// This is the arm that `/api/fs/open-system` (and the content endpoints) depend on
// when the frontend has only explorer identity — the Explorer payload deliberately
// carries no absolute path, so `{pe_id, relative_path}` is all the client holds.
// The variant had no coverage before, which left "does an identity-only ref
// actually resolve to something openable?" resting on inspection rather than a test.

#[tokio::test]
async fn resolve_chat_file_ref_project_variant_yields_the_absolute_path() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir(ws.path().join("docs")).unwrap();
    let target = ws.path().join("docs/report.xlsx");
    std::fs::write(&target, b"x").unwrap();

    let (svc, _db) = service().await;
    let created = svc
        .create_standard("system_default_user", uri_of(ws.path()))
        .await
        .unwrap();

    let abs = svc
        .resolve_chat_file_ref(
            "system_default_user",
            &ChatFileRef::Project {
                pe_id: created.project_explorer.pe_id.clone(),
                relative_path: "docs/report.xlsx".to_owned(),
            },
            &std::env::temp_dir(),
            FileOp::Read,
        )
        .await
        .expect("an identity-only ref must resolve");

    // Compare canonicalized: the tempdir may itself sit behind a symlink (on macOS
    // /var → /private/var), so a raw string compare would fail for the wrong reason.
    assert_eq!(
        std::fs::canonicalize(&abs).unwrap(),
        std::fs::canonicalize(&target).unwrap(),
        "resolved path must point at the real file"
    );
    assert!(std::path::Path::new(&abs).is_absolute(), "must be absolute: {abs}");
}

#[tokio::test]
async fn resolve_chat_file_ref_project_variant_works_for_attached_folders() {
    // Explorer shows workspace and attached roots alike, so opening a file from an
    // attached folder must resolve the same way.
    let ws = tempfile::tempdir().unwrap();
    let att = tempfile::tempdir().unwrap();
    let target = att.path().join("sheet.xlsx");
    std::fs::write(&target, b"x").unwrap();

    let (svc, _db) = service().await;
    let created = svc
        .create_standard("system_default_user", uri_of(ws.path()))
        .await
        .unwrap();
    let entry = svc
        .attach_folder(
            "system_default_user",
            AttachInput {
                project_id: created.project.project_id.clone(),
                uri: uri_of(att.path()),
                display_name: None,
            },
        )
        .await
        .unwrap();

    let abs = svc
        .resolve_chat_file_ref(
            "system_default_user",
            &ChatFileRef::Project {
                pe_id: entry.pe_id.clone(),
                relative_path: "sheet.xlsx".to_owned(),
            },
            &std::env::temp_dir(),
            FileOp::Read,
        )
        .await
        .expect("attached-folder ref must resolve");

    assert_eq!(
        std::fs::canonicalize(&abs).unwrap(),
        std::fs::canonicalize(&target).unwrap()
    );
}

#[tokio::test]
async fn resolve_chat_file_ref_project_variant_rejects_missing_and_escaping_paths() {
    let ws = tempfile::tempdir().unwrap();
    let (svc, _db) = service().await;
    let created = svc
        .create_standard("system_default_user", uri_of(ws.path()))
        .await
        .unwrap();
    let pe_id = created.project_explorer.pe_id.clone();

    // Gone from disk → chat_file_missing, not a path that would then be handed to
    // the system opener.
    let err = svc
        .resolve_chat_file_ref(
            "system_default_user",
            &ChatFileRef::Project {
                pe_id: pe_id.clone(),
                relative_path: "nope.xlsx".to_owned(),
            },
            &std::env::temp_dir(),
            FileOp::Read,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "chat_file_missing");

    // Traversal must not escape the folder root — otherwise identity addressing
    // would become a way to open arbitrary files on the backend host.
    let err = svc
        .resolve_chat_file_ref(
            "system_default_user",
            &ChatFileRef::Project {
                pe_id,
                relative_path: "../../etc/passwd".to_owned(),
            },
            &std::env::temp_dir(),
            FileOp::Read,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err.code(), "invalid_relative_path" | "resource_outside_folder"),
        "traversal must be rejected, got {}",
        err.code()
    );
}
