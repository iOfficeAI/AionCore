// `ApiError` is the intended error type at this HTTP boundary (routes map the
// crate-owned `SidebarError` to it here), so the disallowed_types lint that
// steers service code away from `ApiError` does not apply to this module.
#![allow(clippy::disallowed_types)]

//! Sidebar read + ordering HTTP routes.
//!
//! - `GET /api/sidebar` — first screen (pinned → project area → chats), with a
//!   default `limit` and repeated `win=<scope-token>:<n>` per-scope overrides.
//! - `GET /api/sidebar/items` — one more window of a single group (`scope` +
//!   keyset `cursor`).
//! - `PUT`/`DELETE /api/order/{scene}/{item_type}/{item_id}` — pin / unpin.
//!
//! Handlers only parse the request and shape the response; all classification
//! and ordering lives in [`SidebarService`]. [`SidebarError`] maps to `ApiError`
//! with stable machine codes (`sidebar_bad_request`, `sidebar_scope_gone`,
//! `sidebar_internal`); DB/project detail never leaks to the client.

use std::sync::Arc;

use aionui_api_types::{ApiResponse, RemoveProjectResult, SidebarItemsResponse, SidebarResponse};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, put};
use axum::{Extension, Json, Router};

use crate::service::SidebarService;
use crate::types::{SidebarError, parse_win, validate_limit};

/// Shared state for sidebar route handlers.
#[derive(Clone)]
pub struct SidebarRouterState {
    pub service: Arc<SidebarService>,
}

/// Build the sidebar router. All routes require authentication (applied by the
/// caller, matching the project/team route wiring in `aionui-app`).
pub fn sidebar_routes(state: SidebarRouterState) -> Router {
    Router::new()
        .route("/api/sidebar", get(get_sidebar))
        .route("/api/sidebar/items", get(get_items))
        .route(
            "/api/order/{scene}/{item_type}/{item_id}",
            put(put_order).delete(delete_order),
        )
        .route("/api/sidebar/project/{project_id}", delete(delete_project))
        .with_state(state)
}

/// `GET /api/sidebar?limit=5&win=project:P1:10&win=chats:20` — first screen.
async fn get_sidebar(
    State(state): State<SidebarRouterState>,
    Extension(user): Extension<CurrentUser>,
    RawQuery(query): RawQuery,
) -> Result<Json<ApiResponse<SidebarResponse>>, ApiError> {
    let params = QueryParams::parse(query.as_deref());
    let limit = params.limit().map_err(to_api_error)?;
    if let Some(limit) = limit {
        validate_limit(limit).map_err(to_api_error)?;
    }
    let win = parse_win(&params.multi("win")).map_err(to_api_error)?;

    let response = state
        .service
        .first_screen(&user.id, limit, &win)
        .await
        .map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(response)))
}

/// `GET /api/sidebar/items?scope=<token>&cursor=<c>&limit=10` — page one group.
async fn get_items(
    State(state): State<SidebarRouterState>,
    Extension(user): Extension<CurrentUser>,
    RawQuery(query): RawQuery,
) -> Result<Json<ApiResponse<SidebarItemsResponse>>, ApiError> {
    let params = QueryParams::parse(query.as_deref());
    let scope = params
        .single("scope")
        .ok_or_else(|| to_api_error(SidebarError::BadRequest("missing scope".into())))?;
    let cursor = params.single("cursor");
    let limit = params.limit().map_err(to_api_error)?;

    let response = state
        .service
        .items(&user.id, &scope, cursor.as_deref(), limit)
        .await
        .map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(response)))
}

/// `PUT /api/order/{scene}/{item_type}/{item_id}` — pin (idempotent).
async fn put_order(
    State(state): State<SidebarRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((scene, item_type, item_id)): Path<(String, String, String)>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state
        .service
        .pin(&user.id, &scene, &item_type, &item_id)
        .await
        .map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

/// `DELETE /api/order/{scene}/{item_type}/{item_id}` — unpin (idempotent).
async fn delete_order(
    State(state): State<SidebarRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((scene, item_type, item_id)): Path<(String, String, String)>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state
        .service
        .unpin(&user.id, &scene, &item_type, &item_id)
        .await
        .map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

/// `DELETE /api/sidebar/project/{project_id}?dry_run=true` — remove a project
/// and everything classified into its group (BR-19 "所见即所删").
///
/// With `dry_run=true` nothing is deleted; the response reports the counts that
/// *would* be removed so the frontend can render a confirmation. A missing or
/// non-standard `project_id` maps to `ScopeGone` → 404.
async fn delete_project(
    State(state): State<SidebarRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(project_id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<ApiResponse<RemoveProjectResult>>, ApiError> {
    let params = QueryParams::parse(query.as_deref());
    let dry_run = params.flag("dry_run");

    let result = state
        .service
        .remove_project(&user.id, &project_id, dry_run)
        .await
        .map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(result)))
}

/// Parsed query string: percent-decoded `(key, value)` pairs. `axum::Query`
/// (serde_urlencoded) collapses duplicate keys, so `win` — which repeats — is
/// parsed from the raw query instead.
struct QueryParams {
    pairs: Vec<(String, String)>,
}

impl QueryParams {
    fn parse(query: Option<&str>) -> Self {
        let pairs = query
            .map(|q| {
                url::form_urlencoded::parse(q.as_bytes())
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect()
            })
            .unwrap_or_default();
        Self { pairs }
    }

    fn single(&self, key: &str) -> Option<String> {
        self.pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn multi(&self, key: &str) -> Vec<String> {
        self.pairs
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// A boolean query flag: present with no value / `true` / `1` → true;
    /// absent or any other value → false.
    fn flag(&self, key: &str) -> bool {
        match self.single(key) {
            None => false,
            Some(v) => v.is_empty() || v == "true" || v == "1",
        }
    }

    /// `limit` as an `i64`; a present-but-non-numeric value is a 400.
    fn limit(&self) -> Result<Option<i64>, SidebarError> {
        match self.single("limit") {
            None => Ok(None),
            Some(raw) => raw
                .parse::<i64>()
                .map(Some)
                .map_err(|_| SidebarError::BadRequest(format!("limit is not a number: {raw}"))),
        }
    }
}

/// Map the crate error to an `ApiError` with a stable code. Internal causes
/// (DB / project / unexpected) are logged and flattened so no detail leaks.
fn to_api_error(err: SidebarError) -> ApiError {
    match err {
        SidebarError::BadRequest(msg) => ApiError::coded(StatusCode::BAD_REQUEST, "sidebar_bad_request", msg, None),
        SidebarError::ScopeGone => ApiError::coded(
            StatusCode::NOT_FOUND,
            "sidebar_scope_gone",
            "scope no longer exists",
            None,
        ),
        SidebarError::Db(_) | SidebarError::Project(_) | SidebarError::Internal(_) => {
            tracing::error!(error = %err, "sidebar: internal error");
            ApiError::coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "sidebar_internal",
                "internal error",
                None,
            )
        }
    }
}
