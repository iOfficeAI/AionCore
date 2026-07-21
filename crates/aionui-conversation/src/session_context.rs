use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_ai_agent::session_context::{
    AcpSessionBuildContext, AgentSessionContext, AgentSessionKind, AionrsSessionBuildContext, ConversationContext,
    WorkspaceContext,
};
use aionui_ai_agent::shared_kernel::{ConfigKey, ConfigValue, ModeId, ModelId, PersistedSessionState};
use aionui_ai_agent::types::BuildTaskOptions;
use aionui_api_types::{AcpBuildExtra, AionrsBuildExtra, TeamSessionBinding};
use aionui_common::{AgentType, WorkspacePathValidationError, validate_workspace_path_availability};
use aionui_db::models::ConversationRow;
use aionui_db::{IAcpSessionRepository, IAgentMetadataRepository};
use chrono::Datelike;
use tracing::{debug, warn};

use crate::convert::string_to_enum;
use crate::error::ConversationError;
use crate::task_options::provider_model_from_conversation_row;

const LEGACY_CONVERSATION_ARCHIVED_MESSAGE: &str =
    "This historical conversation can no longer be continued. Please start a new conversation.";

pub(crate) struct SessionContextBuilder<'a> {
    workspace_root: &'a Path,
    agent_metadata_repo: &'a Arc<dyn IAgentMetadataRepository>,
    acp_session_repo: &'a Arc<dyn IAcpSessionRepository>,
}

impl<'a> SessionContextBuilder<'a> {
    pub(crate) fn new(
        workspace_root: &'a Path,
        agent_metadata_repo: &'a Arc<dyn IAgentMetadataRepository>,
        acp_session_repo: &'a Arc<dyn IAcpSessionRepository>,
    ) -> Self {
        Self {
            workspace_root,
            agent_metadata_repo,
            acp_session_repo,
        }
    }

    pub(crate) async fn build_options(
        &self,
        row: &ConversationRow,
        seed: Option<AionrsRuntimePermissionSeed>,
    ) -> Result<BuildTaskOptions, ConversationError> {
        Ok(BuildTaskOptions::new(
            self.build_with_workspace_override(row, None, seed).await?,
        ))
    }

    pub(crate) async fn build_options_with_workspace_override(
        &self,
        row: &ConversationRow,
        workspace_override: Option<&str>,
        seed: Option<AionrsRuntimePermissionSeed>,
    ) -> Result<BuildTaskOptions, ConversationError> {
        Ok(BuildTaskOptions::new(
            self.build_with_workspace_override(row, workspace_override, seed)
                .await?,
        ))
    }

    /// Test-only convenience wrapper. Production entry points thread a real
    /// permission seed via `build_options*`; tests exercise the default
    /// (no-seed) path.
    #[cfg(test)]
    async fn build(&self, row: &ConversationRow) -> Result<AgentSessionContext, ConversationError> {
        self.build_with_workspace_override(row, None, None).await
    }

    async fn build_with_workspace_override(
        &self,
        row: &ConversationRow,
        workspace_override: Option<&str>,
        seed: Option<AionrsRuntimePermissionSeed>,
    ) -> Result<AgentSessionContext, ConversationError> {
        let agent_type: AgentType = string_to_enum(&row.r#type)?;
        reject_deprecated_runtime_kind(row, &agent_type)?;
        let extra = parse_extra(row)?;
        let workspace = self.resolve_workspace(row, &agent_type, &extra, workspace_override)?;
        let model = provider_model_from_conversation_row(row);
        let skills = parse_string_array(extra.get("skills").cloned()).unwrap_or_default();
        let team = TeamSessionBinding::from_extra_value(&extra).map_err(|e| ConversationError::BadRequest {
            reason: format!("Invalid Team runtime context: {e}"),
        })?;
        let kind = self.build_kind(row, &agent_type, extra, team.clone(), seed).await?;

        Ok(AgentSessionContext {
            conversation: ConversationContext {
                conversation_id: row.id.clone(),
                user_id: row.user_id.clone(),
                agent_type,
                source: row.source.clone(),
            },
            workspace,
            model,
            skills,
            runtime_env: Vec::new(),
            team,
            kind,
        })
    }

    fn resolve_workspace(
        &self,
        row: &ConversationRow,
        agent_type: &AgentType,
        extra: &serde_json::Value,
        workspace_override: Option<&str>,
    ) -> Result<WorkspaceContext, ConversationError> {
        let expected_auto_workspace =
            expected_auto_workspace_path(self.workspace_root, &row.id, agent_type, extra.get("backend"));
        let existing_stored_path = extra
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();

        if let Some(override_path) = workspace_override.map(str::trim).filter(|value| !value.is_empty()) {
            let normalized = match validate_workspace_path_availability(override_path) {
                Ok(normalized) => normalized,
                Err(error) => {
                    log_workspace_path_check(&row.id, &error);
                    return Err(map_runtime_workspace_validation_error(error));
                }
            };
            return Ok(WorkspaceContext {
                path: normalized,
                stored_path: existing_stored_path,
                is_custom: true,
            });
        }

        let stored = extra
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty());

        let Some(stored_path) = stored else {
            std::fs::create_dir_all(&expected_auto_workspace)
                .map_err(|e| ConversationError::internal(format!("Failed to create workspace: {e}")))?;
            return Ok(WorkspaceContext {
                path: expected_auto_workspace.to_string_lossy().into_owned(),
                stored_path: String::new(),
                is_custom: false,
            });
        };

        let normalized = match validate_workspace_path_availability(stored_path) {
            Ok(normalized) => normalized,
            Err(WorkspacePathValidationError::DoesNotExist(path))
                if expected_auto_workspace.as_path() == Path::new(stored_path) =>
            {
                path
            }
            Err(error) => {
                log_workspace_path_check(&row.id, &error);
                return Err(map_runtime_workspace_validation_error(error));
            }
        };

        Ok(WorkspaceContext {
            is_custom: Path::new(&normalized) != expected_auto_workspace.as_path(),
            stored_path: stored_path.to_owned(),
            path: normalized,
        })
    }

    async fn build_kind(
        &self,
        row: &ConversationRow,
        agent_type: &AgentType,
        extra: serde_json::Value,
        team: Option<TeamSessionBinding>,
        seed: Option<AionrsRuntimePermissionSeed>,
    ) -> Result<AgentSessionKind, ConversationError> {
        match agent_type {
            AgentType::Acp => self
                .build_acp_context(row, extra, team)
                .await
                .map(|context| AgentSessionKind::Acp(Box::new(context))),
            AgentType::Aionrs => Ok(AgentSessionKind::Aionrs(Box::new(build_aionrs_context(
                row, extra, team, seed,
            )))),
            AgentType::Gemini
            | AgentType::Codex
            | AgentType::OpenclawGateway
            | AgentType::Remote
            | AgentType::Nanobot => {
                unreachable!("legacy agent types are rejected before build_kind")
            }
        }
    }

    async fn build_acp_context(
        &self,
        row: &ConversationRow,
        extra: serde_json::Value,
        team: Option<TeamSessionBinding>,
    ) -> Result<AcpSessionBuildContext, ConversationError> {
        let mut config: AcpBuildExtra =
            serde_json::from_value(extra.clone()).map_err(|e| ConversationError::BadRequest {
                reason: format!("Invalid ACP build options: {e}"),
            })?;
        config.user_id.get_or_insert_with(|| row.user_id.clone());
        apply_team_seed_to_acp_config(&team, &mut config);
        normalize_cron_alias(row, &extra, &mut config.cron_job_id);

        if config.session_mode.is_none()
            && let Some(mode) = extra
                .get("current_mode_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
        {
            debug!(
                conversation_id = %row.id,
                "session_context: using legacy ACP extra.current_mode_id as startup seed"
            );
            config.session_mode = Some(mode.to_owned());
        }

        let belongs_to_team = team.is_some();
        let session_row = self
            .acp_session_repo
            .get(&row.id)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to load acp_session row: {e}")))?;
        self.resolve_acp_identity(row, &mut config, &extra, session_row.as_ref())
            .await?;
        let session_id = session_row.as_ref().and_then(|row| row.session_id.clone());
        let session_snapshot = self
            .load_acp_session_snapshot(row, &config, session_id.as_deref())
            .await?;

        Ok(AcpSessionBuildContext {
            config,
            team,
            belongs_to_team,
            session_id,
            session_snapshot,
        })
    }

    async fn resolve_acp_identity(
        &self,
        row: &ConversationRow,
        config: &mut AcpBuildExtra,
        extra: &serde_json::Value,
        session_row: Option<&aionui_db::models::AcpSessionRow>,
    ) -> Result<(), ConversationError> {
        let agent_id = config.agent_id.as_deref().filter(|value| !value.is_empty());
        if agent_id.is_some() {
            return Ok(());
        }

        if let Some(session_row) = session_row.filter(|row| !row.agent_id.is_empty()) {
            let metadata = self
                .agent_metadata_repo
                .get(&session_row.agent_id)
                .await
                .map_err(|e| ConversationError::internal(format!("agent_metadata lookup: {e}")))?;
            debug!(
                conversation_id = %row.id,
                agent_id = %session_row.agent_id,
                "session_context: restored ACP identity from persisted acp_session row"
            );
            config.agent_id = Some(session_row.agent_id.clone());
            if let Some(metadata) = metadata {
                config.backend = metadata.backend;
            }
            return Ok(());
        }

        let backend = config.backend.as_deref().filter(|value| !value.is_empty());
        let agent_source = extra
            .get("agent_source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("builtin");

        if agent_source != "builtin" {
            return Err(ConversationError::BadRequest {
                reason: "ACP non-builtin agent requires agent_id in extra".to_owned(),
            });
        }

        let Some(backend) = backend else {
            return Ok(());
        };

        let Some(row_meta) = self
            .agent_metadata_repo
            .find_builtin_by_backend(backend)
            .await
            .map_err(|e| ConversationError::internal(format!("agent_metadata lookup: {e}")))?
        else {
            debug!(
                conversation_id = %row.id,
                backend,
                "session_context: legacy ACP backend fallback left for factory resolution"
            );
            return Ok(());
        };

        debug!(
            conversation_id = %row.id,
            backend,
            "session_context: resolved legacy ACP backend fallback"
        );
        config.agent_id = Some(row_meta.id);
        Ok(())
    }

    async fn load_acp_session_snapshot(
        &self,
        row: &ConversationRow,
        config: &AcpBuildExtra,
        session_id: Option<&str>,
    ) -> Result<Option<PersistedSessionState>, ConversationError> {
        if session_id.is_none() {
            debug!(
                conversation_id = %row.id,
                "session_context: skipping ACP runtime snapshot before session assignment"
            );
            return Ok(None);
        }

        let db_state = self
            .acp_session_repo
            .load_runtime_state(&row.id)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to load acp_session runtime state: {e}")))?;
        let snapshot = db_state.map(decode_persisted_session_state);
        if snapshot
            .as_ref()
            .and_then(|state| state.current_model_id.as_ref())
            .is_none()
            && config
                .current_model_id
                .as_deref()
                .is_some_and(|value| !value.is_empty())
        {
            debug!(
                conversation_id = %row.id,
                "session_context: using legacy ACP extra.current_model_id as startup seed"
            );
        }
        Ok(snapshot)
    }
}

/// Runtime permission gate inputs for an aionrs rebuild, loaded from the
/// conversation's persisted assistant snapshot in the service layer
/// (`ConversationService::load_aionrs_permission_seed`) and threaded down so
/// `SessionContextBuilder` needs no `conversation_repo` handle.
///
/// - `default_permission_mode`: the assistant's permission mode (`auto` /
///   `fixed`), decides whether the runtime value may be adopted.
/// - `resolved_permission_value`: the last runtime-selected permission
///   persisted on the snapshot; only honored under `auto` for non-team
///   sessions.
#[derive(Debug, Clone)]
pub(crate) struct AionrsRuntimePermissionSeed {
    pub default_permission_mode: String,
    pub resolved_permission_value: Option<String>,
}

fn build_aionrs_context(
    row: &ConversationRow,
    extra: serde_json::Value,
    team: Option<TeamSessionBinding>,
    permission_seed: Option<AionrsRuntimePermissionSeed>,
) -> AionrsSessionBuildContext {
    let mut config: AionrsBuildExtra = match serde_json::from_value(extra.clone()) {
        Ok(config) => config,
        Err(err) => {
            warn!(
                conversation_id = %row.id,
                error = %err,
                "session_context: invalid aionrs extra; using defaults"
            );
            AionrsBuildExtra::default()
        }
    };
    config.user_id.get_or_insert_with(|| row.user_id.clone());
    apply_team_seed_to_aionrs_config(&team, &mut config);
    let belongs_to_team = team.is_some();
    // Team-bound sessions keep the team seed / create-time value; runtime
    // resolved permission is intentionally NOT read back (centralized team
    // governance — same safety principle as `fixed`). Only non-team sessions
    // consult the persisted runtime permission.
    if !belongs_to_team && let Some(seed) = permission_seed {
        apply_runtime_permission_seed(seed, row, &mut config);
    }
    AionrsSessionBuildContext {
        config,
        team,
        belongs_to_team,
    }
}

/// Applies the persisted runtime permission to the rebuild seed, honoring
/// `default_permission_mode` semantics (spec §7.1/§7.2). Callers must ensure
/// team-bound sessions never reach here.
fn apply_runtime_permission_seed(
    seed: AionrsRuntimePermissionSeed,
    row: &ConversationRow,
    config: &mut AionrsBuildExtra,
) {
    // `fixed` (and any unknown mode): keep the create-time seed
    // (== create-time default_permission_value); never adopt the runtime
    // residue — anti-privilege-escalation gate.
    if seed.default_permission_mode != "auto" {
        return;
    }
    // `auto`: remember and reuse the runtime selection. The resolved value is
    // authoritative and MUST override the create-time seed.
    if let Some(resolved) = seed.resolved_permission_value.filter(|value| !value.is_empty()) {
        debug!(
            conversation_id = %row.id,
            "session_context: aionrs rebuild seeded from resolved runtime permission"
        );
        config.session_mode = Some(resolved);
    }
}

fn apply_team_seed_to_acp_config(team: &Option<TeamSessionBinding>, config: &mut AcpBuildExtra) {
    let Some(team) = team else {
        return;
    };
    if config.backend.as_deref().is_none_or(str::is_empty) {
        config.backend.clone_from(&team.runtime_seed.backend);
    }
    if config.session_mode.as_deref().is_none_or(str::is_empty) {
        config.session_mode.clone_from(&team.runtime_seed.session_mode);
    }
    if config.current_model_id.as_deref().is_none_or(str::is_empty) {
        config.current_model_id.clone_from(&team.runtime_seed.current_model_id);
    }
    if config.team_mcp_stdio_config.is_none() {
        config.team_mcp_stdio_config = team.mcp.as_ref().map(|mcp| mcp.stdio.clone());
    }
}

fn apply_team_seed_to_aionrs_config(team: &Option<TeamSessionBinding>, config: &mut AionrsBuildExtra) {
    let Some(team) = team else {
        return;
    };
    if config.backend.as_deref().is_none_or(str::is_empty) {
        config.backend.clone_from(&team.runtime_seed.backend);
    }
    if config.session_mode.as_deref().is_none_or(str::is_empty) {
        config.session_mode.clone_from(&team.runtime_seed.session_mode);
    }
    if config.team_mcp_stdio_config.is_none() {
        config.team_mcp_stdio_config = team.mcp.as_ref().map(|mcp| mcp.stdio.clone());
    }
}

fn parse_extra(row: &ConversationRow) -> Result<serde_json::Value, ConversationError> {
    serde_json::from_str(&row.extra).map_err(|e| ConversationError::internal(format!("Invalid extra JSON: {e}")))
}

fn reject_deprecated_runtime_kind(row: &ConversationRow, agent_type: &AgentType) -> Result<(), ConversationError> {
    if !agent_type.is_deprecated_runtime() {
        return Ok(());
    }

    debug!(
        conversation_id = %row.id,
        agent_type = agent_type.serde_name(),
        "Rejected deprecated runtime conversation before session context build"
    );

    Err(ConversationError::Archived {
        id: row.id.clone(),
        reason: LEGACY_CONVERSATION_ARCHIVED_MESSAGE.into(),
    })
}

fn parse_string_array(value: Option<serde_json::Value>) -> Option<Vec<String>> {
    serde_json::from_value(value?).ok()
}

fn normalize_cron_alias(row: &ConversationRow, extra: &serde_json::Value, cron_job_id: &mut Option<String>) {
    if cron_job_id.is_none()
        && let Some(legacy) = extra
            .get("cronJobId")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
    {
        debug!(
            conversation_id = %row.id,
            "session_context: normalized legacy cronJobId alias"
        );
        *cron_job_id = Some(legacy.to_owned());
    }
}

fn decode_persisted_session_state(state: aionui_db::PersistedSessionState) -> PersistedSessionState {
    let mut decoded = PersistedSessionState {
        current_mode_id: state.current_mode_id.map(ModeId::new),
        current_model_id: state.current_model_id.map(ModelId::new),
        ..Default::default()
    };
    if let Some(raw) = state.config_selections_json
        && let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&raw)
    {
        decoded.config_selections = map
            .into_iter()
            .map(|(key, value)| (ConfigKey::new(key), ConfigValue::new(value)))
            .collect();
    }
    if let Some(raw) = state.context_usage_json
        && let Ok(usage) = serde_json::from_str(&raw)
    {
        decoded.context_usage = Some(usage);
    }
    decoded
}

fn expected_auto_workspace_path(
    workspace_root: &Path,
    conversation_id: &str,
    agent_type: &AgentType,
    backend: Option<&serde_json::Value>,
) -> PathBuf {
    auto_workspace_parent(workspace_root).join(format!(
        "{}-temp-{conversation_id}",
        conversation_label(agent_type, backend)
    ))
}

fn auto_workspace_parent(workspace_root: &Path) -> PathBuf {
    let now = chrono::Local::now();
    workspace_root
        .join("conversations")
        .join(format!("{:04}", now.year()))
        .join(format!("{:02}", now.month()))
        .join(format!("{:02}", now.day()))
}

fn conversation_label(agent_type: &AgentType, backend: Option<&serde_json::Value>) -> String {
    if *agent_type == AgentType::Acp
        && let Some(serde_json::Value::String(s)) = backend
        && !s.is_empty()
    {
        return s.clone();
    }
    agent_type.serde_name().to_owned()
}

fn map_runtime_workspace_validation_error(error: WorkspacePathValidationError) -> ConversationError {
    match error {
        WorkspacePathValidationError::Empty => ConversationError::BadRequest {
            reason: "Workspace directory is empty".into(),
        },
        WorkspacePathValidationError::DoesNotExist(path)
        | WorkspacePathValidationError::NotDirectory(path)
        | WorkspacePathValidationError::NotAccessible { path, .. } => {
            ConversationError::WorkspacePathRuntimeUnavailable { path }
        }
    }
}

fn log_workspace_path_check(conversation_id: &str, error: &WorkspacePathValidationError) {
    warn!(
        target: "aionui_feedback_diagnostics",
        diagnostic_event = "feedback.runtime.workspace_path_check",
        conversation_id = %conversation_id,
        path_present = !matches!(error, WorkspacePathValidationError::Empty),
        path_exists = matches!(
            error,
            WorkspacePathValidationError::NotDirectory(_) | WorkspacePathValidationError::NotAccessible { .. }
        ),
        error_class = %workspace_error_class(error),
        "feedback.runtime.workspace_path_check"
    );
}

fn workspace_error_class(error: &WorkspacePathValidationError) -> &'static str {
    match error {
        WorkspacePathValidationError::Empty => "empty",
        WorkspacePathValidationError::DoesNotExist(_) => "not_found",
        WorkspacePathValidationError::NotDirectory(_) => "not_directory",
        WorkspacePathValidationError::NotAccessible { .. } => "not_accessible",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::{
        CreateAcpSessionParams, SaveRuntimeStateParams, SqliteAcpSessionRepository, SqliteAgentMetadataRepository,
        UpsertAgentMetadataParams, init_database_memory,
    };
    use std::io::Write;
    use std::sync::Mutex;
    use tracing::Level;
    use tracing_subscriber::fmt;

    struct TestRepos {
        workspace_root: PathBuf,
        metadata_repo: Arc<dyn IAgentMetadataRepository>,
        acp_session_repo: Arc<dyn IAcpSessionRepository>,
    }

    impl TestRepos {
        fn builder(&self) -> SessionContextBuilder<'_> {
            SessionContextBuilder::new(&self.workspace_root, &self.metadata_repo, &self.acp_session_repo)
        }
    }

    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs(max_level: Level, f: impl FnOnce()) -> String {
        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let make_writer = {
            let buffer = Arc::clone(&buffer);
            move || SharedBuf(Arc::clone(&buffer))
        };
        let subscriber = fmt::Subscriber::builder()
            .with_max_level(max_level)
            .with_writer(make_writer)
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(buffer.lock().unwrap().clone()).unwrap()
    }

    async fn setup() -> TestRepos {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let metadata_repo: Arc<dyn IAgentMetadataRepository> =
            Arc::new(SqliteAgentMetadataRepository::new(pool.clone()));
        let acp_session_repo: Arc<dyn IAcpSessionRepository> = Arc::new(SqliteAcpSessionRepository::new(pool));
        let workspace_root = std::env::temp_dir().join(format!(
            "aion-session-context-test-{}",
            aionui_common::generate_short_id()
        ));
        TestRepos {
            workspace_root,
            metadata_repo,
            acp_session_repo,
        }
    }

    fn row(agent_type: &str, extra: serde_json::Value, model: Option<serde_json::Value>) -> ConversationRow {
        ConversationRow {
            id: "conv-1".into(),
            user_id: "user-1".into(),
            name: "test".into(),
            r#type: agent_type.into(),
            model: model.map(|value| serde_json::to_string(&value).unwrap()),
            extra: serde_json::to_string(&extra).unwrap(),
            status: None,
            source: Some("chat".into()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    async fn upsert_builtin(repos: &TestRepos, id: &str, backend: &str) {
        repos
            .metadata_repo
            .upsert(&UpsertAgentMetadataParams {
                id,
                icon: None,
                name: id,
                name_i18n: None,
                description: None,
                description_i18n: None,
                backend: Some(backend),
                agent_type: "acp",
                agent_source: "builtin",
                agent_source_info: None,
                enabled: true,
                command: Some("/bin/echo"),
                args: None,
                env: None,
                native_skills_dirs: None,
                behavior_policy: None,
                yolo_id: None,
                agent_capabilities: None,
                auth_methods: None,
                config_options: None,
                available_modes: None,
                available_models: None,
                available_commands: None,
                sort_order: 0,
            })
            .await
            .unwrap();
    }

    fn acp_context(context: AgentSessionContext) -> AcpSessionBuildContext {
        match context.kind {
            AgentSessionKind::Acp(acp) => *acp,
            other => panic!("expected ACP context, got {other:?}"),
        }
    }

    fn aionrs_context(context: AgentSessionContext) -> AionrsSessionBuildContext {
        match context.kind {
            AgentSessionKind::Aionrs(aionrs) => *aionrs,
            other => panic!("expected Aionrs context, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn acp_agent_id_takes_priority_over_backend() {
        let repos = setup().await;
        let row = row(
            "acp",
            serde_json::json!({
                "agent_id": "custom-agent-1",
                "backend": "claude",
                "agent_source": "custom"
            }),
            None,
        );

        let context = repos.builder().build(&row).await.unwrap();
        let acp = acp_context(context);
        assert_eq!(acp.config.agent_id.as_deref(), Some("custom-agent-1"));
        assert_eq!(acp.config.backend.as_deref(), Some("claude"));
    }

    #[tokio::test]
    async fn acp_builtin_backend_fallback_resolves_agent_id() {
        let repos = setup().await;
        upsert_builtin(&repos, "builtin-claude-test", "claude").await;
        let row = row("acp", serde_json::json!({ "backend": "claude" }), None);

        let context = repos.builder().build(&row).await.unwrap();
        let acp = acp_context(context);
        assert_eq!(acp.config.agent_id.as_deref(), Some("builtin-claude-test"));
        assert_eq!(acp.config.backend.as_deref(), Some("claude"));
    }

    #[tokio::test]
    async fn acp_openclaw_builtin_backend_fallback_resolves_agent_id() {
        let repos = setup().await;
        upsert_builtin(&repos, "builtin-openclaw-test", "openclaw").await;
        let row = row("acp", serde_json::json!({ "backend": "openclaw" }), None);

        let context = repos.builder().build(&row).await.unwrap();
        let acp = acp_context(context);
        assert_eq!(acp.config.agent_id.as_deref(), Some("builtin-openclaw-test"));
        assert_eq!(acp.config.backend.as_deref(), Some("openclaw"));
    }

    #[tokio::test]
    async fn acp_non_builtin_without_agent_id_is_rejected() {
        let repos = setup().await;
        let row = row(
            "acp",
            serde_json::json!({ "backend": "custom", "agent_source": "custom" }),
            None,
        );

        let err = repos.builder().build(&row).await.unwrap_err();
        assert!(err.to_string().contains("requires agent_id"));
    }

    #[tokio::test]
    async fn acp_persisted_runtime_is_loaded_before_legacy_seed() {
        let repos = setup().await;
        upsert_builtin(&repos, "builtin-claude-test", "claude").await;
        repos
            .acp_session_repo
            .create(&CreateAcpSessionParams {
                conversation_id: "conv-1",
                agent_source: "builtin",
                agent_id: "builtin-claude-test",
            })
            .await
            .unwrap();
        repos
            .acp_session_repo
            .update_session_id("conv-1", "sess-1")
            .await
            .unwrap();
        repos
            .acp_session_repo
            .save_runtime_state(
                "conv-1",
                &SaveRuntimeStateParams {
                    current_mode_id: Some(Some("persisted-mode")),
                    current_model_id: Some(Some("persisted-model")),
                    config_selections_json: None,
                    context_usage_json: None,
                },
            )
            .await
            .unwrap();
        let row = row(
            "acp",
            serde_json::json!({
                "backend": "claude",
                "current_mode_id": "legacy-mode",
                "current_model_id": "legacy-model"
            }),
            None,
        );

        let context = repos.builder().build(&row).await.unwrap();
        let acp = acp_context(context);
        let snapshot = acp.session_snapshot.expect("snapshot loaded");
        assert_eq!(snapshot.current_mode_id.unwrap().as_str(), "persisted-mode");
        assert_eq!(snapshot.current_model_id.unwrap().as_str(), "persisted-model");
        assert_eq!(acp.config.session_mode.as_deref(), Some("legacy-mode"));
        assert_eq!(acp.config.current_model_id.as_deref(), Some("legacy-model"));
    }

    #[tokio::test]
    async fn acp_unassigned_session_runtime_is_startup_seed_not_resume_snapshot() {
        let repos = setup().await;
        upsert_builtin(&repos, "builtin-codex-test", "codex").await;
        repos
            .acp_session_repo
            .create(&CreateAcpSessionParams {
                conversation_id: "conv-1",
                agent_source: "builtin",
                agent_id: "builtin-codex-test",
            })
            .await
            .unwrap();
        repos
            .acp_session_repo
            .save_runtime_state(
                "conv-1",
                &SaveRuntimeStateParams {
                    current_mode_id: Some(Some("full-access")),
                    current_model_id: Some(Some("gpt-5.5")),
                    config_selections_json: None,
                    context_usage_json: None,
                },
            )
            .await
            .unwrap();
        let row = row(
            "acp",
            serde_json::json!({
                "backend": "codex",
                "current_mode_id": "full-access",
                "current_model_id": "gpt-5.5"
            }),
            None,
        );

        let context = repos.builder().build(&row).await.unwrap();
        let acp = acp_context(context);
        assert_eq!(acp.config.session_mode.as_deref(), Some("full-access"));
        assert_eq!(acp.config.current_model_id.as_deref(), Some("gpt-5.5"));
        assert!(acp.session_snapshot.is_none());
    }

    #[tokio::test]
    async fn acp_session_identity_takes_priority_over_legacy_backend_seed() {
        let repos = setup().await;
        upsert_builtin(&repos, "builtin-claude-test", "claude").await;
        upsert_builtin(&repos, "builtin-codex-test", "codex").await;
        repos
            .acp_session_repo
            .create(&CreateAcpSessionParams {
                conversation_id: "conv-1",
                agent_source: "builtin",
                agent_id: "builtin-codex-test",
            })
            .await
            .unwrap();
        let row = row("acp", serde_json::json!({ "backend": "claude" }), None);

        let context = repos.builder().build(&row).await.unwrap();
        let acp = acp_context(context);
        assert_eq!(acp.config.agent_id.as_deref(), Some("builtin-codex-test"));
        assert_eq!(acp.config.backend.as_deref(), Some("codex"));
    }

    #[tokio::test]
    async fn acp_legacy_current_mode_becomes_startup_seed_without_runtime() {
        let repos = setup().await;
        upsert_builtin(&repos, "builtin-claude-test", "claude").await;
        let row = row(
            "acp",
            serde_json::json!({ "backend": "claude", "current_mode_id": "legacy-mode" }),
            None,
        );

        let context = repos.builder().build(&row).await.unwrap();
        let acp = acp_context(context);
        assert_eq!(acp.config.session_mode.as_deref(), Some("legacy-mode"));
        assert!(acp.session_snapshot.is_none());
    }

    #[tokio::test]
    async fn acp_extra_thought_level_is_exposed_as_typed_context() {
        let repos = setup().await;
        upsert_builtin(&repos, "builtin-codex-test", "codex").await;
        let row = row(
            "acp",
            serde_json::json!({
                "backend": "codex",
                "thought_level": "high"
            }),
            None,
        );

        let context = repos.builder().build(&row).await.unwrap();
        let acp = acp_context(context);
        assert_eq!(acp.config.thought_level.as_deref(), Some("high"));
        assert!(acp.session_snapshot.is_none());
    }

    #[tokio::test]
    async fn acp_team_extra_is_exposed_as_typed_context() {
        let repos = setup().await;
        upsert_builtin(&repos, "builtin-claude-test", "claude").await;
        let row = row(
            "acp",
            serde_json::json!({
                "teamId": "team-1",
                "slot_id": "lead-1",
                "role": "lead",
                "backend": "claude",
                "session_mode": "yolo",
                "current_model_id": "claude-opus",
                "team_mcp_stdio_config": {
                    "team_id": "team-1",
                    "port": 4242,
                    "token": "tok-1",
                    "slot_id": "lead-1",
                    "binary_path": "/tmp/aioncore"
                }
            }),
            None,
        );

        let context = repos.builder().build(&row).await.unwrap();
        let team = context.team.as_ref().expect("team context");
        assert_eq!(team.team_id, "team-1");
        assert_eq!(team.slot_id.as_deref(), Some("lead-1"));
        assert_eq!(team.role.as_deref(), Some("lead"));
        assert_eq!(team.runtime_seed.backend.as_deref(), Some("claude"));
        assert_eq!(team.runtime_seed.session_mode.as_deref(), Some("yolo"));
        assert_eq!(team.runtime_seed.current_model_id.as_deref(), Some("claude-opus"));
        let mcp = team.mcp.as_ref().expect("typed team mcp");
        assert_eq!(mcp.stdio.port, 4242);
        assert_eq!(mcp.stdio.slot_id, "lead-1");

        let acp = acp_context(context);
        assert!(acp.belongs_to_team);
        assert_eq!(acp.config.team_mcp_stdio_config.unwrap().port, 4242);
    }

    #[tokio::test]
    async fn aionrs_team_extra_is_exposed_as_typed_context() {
        let repos = setup().await;
        let row = row(
            "aionrs",
            serde_json::json!({
                "teamId": "team-2",
                "slot_id": "worker-1",
                "role": "teammate",
                "backend": "aionrs",
                "session_mode": "yolo",
                "team_mcp_stdio_config": {
                    "team_id": "team-2",
                    "port": 5252,
                    "token": "tok-2",
                    "slot_id": "worker-1",
                    "binary_path": "/tmp/aioncore"
                }
            }),
            Some(serde_json::json!({
                "provider_id": "provider-1",
                "model": "gpt-5"
            })),
        );

        let context = repos.builder().build(&row).await.unwrap();
        let team = context.team.as_ref().expect("team context");
        assert_eq!(team.team_id, "team-2");
        assert_eq!(team.slot_id.as_deref(), Some("worker-1"));
        assert_eq!(team.runtime_seed.backend.as_deref(), Some("aionrs"));
        assert_eq!(team.mcp.as_ref().unwrap().stdio.port, 5252);

        let aionrs = aionrs_context(context);
        assert!(aionrs.belongs_to_team);
        assert_eq!(aionrs.config.team_mcp_stdio_config.unwrap().port, 5252);
    }

    #[tokio::test]
    async fn aionrs_uses_conversation_model_and_ignores_legacy_extra_model() {
        let repos = setup().await;
        let row = row(
            "aionrs",
            serde_json::json!({
                "model": { "provider_id": "wrong", "model": "wrong-model" }
            }),
            Some(serde_json::json!({
                "provider_id": "provider-1",
                "model": "gpt-5",
                "use_model": "gpt-5.1"
            })),
        );

        let context = repos.builder().build(&row).await.unwrap();
        assert_eq!(context.model.provider_id, "provider-1");
        assert_eq!(context.model.model, "gpt-5");
        assert_eq!(context.model.use_model.as_deref(), Some("gpt-5.1"));
    }

    #[tokio::test]
    async fn workspace_empty_uses_auto_path_and_is_not_custom() {
        let repos = setup().await;
        let row = row("aionrs", serde_json::json!({}), None);

        let context = repos.builder().build(&row).await.unwrap();
        assert!(!context.workspace.is_custom);
        assert!(context.workspace.stored_path.is_empty());
        assert!(context.workspace.path.ends_with("aionrs-temp-conv-1"));
    }

    #[tokio::test]
    async fn workspace_existing_path_is_custom() {
        let repos = setup().await;
        let custom = repos.workspace_root.join("custom-workspace");
        std::fs::create_dir_all(&custom).unwrap();
        let row = row(
            "aionrs",
            serde_json::json!({ "workspace": custom.to_string_lossy().to_string() }),
            None,
        );

        let context = repos.builder().build(&row).await.unwrap();
        assert!(context.workspace.is_custom);
        assert_eq!(context.workspace.path, custom.to_string_lossy());
    }

    #[test]
    fn workspace_validation_failure_logs_redacted_runtime_check() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let raw_path = "/tmp/aionui-secret-workspace-token-12345";
        let captured = capture_logs(Level::WARN, || {
            runtime.block_on(async {
                let repos = setup().await;
                let row = row("aionrs", serde_json::json!({ "workspace": raw_path }), None);

                let err = repos.builder().build(&row).await.unwrap_err();
                assert!(matches!(err, ConversationError::WorkspacePathRuntimeUnavailable { .. }));
            });
        });

        assert!(captured.contains("aionui_feedback_diagnostics"), "{captured}");
        assert!(captured.contains("feedback.runtime.workspace_path_check"), "{captured}");
        assert!(captured.contains("conversation_id=conv-1"), "{captured}");
        assert!(captured.contains("path_present=true"), "{captured}");
        assert!(captured.contains("path_exists=false"), "{captured}");
        assert!(captured.contains("error_class=not_found"), "{captured}");
        assert!(!captured.contains(raw_path), "{captured}");
        assert!(!captured.contains("token-12345"), "{captured}");
    }

    fn assert_archived(err: ConversationError, expected_id: &str) {
        match err {
            ConversationError::Archived { id, reason } => {
                assert_eq!(id, expected_id);
                assert!(
                    reason.contains("This historical conversation can no longer be continued."),
                    "unexpected archive reason: {reason}"
                );
            }
            other => panic!("expected ConversationError::Archived, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn legacy_agent_types_are_archived_before_runtime_context() {
        let repos = setup().await;

        for (agent_type, extra) in [
            ("gemini", serde_json::json!({})),
            ("codex", serde_json::json!({ "workspace": "/tmp/aionui-codex-history" })),
            (
                "openclaw-gateway",
                serde_json::json!({ "gateway": { "use_external_gateway": true } }),
            ),
            ("nanobot", serde_json::json!({})),
            ("remote", serde_json::json!({})),
        ] {
            let row = row(agent_type, extra, None);
            let err = repos.builder().build(&row).await.unwrap_err();
            assert_archived(err, "conv-1");
        }
    }

    fn aionrs_seed(mode: &str, resolved: Option<&str>) -> AionrsRuntimePermissionSeed {
        AionrsRuntimePermissionSeed {
            default_permission_mode: mode.to_owned(),
            resolved_permission_value: resolved.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn aionrs_auto_mode_rebuild_adopts_resolved_permission_value() {
        // AC#1: auto happy path — runtime yolo survives rebuild.
        let row = row("aionrs", serde_json::json!({ "session_mode": "default" }), None);
        let ctx = build_aionrs_context(
            &row,
            serde_json::json!({ "session_mode": "default" }),
            None,
            Some(aionrs_seed("auto", Some("yolo"))),
        );
        assert_eq!(ctx.config.session_mode.as_deref(), Some("yolo"));
    }

    #[test]
    fn aionrs_auto_mode_resolved_overrides_create_time_seed() {
        // AC#2: existing-data compat — create-time non-yolo seed is overridden.
        let row = row("aionrs", serde_json::json!({ "session_mode": "auto_edit" }), None);
        let ctx = build_aionrs_context(
            &row,
            serde_json::json!({ "session_mode": "auto_edit" }),
            None,
            Some(aionrs_seed("auto", Some("yolo"))),
        );
        assert_eq!(ctx.config.session_mode.as_deref(), Some("yolo"));
    }

    #[test]
    fn aionrs_fixed_mode_ignores_resolved_permission_value() {
        // AC#3: fixed safety gate — runtime residue must NOT escalate.
        let row = row("aionrs", serde_json::json!({ "session_mode": "default" }), None);
        let ctx = build_aionrs_context(
            &row,
            serde_json::json!({ "session_mode": "default" }),
            None,
            Some(aionrs_seed("fixed", Some("yolo"))),
        );
        assert_eq!(ctx.config.session_mode.as_deref(), Some("default"));
    }

    #[test]
    fn aionrs_team_bound_session_ignores_resolved_permission_value() {
        // Team-bound governance: keep team seed, never read resolved runtime value.
        let team = TeamSessionBinding::from_extra_value(&serde_json::json!({
            "teamId": "team-1",
            "slot_id": "worker-1",
            "role": "teammate",
            "session_mode": "auto_edit"
        }))
        .unwrap();
        let row = row("aionrs", serde_json::json!({ "session_mode": "auto_edit" }), None);
        let ctx = build_aionrs_context(
            &row,
            serde_json::json!({ "session_mode": "auto_edit" }),
            team,
            Some(aionrs_seed("auto", Some("yolo"))),
        );
        assert_eq!(ctx.config.session_mode.as_deref(), Some("auto_edit"));
    }
}
