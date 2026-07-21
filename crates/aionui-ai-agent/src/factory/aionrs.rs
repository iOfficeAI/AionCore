use std::collections::HashMap;
use std::sync::Arc;

use aion_agent::session::SessionManager;
use aion_config::compat::OpenAiApiMode;
use aion_config::config::{McpServerConfig, TransportType};
use aionui_api_types::{
    AionrsBuildExtra, SessionMcpServer, SessionMcpTransport, TEAM_MCP_SERVER_NAME, TeamMcpStdioConfig,
};
use aionui_common::ProviderWithModel;
use aionui_db::IMcpServerRepository;
use aionui_db::models::McpServerRow;
use aionui_realtime::EventBroadcaster;
use aionui_runtime::ensure_runtime_command_with_reporter;
use serde_json::{Map, Value};
use tracing::{debug, info, warn};

use crate::agent_task::AgentInstance;
use crate::error::AgentError;
use crate::factory::AgentFactoryDeps;
use crate::factory::context::FactoryContext;
use crate::manager::aionrs::{AionrsAgentManager, sanitize_session_messages};
use crate::runtime_status::conversation_runtime_reporter;
use crate::session_context::AionrsSessionBuildContext;
use crate::types::{AionrsCompatOverrides, AionrsResolvedConfig};
pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    build_context: AionrsSessionBuildContext,
    model: ProviderWithModel,
    ctx: FactoryContext,
) -> Result<AgentInstance, AgentError> {
    let mut overrides = build_context.config;
    let resolved_skills = overrides.skills.clone();

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

    let mut extra_mcp_servers = resolve_mcp_servers(&overrides);
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

    let provider = map_aionrs_provider(&row.platform, &model_id, row.model_protocols.as_deref())?;

    let (base_url, compat_overrides) =
        resolve_aionrs_url_and_compat(&row.platform, &row.base_url, &provider, &model_id, row.is_full_url);

    if compat_overrides.openai_api_mode == Some(OpenAiApiMode::Responses) {
        info!(
            conversation_id = %ctx.conversation_id,
            platform = %row.platform,
            provider = %provider,
            model = %model_id,
            is_full_url = row.is_full_url,
            api_mode = "responses",
            "Resolved Aionrs OpenAI transport"
        );
    }

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
        max_tokens: None,
        max_turns: overrides.max_turns,
        max_tool_call_malformed_turns: overrides.max_tool_call_malformed_turns,
        max_tool_call_failure_turns: overrides.max_tool_call_failure_turns,
        compat_overrides,
        session_directory,
        session_mode: overrides.session_mode,
        skills: resolved_skills,
        extra_mcp_servers,
        bedrock_config,
        runtime_env: ctx.runtime_env,
        prompt_dump_dir: crate::dev_prompt_dump::dump_dir_for_data_dir(&deps.data_dir, deps.dump_prompts),
    };

    if let Some(system_prompt) = config.system_prompt.as_deref()
        && let Some(dump_dir) = crate::dev_prompt_dump::dump_dir_for_data_dir(&deps.data_dir, deps.dump_prompts)
    {
        match crate::dev_prompt_dump::dump_prompt(
            &dump_dir,
            crate::dev_prompt_dump::PromptDump {
                kind: "aionrs-system-prompt",
                backend: None,
                conversation_id: &ctx.conversation_id,
                session_id: None,
                msg_id: None,
                turn_id: None,
                prompt: system_prompt,
            },
        ) {
            Ok(path) => {
                debug!(
                    conversation_id = %ctx.conversation_id,
                    path = %path.display(),
                    "DEV prompt dump written"
                );
            }
            Err(error) => {
                warn!(
                    conversation_id = %ctx.conversation_id,
                    error = %error,
                    "DEV prompt dump failed"
                );
            }
        }
    }

    let agent = AionrsAgentManager::new(ctx.conversation_id, ctx.workspace, config, resume_session).await?;
    Ok(AgentInstance::Aionrs(Arc::new(agent)))
}

/// Map AionUi DB platform/protocol settings to the aionrs provider identifier.
pub(crate) fn map_aionrs_provider(
    platform: &str,
    model_id: &str,
    model_protocols: Option<&str>,
) -> Result<String, AgentError> {
    match platform {
        "anthropic" => return Ok("anthropic".to_owned()),
        "bedrock" => return Ok("bedrock".to_owned()),
        "gemini" | "openai" => return Ok("openai".to_owned()),
        "gemini-vertex-ai" => return Ok("vertex".to_owned()),
        _ => {}
    }

    let protocol = resolve_model_protocol(model_id, model_protocols)?;
    match protocol.as_str() {
        "anthropic" => Ok("anthropic".to_owned()),
        "openai" | "gemini" => Ok("openai".to_owned()),
        other => Err(AgentError::bad_request(format!(
            "Unsupported model protocol '{other}' for model '{model_id}'"
        ))),
    }
}

/// Resolve base_url and compat overrides for the aionrs provider.
///
/// The stored base_url is treated as the user-controlled endpoint prefix.
/// OpenAI-compatible providers use Chat Completions by default; official
/// OpenAI GPT-5.6 models use Responses. Anthropic-compatible providers append
/// `/v1/messages`.
pub(crate) fn resolve_aionrs_url_and_compat(
    platform: &str,
    raw_base_url: &str,
    mapped_provider: &str,
    model_id: &str,
    is_full_url: bool,
) -> (Option<String>, AionrsCompatOverrides) {
    let mut compat = AionrsCompatOverrides::default();
    let use_responses = uses_openai_responses_api(platform, mapped_provider, model_id);

    if is_full_url {
        let trimmed = raw_base_url.trim_end_matches('/');
        if use_responses && let Some(responses_url) = rewrite_chat_completions_url_to_responses(trimmed) {
            compat.openai_api_mode = Some(OpenAiApiMode::Responses);
            compat.api_path = Some(String::new());
            return (Some(responses_url), compat);
        }
        compat.api_path = Some(String::new());
        return (Some(trimmed.to_owned()), compat);
    }

    if platform == "gemini" {
        let trimmed = raw_base_url.trim_end_matches('/');
        let base = format!("{trimmed}/v1beta/openai");
        compat.api_path = Some("/chat/completions".to_owned());
        return (Some(base), compat);
    }

    let trimmed = raw_base_url.trim_end_matches('/');
    let base_url = Some(trimmed.to_owned()).filter(|u| !u.is_empty());

    if use_responses {
        compat.openai_api_mode = Some(OpenAiApiMode::Responses);
    }

    match mapped_provider {
        "openai" if base_url.is_some() => {
            compat.api_path = Some(if use_responses {
                "/responses".to_owned()
            } else {
                "/chat/completions".to_owned()
            });
        }
        "anthropic" if base_url.is_some() && platform != "anthropic" => {
            compat.api_path = Some("/v1/messages".to_owned());
        }
        _ => {}
    }

    if mapped_provider == "openai" && is_openai_host(raw_base_url) {
        compat.max_tokens_field = Some("max_completion_tokens".to_owned());
    }

    (base_url, compat)
}

fn uses_openai_responses_api(platform: &str, mapped_provider: &str, model_id: &str) -> bool {
    if mapped_provider != "openai" || platform == "gemini" {
        return false;
    }

    let model = model_id.to_ascii_lowercase();
    model == "gpt-5.6" || model.starts_with("gpt-5.6-")
}

fn rewrite_chat_completions_url_to_responses(url: &str) -> Option<String> {
    url.strip_suffix("/chat/completions")
        .map(|prefix| format!("{prefix}/responses"))
        .or_else(|| url.ends_with("/responses").then(|| url.to_owned()))
}

fn resolve_model_protocol(model_id: &str, model_protocols: Option<&str>) -> Result<String, AgentError> {
    let Some(protocols_json) = model_protocols.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok("openai".to_owned());
    };
    let map = serde_json::from_str::<Map<String, Value>>(protocols_json).map_err(|error| {
        AgentError::bad_request(format!(
            "Invalid model protocols config for model '{model_id}': {error}"
        ))
    })?;
    match map.get(model_id) {
        Some(Value::String(protocol)) if !protocol.is_empty() => Ok(protocol.clone()),
        _ => Ok("openai".to_owned()),
    }
}

fn is_openai_host(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .map(|rest| rest == "api.openai.com" || rest.starts_with("api.openai.com/"))
        .unwrap_or(false)
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

fn resolve_mcp_servers(overrides: &AionrsBuildExtra) -> HashMap<String, McpServerConfig> {
    if let Some(cfg) = &overrides.team_mcp_stdio_config {
        return team_mcp_to_config(cfg);
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

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_realtime::BroadcastEventBus;
    use aionui_runtime::{ManagedResourcesMode, init as init_runtime, set_managed_resources_mode};
    use std::sync::OnceLock;
    use std::{
        mem,
        path::{Path, PathBuf},
    };

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
        let runtime_root = tmp.path().join("node").join(current_node_runtime_directory_name());
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

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn current_node_runtime_directory_name() -> &'static str {
        "node-v24.11.0-darwin-arm64"
    }

    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    fn current_node_runtime_directory_name() -> &'static str {
        "node-v24.11.0-darwin-x64"
    }

    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    fn current_node_runtime_directory_name() -> &'static str {
        "node-v24.11.0-linux-arm64"
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn current_node_runtime_directory_name() -> &'static str {
        "node-v24.11.0-linux-x64"
    }

    #[cfg(all(
        unix,
        not(any(
            all(target_os = "macos", target_arch = "aarch64"),
            all(target_os = "macos", target_arch = "x86_64"),
            all(target_os = "linux", target_arch = "aarch64"),
            all(target_os = "linux", target_arch = "x86_64")
        ))
    ))]
    fn current_node_runtime_directory_name() -> &'static str {
        panic!("unsupported managed Node runtime test platform")
    }

    #[cfg(unix)]
    struct BundledRuntimeModeGuard;

    #[cfg(unix)]
    impl BundledRuntimeModeGuard {
        fn install(root: &Path) -> Self {
            unsafe { std::env::set_var("AIONUI_BUNDLED_MANAGED_RESOURCES", root) };
            set_managed_resources_mode(ManagedResourcesMode::Bundled);
            Self
        }
    }

    #[cfg(unix)]
    impl Drop for BundledRuntimeModeGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("AIONUI_BUNDLED_MANAGED_RESOURCES") };
            set_managed_resources_mode(ManagedResourcesMode::Download);
        }
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
        let _runtime_mode = BundledRuntimeModeGuard::install(runtime.path());

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
        let command = config.command.as_deref().expect("resolved command");
        assert_ne!(command, "npx");
        assert!(command.ends_with("/npx"), "unexpected stdio command path: {command}");
        assert_eq!(
            config.args.as_ref(),
            Some(&vec!["-y".to_owned(), "@upstash/context7-mcp".to_owned()])
        );
    }

    struct ProviderMappingCase<'a> {
        name: &'a str,
        platform: &'a str,
        model_id: &'a str,
        model_protocols: Option<&'a str>,
        expected_provider: Option<&'a str>,
    }

    #[test]
    fn map_aionrs_provider_table_driven_cases() {
        let cases = [
            ProviderMappingCase {
                name: "anthropic platform defaults anthropic",
                platform: "anthropic",
                model_id: "m",
                model_protocols: None,
                expected_provider: Some("anthropic"),
            },
            ProviderMappingCase {
                name: "bedrock platform defaults bedrock",
                platform: "bedrock",
                model_id: "m",
                model_protocols: None,
                expected_provider: Some("bedrock"),
            },
            ProviderMappingCase {
                name: "gemini platform uses openai-compatible transport",
                platform: "gemini",
                model_id: "m",
                model_protocols: None,
                expected_provider: Some("openai"),
            },
            ProviderMappingCase {
                name: "openai platform defaults openai",
                platform: "openai",
                model_id: "m",
                model_protocols: None,
                expected_provider: Some("openai"),
            },
            ProviderMappingCase {
                name: "gemini vertex platform maps to vertex",
                platform: "gemini-vertex-ai",
                model_id: "m",
                model_protocols: None,
                expected_provider: Some("vertex"),
            },
            ProviderMappingCase {
                name: "custom without protocols defaults openai",
                platform: "custom",
                model_id: "gpt-4o",
                model_protocols: None,
                expected_provider: Some("openai"),
            },
            ProviderMappingCase {
                name: "new-api without protocols defaults openai",
                platform: "new-api",
                model_id: "m",
                model_protocols: None,
                expected_provider: Some("openai"),
            },
            ProviderMappingCase {
                name: "unknown without protocols defaults openai",
                platform: "unknown",
                model_id: "m",
                model_protocols: None,
                expected_provider: Some("openai"),
            },
            ProviderMappingCase {
                name: "new-api can select anthropic by model protocol",
                platform: "new-api",
                model_id: "claude",
                model_protocols: Some(r#"{"claude":"anthropic"}"#),
                expected_provider: Some("anthropic"),
            },
            ProviderMappingCase {
                name: "new-api can select openai by model protocol",
                platform: "new-api",
                model_id: "gpt",
                model_protocols: Some(r#"{"gpt":"openai"}"#),
                expected_provider: Some("openai"),
            },
            ProviderMappingCase {
                name: "new-api gemini protocol uses openai provider",
                platform: "new-api",
                model_id: "gemini",
                model_protocols: Some(r#"{"gemini":"gemini"}"#),
                expected_provider: Some("openai"),
            },
            ProviderMappingCase {
                name: "custom can select anthropic by model protocol",
                platform: "custom",
                model_id: "claude",
                model_protocols: Some(r#"{"claude":"anthropic"}"#),
                expected_provider: Some("anthropic"),
            },
            ProviderMappingCase {
                name: "custom can select openai by model protocol",
                platform: "custom",
                model_id: "gpt",
                model_protocols: Some(r#"{"gpt":"openai"}"#),
                expected_provider: Some("openai"),
            },
            ProviderMappingCase {
                name: "custom missing model protocol defaults openai",
                platform: "custom",
                model_id: "unknown-model",
                model_protocols: Some(r#"{"claude":"anthropic"}"#),
                expected_provider: Some("openai"),
            },
            ProviderMappingCase {
                name: "custom empty protocol map defaults openai",
                platform: "custom",
                model_id: "m",
                model_protocols: Some("{}"),
                expected_provider: Some("openai"),
            },
            ProviderMappingCase {
                name: "custom empty protocol value defaults openai",
                platform: "custom",
                model_id: "m",
                model_protocols: Some(r#"{"m":""}"#),
                expected_provider: Some("openai"),
            },
            ProviderMappingCase {
                name: "custom non-string protocol value defaults openai",
                platform: "custom",
                model_id: "m",
                model_protocols: Some(r#"{"m":123}"#),
                expected_provider: Some("openai"),
            },
            ProviderMappingCase {
                name: "invalid protocol json returns error",
                platform: "custom",
                model_id: "m",
                model_protocols: Some("not json"),
                expected_provider: None,
            },
            ProviderMappingCase {
                name: "unsupported protocol returns error",
                platform: "custom",
                model_id: "m",
                model_protocols: Some(r#"{"m":"unsupported"}"#),
                expected_provider: None,
            },
            ProviderMappingCase {
                name: "known openai platform ignores anthropic protocol",
                platform: "openai",
                model_id: "claude",
                model_protocols: Some(r#"{"claude":"anthropic"}"#),
                expected_provider: Some("openai"),
            },
            ProviderMappingCase {
                name: "known anthropic platform ignores openai protocol",
                platform: "anthropic",
                model_id: "gpt",
                model_protocols: Some(r#"{"gpt":"openai"}"#),
                expected_provider: Some("anthropic"),
            },
        ];

        for case in cases {
            let result = map_aionrs_provider(case.platform, case.model_id, case.model_protocols);
            match case.expected_provider {
                Some(expected) => assert_eq!(result.unwrap(), expected, "{}", case.name),
                None => assert!(result.is_err(), "{}", case.name),
            }
        }
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
    fn openai_protocol_gpt_5_6_family_uses_responses() {
        let cases = [
            ("openai", "https://api.openai.com/v1", "gpt-5.6"),
            ("custom", "https://api.openai.com/v1", "gpt-5.6-sol"),
            ("new-api", "https://proxy.example.com/v1", "GPT-5.6-SOL-2026-07-01"),
        ];

        for (platform, raw_base_url, model_id) in cases {
            let (base_url, compat) = resolve_aionrs_url_and_compat(platform, raw_base_url, "openai", model_id, false);

            assert_eq!(base_url.as_deref(), Some(raw_base_url), "{model_id}");
            assert_eq!(compat.openai_api_mode, Some(OpenAiApiMode::Responses), "{model_id}");
            assert_eq!(compat.api_path.as_deref(), Some("/responses"), "{model_id}");
        }
    }

    #[test]
    fn responses_selection_does_not_leak_to_other_providers_or_full_urls() {
        let cases = [
            ("gemini", "openai", "gpt-5.6-sol", false),
            ("bedrock", "bedrock", "openai.gpt-5.6-sol", false),
            ("openai", "openai", "gpt-5.60-sol", false),
        ];

        for (platform, provider, model_id, is_full_url) in cases {
            let (_, compat) = resolve_aionrs_url_and_compat(
                platform,
                "https://example.test/v1/chat/completions",
                provider,
                model_id,
                is_full_url,
            );

            assert_eq!(compat.openai_api_mode, None, "{platform}/{model_id}");
        }
    }

    #[test]
    fn openai_protocol_gpt_5_6_full_url_is_rewritten_to_responses() {
        for raw_base_url in [
            "https://api.openai.com/v1/chat/completions",
            "https://proxy.example.com/v1/chat/completions/",
            "https://api.openai.com/v1/responses",
        ] {
            let (base_url, compat) =
                resolve_aionrs_url_and_compat("custom", raw_base_url, "openai", "gpt-5.6-sol", true);

            assert!(base_url.as_deref().unwrap().ends_with("/responses"), "{raw_base_url}");
            assert_eq!(compat.openai_api_mode, Some(OpenAiApiMode::Responses), "{raw_base_url}");
            assert_eq!(compat.api_path.as_deref(), Some(""), "{raw_base_url}");
        }
    }

    #[test]
    fn unrelated_full_url_is_not_rewritten_for_gpt_5_6() {
        let (base_url, compat) = resolve_aionrs_url_and_compat(
            "custom",
            "https://proxy.example.com/generate",
            "openai",
            "gpt-5.6-sol",
            true,
        );

        assert_eq!(base_url.as_deref(), Some("https://proxy.example.com/generate"));
        assert_eq!(compat.openai_api_mode, None);
        assert_eq!(compat.api_path.as_deref(), Some(""));
    }

    struct UrlCompatCase<'a> {
        name: &'a str,
        platform: &'a str,
        raw_base_url: &'a str,
        mapped_provider: &'a str,
        is_full_url: bool,
        expected_base_url: Option<&'a str>,
        expected_api_path: Option<&'a str>,
        expected_max_tokens_field: Option<&'a str>,
    }

    #[test]
    fn resolve_aionrs_url_and_compat_table_driven_cases() {
        let cases = [
            UrlCompatCase {
                name: "official openai root appends chat completions",
                platform: "custom",
                raw_base_url: "https://api.openai.com",
                mapped_provider: "openai",
                is_full_url: false,
                expected_base_url: Some("https://api.openai.com"),
                expected_api_path: Some("/chat/completions"),
                expected_max_tokens_field: Some("max_completion_tokens"),
            },
            UrlCompatCase {
                name: "official openai v1 prefix is preserved",
                platform: "custom",
                raw_base_url: "https://api.openai.com/v1",
                mapped_provider: "openai",
                is_full_url: false,
                expected_base_url: Some("https://api.openai.com/v1"),
                expected_api_path: Some("/chat/completions"),
                expected_max_tokens_field: Some("max_completion_tokens"),
            },
            UrlCompatCase {
                name: "official openai v1 trailing slash is trimmed",
                platform: "custom",
                raw_base_url: "https://api.openai.com/v1/",
                mapped_provider: "openai",
                is_full_url: false,
                expected_base_url: Some("https://api.openai.com/v1"),
                expected_api_path: Some("/chat/completions"),
                expected_max_tokens_field: Some("max_completion_tokens"),
            },
            UrlCompatCase {
                name: "deepseek openai-compatible prefix is preserved",
                platform: "custom",
                raw_base_url: "https://api.deepseek.com/v1",
                mapped_provider: "openai",
                is_full_url: false,
                expected_base_url: Some("https://api.deepseek.com/v1"),
                expected_api_path: Some("/chat/completions"),
                expected_max_tokens_field: None,
            },
            UrlCompatCase {
                name: "glm openai-compatible prefix is preserved",
                platform: "custom",
                raw_base_url: "https://open.bigmodel.cn/api/paas/v4",
                mapped_provider: "openai",
                is_full_url: false,
                expected_base_url: Some("https://open.bigmodel.cn/api/paas/v4"),
                expected_api_path: Some("/chat/completions"),
                expected_max_tokens_field: None,
            },
            UrlCompatCase {
                name: "new-api openai-compatible prefix is preserved",
                platform: "new-api",
                raw_base_url: "https://host/v1",
                mapped_provider: "openai",
                is_full_url: false,
                expected_base_url: Some("https://host/v1"),
                expected_api_path: Some("/chat/completions"),
                expected_max_tokens_field: None,
            },
            UrlCompatCase {
                name: "local openai-compatible prefix is preserved",
                platform: "custom",
                raw_base_url: "http://localhost:11434/v1",
                mapped_provider: "openai",
                is_full_url: false,
                expected_base_url: Some("http://localhost:11434/v1"),
                expected_api_path: Some("/chat/completions"),
                expected_max_tokens_field: None,
            },
            UrlCompatCase {
                name: "gemini uses openai-compatible endpoint prefix",
                platform: "gemini",
                raw_base_url: "https://generativelanguage.googleapis.com",
                mapped_provider: "openai",
                is_full_url: false,
                expected_base_url: Some("https://generativelanguage.googleapis.com/v1beta/openai"),
                expected_api_path: Some("/chat/completions"),
                expected_max_tokens_field: None,
            },
            UrlCompatCase {
                name: "official anthropic keeps aionrs defaults",
                platform: "anthropic",
                raw_base_url: "https://api.anthropic.com",
                mapped_provider: "anthropic",
                is_full_url: false,
                expected_base_url: Some("https://api.anthropic.com"),
                expected_api_path: None,
                expected_max_tokens_field: None,
            },
            UrlCompatCase {
                name: "official anthropic trims trailing slash",
                platform: "anthropic",
                raw_base_url: "https://api.anthropic.com/",
                mapped_provider: "anthropic",
                is_full_url: false,
                expected_base_url: Some("https://api.anthropic.com"),
                expected_api_path: None,
                expected_max_tokens_field: None,
            },
            UrlCompatCase {
                name: "custom anthropic-compatible unversioned prefix appends versioned messages path",
                platform: "custom",
                raw_base_url: "https://proxy.example.com/anthropic",
                mapped_provider: "anthropic",
                is_full_url: false,
                expected_base_url: Some("https://proxy.example.com/anthropic"),
                expected_api_path: Some("/v1/messages"),
                expected_max_tokens_field: None,
            },
            UrlCompatCase {
                name: "openai full url mode uses url as-is",
                platform: "custom",
                raw_base_url: "https://proxy.example.com/v1/chat/completions",
                mapped_provider: "openai",
                is_full_url: true,
                expected_base_url: Some("https://proxy.example.com/v1/chat/completions"),
                expected_api_path: Some(""),
                expected_max_tokens_field: None,
            },
            UrlCompatCase {
                name: "anthropic full url mode uses url as-is",
                platform: "custom",
                raw_base_url: "https://proxy.example.com/v1/messages",
                mapped_provider: "anthropic",
                is_full_url: true,
                expected_base_url: Some("https://proxy.example.com/v1/messages"),
                expected_api_path: Some(""),
                expected_max_tokens_field: None,
            },
            UrlCompatCase {
                name: "full url mode trims trailing slash",
                platform: "custom",
                raw_base_url: "https://proxy.example.com/v1/chat/completions/",
                mapped_provider: "openai",
                is_full_url: true,
                expected_base_url: Some("https://proxy.example.com/v1/chat/completions"),
                expected_api_path: Some(""),
                expected_max_tokens_field: None,
            },
            UrlCompatCase {
                name: "empty base url leaves compat unset",
                platform: "custom",
                raw_base_url: "",
                mapped_provider: "openai",
                is_full_url: false,
                expected_base_url: None,
                expected_api_path: None,
                expected_max_tokens_field: None,
            },
        ];

        for case in cases {
            let (base_url, compat) = resolve_aionrs_url_and_compat(
                case.platform,
                case.raw_base_url,
                case.mapped_provider,
                "gpt-4o",
                case.is_full_url,
            );
            assert_eq!(base_url.as_deref(), case.expected_base_url, "{}", case.name);
            assert_eq!(compat.api_path.as_deref(), case.expected_api_path, "{}", case.name);
            assert_eq!(
                compat.max_tokens_field.as_deref(),
                case.expected_max_tokens_field,
                "{}",
                case.name
            );
        }
    }

    struct OpenAiPresetBaseUrlCase<'a> {
        name: &'a str,
        base_url: &'a str,
        expected_max_tokens_field: Option<&'a str>,
    }

    #[test]
    fn ui_model_platform_base_url_presets_default_to_openai_chat_completions() {
        let cases = [
            OpenAiPresetBaseUrlCase {
                name: "OpenAI",
                base_url: "https://api.openai.com/v1",
                expected_max_tokens_field: Some("max_completion_tokens"),
            },
            OpenAiPresetBaseUrlCase {
                name: "DeepSeek",
                base_url: "https://api.deepseek.com/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "MiniMax",
                base_url: "https://api.minimaxi.com/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "Novita",
                base_url: "https://api.novita.ai/openai/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "OpenRouter",
                base_url: "https://openrouter.ai/api/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "Dashscope",
                base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "Dashscope Coding Plan",
                base_url: "https://coding.dashscope.aliyuncs.com/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "SiliconFlow-CN",
                base_url: "https://api.siliconflow.cn/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "SiliconFlow",
                base_url: "https://api.siliconflow.com/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "Zhipu",
                base_url: "https://open.bigmodel.cn/api/paas/v4",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "Moonshot (China)",
                base_url: "https://api.moonshot.cn/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "Moonshot (Global)",
                base_url: "https://api.moonshot.ai/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "xAI",
                base_url: "https://api.x.ai/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "Ark",
                base_url: "https://ark.cn-beijing.volces.com/api/v3",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "Qianfan",
                base_url: "https://qianfan.baidubce.com/v2",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "Hunyuan",
                base_url: "https://api.hunyuan.cloud.tencent.com/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "Lingyi",
                base_url: "https://api.lingyiwanwu.com/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "Poe",
                base_url: "https://api.poe.com/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "PPIO",
                base_url: "https://api.ppinfra.com/v3/openai",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "ModelScope",
                base_url: "https://api-inference.modelscope.cn/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "InfiniAI",
                base_url: "https://cloud.infini-ai.com/maas/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "Ctyun",
                base_url: "https://wishub-x1.ctyun.cn/v1",
                expected_max_tokens_field: None,
            },
            OpenAiPresetBaseUrlCase {
                name: "StepFun",
                base_url: "https://api.stepfun.com/v1",
                expected_max_tokens_field: None,
            },
        ];

        for case in cases {
            let provider = map_aionrs_provider("custom", "m", None).expect(case.name);
            assert_eq!(provider, "openai", "{}", case.name);

            let (base_url, compat) = resolve_aionrs_url_and_compat("custom", case.base_url, &provider, "m", false);
            assert_eq!(base_url.as_deref(), Some(case.base_url), "{}", case.name);
            assert_eq!(compat.api_path.as_deref(), Some("/chat/completions"), "{}", case.name);
            assert_eq!(
                compat.max_tokens_field.as_deref(),
                case.expected_max_tokens_field,
                "{}",
                case.name
            );
            assert_eq!(
                intended_aionrs_final_url(&provider, case.base_url, compat.api_path.as_deref()),
                format!("{}/chat/completions", case.base_url),
                "{}",
                case.name
            );
        }
    }

    struct SpecialModelPlatformCase<'a> {
        name: &'a str,
        platform: &'a str,
        base_url: &'a str,
        expected_provider: &'a str,
        expected_base_url: Option<&'a str>,
        expected_api_path: Option<&'a str>,
        expected_final_url: Option<&'a str>,
    }

    #[test]
    fn ui_model_platform_special_presets_resolve_default_transports() {
        let cases = [
            SpecialModelPlatformCase {
                name: "Custom",
                platform: "custom",
                base_url: "",
                expected_provider: "openai",
                expected_base_url: None,
                expected_api_path: None,
                expected_final_url: None,
            },
            SpecialModelPlatformCase {
                name: "New API",
                platform: "new-api",
                base_url: "",
                expected_provider: "openai",
                expected_base_url: None,
                expected_api_path: None,
                expected_final_url: None,
            },
            SpecialModelPlatformCase {
                name: "Gemini",
                platform: "gemini",
                base_url: "https://generativelanguage.googleapis.com",
                expected_provider: "openai",
                expected_base_url: Some("https://generativelanguage.googleapis.com/v1beta/openai"),
                expected_api_path: Some("/chat/completions"),
                expected_final_url: Some("https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"),
            },
            SpecialModelPlatformCase {
                name: "Gemini (Vertex AI)",
                platform: "gemini-vertex-ai",
                base_url: "",
                expected_provider: "vertex",
                expected_base_url: None,
                expected_api_path: None,
                expected_final_url: None,
            },
            SpecialModelPlatformCase {
                name: "Anthropic",
                platform: "anthropic",
                base_url: "https://api.anthropic.com",
                expected_provider: "anthropic",
                expected_base_url: Some("https://api.anthropic.com"),
                expected_api_path: None,
                expected_final_url: Some("https://api.anthropic.com/v1/messages"),
            },
            SpecialModelPlatformCase {
                name: "AWS Bedrock",
                platform: "bedrock",
                base_url: "",
                expected_provider: "bedrock",
                expected_base_url: None,
                expected_api_path: None,
                expected_final_url: None,
            },
        ];

        for case in cases {
            let provider = map_aionrs_provider(case.platform, "m", None).expect(case.name);
            assert_eq!(provider, case.expected_provider, "{}", case.name);

            let (base_url, compat) = resolve_aionrs_url_and_compat(case.platform, case.base_url, &provider, "m", false);
            assert_eq!(base_url.as_deref(), case.expected_base_url, "{}", case.name);
            assert_eq!(compat.api_path.as_deref(), case.expected_api_path, "{}", case.name);

            if let (Some(base_url), Some(expected_final_url)) = (base_url.as_deref(), case.expected_final_url) {
                assert_eq!(
                    intended_aionrs_final_url(&provider, base_url, compat.api_path.as_deref()),
                    expected_final_url,
                    "{}",
                    case.name
                );
            }
        }
    }

    struct FinalUrlCase<'a> {
        name: &'a str,
        provider: &'a str,
        base_url: &'a str,
        api_path: Option<&'a str>,
        expected_final_url: &'a str,
    }

    fn intended_aionrs_final_url(provider: &str, base_url: &str, api_path: Option<&str>) -> String {
        let default_path = match provider {
            "anthropic" => "/v1/messages",
            _ => "/v1/chat/completions",
        };
        format!("{}{}", base_url, api_path.unwrap_or(default_path))
    }

    #[test]
    fn resolved_aionrs_final_url_semantics_table_driven_cases() {
        let cases = [
            FinalUrlCase {
                name: "deepseek openai-compatible chat completions",
                provider: "openai",
                base_url: "https://api.deepseek.com/v1",
                api_path: Some("/chat/completions"),
                expected_final_url: "https://api.deepseek.com/v1/chat/completions",
            },
            FinalUrlCase {
                name: "glm openai-compatible chat completions",
                provider: "openai",
                base_url: "https://open.bigmodel.cn/api/paas/v4",
                api_path: Some("/chat/completions"),
                expected_final_url: "https://open.bigmodel.cn/api/paas/v4/chat/completions",
            },
            FinalUrlCase {
                name: "openai full url mode appends nothing",
                provider: "openai",
                base_url: "https://x/v1/chat/completions",
                api_path: Some(""),
                expected_final_url: "https://x/v1/chat/completions",
            },
            FinalUrlCase {
                name: "official anthropic uses default versioned messages path",
                provider: "anthropic",
                base_url: "https://api.anthropic.com",
                api_path: None,
                expected_final_url: "https://api.anthropic.com/v1/messages",
            },
            FinalUrlCase {
                name: "custom anthropic root uses default versioned messages path",
                provider: "anthropic",
                base_url: "https://proxy.example.com",
                api_path: None,
                expected_final_url: "https://proxy.example.com/v1/messages",
            },
            FinalUrlCase {
                name: "anthropic full url mode appends nothing",
                provider: "anthropic",
                base_url: "https://x/v1/messages",
                api_path: Some(""),
                expected_final_url: "https://x/v1/messages",
            },
        ];

        for case in cases {
            assert_eq!(
                intended_aionrs_final_url(case.provider, case.base_url, case.api_path),
                case.expected_final_url,
                "{}",
                case.name
            );
        }
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
            backend: Some("aionrs".into()),
            ..Default::default()
        };

        let result = resolve_mcp_servers(&overrides);
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
    fn resolve_mcp_servers_without_team_returns_empty_map() {
        let overrides = AionrsBuildExtra {
            backend: Some("aionrs".into()),
            ..Default::default()
        };

        let result = resolve_mcp_servers(&overrides);
        assert!(result.is_empty());
    }

    #[test]
    fn resolve_mcp_servers_empty_when_no_config() {
        let overrides = AionrsBuildExtra::default();
        let result = resolve_mcp_servers(&overrides);
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
