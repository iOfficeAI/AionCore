#![allow(clippy::disallowed_types)]

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, State};
use axum::middleware::from_fn;
use axum::routing::{get, post};

use aionui_api_types::{
    ApiResponse, HubExtensionListItem, HubOperationResponse, HubUpdateInfo as ApiHubUpdateInfo, InstallExtensionRequest,
};
use aionui_auth::admin_required_middleware;
use aionui_common::ApiError;

use crate::hub::index_manager::HubIndexManager;
use crate::hub::installer::HubInstaller;

// ---------------------------------------------------------------------------
// Router state
// ---------------------------------------------------------------------------

/// Shared state for Hub route handlers.
#[derive(Clone)]
pub struct HubRouterState {
    pub index_manager: HubIndexManager,
    pub installer: HubInstaller,
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the Hub router with all `/api/hub/*` routes.
///
/// All routes require authentication (applied by the caller).
pub fn hub_routes(state: HubRouterState) -> Router {
    Router::new()
        .route("/api/hub/extensions", get(get_hub_extensions))
        .route(
            "/api/hub/install",
            post(install_extension).route_layer(from_fn(admin_required_middleware)),
        )
        .route(
            "/api/hub/retry-install",
            post(retry_install).route_layer(from_fn(admin_required_middleware)),
        )
        .route("/api/hub/check-updates", post(check_updates))
        .route(
            "/api/hub/update",
            post(update_extension).route_layer(from_fn(admin_required_middleware)),
        )
        .route(
            "/api/hub/uninstall",
            post(uninstall_extension).route_layer(from_fn(admin_required_middleware)),
        )
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/hub/extensions` — get Hub extension list with statuses.
async fn get_hub_extensions(
    State(state): State<HubRouterState>,
) -> Result<Json<ApiResponse<Vec<HubExtensionListItem>>>, ApiError> {
    let entries = state.index_manager.load_index().await;
    let items: Vec<HubExtensionListItem> = entries
        .into_iter()
        .map(|e| {
            let status_str = serde_json::to_value(e.status)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| "notInstalled".to_string());
            HubExtensionListItem {
                name: e.name,
                version: e.version,
                display_name: e.display_name,
                description: e.description,
                author: e.author,
                icon: e.icon,
                tags: e.tags,
                bundled: e.bundled,
                status: status_str,
            }
        })
        .collect();
    Ok(Json(ApiResponse::ok(items)))
}

/// `POST /api/hub/install` — install an extension from the Hub.
async fn install_extension(
    State(state): State<HubRouterState>,
    body: Result<Json<InstallExtensionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<HubOperationResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state.installer.install(&req.name).await;
    Ok(Json(ApiResponse::ok(HubOperationResponse {
        success: result.success,
        msg: result.msg,
    })))
}

/// `POST /api/hub/retry-install` — retry a failed installation.
async fn retry_install(
    State(state): State<HubRouterState>,
    body: Result<Json<InstallExtensionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<HubOperationResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state.installer.retry_install(&req.name).await;
    Ok(Json(ApiResponse::ok(HubOperationResponse {
        success: result.success,
        msg: result.msg,
    })))
}

/// `POST /api/hub/check-updates` — check for available updates.
async fn check_updates(
    State(state): State<HubRouterState>,
) -> Result<Json<ApiResponse<Vec<ApiHubUpdateInfo>>>, ApiError> {
    let updates = state.installer.check_updates().await;
    let resp: Vec<ApiHubUpdateInfo> = updates
        .into_iter()
        .map(|u| ApiHubUpdateInfo {
            name: u.name,
            current_version: u.current_version,
            latest_version: u.latest_version,
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

/// `POST /api/hub/update` — update an installed extension.
async fn update_extension(
    State(state): State<HubRouterState>,
    body: Result<Json<InstallExtensionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<HubOperationResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state.installer.update(&req.name).await;
    Ok(Json(ApiResponse::ok(HubOperationResponse {
        success: result.success,
        msg: result.msg,
    })))
}

/// `POST /api/hub/uninstall` — uninstall an extension.
async fn uninstall_extension(
    State(state): State<HubRouterState>,
    body: Result<Json<InstallExtensionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<HubOperationResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state.installer.uninstall(&req.name).await;
    Ok(Json(ApiResponse::ok(HubOperationResponse {
        success: result.success,
        msg: result.msg,
    })))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ExtensionRegistry;
    use crate::state::ExtensionStateStore;
    use aionui_auth::CurrentUser;
    use aionui_db::{SiteRole, UserStatus, UserType};
    use aionui_realtime::BroadcastEventBus;
    use axum::Extension;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn make_state() -> HubRouterState {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ExtensionStateStore::new(tmp.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        let hub_dir = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let registry = ExtensionRegistry::new(store, bus, "1.0.0".into());
        let index_manager = HubIndexManager::new(hub_dir, registry.clone());
        let installer = HubInstaller::new(index_manager.clone(), registry);
        HubRouterState {
            index_manager,
            installer,
        }
    }

    #[test]
    fn hub_routes_builds_router() {
        let state = make_state();
        let _router = hub_routes(state);
    }

    fn current_user(role: SiteRole) -> CurrentUser {
        CurrentUser {
            id: "test-user".to_owned(),
            username: "test-user".to_owned(),
            user_type: UserType::Local,
            status: UserStatus::Active,
            site_role: role,
            must_change_password: false,
        }
    }

    fn operation_request(path: &str) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"name":"missing-extension"}"#))
            .unwrap()
    }

    #[tokio::test]
    async fn member_cannot_mutate_global_hub_state_but_can_list() {
        let app = hub_routes(make_state()).layer(Extension(current_user(SiteRole::Member)));

        for path in [
            "/api/hub/install",
            "/api/hub/retry-install",
            "/api/hub/update",
            "/api/hub/uninstall",
        ] {
            let response = app.clone().oneshot(operation_request(path)).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "member mutation must fail: {path}"
            );
        }

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/hub/extensions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn live_local_admin_reaches_global_hub_mutation_handler() {
        let app = hub_routes(make_state()).layer(Extension(current_user(SiteRole::Admin)));
        let response = app.oneshot(operation_request("/api/hub/install")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
