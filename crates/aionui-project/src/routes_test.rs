//! Route-level tests for the project control plane: response shape,
//! attach idempotency (focus / duplicate / overlap), remove semantics, and
//! error-code mapping. Uses a real in-memory DB store + a fresh tempdir
//! workspace, exercised through the axum router via `oneshot`.

// `ApiError` is the type under test in the wire-mapping cases below.
#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use aionui_common::ApiError;
use aionui_db::{Database, IProjectStore, SqliteProjectStore, init_database_memory};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

use super::{ProjectRouterState, project_routes};
use crate::ProjectService;
use crate::canonical::to_file_uri;
use crate::types::ProjectError;

/// Build a router over an in-memory-DB `ProjectService` with a fresh tempdir
/// registered as the standard (workspace) project. Returns the project_id, the
/// workspace pe_id, and the tempdir + Database (kept alive for the test).
async fn setup() -> (Router, String, String, TempDir, Database) {
    let db = init_database_memory().await.unwrap();
    let store: Arc<dyn IProjectStore> = Arc::new(SqliteProjectStore::new(db.pool().clone()));
    let service = Arc::new(ProjectService::new(Arc::clone(&store), std::env::temp_dir()));

    let dir = tempfile::tempdir().unwrap();
    let created = service
        .create_standard("system_default_user", to_file_uri(dir.path()).unwrap())
        .await
        .unwrap();
    let project_id = created.project.project_id;
    let workspace_pe_id = created.project_explorer.pe_id;

    // Handlers extract `Extension<CurrentUser>`; production wiring injects it
    // via the auth middleware — tests inject the seeded default user directly.
    let router =
        project_routes(ProjectRouterState { project: service }).layer(axum::Extension(aionui_auth::CurrentUser {
            id: "system_default_user".to_owned(),
            username: "admin".to_owned(),
            user_type: aionui_db::UserType::Local,
            status: aionui_db::UserStatus::Active,
        }));
    (router, project_id, workspace_pe_id, dir, db)
}

/// Fire one request through the router and return `(status, parsed_body)`.
/// An empty body (e.g. 204) parses to `Value::Null`.
async fn send(router: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let parsed = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, parsed)
}

fn folders_url(project_id: &str) -> String {
    format!("/api/projects/{project_id}/folders")
}

/// Render the `From<ProjectError> for ApiError` wire mapping to `(status,
/// body)`. These errors are produced by `resolve_chat_message` (called from
/// conversation/team), not by any route in this crate's router, so the mapping
/// arc — HTTP status + stable `code` string the frontend branches on — must be
/// asserted directly rather than through a request.
async fn map_error(err: ProjectError) -> (StatusCode, Value) {
    let response = ApiError::from(err).into_response();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    (status, body)
}

#[tokio::test]
async fn local_path_not_readable_maps_to_400_with_stable_code() {
    let (status, body) = map_error(ProjectError::LocalPathNotReadable {
        path: "/host/file".into(),
    })
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "local_path_not_readable");
    assert_eq!(body["success"], false);
}

#[tokio::test]
async fn upload_path_outside_root_maps_to_400_with_stable_code() {
    let (status, body) = map_error(ProjectError::UploadPathOutsideRoot {
        path: "/outside/root".into(),
    })
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "upload_path_outside_root");
    assert_eq!(body["success"], false);
}

#[tokio::test]
async fn get_project_returns_workspace_root() {
    let (router, project_id, workspace_pe_id, _dir, _db) = setup().await;

    let (status, body) = send(&router, "GET", &format!("/api/projects/{project_id}"), None).await;
    assert_eq!(status, StatusCode::OK);

    let data = &body["data"];
    assert_eq!(data["project_id"], project_id);
    let entries = data["explorer"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["role"], "workspace");
    assert_eq!(entries[0]["pe_id"], workspace_pe_id);
    assert_eq!(data["explorer"]["workspace_pe_id"], workspace_pe_id);
    assert_eq!(entries[0]["runtime_status"], "available");
    // display_path is derived + non-empty; absolute path / canonical are absent.
    assert!(!entries[0]["display_path"].as_str().unwrap().is_empty());
    assert!(entries[0].get("resource_canonical").is_none());
    assert!(entries[0].get("resource_uri").is_none());
    assert!(entries[0].get("folder_id").is_none());
}

#[tokio::test]
async fn get_project_not_found_returns_domain_code() {
    let (router, _pid, _ws, _dir, _db) = setup().await;

    let (status, body) = send(&router, "GET", "/api/projects/does-not-exist", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "project_not_found");
    assert_eq!(body["success"], false);
}

#[tokio::test]
async fn attach_new_folder_returns_attached_entry() {
    let (router, project_id, _ws, _dir, _db) = setup().await;
    let other = tempfile::tempdir().unwrap();

    let (status, body) = send(
        &router,
        "POST",
        &folders_url(&project_id),
        Some(json!({ "uri": to_file_uri(other.path()).unwrap() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["role"], "attached");
    assert!(!body["data"]["pe_id"].as_str().unwrap().is_empty());
    assert_eq!(body["data"]["order_index"], 1);

    // Now visible as a second root via GET.
    let (_s, detail) = send(&router, "GET", &format!("/api/projects/{project_id}"), None).await;
    assert_eq!(detail["data"]["explorer"]["entries"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn attach_idempotency_focus_duplicate_overlap() {
    let (router, project_id, _ws, _dir, _db) = setup().await;

    // A controlled parent/child hierarchy independent of the workspace dir.
    let parent = tempfile::tempdir().unwrap();
    let child = parent.path().join("child");
    std::fs::create_dir(&child).unwrap();
    let grandchild = child.join("g");
    std::fs::create_dir(&grandchild).unwrap();

    // Attach `child` → new attached entry.
    let (status, body) = send(
        &router,
        "POST",
        &folders_url(&project_id),
        Some(json!({ "uri": to_file_uri(&child).unwrap() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let child_pe = body["data"]["pe_id"].as_str().unwrap().to_string();

    // Attach a descendant of `child` → focus-in-place: 200 returning the SAME entry.
    let (status, body) = send(
        &router,
        "POST",
        &folders_url(&project_id),
        Some(json!({ "uri": to_file_uri(&grandchild).unwrap() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["pe_id"].as_str().unwrap(), child_pe);

    // Attach the exact same folder again → 409 duplicate.
    let (status, body) = send(
        &router,
        "POST",
        &folders_url(&project_id),
        Some(json!({ "uri": to_file_uri(&child).unwrap() })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "project_explorer_duplicate");

    // Attach an ancestor of an existing entry → 409 overlap.
    let (status, body) = send(
        &router,
        "POST",
        &folders_url(&project_id),
        Some(json!({ "uri": to_file_uri(parent.path()).unwrap() })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "project_explorer_overlap");
}

#[tokio::test]
async fn remove_attached_then_workspace_immutable() {
    let (router, project_id, workspace_pe_id, _dir, _db) = setup().await;
    let other = tempfile::tempdir().unwrap();

    let (_s, body) = send(
        &router,
        "POST",
        &folders_url(&project_id),
        Some(json!({ "uri": to_file_uri(other.path()).unwrap() })),
    )
    .await;
    let attached_pe = body["data"]["pe_id"].as_str().unwrap().to_string();

    // Remove the attached root → 204, gone from the project.
    let (status, _b) = send(
        &router,
        "DELETE",
        &format!("/api/projects/{project_id}/folders/{attached_pe}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_s, detail) = send(&router, "GET", &format!("/api/projects/{project_id}"), None).await;
    assert_eq!(detail["data"]["explorer"]["entries"].as_array().unwrap().len(), 1);

    // The workspace root cannot be removed → 409 with a stable code.
    let (status, body) = send(
        &router,
        "DELETE",
        &format!("/api/projects/{project_id}/folders/{workspace_pe_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "workspace_entry_immutable");
}

// ── POST /api/projects/{id}/resolve-ref ────────────────────────────────

#[tokio::test]
async fn resolve_ref_upgrades_a_local_path_under_a_root() {
    let (router, project_id, workspace_pe_id, dir, _db) = setup().await;
    std::fs::create_dir(dir.path().join("src")).unwrap();
    let file = dir.path().join("src/main.rs");
    std::fs::write(&file, b"fn main() {}").unwrap();

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/projects/{project_id}/resolve-ref"),
        Some(json!({"file": {"kind": "local", "path": file.to_string_lossy()}})),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["file"]["kind"], "project");
    assert_eq!(body["data"]["file"]["pe_id"], workspace_pe_id);
    assert_eq!(body["data"]["file"]["relative_path"], "src/main.rs");
    assert_eq!(body["data"]["upgraded"], true);

    // The absolute path must not come back in any form: the client addresses by
    // identity and never had this path to begin with.
    let rendered = body.to_string();
    assert!(
        !rendered.contains("src/main.rs\"") || !rendered.contains(&dir.path().to_string_lossy().to_string()),
        "response must not echo the absolute path, got {rendered}"
    );
    assert!(
        !rendered.contains(&dir.path().to_string_lossy().to_string()),
        "response must not echo the root's absolute path, got {rendered}"
    );
}

#[tokio::test]
async fn resolve_ref_echoes_a_path_outside_every_root() {
    let (router, project_id, _pe, _dir, _db) = setup().await;
    let outside = tempfile::tempdir().unwrap();
    let file = outside.path().join("other.txt");
    std::fs::write(&file, b"x").unwrap();

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/projects/{project_id}/resolve-ref"),
        Some(json!({"file": {"kind": "local", "path": file.to_string_lossy()}})),
    )
    .await;

    // Not an error: the caller still needs an addressable ref, and "outside the
    // project" is an ordinary answer.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["file"]["kind"], "local");
    assert_eq!(body["data"]["upgraded"], false);
}

/// `upgraded` exists so a caller can skip a state write; it must track whether the
/// ref actually changed, not merely whether the request succeeded.
#[tokio::test]
async fn resolve_ref_reports_not_upgraded_for_an_already_project_ref() {
    let (router, project_id, workspace_pe_id, _dir, _db) = setup().await;

    let (status, body) = send(
        &router,
        "POST",
        &format!("/api/projects/{project_id}/resolve-ref"),
        Some(json!({
            "file": {"kind": "project", "pe_id": workspace_pe_id, "relative_path": "a.md"}
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["file"]["kind"], "project");
    assert_eq!(body["data"]["upgraded"], false);
}

/// An unknown project must not resolve. The file has to exist for this to prove
/// anything: a missing path returns early — before any project lookup — so pointing
/// at a nonexistent file would yield 200 regardless of the project id and the test
/// would assert nothing about scoping.
#[tokio::test]
async fn resolve_ref_on_an_unknown_project_is_not_found() {
    let (router, _project_id, _pe, dir, _db) = setup().await;
    let file = dir.path().join("real.md");
    std::fs::write(&file, b"x").unwrap();

    let (status, body) = send(
        &router,
        "POST",
        "/api/projects/proj_does_not_exist/resolve-ref",
        Some(json!({"file": {"kind": "local", "path": file.to_string_lossy()}})),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "project_not_found");
}

/// The early return for a missing file happens before the project is read, so an
/// unknown project plus a missing file is a 200 with the ref echoed back. Pinned
/// deliberately: it is the ordering that makes the missing-file contract work, and
/// a future reader moving the project lookup earlier would turn a renderable
/// missing-file state into an error.
#[tokio::test]
async fn resolve_ref_returns_the_ref_when_the_file_is_missing_even_for_an_unknown_project() {
    let (router, _project_id, _pe, dir, _db) = setup().await;
    let missing = dir.path().join("never-written.md");

    let (status, body) = send(
        &router,
        "POST",
        "/api/projects/proj_does_not_exist/resolve-ref",
        Some(json!({"file": {"kind": "local", "path": missing.to_string_lossy()}})),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["file"]["kind"], "local");
    assert_eq!(body["data"]["upgraded"], false);
}
