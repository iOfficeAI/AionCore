//! Agent-session operations on ConversationService.
//!
//! These forward to the active AgentInstance (via `self.task(id)`) for
//! mode/model/usage/slash-commands/side-question/openclaw-runtime queries,
//! plus workspace browsing that needs the conversations.extra.workspace
//! field.
//!
//! Kept in a separate file from service.rs to avoid pushing that file
//! over 2000 lines.

use std::path::Component;

use aionui_api_types::{
    AgentModeResponse, GetModelInfoResponse, SetModeRequest, SetModelRequest, SideQuestionRequest,
    SideQuestionResponse, SlashCommandItem, WorkspaceBrowseQuery, WorkspaceEntry,
};
use aionui_common::AppError;

use crate::service::ConversationService;

const MAX_DIR_DEPTH: usize = 10;

impl ConversationService {
    // ── Mode ────────────────────────────────────────────────────────

    pub async fn get_mode(&self, conversation_id: &str) -> Result<AgentModeResponse, AppError> {
        self.task(conversation_id)?.get_mode().await
    }

    pub async fn set_mode(&self, conversation_id: &str, req: SetModeRequest) -> Result<(), AppError> {
        if req.mode.trim().is_empty() {
            return Err(AppError::BadRequest("mode must not be empty".into()));
        }
        self.task(conversation_id)?.set_mode(&req.mode).await
    }

    // ── Model ───────────────────────────────────────────────────────

    pub async fn get_model(&self, conversation_id: &str) -> Result<GetModelInfoResponse, AppError> {
        self.task(conversation_id)?.get_model().await
    }

    pub async fn set_model(&self, conversation_id: &str, req: SetModelRequest) -> Result<(), AppError> {
        if req.model_id.trim().is_empty() {
            return Err(AppError::BadRequest("model_id must not be empty".into()));
        }
        let task = match self.task(conversation_id) {
            Ok(task) => task,
            Err(err) => {
                tracing::warn!(
                    conversation_id,
                    model_id = %req.model_id,
                    error_code = err.error_code(),
                    "Set model skipped because active agent task is unavailable"
                );
                return Err(err);
            }
        };
        task.set_model(&req.model_id).await
    }

    // ── Usage / Slash commands ──────────────────────────────────────

    pub async fn get_usage(&self, conversation_id: &str) -> Result<Option<serde_json::Value>, AppError> {
        self.task(conversation_id)?.get_usage().await
    }

    pub async fn get_slash_commands(&self, conversation_id: &str) -> Result<Vec<SlashCommandItem>, AppError> {
        self.task(conversation_id)?.get_slash_commands().await
    }

    // ── Side question ───────────────────────────────────────────────

    pub async fn handle_side_question(
        &self,
        conversation_id: &str,
        req: SideQuestionRequest,
    ) -> Result<SideQuestionResponse, AppError> {
        // `AgentInstance::handle_side_question` already validates that the
        // question is non-empty; no need to duplicate the check here.
        self.task(conversation_id)?.handle_side_question(req).await
    }

    // ── OpenClaw runtime diagnostics ────────────────────────────────

    pub async fn get_openclaw_runtime(&self, conversation_id: &str) -> Result<serde_json::Value, AppError> {
        self.task(conversation_id)?.get_openclaw_runtime().await
    }

    // ── Workspace browsing ──────────────────────────────────────────

    /// Enumerate entries under `query.path` inside the conversation's
    /// workspace root. Enforces workspace isolation (no traversal outside
    /// the root, with an allowance for symlinked sub-directories) and a
    /// depth cap of [`MAX_DIR_DEPTH`].
    pub async fn browse_workspace(
        &self,
        conversation_id: &str,
        query: WorkspaceBrowseQuery,
    ) -> Result<Vec<WorkspaceEntry>, AppError> {
        if query.path.trim().is_empty() {
            return Err(AppError::BadRequest("path must not be empty".into()));
        }

        let row = self
            .conversation_repo()
            .get(conversation_id)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to load conversation: {e}")))?
            .ok_or_else(|| AppError::NotFound(format!("Conversation '{conversation_id}' not found")))?;

        let extra: serde_json::Value =
            serde_json::from_str(&row.extra).map_err(|e| AppError::Internal(format!("Invalid extra JSON: {e}")))?;
        let workspace = extra
            .get("workspace")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        if workspace.is_empty() {
            return Err(AppError::BadRequest("Conversation has no workspace assigned".into()));
        }

        let relative_path = query.path.trim_start_matches('/');
        let relative_path_obj = std::path::Path::new(relative_path);
        if relative_path_obj
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(AppError::BadRequest(
                "Path traversal outside workspace is not allowed".into(),
            ));
        }

        // Resolve the browsed path relative to the workspace root
        let base = std::path::Path::new(&workspace);
        let browse_path = if relative_path.is_empty() {
            base.to_path_buf()
        } else {
            base.join(relative_path_obj)
        };

        // Security: reject direct traversal outside the workspace root, but allow
        // symlinked directories mounted inside the workspace (e.g. native skill
        // dirs that point at the builtin skills corpus under data-dir).
        let canonical_base = base
            .canonicalize()
            .map_err(|e| AppError::Internal(format!("Failed to resolve workspace path: {e}")))?;
        let canonical_browse = browse_path
            .canonicalize()
            .map_err(|_| AppError::NotFound("Directory not found".into()))?;
        if !browse_path.starts_with(base) && !canonical_browse.starts_with(&canonical_base) {
            return Err(AppError::BadRequest(
                "Path traversal outside workspace is not allowed".into(),
            ));
        }

        // Check depth limit
        let depth = relative_path_obj.components().count();
        if depth > MAX_DIR_DEPTH {
            return Err(AppError::BadRequest(format!(
                "Directory depth exceeds maximum of {MAX_DIR_DEPTH}"
            )));
        }

        let mut entries = Vec::new();
        let mut dir_reader = tokio::fs::read_dir(&canonical_browse)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to read directory: {e}")))?;

        while let Ok(Some(entry)) = dir_reader.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();

            // Apply search filter if provided
            if let Some(ref search) = query.search
                && !search.is_empty()
                && !name.to_lowercase().contains(&search.to_lowercase())
            {
                continue;
            }

            let entry_path = entry.path();
            let metadata = tokio::fs::metadata(&entry_path)
                .await
                .map_err(|e| AppError::Internal(format!("Failed to read entry metadata: {e}")))?;

            let entry_type = if metadata.is_dir() { "directory" } else { "file" };

            entries.push(WorkspaceEntry {
                name,
                entry_type: entry_type.into(),
            });
        }

        // Sort: directories first, then alphabetically
        entries.sort_by(|a, b| {
            let type_cmp = a.entry_type.cmp(&b.entry_type);
            if type_cmp == std::cmp::Ordering::Equal {
                a.name.to_lowercase().cmp(&b.name.to_lowercase())
            } else {
                type_cmp
            }
        });

        Ok(entries)
    }
}
