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
    let created = service.create_standard(to_file_uri(dir.path()).unwrap()).await.unwrap();
    let project_id = created.project.project_id;
    let workspace_pe_id = created.project_explorer.pe_id;

    let router = project_routes(ProjectRouterState { project: service });
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
