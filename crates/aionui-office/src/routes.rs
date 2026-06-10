#![allow(clippy::disallowed_types)]

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use std::path::{Path as FsPath, PathBuf};

use aionui_api_types::{
    ApiResponse, DetectStarOfficeRequest, DocumentConversionRequest, GetSnapshotContentRequest, ListSnapshotsRequest,
    PreviewSnapshotInfoDto, PreviewUrlResponse, SaveSnapshotRequest, SnapshotContentResponse, StarOfficeDetectResponse,
    StartPreviewRequest, StopPreviewRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use aionui_file::{FileError, path_safety::validate_path_with_extra_root};

use crate::error::OfficeError;
use crate::proxy::ProxyError;
use crate::state::OfficeRouterState;
use crate::types::DocType;

impl From<OfficeError> for ApiError {
    fn from(err: OfficeError) -> Self {
        match err {
            OfficeError::OfficecliNotFound => ApiError::BadRequest("officecli not found".into()),
            OfficeError::InstallFailed(_) => ApiError::Internal("officecli install failed".into()),
            OfficeError::StartFailed(msg) => ApiError::Internal(format!("preview start failed: {msg}")),
            OfficeError::PortTimeout(path) => {
                ApiError::Internal(format!("preview service readiness timeout for {path}"))
            }
            OfficeError::Io(e) => ApiError::Internal(format!("IO error: {e}")),
            OfficeError::Snapshot(msg) => ApiError::Internal(format!("snapshot error: {msg}")),
            OfficeError::Json(e) => ApiError::Internal(format!("JSON error: {e}")),
            OfficeError::Conversion(msg) => ApiError::Internal(format!("conversion error: {msg}")),
            OfficeError::ToolNotFound(tool) => ApiError::BadRequest(format!("{tool} is not installed")),
        }
    }
}

impl From<ProxyError> for ApiError {
    fn from(err: ProxyError) -> Self {
        match err {
            ProxyError::PortNotActive(_) => ApiError::Forbidden(err.to_string()),
            ProxyError::Timeout => ApiError::Timeout(err.to_string()),
            ProxyError::ConnectionFailed(msg) => ApiError::BadGateway(msg),
            ProxyError::RequestFailed(msg) => ApiError::BadGateway(msg),
        }
    }
}

pub fn office_routes(state: OfficeRouterState) -> Router {
    Router::new()
        .route("/api/word-preview/start", post(start_word_preview))
        .route("/api/word-preview/stop", post(stop_word_preview))
        .route("/api/excel-preview/start", post(start_excel_preview))
        .route("/api/excel-preview/stop", post(stop_excel_preview))
        .route("/api/ppt-preview/start", post(start_ppt_preview))
        .route("/api/ppt-preview/stop", post(stop_ppt_preview))
        .route("/api/preview-history/list", post(list_snapshots))
        .route("/api/preview-history/save", post(save_snapshot))
        .route("/api/preview-history/get-content", post(get_snapshot_content))
        .route("/api/star-office/detect", post(detect_star_office))
        .route("/api/document/convert", post(convert_document))
        .with_state(state)
}

pub fn office_proxy_routes(state: OfficeRouterState) -> Router {
    Router::new()
        .route("/api/ppt-proxy/{port}", get(ppt_proxy))
        .route("/api/ppt-proxy/{port}/{*path}", get(ppt_proxy))
        .route("/api/office-watch-proxy/{port}", get(office_watch_proxy))
        .route("/api/office-watch-proxy/{port}/{*path}", get(office_watch_proxy))
        .with_state(state)
}

#[derive(serde::Deserialize)]
struct ProxyPortPath {
    port: u16,
    path: Option<String>,
}

// -- Preview start/stop handlers ------------------------------------------

async fn start_word_preview(
    State(state): State<OfficeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<StartPreviewRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<PreviewUrlResponse>>, ApiError> {
    start_preview(state, body, DocType::Word).await
}

async fn stop_word_preview(
    State(state): State<OfficeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<StopPreviewRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    stop_preview(state, body, DocType::Word).await
}

async fn start_excel_preview(
    State(state): State<OfficeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<StartPreviewRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<PreviewUrlResponse>>, ApiError> {
    start_preview(state, body, DocType::Excel).await
}

async fn stop_excel_preview(
    State(state): State<OfficeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<StopPreviewRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    stop_preview(state, body, DocType::Excel).await
}

async fn start_ppt_preview(
    State(state): State<OfficeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<StartPreviewRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<PreviewUrlResponse>>, ApiError> {
    start_preview(state, body, DocType::Ppt).await
}

async fn stop_ppt_preview(
    State(state): State<OfficeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<StopPreviewRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    stop_preview(state, body, DocType::Ppt).await
}

async fn start_preview(
    state: OfficeRouterState,
    body: Result<Json<StartPreviewRequest>, JsonRejection>,
    doc_type: DocType,
) -> Result<Json<ApiResponse<PreviewUrlResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let validated_path = validate_office_path(&state, &req.file_path, req.workspace.as_deref())?;
    let validated_path = validated_path.to_string_lossy().into_owned();

    let result = state.watch_manager.start(&validated_path, doc_type).await;

    let resp = match result {
        Ok(port) => {
            let url = format!("/api/{}/{}", doc_type.proxy_prefix(), port);
            PreviewUrlResponse { url, error: None }
        }
        Err(e) => PreviewUrlResponse {
            url: String::new(),
            error: Some(preview_error_code(&e).to_owned()),
        },
    };

    Ok(Json(ApiResponse::ok(resp)))
}

async fn stop_preview(
    state: OfficeRouterState,
    body: Result<Json<StopPreviewRequest>, JsonRejection>,
    doc_type: DocType,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state.watch_manager.stop(&req.file_path, doc_type).await;
    Ok(Json(ApiResponse::success()))
}

// -- Snapshot handlers ----------------------------------------------------

async fn list_snapshots(
    State(state): State<OfficeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<ListSnapshotsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Vec<PreviewSnapshotInfoDto>>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let snapshots = state.snapshot_service.list(&req.target).await?;
    Ok(Json(ApiResponse::ok(snapshots)))
}

async fn save_snapshot(
    State(state): State<OfficeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<SaveSnapshotRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<PreviewSnapshotInfoDto>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let info = state.snapshot_service.save(&req.target, &req.content).await?;
    Ok(Json(ApiResponse::ok(info)))
}

async fn get_snapshot_content(
    State(state): State<OfficeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<GetSnapshotContentRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Option<SnapshotContentResponse>>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let result = state
        .snapshot_service
        .get_content(&req.target, &req.snapshot_id)
        .await?;
    Ok(Json(ApiResponse::ok(result)))
}

// -- Star Office detection ------------------------------------------------

async fn detect_star_office(
    State(state): State<OfficeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<DetectStarOfficeRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<StarOfficeDetectResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let url = state
        .star_office_detector
        .detect(req.preferred_url.as_deref(), req.force.unwrap_or(false), req.timeout_ms)
        .await;
    Ok(Json(ApiResponse::ok(StarOfficeDetectResponse { url })))
}

// -- Document conversion --------------------------------------------------

async fn convert_document(
    State(state): State<OfficeRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<DocumentConversionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_api_types::DocumentConversionResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let validated_path = validate_office_path(&state, &req.file_path, req.workspace.as_deref())?;
    let resp = state
        .conversion_service
        .convert(validated_path.to_string_lossy().as_ref(), req.to)
        .await?;
    Ok(Json(ApiResponse::ok(resp)))
}

fn validate_office_path(
    state: &OfficeRouterState,
    file_path: &str,
    workspace: Option<&str>,
) -> Result<PathBuf, ApiError> {
    let allowed_roots: Vec<&FsPath> = state.allowed_roots.iter().map(PathBuf::as_path).collect();
    validate_path_with_extra_root(file_path, &allowed_roots, workspace.map(FsPath::new))
        .map_err(file_error_to_api_error)
}

fn file_error_to_api_error(error: FileError) -> ApiError {
    match error {
        FileError::BadRequest(message) => ApiError::BadRequest(message),
        FileError::Forbidden(message) => ApiError::Forbidden(message),
        FileError::PathOutsideSandbox {
            message,
            field,
            operation,
        } => ApiError::PathOutsideSandbox {
            message,
            field,
            operation,
        },
        FileError::NotFound(message) => ApiError::NotFound(message),
        FileError::Internal(message) => ApiError::Internal(message),
    }
}

fn preview_error_code(error: &OfficeError) -> &'static str {
    match error {
        OfficeError::OfficecliNotFound => "OFFICECLI_NOT_FOUND",
        OfficeError::InstallFailed(_) => "OFFICECLI_INSTALL_FAILED",
        OfficeError::PortTimeout(_) => "OFFICECLI_PORT_TIMEOUT",
        OfficeError::StartFailed(_)
        | OfficeError::Io(_)
        | OfficeError::Snapshot(_)
        | OfficeError::Json(_)
        | OfficeError::Conversion(_)
        | OfficeError::ToolNotFound(_) => "OFFICECLI_START_FAILED",
    }
}

// -- Reverse proxy handlers -----------------------------------------------

async fn ppt_proxy(
    State(state): State<OfficeRouterState>,
    Path(params): Path<ProxyPortPath>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let path = params.path.as_deref().unwrap_or("/");
    proxy_forward(state, params.port, path, DocType::Ppt, &headers).await
}

async fn office_watch_proxy(
    State(state): State<OfficeRouterState>,
    Path(params): Path<ProxyPortPath>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let path = params.path.as_deref().unwrap_or("/");
    let request_headers: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|val| (k.as_str().to_owned(), val.to_owned())))
        .collect();

    let proxy_resp = state
        .proxy_service
        .forward_watch(params.port, path, &request_headers)
        .await?;

    let status = StatusCode::from_u16(proxy_resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = axum::response::Response::builder().status(status);

    for (key, value) in &proxy_resp.headers {
        response = response.header(key.as_str(), value.as_str());
    }

    Ok(response
        .body(axum::body::Body::from(proxy_resp.body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
}

async fn proxy_forward(
    state: OfficeRouterState,
    port: u16,
    path: &str,
    doc_type: DocType,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let request_headers: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|val| (k.as_str().to_owned(), val.to_owned())))
        .collect();

    let proxy_resp = state
        .proxy_service
        .forward(port, path, doc_type, &request_headers)
        .await?;

    let status = StatusCode::from_u16(proxy_resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = axum::response::Response::builder().status(status);

    for (key, value) in &proxy_resp.headers {
        response = response.header(key.as_str(), value.as_str());
    }

    Ok(response
        .body(axum::body::Body::from(proxy_resp.body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aionui_file::FileError;

    use crate::conversion::ConversionService;
    use crate::error::OfficeError;
    use crate::proxy::{ProxyError, ProxyService};
    use crate::snapshot::SnapshotService;
    use crate::star_office::StarOfficeDetector;
    use crate::state::OfficeRouterState;
    use crate::types::DocType;
    use crate::watch_manager::{OfficecliWatchManager, ProcessHandle, ProcessSpawner};

    use super::{ApiError, file_error_to_api_error, office_proxy_routes, office_routes};

    #[test]
    fn office_routes_builds_without_panic() {
        let state = build_test_state();
        let _router = office_routes(state);
    }

    #[test]
    fn office_proxy_routes_builds_without_panic() {
        let state = build_test_state();
        let _router = office_proxy_routes(state);
    }

    #[test]
    fn officecli_not_found_maps_to_bad_request() {
        let err = ApiError::from(OfficeError::OfficecliNotFound);
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn install_failed_maps_to_internal() {
        let err = ApiError::from(OfficeError::InstallFailed("installer stderr".into()));
        assert!(matches!(err, ApiError::Internal(msg) if msg == "officecli install failed"));
    }

    #[test]
    fn start_failed_maps_to_internal() {
        let err = ApiError::from(OfficeError::StartFailed("spawn error".into()));
        assert!(matches!(err, ApiError::Internal(msg) if msg.contains("spawn error")));
    }

    #[test]
    fn port_timeout_maps_to_internal() {
        let err = ApiError::from(OfficeError::PortTimeout("/a.docx".into()));
        assert!(matches!(err, ApiError::Internal(msg) if msg.contains("/a.docx")));
    }

    #[test]
    fn io_error_maps_to_internal() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err = ApiError::from(OfficeError::Io(io_err));
        assert!(matches!(err, ApiError::Internal(msg) if msg.contains("file missing")));
    }

    #[test]
    fn conversion_error_maps_to_internal() {
        let err = ApiError::from(OfficeError::Conversion("bad format".into()));
        assert!(matches!(err, ApiError::Internal(msg) if msg.contains("bad format")));
    }

    #[test]
    fn file_path_outside_sandbox_maps_to_explicit_api_code() {
        let err = file_error_to_api_error(FileError::PathOutsideSandbox {
            message: "path is outside allowed roots".into(),
            field: Some("file_path"),
            operation: Some("preview"),
        });

        assert!(matches!(
            err,
            ApiError::PathOutsideSandbox {
                message,
                field: Some("file_path"),
                operation: Some("preview"),
            } if message == "path is outside allowed roots"
        ));
    }

    #[test]
    fn tool_not_found_maps_to_bad_request() {
        let err = ApiError::from(OfficeError::ToolNotFound("pandoc".into()));
        assert!(matches!(err, ApiError::BadRequest(msg) if msg.contains("pandoc")));
    }

    #[test]
    fn proxy_error_port_not_active_maps_to_forbidden() {
        let err = ApiError::from(ProxyError::PortNotActive(8080));
        assert!(matches!(err, ApiError::Forbidden(_)));
    }

    #[test]
    fn proxy_error_timeout_maps_to_timeout() {
        let err = ApiError::from(ProxyError::Timeout);
        assert!(matches!(err, ApiError::Timeout(_)));
    }

    #[test]
    fn proxy_error_connection_failed_maps_to_bad_gateway() {
        let err = ApiError::from(ProxyError::ConnectionFailed("refused".into()));
        assert!(matches!(err, ApiError::BadGateway(_)));
    }

    #[test]
    fn proxy_error_request_failed_maps_to_bad_gateway() {
        let err = ApiError::from(ProxyError::RequestFailed("network error".into()));
        assert!(matches!(err, ApiError::BadGateway(_)));
    }

    fn build_test_state() -> OfficeRouterState {
        struct NoopSpawner;

        #[async_trait::async_trait]
        impl ProcessSpawner for NoopSpawner {
            async fn spawn_officecli(
                &self,
                _file_path: &str,
                _port: u16,
                _doc_type: DocType,
            ) -> Result<Box<dyn ProcessHandle>, OfficeError> {
                Err(OfficeError::OfficecliNotFound)
            }
            async fn install_officecli(&self) -> Result<(), OfficeError> {
                Err(OfficeError::InstallFailed("noop".into()))
            }
            async fn is_officecli_installed(&self) -> bool {
                false
            }
            async fn check_update(&self, _doc_type: DocType) -> Result<(), OfficeError> {
                Ok(())
            }
        }

        struct NoopBroadcaster;
        impl aionui_realtime::EventBroadcaster for NoopBroadcaster {
            fn broadcast(&self, _msg: aionui_api_types::WebSocketMessage<serde_json::Value>) {}
        }

        let spawner = Arc::new(NoopSpawner);
        let bc: Arc<dyn aionui_realtime::EventBroadcaster> = Arc::new(NoopBroadcaster);
        let wm = Arc::new(OfficecliWatchManager::new(spawner, bc));

        let snapshot = Arc::new(SnapshotService::new(std::path::Path::new("/tmp/test")));
        let detector = Arc::new(StarOfficeDetector::new(reqwest::Client::new()));
        let conversion = Arc::new(ConversionService::new(None));
        let proxy = Arc::new(ProxyService::new(wm.clone()));

        OfficeRouterState {
            watch_manager: wm,
            snapshot_service: snapshot,
            star_office_detector: detector,
            conversion_service: conversion,
            proxy_service: proxy,
            allowed_roots: vec![std::env::temp_dir()],
        }
    }
}
