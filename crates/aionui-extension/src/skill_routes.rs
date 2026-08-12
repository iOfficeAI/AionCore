#![allow(clippy::disallowed_types)]

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path as AxumPath, State};
use axum::http::StatusCode;
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
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use aionui_db::{ISkillRepository, SiteRole};

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

fn is_auto_inject_builtin_skill(source: SkillSource, relative_location: Option<&str>) -> bool {
    source == SkillSource::Builtin && relative_location.is_some_and(|location| location.starts_with("auto-inject/"))
}

fn admin_required(user: &CurrentUser) -> Result<(), ApiError> {
    if user.site_role != SiteRole::Admin {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "ADMIN_REQUIRED",
            "Administrator access required.",
            None,
        ));
    }
    Ok(())
}

fn user_filesystem_denied() -> ApiError {
    ApiError::coded(
        StatusCode::FORBIDDEN,
        "USER_FILESYSTEM_DENIED",
        "User filesystem access denied.",
        None,
    )
}

fn normalized_absolute_path(path: &Path) -> Result<PathBuf, ApiError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| user_filesystem_denied())?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => return Err(user_filesystem_denied()),
            Component::Normal(name) => normalized.push(name),
        }
    }
    Ok(normalized)
}

fn user_dir_name(user: &CurrentUser) -> Result<String, ApiError> {
    aionui_common::user_dir_name(&user.id).map_err(|_| user_filesystem_denied())
}

fn own_managed_roots(state: &SkillRouterState, user: &CurrentUser) -> Result<Vec<PathBuf>, ApiError> {
    let user_dir = user_dir_name(user)?;
    Ok(vec![
        state.skill_paths.user_skills_dir.join("users").join(&user_dir),
        state.skill_paths.data_dir.join("uploads").join("users").join(&user_dir),
        state
            .skill_paths
            .data_dir
            .join("user-workspaces")
            .join("conversations")
            .join("users")
            .join(&user_dir),
        state.skill_paths.assistant_rules_dir.join("users").join(&user_dir),
        state.skill_paths.assistant_skills_dir.join("users").join(user_dir),
    ])
}

fn private_managed_roots(state: &SkillRouterState) -> Vec<PathBuf> {
    vec![
        state.skill_paths.user_skills_dir.join("users"),
        state.skill_paths.data_dir.join("uploads").join("users"),
        state
            .skill_paths
            .data_dir
            .join("user-workspaces")
            .join("conversations")
            .join("users"),
        state.skill_paths.assistant_rules_dir.join("users"),
        state.skill_paths.assistant_skills_dir.join("users"),
        state.skill_paths.cron_skills_dir.clone(),
    ]
}

async fn path_matches_root(raw_path: &Path, canonical_path: &Path, root: &Path) -> bool {
    let Ok(raw_root) = normalized_absolute_path(root) else {
        return false;
    };
    let Ok(canonical_root) = tokio::fs::canonicalize(root).await else {
        return false;
    };
    raw_path.starts_with(raw_root) && canonical_path.starts_with(canonical_root)
}

async fn canonical_path_matches_root(canonical_path: &Path, root: &Path) -> bool {
    tokio::fs::canonicalize(root)
        .await
        .is_ok_and(|canonical_root| canonical_path.starts_with(canonical_root))
}

async fn resolve_path_for_authorization(raw_path: &Path) -> Result<PathBuf, ApiError> {
    if let Ok(canonical_path) = tokio::fs::canonicalize(raw_path).await {
        return Ok(canonical_path);
    }

    // Preserve domain-level not-found behavior without trusting lexical
    // ancestry: canonicalize the nearest existing ancestor, then append only
    // the already-normalized missing suffix.
    let mut ancestor = raw_path;
    let mut suffix = Vec::new();
    loop {
        if let Ok(mut canonical_ancestor) = tokio::fs::canonicalize(ancestor).await {
            for component in suffix.iter().rev() {
                canonical_ancestor.push(component);
            }
            return Ok(canonical_ancestor);
        }
        let Some(name) = ancestor.file_name() else {
            return Err(user_filesystem_denied());
        };
        suffix.push(name.to_os_string());
        ancestor = ancestor.parent().ok_or_else(user_filesystem_denied)?;
    }
}

/// Resolve a client path and enforce per-user managed roots. Administrators
/// may use host infrastructure paths, but never another user's managed data.
async fn authorize_skill_path(
    state: &SkillRouterState,
    user: &CurrentUser,
    requested_path: &Path,
) -> Result<PathBuf, ApiError> {
    let raw_path = normalized_absolute_path(requested_path)?;
    let canonical_path = resolve_path_for_authorization(&raw_path).await?;
    let own_roots = own_managed_roots(state, user)?;

    for root in &own_roots {
        if path_matches_root(&raw_path, &canonical_path, root).await {
            return Ok(canonical_path);
        }
    }

    if user.site_role != SiteRole::Admin {
        return Err(user_filesystem_denied());
    }

    // An administrator may reach their own managed root through a host alias,
    // but must not use either a lexical path or a symlink target to reach a
    // different user's private tree.
    for root in &own_roots {
        if canonical_path_matches_root(&canonical_path, root).await {
            return Ok(canonical_path);
        }
    }
    for root in private_managed_roots(state) {
        let raw_root = normalized_absolute_path(&root)?;
        if raw_path.starts_with(raw_root) || canonical_path_matches_root(&canonical_path, &root).await {
            return Err(user_filesystem_denied());
        }
    }

    Ok(canonical_path)
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
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<SkillListItemResponse>>>, ApiError> {
    let items = skill_service::list_available_skills_with_repo_for_user(
        &state.skill_paths,
        state.skill_repo.as_ref(),
        &current_user.id,
    )
    .await?;
    let resp: Vec<SkillListItemResponse> = items
        .into_iter()
        .map(|s| SkillListItemResponse {
            is_auto_inject: is_auto_inject_builtin_skill(s.source, s.relative_location.as_deref()),
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
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<ReadSkillInfoRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ReadSkillInfoResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let skill_path = authorize_skill_path(&state, &current_user, Path::new(&req.skill_path)).await?;
    let (name, description) = skill_service::read_skill_info(&skill_path).await?;
    Ok(Json(ApiResponse::ok(ReadSkillInfoResponse { name, description })))
}

/// `GET /api/skills/paths` — get user and built-in skill directories.
async fn get_skill_paths(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<SkillPathsResponse>>, ApiError> {
    let (user_dir, builtin_dir) = if current_user.site_role == SiteRole::Admin {
        skill_service::get_skill_paths(&state.skill_paths)
    } else {
        let own_root = own_managed_roots(&state, &current_user)?
            .into_iter()
            .next()
            .ok_or_else(user_filesystem_denied)?;
        (
            own_root.to_string_lossy().into_owned(),
            state.skill_paths.builtin_skills_dir.to_string_lossy().into_owned(),
        )
    };
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
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<ImportSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ImportSkillResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let source_path = authorize_skill_path(&state, &current_user, Path::new(&req.skill_path)).await?;
    let outcome = match skill_service::import_skills_with_repo_for_user(
        &state.skill_paths,
        state.skill_repo.as_ref(),
        &current_user.id,
        &source_path,
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
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<ExportSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    admin_required(&current_user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let skill_path = authorize_skill_path(&state, &current_user, Path::new(&req.skill_path)).await?;
    let target_dir = authorize_skill_path(&state, &current_user, Path::new(&req.target_dir)).await?;
    skill_service::export_skill_with_symlink(&skill_path, &target_dir).await?;
    Ok(Json(ApiResponse::success()))
}

/// `DELETE /api/skills/:name` — delete a user-custom skill.
async fn delete_skill(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    skill_service::delete_skill_with_repo_for_user(
        &state.skill_paths,
        state.skill_repo.as_ref(),
        &current_user.id,
        &name,
    )
    .await?;
    Ok(Json(ApiResponse::success()))
}

/// `GET /api/skills/import-history` — list recent skill import records.
async fn list_import_history(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<SkillImportRecordResponse>>>, ApiError> {
    let records = state
        .skill_repo
        .list_import_records_for_user(&current_user.id, 100)
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
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<ScanForSkillsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ScanForSkillsResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let folder_path = authorize_skill_path(&state, &current_user, Path::new(&req.folder_path)).await?;
    let skills = skill_service::scan_for_skills(&folder_path).await?;
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
async fn detect_paths(
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<NamedPathResponse>>>, ApiError> {
    admin_required(&current_user)?;
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
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<ExternalSkillSourceResponse>>>, ApiError> {
    admin_required(&current_user)?;
    let custom = state.external_paths_manager.get_custom_external_paths().await;
    let mut authorized_custom = Vec::new();
    for path in custom {
        if authorize_skill_path(&state, &current_user, Path::new(&path.path))
            .await
            .is_ok()
        {
            authorized_custom.push(path);
        }
    }
    let mut authorized_common = Vec::new();
    for path in skill_service::detect_common_skill_paths().await {
        if let Ok(canonical_path) = authorize_skill_path(&state, &current_user, Path::new(&path.path)).await {
            authorized_common.push(canonical_path);
        }
    }
    let sources = skill_service::detect_and_count_external_skills_with_allowed_common_paths(
        &authorized_custom,
        &authorized_common,
    )
    .await;
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
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<MaterializeSkillsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<MaterializeSkillsResponse>>, ApiError> {
    admin_required(&current_user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    if req.conversation_id.trim().is_empty() {
        return Err(ApiError::BadRequest("conversationId must not be empty".into()));
    }
    let resolved = skill_service::materialize_skills_for_agent_with_repo_for_user(
        &state.skill_paths,
        state.skill_repo.as_ref(),
        &current_user.id,
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
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<ReadAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let content = dispatcher
            .read_rule(&current_user.id, &req.assistant_id, req.locale.as_deref())
            .await?;
        return Ok(Json(ApiResponse::ok(content)));
    }
    tracing::warn!(
        assistant_id = %req.assistant_id,
        "assistant_dispatcher not configured; using unscoped legacy assistant-rule read fallback (must not happen in production)"
    );
    let content =
        skill_service::read_assistant_rule(&state.skill_paths, &req.assistant_id, req.locale.as_deref()).await?;
    Ok(Json(ApiResponse::ok(content)))
}

/// `POST /api/skills/assistant-rule/write` — write an assistant rule.
///
/// Dispatches by source: builtin / extension ids reject with 400.
async fn write_assistant_rule(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<WriteAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<bool>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        dispatcher
            .write_rule(&current_user.id, &req.assistant_id, req.locale.as_deref(), &req.content)
            .await?;
        return Ok(Json(ApiResponse::ok(true)));
    }
    tracing::warn!(
        assistant_id = %req.assistant_id,
        "assistant_dispatcher not configured; using unscoped legacy assistant-rule write fallback (must not happen in production)"
    );
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
    Extension(current_user): Extension<CurrentUser>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ApiResponse<bool>>, ApiError> {
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let ok = dispatcher.delete_rule(&current_user.id, &id).await?;
        return Ok(Json(ApiResponse::ok(ok)));
    }
    tracing::warn!(
        assistant_id = %id,
        "assistant_dispatcher not configured; using unscoped legacy assistant-rule delete fallback (must not happen in production)"
    );
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
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<ReadAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let content = dispatcher
            .read_skill(&current_user.id, &req.assistant_id, req.locale.as_deref())
            .await?;
        return Ok(Json(ApiResponse::ok(content)));
    }
    tracing::warn!(
        assistant_id = %req.assistant_id,
        "assistant_dispatcher not configured; using unscoped legacy assistant-skill read fallback (must not happen in production)"
    );
    let content =
        skill_service::read_assistant_skill(&state.skill_paths, &req.assistant_id, req.locale.as_deref()).await?;
    Ok(Json(ApiResponse::ok(content)))
}

/// `POST /api/skills/assistant-skill/write` — write an assistant skill.
///
/// Dispatches by source: builtin / extension ids reject with 400.
async fn write_assistant_skill(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<WriteAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<bool>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        dispatcher
            .write_skill(&current_user.id, &req.assistant_id, req.locale.as_deref(), &req.content)
            .await?;
        return Ok(Json(ApiResponse::ok(true)));
    }
    tracing::warn!(
        assistant_id = %req.assistant_id,
        "assistant_dispatcher not configured; using unscoped legacy assistant-skill write fallback (must not happen in production)"
    );
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
    Extension(current_user): Extension<CurrentUser>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ApiResponse<bool>>, ApiError> {
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let ok = dispatcher.delete_skill(&current_user.id, &id).await?;
        return Ok(Json(ApiResponse::ok(ok)));
    }
    tracing::warn!(
        assistant_id = %id,
        "assistant_dispatcher not configured; using unscoped legacy assistant-skill delete fallback (must not happen in production)"
    );
    let ok = skill_service::delete_assistant_skill(&state.skill_paths, &id).await?;
    Ok(Json(ApiResponse::ok(ok)))
}

// ---------------------------------------------------------------------------
// External path management
// ---------------------------------------------------------------------------

/// `GET /api/skills/external-paths` — list custom external paths.
async fn get_external_paths(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<NamedPathResponse>>>, ApiError> {
    admin_required(&current_user)?;
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
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<AddExternalPathRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    admin_required(&current_user)?;
    let Json(req) = body.map_err(ApiError::from)?;
    authorize_skill_path(&state, &current_user, Path::new(&req.path)).await?;
    state
        .external_paths_manager
        .add_custom_external_path(&req.name, &req.path)
        .await?;
    Ok(Json(ApiResponse::success()))
}

/// `DELETE /api/skills/external-paths` — remove a custom external path.
async fn remove_external_path(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<RemoveExternalPathRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    admin_required(&current_user)?;
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
async fn enable_skills_market(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    admin_required(&current_user)?;
    state.external_paths_manager.enable_skills_market().await?;
    Ok(Json(ApiResponse::success()))
}

/// `POST /api/skills/market/disable` — disable the aionui skills market.
async fn disable_skills_market(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    admin_required(&current_user)?;
    state.external_paths_manager.disable_skills_market().await?;
    Ok(Json(ApiResponse::success()))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::{UserStatus, UserType};
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
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

    fn current_user(id: &str, site_role: SiteRole) -> CurrentUser {
        CurrentUser {
            id: id.to_owned(),
            username: id.to_owned(),
            user_type: UserType::Local,
            status: UserStatus::Active,
            site_role,
            must_change_password: false,
        }
    }

    fn write_skill(path: &Path, name: &str) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Test skill\n---\n\n# {name}\n"),
        )
        .unwrap();
    }

    fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    async fn response_code(response: axum::response::Response) -> String {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["code"]
            .as_str()
            .unwrap()
            .to_owned()
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

    #[tokio::test]
    async fn member_cannot_manage_host_skill_infrastructure() {
        let state = make_state().await;
        let app = skill_routes(state).layer(Extension(current_user("user_member-a", SiteRole::Member)));
        let requests = [
            Request::builder()
                .uri("/api/skills/external-paths")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .uri("/api/skills/detect-paths")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .uri("/api/skills/detect-external")
                .body(Body::empty())
                .unwrap(),
            json_request("POST", "/api/skills/market/enable", serde_json::json!({})),
            json_request("POST", "/api/skills/export-symlink", serde_json::json!({})),
            json_request("POST", "/api/skills/materialize-for-agent", serde_json::json!({})),
        ];

        for request in requests {
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(response_code(response).await, "ADMIN_REQUIRED");
        }
    }

    #[tokio::test]
    async fn member_export_cannot_replace_an_existing_target() {
        let state = make_state().await;
        let host_temp = tempfile::tempdir().unwrap();
        let source = host_temp.path().join("source-skill");
        let target = host_temp.path().join("agent-skills");
        let existing = target.join("source-skill");
        write_skill(&source, "source-skill");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("sentinel.txt"), "keep").unwrap();

        let app = skill_routes(state).layer(Extension(current_user("user_member-a", SiteRole::Member)));
        let response = app
            .oneshot(json_request(
                "POST",
                "/api/skills/export-symlink",
                serde_json::json!({ "skill_path": source, "target_dir": target }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response_code(response).await, "ADMIN_REQUIRED");
        assert_eq!(std::fs::read_to_string(existing.join("sentinel.txt")).unwrap(), "keep");
    }

    #[tokio::test]
    async fn member_import_is_limited_to_their_managed_roots() {
        let state = make_state().await;
        let own_upload = state
            .skill_paths
            .data_dir
            .join("uploads/users/system_default_user/source-skill");
        let foreign_upload = state.skill_paths.data_dir.join("uploads/users/member-b/foreign-skill");
        write_skill(&own_upload, "owned-skill");
        write_skill(&foreign_upload, "foreign-skill");
        let host_temp = tempfile::tempdir().unwrap();
        let host_skill = host_temp.path().join("host-skill");
        write_skill(&host_skill, "host-skill");

        // The in-memory database seeds this identity, which lets the import
        // exercise its ownership foreign key while the injected live role is
        // intentionally Member.
        let app = skill_routes(state.clone()).layer(Extension(current_user("system_default_user", SiteRole::Member)));
        let own_response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/skills/import",
                serde_json::json!({ "skill_path": own_upload }),
            ))
            .await
            .unwrap();
        let own_status = own_response.status();
        let own_body = to_bytes(own_response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            own_status,
            StatusCode::OK,
            "managed import failed: {}",
            String::from_utf8_lossy(&own_body)
        );
        assert!(
            state
                .skill_paths
                .user_skills_dir
                .join("users/system_default_user/owned-skill/SKILL.md")
                .is_file()
        );

        for denied_path in [&foreign_upload, &host_skill] {
            let response = app
                .clone()
                .oneshot(json_request(
                    "POST",
                    "/api/skills/import",
                    serde_json::json!({ "skill_path": denied_path }),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(response_code(response).await, "USER_FILESYSTEM_DENIED");
        }

        let paths_response = app
            .oneshot(Request::builder().uri("/api/skills/paths").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(paths_response.status(), StatusCode::OK);
        let body = to_bytes(paths_response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            payload["data"]["user_skills_dir"],
            state
                .skill_paths
                .user_skills_dir
                .join("users/system_default_user")
                .to_string_lossy()
                .as_ref()
        );
    }

    #[tokio::test]
    async fn admin_can_use_host_paths_but_not_foreign_managed_content() {
        let state = make_state().await;
        let host_temp = tempfile::tempdir().unwrap();
        let host_skill = host_temp.path().join("host-skill");
        write_skill(&host_skill, "host-skill");
        let own_skill = state.skill_paths.data_dir.join("uploads/users/admin-a/own-skill");
        let foreign_skill = state.skill_paths.data_dir.join("uploads/users/member-b/foreign-skill");
        write_skill(&own_skill, "own-skill");
        write_skill(&foreign_skill, "foreign-skill");

        let app = skill_routes(state).layer(Extension(current_user("user_admin-a", SiteRole::Admin)));
        for allowed_path in [&host_skill, &own_skill] {
            let response = app
                .clone()
                .oneshot(json_request(
                    "POST",
                    "/api/skills/info",
                    serde_json::json!({ "skill_path": allowed_path }),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        let foreign_response = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/skills/info",
                serde_json::json!({ "skill_path": foreign_skill }),
            ))
            .await
            .unwrap();
        assert_eq!(foreign_response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response_code(foreign_response).await, "USER_FILESYSTEM_DENIED");

        let external_path_response = app
            .oneshot(json_request(
                "POST",
                "/api/skills/external-paths",
                serde_json::json!({ "name": "Host Skills", "path": host_temp.path() }),
            ))
            .await
            .unwrap();
        assert_eq!(external_path_response.status(), StatusCode::OK);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_cannot_escape_or_alias_foreign_managed_roots() {
        use std::os::unix::fs::symlink;

        let state = make_state().await;
        let host_temp = tempfile::tempdir().unwrap();
        let host_skill = host_temp.path().join("host-skill");
        write_skill(&host_skill, "host-skill");
        let own_upload_root = state.skill_paths.data_dir.join("uploads/users/member-a");
        std::fs::create_dir_all(&own_upload_root).unwrap();
        let escape_link = own_upload_root.join("escape");
        symlink(&host_skill, &escape_link).unwrap();

        let member_app = skill_routes(state.clone()).layer(Extension(current_user("user_member-a", SiteRole::Member)));
        let escape_response = member_app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/skills/info",
                serde_json::json!({ "skill_path": escape_link }),
            ))
            .await
            .unwrap();
        assert_eq!(escape_response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response_code(escape_response).await, "USER_FILESYSTEM_DENIED");

        let scan_response = member_app
            .oneshot(json_request(
                "POST",
                "/api/skills/scan",
                serde_json::json!({ "folder_path": own_upload_root }),
            ))
            .await
            .unwrap();
        assert_eq!(scan_response.status(), StatusCode::OK);
        let scan_body = to_bytes(scan_response.into_body(), usize::MAX).await.unwrap();
        let scan_payload: serde_json::Value = serde_json::from_slice(&scan_body).unwrap();
        assert_eq!(scan_payload["data"]["skills"], serde_json::json!([]));

        let foreign_skill = state.skill_paths.data_dir.join("uploads/users/member-b/foreign-skill");
        write_skill(&foreign_skill, "foreign-skill");
        let foreign_alias = host_temp.path().join("foreign-alias");
        symlink(&foreign_skill, &foreign_alias).unwrap();
        let admin_app = skill_routes(state).layer(Extension(current_user("user_admin-a", SiteRole::Admin)));
        let alias_response = admin_app
            .oneshot(json_request(
                "POST",
                "/api/skills/info",
                serde_json::json!({ "skill_path": foreign_alias }),
            ))
            .await
            .unwrap();
        assert_eq!(alias_response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response_code(alias_response).await, "USER_FILESYSTEM_DENIED");
    }
}
