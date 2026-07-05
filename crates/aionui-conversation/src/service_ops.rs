//! Agent-session operations on ConversationService.
//!
//! These forward to the active AgentInstance (via `self.task(id)`) for
//! config-options/usage/slash-commands/side-question queries, plus workspace
//! browsing that needs the conversations.extra.workspace field.
//!
//! Kept in a separate file from service.rs to avoid pushing that file
//! over 2000 lines.

use std::path::{Component, Path, PathBuf};

use aionui_ai_agent::{AcpError, AgentError};
use aionui_api_types::{
    ConfigOptionConfirmation, GetConfigOptionsResponse, SetConfigOptionRequest, SetConfigOptionResponse,
    SideQuestionRequest, SideQuestionResponse, SlashCommandItem, WorkspaceBrowseQuery, WorkspaceEntry,
    WorkspaceSearchMatchKind, WorkspaceSearchMode,
};
use aionui_common::{AgentKillReason, ErrorChain};
use ignore::{DirEntry, WalkBuilder};
use tracing::warn;

use crate::ConversationError;
use crate::service::{AssistantRuntimePreferenceUpdate, ConversationService};

const MAX_DIR_DEPTH: usize = 10;
const MAX_WORKSPACE_SEARCH_RESULTS: usize = 200;
const MAX_WORKSPACE_SEARCH_SCANNED: usize = 5000;
const MAX_WORKSPACE_SEARCH_FILE_BYTES: u64 = 512 * 1024;
const WORKSPACE_SEARCH_PRUNED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".cache",
    ".next",
    ".turbo",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "out",
    "target",
];

fn workspace_search_matches_content(path: &Path, needle: &str) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() > MAX_WORKSPACE_SEARCH_FILE_BYTES {
        return false;
    }

    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content.to_lowercase().contains(needle)
}

fn workspace_search_entry(
    base: &Path,
    path: &Path,
    is_dir: bool,
    match_kind: WorkspaceSearchMatchKind,
) -> Option<WorkspaceEntry> {
    let relative = path.strip_prefix(base).ok()?;
    let name = relative.to_string_lossy().replace('\\', "/");
    if name.is_empty() {
        return None;
    }

    Some(WorkspaceEntry {
        name,
        entry_type: if is_dir { "directory" } else { "file" }.into(),
        match_kind: Some(match_kind),
    })
}

fn should_prune_workspace_search_entry(entry: &DirEntry, search_root: &Path) -> bool {
    if entry.path() == search_root {
        return false;
    }

    if !entry.file_type().map(|file_type| file_type.is_dir()).unwrap_or(false) {
        return false;
    }

    let Some(name) = entry.file_name().to_str() else {
        return false;
    };

    WORKSPACE_SEARCH_PRUNED_DIRS.contains(&name)
}

fn search_workspace_entries_sync(
    base: PathBuf,
    search_root: PathBuf,
    search: String,
    search_mode: WorkspaceSearchMode,
) -> Vec<WorkspaceEntry> {
    let needle = search.to_lowercase();
    let mut entries = Vec::new();
    let mut scanned = 0usize;

    let mut walker_builder = WalkBuilder::new(&search_root);
    let filter_search_root = search_root.clone();
    walker_builder
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .filter_entry(move |entry| !should_prune_workspace_search_entry(entry, &filter_search_root));
    let walker = walker_builder.build();

    for entry in walker {
        if entries.len() >= MAX_WORKSPACE_SEARCH_RESULTS || scanned >= MAX_WORKSPACE_SEARCH_SCANNED {
            break;
        }

        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if path == search_root {
            continue;
        }

        scanned += 1;
        let Some(file_type) = entry.file_type() else {
            continue;
        };

        let is_dir = file_type.is_dir();
        let is_file = file_type.is_file();
        let name_matches = !matches!(search_mode, WorkspaceSearchMode::Content)
            && (entry.file_name().to_string_lossy().to_lowercase().contains(&needle)
                || path
                    .strip_prefix(&base)
                    .ok()
                    .map(|relative| relative.to_string_lossy().to_lowercase().contains(&needle))
                    .unwrap_or(false));
        let content_matches = !matches!(search_mode, WorkspaceSearchMode::Name)
            && is_file
            && workspace_search_matches_content(path, &needle);

        let match_kind = if name_matches {
            Some(WorkspaceSearchMatchKind::Name)
        } else if content_matches {
            Some(WorkspaceSearchMatchKind::Content)
        } else {
            None
        };

        if let Some(match_kind) = match_kind
            && let Some(search_entry) = workspace_search_entry(&base, path, is_dir, match_kind)
        {
            entries.push(search_entry);
        }
    }

    entries.sort_by(|a, b| {
        let match_cmp = search_match_rank(a.match_kind).cmp(&search_match_rank(b.match_kind));
        if match_cmp != std::cmp::Ordering::Equal {
            return match_cmp;
        }

        let type_cmp = a.entry_type.cmp(&b.entry_type);
        if type_cmp == std::cmp::Ordering::Equal {
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        } else {
            type_cmp
        }
    });

    entries
}

fn search_match_rank(match_kind: Option<WorkspaceSearchMatchKind>) -> u8 {
    match match_kind {
        Some(WorkspaceSearchMatchKind::Name) => 0,
        Some(WorkspaceSearchMatchKind::Content) => 1,
        None => 2,
    }
}

impl ConversationService {
    // ── Config Options ──────────────────────────────────────────────

    pub async fn get_config_options(
        &self,
        conversation_id: &str,
    ) -> Result<GetConfigOptionsResponse, ConversationError> {
        self.task(conversation_id)?
            .get_config_options()
            .await
            .map_err(ConversationError::from)
    }

    pub async fn set_config_option(
        &self,
        conversation_id: &str,
        option_id: &str,
        req: SetConfigOptionRequest,
    ) -> Result<SetConfigOptionResponse, ConversationError> {
        if option_id.trim().is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "option_id must not be empty".into(),
            });
        }
        if req.value.trim().is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "value must not be empty".into(),
            });
        }
        let agent = self.task(conversation_id)?;
        let response = match agent.set_config_option(option_id, &req.value).await {
            Ok(response) => response,
            Err(err @ AgentError::Acp(AcpError::NotConnected)) => {
                warn!(
                    conversation_id,
                    option_id,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    error = %ErrorChain(&err),
                    "ACP config option failed because protocol is disconnected; evicting task"
                );
                self.task_manager()
                    .kill_and_wait(conversation_id, Some(AgentKillReason::AgentErrorRecovery))
                    .await;
                return Err(ConversationError::from(err));
            }
            Err(err) => return Err(ConversationError::from(err)),
        };

        // Mirror runtime model/mode/thought-level switches into the persisted assistant
        // snapshot + preference so the next conversation seeded from this
        // assistant in `auto` mode reflects the latest pick. We only act on
        // observed confirmations — `command_ack` means the agent merely
        // accepted the request, not that the value is in effect. Persistence
        // failures are logged but do not roll back the
        // user-facing config switch.
        if response.confirmation == ConfigOptionConfirmation::Observed {
            let category = response
                .config_options
                .as_ref()
                .and_then(|options| options.iter().find(|option| option.id == option_id))
                .and_then(|option| option.category.as_deref())
                .unwrap_or(option_id);
            let updates = match category {
                "model" => Some(AssistantRuntimePreferenceUpdate {
                    model: Some(req.value.as_str()),
                    permission: None,
                    thought_level: None,
                }),
                "mode" => Some(AssistantRuntimePreferenceUpdate {
                    model: None,
                    permission: Some(req.value.as_str()),
                    thought_level: None,
                }),
                "thought_level" | "reasoning_effort" => Some(AssistantRuntimePreferenceUpdate {
                    model: None,
                    permission: None,
                    thought_level: Some(req.value.as_str()),
                }),
                _ => None,
            };
            if let Some(updates) = updates {
                if let Err(err) = self.persist_runtime_assistant_snapshot(conversation_id, updates).await {
                    warn!(
                        conversation_id,
                        option_id,
                        error = %ErrorChain(&err),
                        "Failed to persist runtime assistant snapshot after set_config_option",
                    );
                }
                if let Err(err) = self
                    .persist_runtime_assistant_preferences(conversation_id, updates)
                    .await
                {
                    warn!(
                        conversation_id,
                        option_id,
                        error = %ErrorChain(&err),
                        "Failed to persist runtime assistant preferences after set_config_option",
                    );
                }
            }
        }

        Ok(response)
    }

    // ── Usage / Slash commands ──────────────────────────────────────

    pub async fn get_usage(&self, conversation_id: &str) -> Result<Option<serde_json::Value>, ConversationError> {
        self.task(conversation_id)?
            .get_usage()
            .await
            .map_err(ConversationError::from)
    }

    pub async fn get_slash_commands(&self, conversation_id: &str) -> Result<Vec<SlashCommandItem>, ConversationError> {
        self.task(conversation_id)?
            .get_slash_commands()
            .await
            .map_err(ConversationError::from)
    }

    // ── Side question ───────────────────────────────────────────────

    pub async fn handle_side_question(
        &self,
        conversation_id: &str,
        req: SideQuestionRequest,
    ) -> Result<SideQuestionResponse, ConversationError> {
        // `AgentInstance::handle_side_question` already validates that the
        // question is non-empty; no need to duplicate the check here.
        self.task(conversation_id)?
            .handle_side_question(req)
            .await
            .map_err(ConversationError::from)
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
    ) -> Result<Vec<WorkspaceEntry>, ConversationError> {
        if query.path.trim().is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "path must not be empty".into(),
            });
        }

        let row = self
            .conversation_repo()
            .get(conversation_id)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to load conversation: {e}")))?
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;

        let extra: serde_json::Value = serde_json::from_str(&row.extra)
            .map_err(|e| ConversationError::internal(format!("Invalid extra JSON: {e}")))?;
        let workspace = extra
            .get("workspace")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        if workspace.is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "Conversation has no workspace assigned".into(),
            });
        }

        let relative_path = query.path.trim_start_matches('/');
        let relative_path_obj = std::path::Path::new(relative_path);
        if relative_path_obj
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(ConversationError::BadRequest {
                reason: "Path traversal outside workspace is not allowed".into(),
            });
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
            .map_err(|e| ConversationError::internal(format!("Failed to resolve workspace path: {e}")))?;
        let canonical_browse = browse_path
            .canonicalize()
            .map_err(|_| ConversationError::not_found_reason("Directory not found"))?;
        if !browse_path.starts_with(base) && !canonical_browse.starts_with(&canonical_base) {
            return Err(ConversationError::BadRequest {
                reason: "Path traversal outside workspace is not allowed".into(),
            });
        }

        // Check depth limit
        let depth = relative_path_obj.components().count();
        if depth > MAX_DIR_DEPTH {
            return Err(ConversationError::BadRequest {
                reason: format!("Directory depth exceeds maximum of {MAX_DIR_DEPTH}"),
            });
        }

        if let Some(search) = query
            .search
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            let search_mode = query.search_mode.unwrap_or(WorkspaceSearchMode::All);
            let entries = tokio::task::spawn_blocking({
                let canonical_base = canonical_base.clone();
                let canonical_browse = canonical_browse.clone();
                let search = search.to_owned();
                move || search_workspace_entries_sync(canonical_base, canonical_browse, search, search_mode)
            })
            .await
            .map_err(|e| ConversationError::internal(format!("Workspace search task failed: {e}")))?;
            return Ok(entries);
        }

        let mut entries = Vec::new();
        let mut dir_reader = tokio::fs::read_dir(&canonical_browse)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to read directory: {e}")))?;

        while let Ok(Some(entry)) = dir_reader.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();

            let entry_path = entry.path();
            let metadata = tokio::fs::metadata(&entry_path)
                .await
                .map_err(|e| ConversationError::internal(format!("Failed to read entry metadata: {e}")))?;

            let entry_type = if metadata.is_dir() { "directory" } else { "file" };

            entries.push(WorkspaceEntry {
                name,
                entry_type: entry_type.into(),
                match_kind: None,
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
