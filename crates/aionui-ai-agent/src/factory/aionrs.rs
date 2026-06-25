use std::collections::HashMap;
use std::sync::Arc;

use aion_agent::session::SessionManager;
use aion_config::config::{McpServerConfig, TransportType};
use aionui_api_types::{
    AionrsBuildExtra, GuideMcpConfig, SessionMcpServer, SessionMcpTransport, TEAM_MCP_SERVER_NAME, TeamMcpStdioConfig,
};
use aionui_common::ProviderWithModel;
use aionui_db::IMcpServerRepository;
use aionui_db::models::McpServerRow;
use aionui_realtime::EventBroadcaster;
use aionui_runtime::ensure_runtime_command_with_reporter;
use tracing::{debug, info, warn};

use crate::agent_task::AgentInstance;
use crate::error::AgentError;
use crate::factory::AgentFactoryDeps;
use crate::factory::context::FactoryContext;
use crate::manager::aionrs::{AionrsAgentManager, sanitize_session_messages};
use crate::runtime_status::conversation_runtime_reporter;
use crate::session_context::AionrsSessionBuildContext;
use crate::types::{AionrsCompatOverrides, AionrsResolvedConfig};
use aionui_team_prompts::guide as team_guide_prompt;

const TEAM_CAPABLE_BACKENDS: &[&str] = &["claude", "codex", "gemini", "aionrs", "codebuddy"];

pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    build_context: AionrsSessionBuildContext,
    model: ProviderWithModel,
    ctx: FactoryContext,
) -> Result<AgentInstance, AgentError> {
    let belongs_to_team = build_context.team.is_some();
    let mut overrides = build_context.config;

    // Merge preset assistant rules into system_prompt (used as custom_prompt
    // in aionrs's build_system_prompt). Mirrors the old architecture's
    // `init_history` injection of `[Assistant System Rules]`.
    // AionrsBuildExtra parses `skills` so Team preset snapshots preserve the
    // target contract. Native skill materialization for Aionrs is tracked as a
    // separate follow-up because this factory currently has no stable Aionrs
    // skill-loading path.
    if let Some(rules) = overrides.preset_rules.take() {
        overrides.system_prompt = Some(match overrides.system_prompt.take() {
            Some(existing) => format!("{existing}\n\n{rules}"),
            None => rules,
        });
    }

    // Inject Guide MCP config for solo (non-team) sessions, mirroring acp.rs.
    // Skip if the conversation already belongs to a team.
    if overrides.team_mcp_stdio_config.is_none()
        && overrides.guide_mcp_config.is_none()
        && deps.guide_mcp_config.is_some()
        && !belongs_to_team
    {
        overrides.guide_mcp_config.clone_from(&deps.guide_mcp_config);
        overrides.backend.get_or_insert_with(|| "aionrs".to_owned());
    }

    let mut extra_mcp_servers = resolve_mcp_servers(&overrides, &ctx.conversation_id);
    if let Some(repo) = deps.mcp_server_repo.as_ref() {
        for (name, config) in load_user_mcp_servers(
            repo.as_ref(),
            overrides.mcp_server_ids.as_deref(),
            &ctx.conversation_id,
            deps.broadcaster.clone(),
        )
        .await
        {
            extra_mcp_servers.entry(name).or_insert(config);
        }
    }
    merge_session_snapshot_mcp_servers(
        &mut extra_mcp_servers,
        &overrides.session_mcp_servers,
        &ctx.conversation_id,
        deps.broadcaster.clone(),
    )
    .await;

    // Inject team guide system prompt for solo sessions with guide MCP
    if overrides.team_mcp_stdio_config.is_none()
        && overrides.guide_mcp_config.is_some()
        && team_guide_prompt::is_solo_team_guide_backend(overrides.backend.as_deref().unwrap_or("aionrs"))
    {
        let guide_prompt =
            team_guide_prompt::build_solo_team_guide_prompt(overrides.backend.as_deref().unwrap_or("aionrs"));
        overrides.system_prompt = Some(match overrides.system_prompt.take() {
            Some(existing) => format!("{existing}\n\n{guide_prompt}"),
            None => guide_prompt,
        });
    }

    if !extra_mcp_servers.is_empty() {
        info!(
            conversation_id = %ctx.conversation_id,
            mcp_count = extra_mcp_servers.len(),
            mcp_names = ?extra_mcp_servers.keys().collect::<Vec<_>>(),
            "Injecting MCP servers into aionrs session"
        );
    }

    let provider_id = &model.provider_id;
    let row = deps
        .provider_repo
        .find_by_id(provider_id)
        .await
        .map_err(|e| AgentError::internal(format!("Failed to load provider config: {e}")))?
        .ok_or_else(|| AgentError::bad_request(format!("Provider '{provider_id}' not found")))?;

    let api_key = aionui_common::decrypt_string(&row.api_key_encrypted, &deps.encryption_key)
        .map_err(|e| AgentError::internal(e.to_string()))?;

    let model_id = model
        .use_model
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&model.model)
        .to_owned();

    let provider = map_aionrs_provider(&row.platform, &model_id, row.model_protocols.as_deref());

    let (base_url, compat_overrides) =
        resolve_aionrs_url_and_compat(&row.platform, &row.base_url, &provider, row.is_full_url);

    let bedrock_config = if row.platform == "bedrock" {
        resolve_bedrock_config(row.bedrock_config.as_deref())
    } else {
        None
    };

    let session_directory = deps.data_dir.join("aionrs-sessions");

    let resume_session = {
        let session_mgr = SessionManager::new(session_directory.clone(), 100);
        match session_mgr.load(&ctx.conversation_id) {
            Ok(mut session) => {
                // Drop orphaned assistant tool-calls left behind when the user
                // pressed Stop mid-stream. Strict providers (Ollama-style,
                // some OpenAI-compatible proxies) reject replayed assistants
                // with `tool_calls != null` and `content == null` when no
                // matching tool_result follows. See ELECTRON-1HV / ELECTRON-1J6.
                let dropped = sanitize_session_messages(&mut session.messages);
                info!(
                    conversation_id = %ctx.conversation_id,
                    session_id = %session.id,
                    message_count = session.messages.len(),
                    sanitized_dropped = dropped,
                    "Loaded existing aionrs session for resume"
                );
                Some(session)
            }
            Err(_) => {
                // Fallback: old architecture stored sessions inside the workspace
                let legacy_dir = std::path::Path::new(&ctx.workspace).join(".aionrs/sessions");
                let legacy_mgr = SessionManager::new(legacy_dir.clone(), 100);
                match legacy_mgr.load(&ctx.conversation_id) {
                    Ok(mut session) => {
                        let dropped = sanitize_session_messages(&mut session.messages);
                        info!(
                            conversation_id = %ctx.conversation_id,
                            session_id = %session.id,
                            message_count = session.messages.len(),
                            sanitized_dropped = dropped,
                            "Loaded legacy aionrs session from workspace"
                        );
                        Some(session)
                    }
                    Err(e) => {
                        debug!(
                            conversation_id = %ctx.conversation_id,
                            error = %e,
                            "No existing aionrs session found, starting fresh"
                        );
                        None
                    }
                }
            }
        }
    };

    let config = AionrsResolvedConfig {
        provider,
        api_key,
        model: model_id,
        base_url,
        system_prompt: overrides.system_prompt,
        max_tokens: overrides.max_tokens,
        max_turns: overrides.max_turns,
        max_tool_call_malformed_turns: overrides.max_tool_call_malformed_turns,
        max_tool_call_failure_turns: overrides.max_tool_call_failure_turns,
        compat_overrides,
        session_directory,
        session_mode: overrides.session_mode,
        extra_mcp_servers,
        bedrock_config,
    };

    let agent = AionrsAgentManager::new(ctx.conversation_id, ctx.workspace, config, resume_session).await?;
    Ok(AgentInstance::Aionrs(Arc::new(agent)))
}

/// Map AionUi DB platform name to the aionrs provider identifier.
///
/// Mirrors the frontend `src/process/agent/aionrs/envBuilder.ts` mapping.
/// For `new-api` platform, per-model protocol overrides from `model_protocols`
/// JSON take precedence.
pub(crate) fn map_aionrs_provider(platform: &str, model_id: &str, model_protocols: Option<&str>) -> String {
    if platform == "new-api"
        && let Some(protocols_json) = model_protocols
        && let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(protocols_json)
        && let Some(serde_json::Value::String(protocol)) = map.get(model_id)
        && protocol == "anthropic"
    {
        return "anthropic".to_owned();
    }

    match platform {
        "anthropic" => "anthropic",
        "bedrock" => "bedrock",
        "gemini-vertex-ai" => "vertex",
        _ => "openai",
    }
    .to_owned()
}

/// Resolve base_url and compat overrides for the aionrs provider.
///
/// Mirrors the frontend `envBuilder.ts` logic:
/// - Strips trailing `/v1` from base_url (aionrs appends its own path)
/// - Gemini: prepends `/v1beta/openai` and overrides `api_path`
/// - OpenAI official (`api.openai.com`): sets `max_completion_tokens`
pub(crate) fn resolve_aionrs_url_and_compat(
    platform: &str,
    raw_base_url: &str,
    mapped_provider: &str,
    is_full_url: bool,
) -> (Option<String>, AionrsCompatOverrides) {
    let mut compat = AionrsCompatOverrides::default();

    if is_full_url {
        let trimmed = raw_base_url.trim_end_matches('/');
        compat.api_path = Some(String::new());
        return (Some(trimmed.to_owned()), compat);
    }

    if platform == "gemini" {
        let trimmed = raw_base_url.trim_end_matches('/');
        let base = format!("{trimmed}/v1beta/openai");
        compat.api_path = Some("/chat/completions".to_owned());
        return (Some(base), compat);
    }

    let normalized = normalize_aionrs_base_url(raw_base_url);
    let base_url = Some(normalized).filter(|u| !u.is_empty());

    if mapped_provider == "openai" && is_openai_host(raw_base_url) {
        compat.max_tokens_field = Some("max_completion_tokens".to_owned());
    }

    (base_url, compat)
}

fn is_openai_host(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .map(|rest| rest == "api.openai.com" || rest.starts_with("api.openai.com/"))
        .unwrap_or(false)
}

/// Strip trailing `/v1`, `/v1/`, or lone `/` from a base URL so that
/// aionrs can append its own path suffix (`/v1/messages`, `/v1/chat/completions`).
fn normalize_aionrs_base_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_owned()
}

pub(crate) fn resolve_bedrock_config(json: Option<&str>) -> Option<aion_config::config::BedrockConfig> {
    let bc: aionui_api_types::BedrockConfig = serde_json::from_str(json?).ok()?;
    Some(aion_config::config::BedrockConfig {
        region: Some(bc.region),
        access_key_id: bc.access_key_id,
        secret_access_key: bc.secret_access_key,
        session_token: None,
        profile: bc.profile,
    })
}

async fn load_user_mcp_servers(
    repo: &dyn IMcpServerRepository,
    selected_ids: Option<&[String]>,
    conversation_id: &str,
    broadcaster: Arc<dyn EventBroadcaster>,
) -> HashMap<String, McpServerConfig> {
    let rows_result = match selected_ids {
        Some(ids) => repo.list_by_ids_any(ids).await,
        None => repo.list().await,
    };
    let rows = match rows_result {
        Ok(r) => r,
        Err(err) => {
            warn!(
                conversation_id,
                error = %err,
                "user_mcp: list() failed; skipping injection"
            );
            return HashMap::new();
        }
    };

    let mut servers = HashMap::new();
    for row in rows {
        let selected = selected_ids
            .map(|ids| ids.iter().any(|id| id == &row.id))
            .unwrap_or(row.enabled);
        if !selected || row.builtin {
            continue;
        }

        match row_to_mcp_server_config(&row, conversation_id, broadcaster.clone()).await {
            Ok(config) => {
                servers.insert(row.name.clone(), config);
            }
            Err(err) => {
                warn!(
                    conversation_id,
                    server_id = %row.id,
                    server_name = %row.name,
                    error = %err,
                    "user_mcp: failed to convert row; skipping"
                );
            }
        }
    }

    servers
}

async fn row_to_mcp_server_config(
    row: &McpServerRow,
    conversation_id: &str,
    broadcaster: Arc<dyn EventBroadcaster>,
) -> Result<McpServerConfig, String> {
    let value: serde_json::Value =
        serde_json::from_str(&row.transport_config).map_err(|e| format!("invalid transport_config JSON: {e}"))?;

    match row.transport_type.as_str() {
        "stdio" => {
            let command = value
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "stdio: missing command".to_owned())?;
            let args: Vec<String> = value
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(ToOwned::to_owned)).collect())
                .unwrap_or_default();
            let env_entries: Vec<(String, String)> = value
                .get("env")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                        .collect()
                })
                .unwrap_or_default();
            let (resolved_command, args, env) =
                ensure_stdio_launch(command, &args, &env_entries, conversation_id, broadcaster).await?;

            Ok(McpServerConfig {
                transport: TransportType::Stdio,
                command: Some(resolved_command),
                args: Some(args),
                env: Some(env),
                url: None,
                headers: None,
                deferred: Some(false),
                startup_timeout_ms: None,
            })
        }
        "http" | "streamable_http" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "http: missing url".to_owned())?;
            let headers = value
                .get("headers")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default();

            Ok(McpServerConfig {
                transport: TransportType::StreamableHttp,
                command: None,
                args: None,
                env: None,
                url: Some(url.to_owned()),
                headers: Some(headers),
                deferred: Some(false),
                startup_timeout_ms: None,
            })
        }
        "sse" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "sse: missing url".to_owned())?;
            let headers = value
                .get("headers")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default();

            Ok(McpServerConfig {
                transport: TransportType::Sse,
                command: None,
                args: None,
                env: None,
                url: Some(url.to_owned()),
                headers: Some(headers),
                deferred: Some(false),
                startup_timeout_ms: None,
            })
        }
        other => Err(format!("unsupported transport_type: {other}")),
    }
}

async fn session_server_to_mcp_server_config(
    server: &SessionMcpServer,
    conversation_id: &str,
    broadcaster: Arc<dyn EventBroadcaster>,
) -> Result<McpServerConfig, String> {
    match &server.transport {
        SessionMcpTransport::Stdio { command, args, env } => {
            if command.is_empty() {
                return Err("stdio: missing command".to_owned());
            }
            let entries: Vec<(String, String)> = env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            let (command, args, env) =
                ensure_stdio_launch(command, args, &entries, conversation_id, broadcaster).await?;
            Ok(McpServerConfig {
                transport: TransportType::Stdio,
                command: Some(command),
                args: Some(args),
                env: Some(env),
                url: None,
                headers: None,
                deferred: Some(false),
                startup_timeout_ms: None,
            })
        }
        SessionMcpTransport::Http { url, headers } => {
            if url.is_empty() {
                return Err("http: missing url".to_owned());
            }
            Ok(McpServerConfig {
                transport: TransportType::StreamableHttp,
                command: None,
                args: None,
                env: None,
                url: Some(url.clone()),
                headers: Some(headers.clone()),
                deferred: Some(false),
                startup_timeout_ms: None,
            })
        }
        SessionMcpTransport::Sse { url, headers } => {
            if url.is_empty() {
                return Err("sse: missing url".to_owned());
            }
            Ok(McpServerConfig {
                transport: TransportType::Sse,
                command: None,
                args: None,
                env: None,
                url: Some(url.clone()),
                headers: Some(headers.clone()),
                deferred: Some(false),
                startup_timeout_ms: None,
            })
        }
        SessionMcpTransport::StreamableHttp { url, headers } => {
            if url.is_empty() {
                return Err("streamable_http: missing url".to_owned());
            }
            Ok(McpServerConfig {
                transport: TransportType::StreamableHttp,
                command: None,
                args: None,
                env: None,
                url: Some(url.clone()),
                headers: Some(headers.clone()),
                deferred: Some(false),
                startup_timeout_ms: None,
            })
        }
    }
}

async fn merge_session_snapshot_mcp_servers(
    extra_mcp_servers: &mut HashMap<String, McpServerConfig>,
    session_mcp_servers: &[SessionMcpServer],
    conversation_id: &str,
    broadcaster: Arc<dyn EventBroadcaster>,
) {
    for server in session_mcp_servers {
        match session_server_to_mcp_server_config(server, conversation_id, broadcaster.clone()).await {
            Ok(config) => {
                if extra_mcp_servers.insert(server.name.clone(), config).is_some() {
                    debug!(
                        conversation_id = %conversation_id,
                        server_name = %server.name,
                        "session_mcp: session snapshot overrides repo-backed MCP config"
                    );
                }
            }
            Err(err) => {
                warn!(
                    conversation_id = %conversation_id,
                    server_id = %server.id,
                    server_name = %server.name,
                    error = %err,
                    "session_mcp: failed to convert session snapshot; skipping"
                );
            }
        }
    }
}

async fn ensure_stdio_launch(
    command: &str,
    args: &[String],
    env: &[(String, String)],
    conversation_id: &str,
    broadcaster: Arc<dyn aionui_realtime::EventBroadcaster>,
) -> Result<(String, Vec<String>, HashMap<String, String>), String> {
    let reporter = conversation_runtime_reporter(broadcaster, conversation_id.to_owned());
    let resolved = ensure_runtime_command_with_reporter(command, Some(reporter.as_ref()))
        .await
        .map_err(|error| error.to_string())?;

    let mut final_args: Vec<String> = resolved
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    final_args.extend(args.iter().cloned());

    let mut final_env: HashMap<String, String> = env.iter().cloned().collect();
    final_env.extend(resolved.env.iter().map(|(name, value)| {
        (
            name.to_string_lossy().into_owned(),
            value.to_string_lossy().into_owned(),
        )
    }));

    Ok((resolved.program.to_string_lossy().into_owned(), final_args, final_env))
}

fn resolve_mcp_servers(overrides: &AionrsBuildExtra, conversation_id: &str) -> HashMap<String, McpServerConfig> {
    if let Some(cfg) = &overrides.team_mcp_stdio_config {
        return team_mcp_to_config(cfg);
    }
    if let Some(guide_cfg) = &overrides.guide_mcp_config
        && overrides
            .backend
            .as_deref()
            .is_some_and(|b| TEAM_CAPABLE_BACKENDS.contains(&b))
    {
        return guide_mcp_to_config(guide_cfg, overrides, conversation_id);
    }
    HashMap::new()
}

fn team_mcp_to_config(cfg: &TeamMcpStdioConfig) -> HashMap<String, McpServerConfig> {
    let mut env = HashMap::new();
    env.insert(TeamMcpStdioConfig::ENV_PORT.into(), cfg.port.to_string());
    env.insert(TeamMcpStdioConfig::ENV_TOKEN.into(), cfg.token.clone());
    env.insert(TeamMcpStdioConfig::ENV_SLOT_ID.into(), cfg.slot_id.clone());

    let server = McpServerConfig {
        transport: TransportType::Stdio,
        command: Some(cfg.binary_path.clone()),
        args: Some(vec!["mcp-team-stdio".into()]),
        env: Some(env),
        url: None,
        headers: None,
        deferred: Some(false),
        startup_timeout_ms: None,
    };

    HashMap::from([(TEAM_MCP_SERVER_NAME.to_owned(), server)])
}

fn guide_mcp_to_config(
    cfg: &GuideMcpConfig,
    overrides: &AionrsBuildExtra,
    conversation_id: &str,
) -> HashMap<String, McpServerConfig> {
    let mut env = HashMap::new();
    env.insert("AION_MCP_PORT".into(), cfg.port.to_string());
    env.insert("AION_MCP_TOKEN".into(), cfg.token.clone());
    env.insert("AION_MCP_BACKEND".into(), overrides.backend.clone().unwrap_or_default());
    env.insert("AION_MCP_CONVERSATION_ID".into(), conversation_id.to_owned());
    env.insert("AION_MCP_USER_ID".into(), overrides.user_id.clone().unwrap_or_default());

    let server = McpServerConfig {
        transport: TransportType::Stdio,
        command: Some(cfg.binary_path.clone()),
        args: Some(vec!["mcp-guide-stdio".into()]),
        env: Some(env),
        url: None,
        headers: None,
        deferred: Some(false),
        startup_timeout_ms: None,
    };

    HashMap::from([("aionui-team-guide".into(), server)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_realtime::BroadcastEventBus;
    use aionui_runtime::init as init_runtime;
    use std::sync::OnceLock;
    use std::{mem, path::PathBuf};

    fn path_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    #[cfg(unix)]
    fn test_runtime_data_dir() -> &'static PathBuf {
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        DIR.get_or_init(|| {
            let temp = tempfile::tempdir().expect("tempdir");
            let path = temp.path().to_path_buf();
            mem::forget(temp);
            init_runtime(&path);
            path
        })
    }

    #[cfg(unix)]
    fn install_fake_bundled_runtime() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let runtime_root = tmp.path().join("node").join("node-v24.11.0-darwin-arm64");
        let bin = runtime_root.join("bin");
        std::fs::create_dir_all(&bin).expect("create bin");

        for tool in ["node", "npm", "npx"] {
            let path = bin.join(tool);
            std::fs::write(&path, "#!/bin/sh\necho v24.11.0\n").expect("write tool");
            let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }

        tmp
    }

    fn make_row(
        name: &str,
        transport_type: &str,
        transport_config: &str,
        enabled: bool,
        builtin: bool,
    ) -> McpServerRow {
        McpServerRow {
            id: format!("mcp_{name}"),
            name: name.to_owned(),
            description: None,
            enabled,
            transport_type: transport_type.into(),
            transport_config: transport_config.into(),
            tools: None,
            last_test_status: "disconnected".into(),
            last_connected: None,
            original_json: None,
            builtin,
            deleted_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    struct MockMcpRepo {
        rows: Vec<McpServerRow>,
    }

    #[async_trait::async_trait]
    impl IMcpServerRepository for MockMcpRepo {
        async fn list(&self) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            Ok(self.rows.clone())
        }

        async fn find_by_id(&self, id: &str) -> Result<Option<McpServerRow>, aionui_db::DbError> {
            Ok(self.rows.iter().find(|row| row.id == id).cloned())
        }

        async fn find_by_name(&self, name: &str) -> Result<Option<McpServerRow>, aionui_db::DbError> {
            Ok(self.rows.iter().find(|row| row.name == name).cloned())
        }

        async fn create(
            &self,
            _params: aionui_db::CreateMcpServerParams<'_>,
        ) -> Result<McpServerRow, aionui_db::DbError> {
            unimplemented!("not needed for factory tests")
        }

        async fn update(
            &self,
            _id: &str,
            _params: aionui_db::UpdateMcpServerParams<'_>,
        ) -> Result<McpServerRow, aionui_db::DbError> {
            unimplemented!("not needed for factory tests")
        }

        async fn delete(&self, _id: &str) -> Result<(), aionui_db::DbError> {
            unimplemented!("not needed for factory tests")
        }

        async fn batch_upsert(
            &self,
            _servers: &[aionui_db::CreateMcpServerParams<'_>],
        ) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            unimplemented!("not needed for factory tests")
        }

        async fn update_status(
            &self,
            _id: &str,
            _status: &str,
            _last_connected: Option<aionui_common::TimestampMs>,
        ) -> Result<(), aionui_db::DbError> {
            unimplemented!("not needed for factory tests")
        }

        async fn update_tools(&self, _id: &str, _tools: Option<&str>) -> Result<(), aionui_db::DbError> {
            unimplemented!("not needed for factory tests")
        }
    }

    fn test_broadcaster() -> Arc<dyn EventBroadcaster> {
        Arc::new(BroadcastEventBus::new(16))
    }

    #[tokio::test]
    async fn aionrs_loads_mcp_servers_from_frozen_selection_snapshot() {
        let mut row = make_row(
            "mcp-docs",
            "http",
            r#"{"url":"http://localhost:54321/mcp","headers":{"Authorization":"Bearer frozen"}}"#,
            false,
            false,
        );
        row.id = "mcp-docs".into();
        let repo = MockMcpRepo { rows: vec![row] };
        let selected = vec!["mcp-docs".to_owned()];

        let extra_mcp_servers =
            load_user_mcp_servers(&repo, Some(&selected), "conv-frozen-mcp", test_broadcaster()).await;

        assert!(extra_mcp_servers.contains_key("mcp-docs"));
        assert_eq!(extra_mcp_servers["mcp-docs"].transport, TransportType::StreamableHttp);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn row_to_mcp_server_config_flattens_resolved_npx_command() {
        let _lock = path_test_lock().lock().await;
        let runtime = install_fake_bundled_runtime();
        let _runtime_data_dir = test_runtime_data_dir();
        unsafe { std::env::set_var("AIONUI_BUNDLED_MANAGED_RESOURCES", runtime.path()) };

        let row = make_row(
            "ctx7",
            "stdio",
            r#"{"command":"npx","args":["-y","@upstash/context7-mcp"],"env":{"K":"V"}}"#,
            true,
            false,
        );

        let config = row_to_mcp_server_config(&row, "conv-row", test_broadcaster())
            .await
            .expect("convert");
        unsafe { std::env::remove_var("AIONUI_BUNDLED_MANAGED_RESOURCES") };
        let command = config.command.as_deref().expect("resolved command");
        assert_ne!(command, "npx");
        assert!(command.ends_with("/npx"), "unexpected stdio command path: {command}");
        assert_eq!(
            config.args.as_ref(),
            Some(&vec!["-y".to_owned(), "@upstash/context7-mcp".to_owned()])
        );
    }

    #[test]
    fn normalize_aionrs_base_url_strips_v1() {
        assert_eq!(
            normalize_aionrs_base_url("https://api.openai.com/v1"),
            "https://api.openai.com"
        );
        assert_eq!(
            normalize_aionrs_base_url("https://api.openai.com/v1/"),
            "https://api.openai.com"
        );
        assert_eq!(
            normalize_aionrs_base_url("https://api.anthropic.com"),
            "https://api.anthropic.com"
        );
        assert_eq!(
            normalize_aionrs_base_url("https://api.deepseek.com/"),
            "https://api.deepseek.com"
        );
        assert_eq!(
            normalize_aionrs_base_url("http://localhost:11434"),
            "http://localhost:11434"
        );
        assert_eq!(normalize_aionrs_base_url(""), "");
    }

    #[test]
    fn map_aionrs_provider_known_platforms() {
        assert_eq!(map_aionrs_provider("anthropic", "m", None), "anthropic");
        assert_eq!(map_aionrs_provider("bedrock", "m", None), "bedrock");
        assert_eq!(map_aionrs_provider("gemini-vertex-ai", "m", None), "vertex");
    }

    #[test]
    fn map_aionrs_provider_custom_and_others_default_to_openai() {
        assert_eq!(map_aionrs_provider("custom", "gpt-4o", None), "openai");
        assert_eq!(map_aionrs_provider("gemini", "gemini-2.5-pro", None), "openai");
        assert_eq!(map_aionrs_provider("new-api", "m", None), "openai");
        assert_eq!(map_aionrs_provider("unknown", "m", None), "openai");
    }

    #[test]
    fn map_aionrs_provider_new_api_with_anthropic_protocol() {
        let protocols = r#"{"claude-sonnet":"anthropic","gpt-4o":"openai"}"#;
        assert_eq!(
            map_aionrs_provider("new-api", "claude-sonnet", Some(protocols)),
            "anthropic"
        );
        assert_eq!(map_aionrs_provider("new-api", "gpt-4o", Some(protocols)), "openai");
        assert_eq!(
            map_aionrs_provider("new-api", "unknown-model", Some(protocols)),
            "openai"
        );
    }

    #[test]
    fn map_aionrs_provider_new_api_with_invalid_json() {
        assert_eq!(map_aionrs_provider("new-api", "m", Some("not json")), "openai");
    }

    #[test]
    fn map_aionrs_provider_non_new_api_ignores_protocols() {
        let protocols = r#"{"m":"anthropic"}"#;
        assert_eq!(map_aionrs_provider("custom", "m", Some(protocols)), "openai");
    }

    #[test]
    fn is_openai_host_detects_official_api() {
        assert!(is_openai_host("https://api.openai.com/v1"));
        assert!(is_openai_host("https://api.openai.com"));
        assert!(is_openai_host("https://API.OPENAI.COM/v1"));
        assert!(!is_openai_host("https://api.deepseek.com/v1"));
        assert!(!is_openai_host("https://openai.example.com/v1"));
        assert!(!is_openai_host(""));
        assert!(!is_openai_host("not-a-url"));
    }

    #[test]
    fn resolve_openai_official_sets_max_completion_tokens() {
        let (base_url, compat) = resolve_aionrs_url_and_compat("custom", "https://api.openai.com/v1", "openai", false);
        assert_eq!(base_url.as_deref(), Some("https://api.openai.com"));
        assert_eq!(compat.max_tokens_field.as_deref(), Some("max_completion_tokens"));
        assert!(compat.api_path.is_none());
    }

    #[test]
    fn resolve_non_openai_keeps_default_max_tokens() {
        let (base_url, compat) =
            resolve_aionrs_url_and_compat("custom", "https://api.deepseek.com/v1", "openai", false);
        assert_eq!(base_url.as_deref(), Some("https://api.deepseek.com"));
        assert!(compat.max_tokens_field.is_none());
    }

    #[test]
    fn resolve_gemini_prepends_path_and_sets_api_path() {
        let (base_url, compat) =
            resolve_aionrs_url_and_compat("gemini", "https://generativelanguage.googleapis.com", "openai", false);
        assert_eq!(
            base_url.as_deref(),
            Some("https://generativelanguage.googleapis.com/v1beta/openai")
        );
        assert_eq!(compat.api_path.as_deref(), Some("/chat/completions"));
        assert!(compat.max_tokens_field.is_none());
    }

    #[test]
    fn resolve_anthropic_no_compat_overrides() {
        let (base_url, compat) =
            resolve_aionrs_url_and_compat("anthropic", "https://api.anthropic.com", "anthropic", false);
        assert_eq!(base_url.as_deref(), Some("https://api.anthropic.com"));
        assert!(compat.max_tokens_field.is_none());
        assert!(compat.api_path.is_none());
    }

    #[test]
    fn resolve_full_url_mode_uses_url_as_is() {
        let (base_url, compat) = resolve_aionrs_url_and_compat(
            "custom",
            "https://proxy.example.com/v1/chat/completions",
            "openai",
            true,
        );
        assert_eq!(
            base_url.as_deref(),
            Some("https://proxy.example.com/v1/chat/completions")
        );
        assert_eq!(compat.api_path.as_deref(), Some(""));
        assert!(compat.max_tokens_field.is_none());
    }

    #[test]
    fn resolve_full_url_mode_strips_trailing_slash() {
        let (base_url, compat) = resolve_aionrs_url_and_compat(
            "custom",
            "https://proxy.example.com/v1/chat/completions/",
            "openai",
            true,
        );
        assert_eq!(
            base_url.as_deref(),
            Some("https://proxy.example.com/v1/chat/completions")
        );
        assert_eq!(compat.api_path.as_deref(), Some(""));
    }

    #[test]
    fn resolve_full_url_false_still_normalizes() {
        let (base_url, compat) =
            resolve_aionrs_url_and_compat("custom", "https://api.deepseek.com/v1", "openai", false);
        assert_eq!(base_url.as_deref(), Some("https://api.deepseek.com"));
        assert!(compat.api_path.is_none());
    }

    #[test]
    fn resolve_mcp_servers_team_takes_priority() {
        let overrides = AionrsBuildExtra {
            team_mcp_stdio_config: Some(TeamMcpStdioConfig {
                team_id: "team-42".into(),
                port: 9000,
                token: "tok".into(),
                slot_id: "slot-1".into(),
                binary_path: "/usr/bin/backend".into(),
            }),
            guide_mcp_config: Some(GuideMcpConfig {
                port: 8000,
                token: "guide-tok".into(),
                binary_path: "/usr/bin/backend".into(),
            }),
            backend: Some("aionrs".into()),
            ..Default::default()
        };

        let result = resolve_mcp_servers(&overrides, "conv-1");
        assert_eq!(result.len(), 1);
        assert!(result.contains_key(TEAM_MCP_SERVER_NAME));

        let server = &result[TEAM_MCP_SERVER_NAME];
        assert_eq!(server.transport, TransportType::Stdio);
        assert_eq!(server.command.as_deref(), Some("/usr/bin/backend"));
        assert_eq!(server.args.as_deref(), Some(&["mcp-team-stdio".to_owned()][..]));
        assert_eq!(server.deferred, Some(false));

        let env = server.env.as_ref().unwrap();
        assert_eq!(env.get("TEAM_MCP_PORT"), Some(&"9000".to_owned()));
        assert_eq!(env.get("TEAM_MCP_TOKEN"), Some(&"tok".to_owned()));
        assert_eq!(env.get("TEAM_AGENT_SLOT_ID"), Some(&"slot-1".to_owned()));
    }

    #[test]
    fn resolve_mcp_servers_guide_when_no_team() {
        let overrides = AionrsBuildExtra {
            guide_mcp_config: Some(GuideMcpConfig {
                port: 8000,
                token: "guide-tok".into(),
                binary_path: "/usr/bin/backend".into(),
            }),
            backend: Some("aionrs".into()),
            user_id: Some("user-1".into()),
            ..Default::default()
        };

        let result = resolve_mcp_servers(&overrides, "conv-2");
        assert_eq!(result.len(), 1);
        assert!(result.contains_key("aionui-team-guide"));

        let server = &result["aionui-team-guide"];
        assert_eq!(server.transport, TransportType::Stdio);
        assert_eq!(server.command.as_deref(), Some("/usr/bin/backend"));
        assert_eq!(server.args.as_deref(), Some(&["mcp-guide-stdio".to_owned()][..]));
        assert_eq!(server.deferred, Some(false));

        let env = server.env.as_ref().unwrap();
        assert_eq!(env.get("AION_MCP_PORT"), Some(&"8000".to_owned()));
        assert_eq!(env.get("AION_MCP_TOKEN"), Some(&"guide-tok".to_owned()));
        assert_eq!(env.get("AION_MCP_BACKEND"), Some(&"aionrs".to_owned()));
        assert_eq!(env.get("AION_MCP_CONVERSATION_ID"), Some(&"conv-2".to_owned()));
        assert_eq!(env.get("AION_MCP_USER_ID"), Some(&"user-1".to_owned()));
    }

    #[test]
    fn aionrs_guide_prompt_hands_off_after_create_team() {
        let overrides = AionrsBuildExtra {
            system_prompt: Some(team_guide_prompt::build_solo_team_guide_prompt("aionrs")),
            ..Default::default()
        };

        let prompt = overrides.system_prompt.as_deref().unwrap();
        assert!(prompt.contains("aion_create_team"));
        assert!(prompt.contains("aion_list_models"));
        assert!(prompt.contains("hand off to the created Team conversation"));
        assert!(!prompt.contains("Immediately"));
        assert!(!prompt.contains(
            "use team tools (`team_spawn_agent`, `team_send_message`, `team_members`, `team_task_create`, etc.) to manage your team"
        ));
    }

    #[test]
    fn resolve_mcp_servers_empty_when_no_config() {
        let overrides = AionrsBuildExtra::default();
        let result = resolve_mcp_servers(&overrides, "conv-3");
        assert!(result.is_empty());
    }

    #[test]
    fn resolve_mcp_servers_guide_skipped_for_unknown_backend() {
        let overrides = AionrsBuildExtra {
            guide_mcp_config: Some(GuideMcpConfig {
                port: 8000,
                token: "tok".into(),
                binary_path: "/bin/x".into(),
            }),
            backend: Some("unknown-vendor".into()),
            ..Default::default()
        };

        let result = resolve_mcp_servers(&overrides, "conv-4");
        assert!(result.is_empty());
    }

    #[test]
    fn resolve_mcp_servers_guide_skipped_when_backend_none() {
        let overrides = AionrsBuildExtra {
            guide_mcp_config: Some(GuideMcpConfig {
                port: 8000,
                token: "tok".into(),
                binary_path: "/bin/x".into(),
            }),
            backend: None,
            ..Default::default()
        };

        let result = resolve_mcp_servers(&overrides, "conv-5");
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn session_snapshot_overrides_repo_backed_mcp_config() {
        let snapshot_command = std::env::current_exe()
            .expect("current test executable")
            .to_string_lossy()
            .into_owned();
        let mut servers = HashMap::from([(
            "demo-mcp".to_owned(),
            McpServerConfig {
                transport: TransportType::Stdio,
                command: Some("npx".into()),
                args: Some(vec!["-y".into(), "@old/server".into()]),
                env: Some(HashMap::new()),
                url: None,
                headers: None,
                deferred: Some(false),
                startup_timeout_ms: None,
            },
        )]);

        let snapshot = vec![SessionMcpServer {
            id: "mcp_1".into(),
            name: "demo-mcp".into(),
            transport: SessionMcpTransport::Stdio {
                command: snapshot_command.clone(),
                args: vec!["new-server".into()],
                env: HashMap::from([("TOKEN".into(), "abc".into())]),
            },
        }];

        merge_session_snapshot_mcp_servers(&mut servers, &snapshot, "conv-override", test_broadcaster()).await;

        let server = servers.get("demo-mcp").expect("snapshot should remain");
        assert_eq!(server.transport, TransportType::Stdio);
        let command = server.command.as_deref().expect("stdio command should exist");
        assert_eq!(command, snapshot_command);
        assert_eq!(server.args.as_deref(), Some(&["new-server".to_owned()][..]));
        assert_eq!(
            server.env.as_ref().and_then(|env| env.get("TOKEN")),
            Some(&"abc".to_owned())
        );
    }

    #[test]
    fn resolve_bedrock_config_access_key() {
        let json = r#"{"auth_method":"accessKey","region":"us-west-2","access_key_id":"AKIA123","secret_access_key":"secret456"}"#;
        let result = resolve_bedrock_config(Some(json)).unwrap();
        assert_eq!(result.region.as_deref(), Some("us-west-2"));
        assert_eq!(result.access_key_id.as_deref(), Some("AKIA123"));
        assert_eq!(result.secret_access_key.as_deref(), Some("secret456"));
        assert!(result.profile.is_none());
        assert!(result.session_token.is_none());
    }

    #[test]
    fn resolve_bedrock_config_profile() {
        let json = r#"{"auth_method":"profile","region":"eu-west-1","profile":"my-profile"}"#;
        let result = resolve_bedrock_config(Some(json)).unwrap();
        assert_eq!(result.region.as_deref(), Some("eu-west-1"));
        assert_eq!(result.profile.as_deref(), Some("my-profile"));
        assert!(result.access_key_id.is_none());
        assert!(result.secret_access_key.is_none());
    }

    #[test]
    fn resolve_bedrock_config_none_when_json_missing() {
        assert!(resolve_bedrock_config(None).is_none());
    }

    #[test]
    fn resolve_bedrock_config_none_when_json_invalid() {
        assert!(resolve_bedrock_config(Some("not-json")).is_none());
    }

    #[test]
    fn preset_rules_merged_into_system_prompt_when_no_existing() {
        let json = serde_json::json!({
            "preset_rules": "You are a data analyst. Always use Python.",
        });
        let mut overrides: AionrsBuildExtra = serde_json::from_value(json).unwrap();

        if let Some(rules) = overrides.preset_rules.take() {
            overrides.system_prompt = Some(match overrides.system_prompt.take() {
                Some(existing) => format!("{existing}\n\n{rules}"),
                None => rules,
            });
        }

        assert_eq!(
            overrides.system_prompt.as_deref(),
            Some("You are a data analyst. Always use Python.")
        );
        assert!(overrides.preset_rules.is_none());
    }

    #[test]
    fn preset_rules_appended_to_existing_system_prompt() {
        let json = serde_json::json!({
            "system_prompt": "Be concise.",
            "preset_rules": "You are a data analyst.",
        });
        let mut overrides: AionrsBuildExtra = serde_json::from_value(json).unwrap();

        if let Some(rules) = overrides.preset_rules.take() {
            overrides.system_prompt = Some(match overrides.system_prompt.take() {
                Some(existing) => format!("{existing}\n\n{rules}"),
                None => rules,
            });
        }

        assert_eq!(
            overrides.system_prompt.as_deref(),
            Some("Be concise.\n\nYou are a data analyst.")
        );
    }

    #[test]
    fn no_preset_rules_leaves_system_prompt_unchanged() {
        let json = serde_json::json!({
            "system_prompt": "Be concise.",
        });
        let mut overrides: AionrsBuildExtra = serde_json::from_value(json).unwrap();

        if let Some(rules) = overrides.preset_rules.take() {
            overrides.system_prompt = Some(match overrides.system_prompt.take() {
                Some(existing) => format!("{existing}\n\n{rules}"),
                None => rules,
            });
        }

        assert_eq!(overrides.system_prompt.as_deref(), Some("Be concise."));
    }
}
