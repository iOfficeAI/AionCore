#![allow(clippy::disallowed_types)]

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path as AxumPath, State};
use axum::routing::{delete, get, post};
use tracing::warn;

use aionui_api_types::{
    AddExternalPathRequest, ApiResponse, ExportSkillRequest, ExternalSkillSourceResponse, ImportSkillFailureResponse,
    ImportSkillRequest, ImportSkillResponse, MaterializeSkillsRequest, MaterializeSkillsResponse, MaterializedSkillRef,
    NamedPathResponse, ReadAssistantRuleRequest, ReadBuiltinResourceRequest, ReadSkillInfoRequest,
    ReadSkillInfoResponse, RemoveExternalPathRequest, ScanForSkillsRequest, ScanForSkillsResponse,
    ScannedSkillResponse, SkillImportLimitsResponse, SkillImportRecordResponse, SkillListItemResponse,
    SkillPathsResponse, SkillSourceResponse, WriteAssistantRuleRequest,
};
use aionui_common::ApiError;
use aionui_db::ISkillRepository;

use crate::classifier::AssistantRuleDispatcher;
use crate::error::ExtensionError;
use crate::external_paths::ExternalPathsManager;
use crate::skill_service::{self, SkillPaths, SkillSource};

fn to_source_response(source: SkillSource) -> SkillSourceResponse {
    match source {
        SkillSource::Builtin => SkillSourceResponse::Builtin,
        SkillSource::Custom => SkillSourceResponse::Custom,
        SkillSource::Cron => SkillSourceResponse::Cron,
        SkillSource::Extension => SkillSourceResponse::Extension,
    }
}

// ---------------------------------------------------------------------------
// Router state
// ---------------------------------------------------------------------------

/// Shared state for skill/rule route handlers.
#[derive(Clone)]
pub struct SkillRouterState {
    pub skill_paths: SkillPaths,
    pub skill_repo: Arc<dyn ISkillRepository>,
    pub external_paths_manager: Arc<ExternalPathsManager>,
    /// Optional dispatcher that routes assistant-rule / assistant-skill
    /// read/write/delete by source (builtin / extension / user). When
    /// `None`, the legacy user-directory-only behavior is preserved.
    #[allow(clippy::type_complexity)]
    pub assistant_dispatcher: Option<Arc<dyn AssistantRuleDispatcher>>,
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the skill router with all `/api/skills/*` routes.
///
/// All routes require authentication (applied by the caller).
pub fn skill_routes(state: SkillRouterState) -> Router {
    Router::new()
        // Skill listing & info
        .route("/api/skills", get(list_skills))
        .route("/api/skills/import-history", get(list_import_history))
        .route("/api/skills/import-limits", get(get_import_limits))
        .route("/api/skills/info", post(read_skill_info))
        .route("/api/skills/paths", get(get_skill_paths))
        // Import / export / delete
        .route("/api/skills/import", post(import_skill))
        .route("/api/skills/export-symlink", post(export_skill_symlink))
        .route("/api/skills/{name}", delete(delete_skill))
        // Scanning & discovery
        .route("/api/skills/scan", post(scan_for_skills))
        .route("/api/skills/detect-paths", get(detect_paths))
        .route("/api/skills/detect-external", get(detect_external))
        // Built-in resources
        .route("/api/skills/builtin-rule", post(read_builtin_rule))
        .route("/api/skills/builtin-skill", post(read_builtin_skill))
        // Per-agent skill resolution (for agent CLI symlink layout).
        .route("/api/skills/materialize-for-agent", post(materialize_for_agent))
        // Assistant rules CRUD
        .route("/api/skills/assistant-rule/read", post(read_assistant_rule))
        .route("/api/skills/assistant-rule/write", post(write_assistant_rule))
        .route("/api/skills/assistant-rule/{id}", delete(delete_assistant_rule))
        // Assistant skills CRUD
        .route("/api/skills/assistant-skill/read", post(read_assistant_skill))
        .route("/api/skills/assistant-skill/write", post(write_assistant_skill))
        .route("/api/skills/assistant-skill/{id}", delete(delete_assistant_skill))
        // External path management
        .route(
            "/api/skills/external-paths",
            get(get_external_paths)
                .post(add_external_path)
                .delete(remove_external_path),
        )
        // Skills market
        .route("/api/skills/market/enable", post(enable_skills_market))
        .route("/api/skills/market/disable", post(disable_skills_market))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Skill listing & info
// ---------------------------------------------------------------------------

/// `GET /api/skills` — list all available skills.
async fn list_skills(
    State(state): State<SkillRouterState>,
) -> Result<Json<ApiResponse<Vec<SkillListItemResponse>>>, ApiError> {
    let items = skill_service::list_available_skills_with_repo(&state.skill_paths, state.skill_repo.as_ref()).await?;
    let resp: Vec<SkillListItemResponse> = items
        .into_iter()
        .map(|s| SkillListItemResponse {
            name: s.name,
            description: s.description,
            location: s.location,
            relative_location: s.relative_location,
            is_custom: s.is_custom,
            source: to_source_response(s.source),
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

/// `POST /api/skills/info` — read skill info without importing.
async fn read_skill_info(
    body: Result<Json<ReadSkillInfoRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ReadSkillInfoResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let (name, description) = skill_service::read_skill_info(Path::new(&req.skill_path)).await?;
    Ok(Json(ApiResponse::ok(ReadSkillInfoResponse { name, description })))
}

/// `GET /api/skills/paths` — get user and built-in skill directories.
async fn get_skill_paths(
    State(state): State<SkillRouterState>,
) -> Result<Json<ApiResponse<SkillPathsResponse>>, ApiError> {
    let (user_dir, builtin_dir) = skill_service::get_skill_paths(&state.skill_paths);
    Ok(Json(ApiResponse::ok(SkillPathsResponse {
        user_skills_dir: user_dir,
        builtin_skills_dir: builtin_dir,
    })))
}

/// `GET /api/skills/import-limits` — get server-side skill import limits.
async fn get_import_limits() -> Result<Json<ApiResponse<SkillImportLimitsResponse>>, ApiError> {
    let limits = skill_service::skill_import_limits();
    Ok(Json(ApiResponse::ok(SkillImportLimitsResponse {
        max_file_bytes: limits.max_file_bytes,
        max_total_bytes: limits.max_total_bytes,
    })))
}

// ---------------------------------------------------------------------------
// Import / export / delete
// ---------------------------------------------------------------------------

/// `POST /api/skills/import` — import skill directories or zip packages by copying.
async fn import_skill(
    State(state): State<SkillRouterState>,
    body: Result<Json<ImportSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ImportSkillResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let outcome = match skill_service::import_skills_with_repo(
        &state.skill_paths,
        state.skill_repo.as_ref(),
        Path::new(&req.skill_path),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            warn!(
                source_path = %req.skill_path,
                error = %err,
                "skill import failed"
            );
            return Err(err.into());
        }
    };
    if !outcome.failed.is_empty() {
        warn!(
            source_path = %req.skill_path,
            imported_count = outcome.imported.len(),
            failed_count = outcome.failed.len(),
            failures = ?outcome.failed,
            "skill batch import completed with failures"
        );
    }
    let names = outcome.imported;
    let first_name = names.first().cloned().unwrap_or_default();
    let failed = outcome
        .failed
        .into_iter()
        .map(|failure| ImportSkillFailureResponse {
            source_name: failure.source_name,
            code: failure.code,
            error_path: failure.error_path,
            actual_bytes: failure.actual_bytes,
            limit_bytes: failure.limit_bytes,
            line: failure.line,
            column: failure.column,
        })
        .collect();
    Ok(Json(ApiResponse::ok(ImportSkillResponse {
        skill_name: first_name,
        skill_names: names,
        failed,
    })))
}

/// `POST /api/skills/export-symlink` — export a skill symlink.
async fn export_skill_symlink(
    body: Result<Json<ExportSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    skill_service::export_skill_with_symlink(Path::new(&req.skill_path), Path::new(&req.target_dir)).await?;
    Ok(Json(ApiResponse::success()))
}

/// `DELETE /api/skills/:name` — delete a user-custom skill.
async fn delete_skill(
    State(state): State<SkillRouterState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    skill_service::delete_skill_with_repo(&state.skill_paths, state.skill_repo.as_ref(), &name).await?;
    Ok(Json(ApiResponse::success()))
}

/// `GET /api/skills/import-history` — list recent skill import records.
async fn list_import_history(
    State(state): State<SkillRouterState>,
) -> Result<Json<ApiResponse<Vec<SkillImportRecordResponse>>>, ApiError> {
    let records = state
        .skill_repo
        .list_import_records(100)
        .await
        .map_err(ExtensionError::from)?;
    let resp = records
        .into_iter()
        .map(|row| SkillImportRecordResponse {
            id: row.id,
            operation_id: row.operation_id,
            source_label: row.source_label,
            source_path: row.source_path,
            source_name: row.source_name,
            skill_id: row.skill_id,
            skill_name: row.skill_name,
            status: row.status,
            error_code: row.error_code,
            error_path: row.error_path,
            actual_bytes: row.actual_bytes,
            limit_bytes: row.limit_bytes,
            line: row.line,
            column: row.column,
            created_at: row.created_at,
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

// ---------------------------------------------------------------------------
// Scanning & discovery
// ---------------------------------------------------------------------------

/// `POST /api/skills/scan` — scan a directory for skills.
async fn scan_for_skills(
    body: Result<Json<ScanForSkillsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ScanForSkillsResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let skills = skill_service::scan_for_skills(Path::new(&req.folder_path)).await?;
    let resp = ScanForSkillsResponse {
        skills: skills
            .into_iter()
            .map(|s| ScannedSkillResponse {
                name: s.name,
                description: s.description,
                path: s.path,
            })
            .collect(),
    };
    Ok(Json(ApiResponse::ok(resp)))
}

/// `GET /api/skills/detect-paths` — detect common skill paths.
async fn detect_paths() -> Result<Json<ApiResponse<Vec<NamedPathResponse>>>, ApiError> {
    let paths = skill_service::detect_common_skill_paths().await;
    let resp: Vec<NamedPathResponse> = paths
        .into_iter()
        .map(|p| NamedPathResponse {
            name: p.name,
            path: p.path,
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

/// `GET /api/skills/detect-external` — discover external skills from all sources.
async fn detect_external(
    State(state): State<SkillRouterState>,
) -> Result<Json<ApiResponse<Vec<ExternalSkillSourceResponse>>>, ApiError> {
    let custom = state.external_paths_manager.get_custom_external_paths().await;
    let sources = skill_service::detect_and_count_external_skills(&custom).await;
    let resp: Vec<ExternalSkillSourceResponse> = sources
        .into_iter()
        .map(|s| ExternalSkillSourceResponse {
            name: s.name,
            path: s.path,
            source: s.source,
            skill_count: s.skill_count,
            skills: s
                .skills
                .into_iter()
                .map(|sk| ScannedSkillResponse {
                    name: sk.name,
                    description: sk.description,
                    path: sk.path,
                })
                .collect(),
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

// ---------------------------------------------------------------------------
// Built-in resources
// ---------------------------------------------------------------------------

/// `POST /api/skills/builtin-rule` — read a built-in rule file.
async fn read_builtin_rule(
    State(state): State<SkillRouterState>,
    body: Result<Json<ReadBuiltinResourceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let content = skill_service::read_builtin_rule(&state.skill_paths, &req.file_name).await?;
    Ok(Json(ApiResponse::ok(content)))
}

/// `POST /api/skills/builtin-skill` — read a built-in skill file.
async fn read_builtin_skill(
    State(state): State<SkillRouterState>,
    body: Result<Json<ReadBuiltinResourceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let content = skill_service::read_builtin_skill(&state.skill_paths, &req.file_name).await?;
    Ok(Json(ApiResponse::ok(content)))
}

/// `POST /api/skills/materialize-for-agent` — resolve each requested skill
/// name to its on-disk source directory. The frontend symlinks each
/// returned `source_path` into the agent CLI's native skills dir. The
/// backend no longer copies any files per-conversation.
async fn materialize_for_agent(
    State(state): State<SkillRouterState>,
    body: Result<Json<MaterializeSkillsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<MaterializeSkillsResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if req.conversation_id.trim().is_empty() {
        return Err(ApiError::BadRequest("conversationId must not be empty".into()));
    }
    let resolved = skill_service::materialize_skills_for_agent_with_repo(
        &state.skill_paths,
        state.skill_repo.as_ref(),
        &req.conversation_id,
        &req.skills,
    )
    .await?;
    let skills: Vec<MaterializedSkillRef> = resolved
        .into_iter()
        .map(|s| MaterializedSkillRef {
            name: s.name,
            source_path: s.source_path.to_string_lossy().into_owned(),
        })
        .collect();
    Ok(Json(ApiResponse::ok(MaterializeSkillsResponse { skills })))
}

// ---------------------------------------------------------------------------
// Assistant rules CRUD
// ---------------------------------------------------------------------------

/// `POST /api/skills/assistant-rule/read` — read an assistant rule.
///
/// Dispatches by source via [`AssistantRuleDispatcher`] when wired; falls
/// back to user-directory-only legacy behavior otherwise.
async fn read_assistant_rule(
    State(state): State<SkillRouterState>,
    body: Result<Json<ReadAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let content = dispatcher.read_rule(&req.assistant_id, req.locale.as_deref()).await?;
        return Ok(Json(ApiResponse::ok(content)));
    }
    let content =
        skill_service::read_assistant_rule(&state.skill_paths, &req.assistant_id, req.locale.as_deref()).await?;
    Ok(Json(ApiResponse::ok(content)))
}

/// `POST /api/skills/assistant-rule/write` — write an assistant rule.
///
/// Dispatches by source: builtin / extension ids reject with 400.
async fn write_assistant_rule(
    State(state): State<SkillRouterState>,
    body: Result<Json<WriteAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<bool>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        dispatcher
            .write_rule(&req.assistant_id, req.locale.as_deref(), &req.content)
            .await?;
        return Ok(Json(ApiResponse::ok(true)));
    }
    let ok = skill_service::write_assistant_rule(
        &state.skill_paths,
        &req.assistant_id,
        &req.content,
        req.locale.as_deref(),
    )
    .await?;
    Ok(Json(ApiResponse::ok(ok)))
}

/// `DELETE /api/skills/assistant-rule/:id` — delete all locale versions.
async fn delete_assistant_rule(
    State(state): State<SkillRouterState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ApiResponse<bool>>, ApiError> {
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let ok = dispatcher.delete_rule(&id).await?;
        return Ok(Json(ApiResponse::ok(ok)));
    }
    let ok = skill_service::delete_assistant_rule(&state.skill_paths, &id).await?;
    Ok(Json(ApiResponse::ok(ok)))
}

// ---------------------------------------------------------------------------
// Assistant skills CRUD
// ---------------------------------------------------------------------------

/// `POST /api/skills/assistant-skill/read` — read an assistant skill.
///
/// Dispatches by source via [`AssistantRuleDispatcher`] when wired.
async fn read_assistant_skill(
    State(state): State<SkillRouterState>,
    body: Result<Json<ReadAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let content = dispatcher.read_skill(&req.assistant_id, req.locale.as_deref()).await?;
        return Ok(Json(ApiResponse::ok(content)));
    }
    let content =
        skill_service::read_assistant_skill(&state.skill_paths, &req.assistant_id, req.locale.as_deref()).await?;
    Ok(Json(ApiResponse::ok(content)))
}

/// `POST /api/skills/assistant-skill/write` — write an assistant skill.
///
/// Dispatches by source: builtin / extension ids reject with 400.
async fn write_assistant_skill(
    State(state): State<SkillRouterState>,
    body: Result<Json<WriteAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<bool>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        dispatcher
            .write_skill(&req.assistant_id, req.locale.as_deref(), &req.content)
            .await?;
        return Ok(Json(ApiResponse::ok(true)));
    }
    let ok = skill_service::write_assistant_skill(
        &state.skill_paths,
        &req.assistant_id,
        &req.content,
        req.locale.as_deref(),
    )
    .await?;
    Ok(Json(ApiResponse::ok(ok)))
}

/// `DELETE /api/skills/assistant-skill/:id` — delete all locale versions.
async fn delete_assistant_skill(
    State(state): State<SkillRouterState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ApiResponse<bool>>, ApiError> {
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let ok = dispatcher.delete_skill(&id).await?;
        return Ok(Json(ApiResponse::ok(ok)));
    }
    let ok = skill_service::delete_assistant_skill(&state.skill_paths, &id).await?;
    Ok(Json(ApiResponse::ok(ok)))
}

// ---------------------------------------------------------------------------
// External path management
// ---------------------------------------------------------------------------

/// `GET /api/skills/external-paths` — list custom external paths.
async fn get_external_paths(
    State(state): State<SkillRouterState>,
) -> Result<Json<ApiResponse<Vec<NamedPathResponse>>>, ApiError> {
    let paths = state.external_paths_manager.get_custom_external_paths().await;
    let resp: Vec<NamedPathResponse> = paths
        .into_iter()
        .map(|p| NamedPathResponse {
            name: p.name,
            path: p.path,
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

/// `POST /api/skills/external-paths` — add a custom external path.
async fn add_external_path(
    State(state): State<SkillRouterState>,
    body: Result<Json<AddExternalPathRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .external_paths_manager
        .add_custom_external_path(&req.name, &req.path)
        .await?;
    Ok(Json(ApiResponse::success()))
}

/// `DELETE /api/skills/external-paths` — remove a custom external path.
async fn remove_external_path(
    State(state): State<SkillRouterState>,
    body: Result<Json<RemoveExternalPathRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .external_paths_manager
        .remove_custom_external_path(&req.path)
        .await?;
    Ok(Json(ApiResponse::success()))
}

// ---------------------------------------------------------------------------
// Skills market
// ---------------------------------------------------------------------------

/// `POST /api/skills/market/enable` — enable the aionui skills market.
async fn enable_skills_market(State(state): State<SkillRouterState>) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.external_paths_manager.enable_skills_market().await?;
    Ok(Json(ApiResponse::success()))
}

/// `POST /api/skills/market/disable` — disable the aionui skills market.
async fn disable_skills_market(State(state): State<SkillRouterState>) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.external_paths_manager.disable_skills_market().await?;
    Ok(Json(ApiResponse::success()))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn make_state() -> SkillRouterState {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = SkillPaths {
            data_dir: tmp.path().to_path_buf(),
            user_skills_dir: tmp.path().join("skills"),
            cron_skills_dir: tmp.path().join("cron").join("skills"),
            builtin_skills_dir: tmp.path().join("builtin-skills"),
            builtin_rules_dir: tmp.path().join("builtin-rules"),
            assistant_rules_dir: tmp.path().join("assistant-rules"),
            assistant_skills_dir: tmp.path().join("assistant-skills"),
        };
        let ext_mgr = Arc::new(ExternalPathsManager::with_file(tmp.path().join("paths.json")).await);
        let db = aionui_db::init_database_memory().await.unwrap();
        let skill_repo = Arc::new(aionui_db::SqliteSkillRepository::new(db.pool().clone()));
        std::mem::forget(tmp);
        SkillRouterState {
            skill_paths: paths,
            skill_repo,
            external_paths_manager: ext_mgr,
            assistant_dispatcher: None,
        }
    }

    #[tokio::test]
    async fn skill_routes_builds_router() {
        let state = make_state().await;
        let _router = skill_routes(state);
    }

    #[tokio::test]
    async fn builtin_auto_skill_list_get_is_not_registered() {
        let state = make_state().await;
        let response = skill_routes(state)
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/skills/builtin-auto")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
