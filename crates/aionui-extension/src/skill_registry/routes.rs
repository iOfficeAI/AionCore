use aionui_api_types::{
    ApiResponse, InstallOfficialSkillRequest, OfficialSkillDetail, OfficialSkillFile,
    OfficialSkillInstallationResponse, OfficialSkillSearchQuery, OfficialSkillSearchResponse, OfficialSkillSummary,
    UpdateOfficialSkillRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use serde_json::json;
use tracing::warn;

use super::{SkillRegistryError, SkillRegistryService};

pub fn skill_registry_routes(service: SkillRegistryService) -> Router {
    Router::new()
        .route("/api/skill-registry/skills", get(search_skills))
        .route("/api/skill-registry/skills/{namespace}/{slug}", get(get_skill))
        .route(
            "/api/skill-registry/skills/{namespace}/{slug}/versions/{version}/files",
            get(get_skill_files),
        )
        .route("/api/skill-registry/installations", post(install_skill))
        .route("/api/skill-registry/installations/updates", get(get_updates))
        .route(
            "/api/skill-registry/installations/{namespace}/{slug}",
            put(update_skill),
        )
        .with_state(service)
}

async fn search_skills(
    State(service): State<SkillRegistryService>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<OfficialSkillSearchQuery>,
) -> Result<Json<ApiResponse<OfficialSkillSearchResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        service.search(&user.id, &query).await.map_err(map_error)?,
    )))
}

async fn get_skill(
    State(service): State<SkillRegistryService>,
    Extension(user): Extension<CurrentUser>,
    Path((namespace, slug)): Path<(String, String)>,
) -> Result<Json<ApiResponse<OfficialSkillDetail>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        service.detail(&user.id, &namespace, &slug).await.map_err(map_error)?,
    )))
}

async fn get_skill_files(
    State(service): State<SkillRegistryService>,
    Path((namespace, slug, version)): Path<(String, String, String)>,
) -> Result<Json<ApiResponse<Vec<OfficialSkillFile>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        service.files(&namespace, &slug, &version).await.map_err(map_error)?,
    )))
}

async fn install_skill(
    State(service): State<SkillRegistryService>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<InstallOfficialSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<OfficialSkillInstallationResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        service.install(&user.id, &request).await.map_err(map_error)?,
    )))
}

async fn get_updates(
    State(service): State<SkillRegistryService>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<OfficialSkillSummary>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        service.updates(&user.id).await.map_err(map_error)?,
    )))
}

async fn update_skill(
    State(service): State<SkillRegistryService>,
    Extension(user): Extension<CurrentUser>,
    Path((namespace, slug)): Path<(String, String)>,
    body: Result<Json<UpdateOfficialSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<OfficialSkillInstallationResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        service
            .update(&user.id, &namespace, &slug, &request.version)
            .await
            .map_err(map_error)?,
    )))
}

fn map_error(error: SkillRegistryError) -> ApiError {
    warn!(error = %error, "official SkillHub request failed");
    match error {
        SkillRegistryError::InvalidRequest => ApiError::BadRequest("Invalid SkillHub request.".into()),
        SkillRegistryError::Unavailable => ApiError::coded(
            StatusCode::BAD_GATEWAY,
            "SKILL_REGISTRY_UNAVAILABLE",
            "The official SkillHub is unavailable.",
            None,
        ),
        SkillRegistryError::Timeout => ApiError::coded(
            StatusCode::GATEWAY_TIMEOUT,
            "SKILL_REGISTRY_UNAVAILABLE",
            "The official SkillHub request timed out.",
            None,
        ),
        SkillRegistryError::VersionNotFound | SkillRegistryError::InstallationNotFound => ApiError::coded(
            StatusCode::NOT_FOUND,
            "SKILL_REGISTRY_VERSION_NOT_FOUND",
            "The requested official skill version is unavailable.",
            None,
        ),
        SkillRegistryError::PackageInvalid => ApiError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "SKILL_REGISTRY_PACKAGE_INVALID",
            "The official skill package is invalid.",
            None,
        ),
        SkillRegistryError::HashMismatch => ApiError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "SKILL_REGISTRY_HASH_MISMATCH",
            "The official skill package failed integrity verification.",
            None,
        ),
        SkillRegistryError::NameConflict { skill_name } => ApiError::coded(
            StatusCode::CONFLICT,
            "SKILL_REGISTRY_NAME_CONFLICT",
            "A local skill already uses this name.",
            Some(json!({ "skill_name": skill_name })),
        ),
        SkillRegistryError::OperationInProgress => ApiError::coded(
            StatusCode::CONFLICT,
            "SKILL_REGISTRY_OPERATION_IN_PROGRESS",
            "An operation for this official skill is already running.",
            None,
        ),
        SkillRegistryError::Persistence => ApiError::Internal("SkillHub persistence failed".into()),
    }
}
