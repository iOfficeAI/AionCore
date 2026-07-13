//! `aioncore diagnose` subcommand: agent-facing read-only troubleshooting CLI.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Map, Value, json};

use crate::cli::{
    DiagnoseArgs, DiagnoseCommand, DiagnoseConversationsArgs, DiagnoseConversationsCommand, DiagnoseCronArgs,
    DiagnoseHttpArgs, DiagnoseHttpCommand, DiagnoseLogsArgs, DiagnoseLogsCommand, DiagnoseMcpArgs,
    DiagnoseProvidersArgs, DiagnoseSummaryCommand, DiagnoseTeamsArgs,
};
use crate::commands::diagnose_capabilities;

const ENV_BASE_URL: &str = "AIONUI_BASE_URL";
const ENV_CONVERSATION_ID: &str = "AIONUI_CONVERSATION_ID";
const ENV_USER_ID: &str = "AIONUI_USER_ID";
const ENV_LOG_DIR: &str = "AIONUI_LOG_DIR";
const MAX_HTTP_OUTPUT_BYTES: usize = 200_000;
const MAX_LOG_LINES: usize = 1000;
const MAX_LOG_SEARCH_DEPTH: usize = 6;

pub async fn run_diagnose(args: DiagnoseArgs) -> ExitCode {
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", error.stderr_line());
            error.exit_code()
        }
    }
}

async fn run(args: DiagnoseArgs) -> Result<(), DiagnoseError> {
    let client = reqwest::Client::new();
    match args.command {
        DiagnoseCommand::Capabilities => print_envelope(
            diagnose_capabilities::data(),
            meta_from_map(Map::new()),
            "diagnose capabilities",
        ),
        DiagnoseCommand::Context => run_context(),
        DiagnoseCommand::Health => run_health(&client).await,
        DiagnoseCommand::Overview => run_overview(&client).await,
        DiagnoseCommand::Conversations(args) => run_conversations(&client, args).await,
        DiagnoseCommand::Providers(args) => run_providers(&client, args).await,
        DiagnoseCommand::Mcp(args) => run_mcp(&client, args).await,
        DiagnoseCommand::Cron(args) => run_cron(&client, args).await,
        DiagnoseCommand::Teams(args) => run_teams(&client, args).await,
        DiagnoseCommand::Logs(args) => run_logs(args),
        DiagnoseCommand::Http(args) => run_http(&client, args).await,
    }
}

fn run_context() -> Result<(), DiagnoseError> {
    let command = "diagnose context";
    let env = DiagnoseEnv::from_env(command)?;
    print_envelope(
        json!({
            "user_id": env.user_id,
            "conversation_id": env.conversation_id,
            "base_url": env.base_url,
            "log_dir": std::env::var(ENV_LOG_DIR).ok().filter(|value| !value.trim().is_empty()),
        }),
        meta_from_map(Map::new()),
        command,
    )
}

async fn run_health(client: &reqwest::Client) -> Result<(), DiagnoseError> {
    let command = "diagnose health";
    let env = DiagnoseEnv::from_env(command)?;
    let data = request_json(client, &env, "/health", command).await?;
    print_envelope(redact_value(data), meta_from_map(Map::new()), command)
}

async fn run_overview(client: &reqwest::Client) -> Result<(), DiagnoseError> {
    let command = "diagnose overview";
    let env = DiagnoseEnv::from_env(command)?;
    let health = request_json(client, &env, "/health", command).await?;
    let providers = request_json(client, &env, "/api/providers", command).await?;
    let mcp = request_json(client, &env, "/api/mcp/servers", command).await?;
    let cron = request_json(client, &env, "/api/cron/jobs", command).await?;
    let conversations = request_json(client, &env, "/api/conversations?limit=50", command).await?;

    print_envelope(
        redact_value(json!({
            "health": health,
            "providers": provider_overview(&providers),
            "mcp": mcp_overview(&mcp),
            "cron": cron_overview(&cron),
            "running_conversations": running_conversations(&conversations),
        })),
        meta_from_map(Map::new()),
        command,
    )
}

async fn run_conversations(client: &reqwest::Client, args: DiagnoseConversationsArgs) -> Result<(), DiagnoseError> {
    match args.command {
        DiagnoseConversationsCommand::List => {
            let command = "diagnose conversations list";
            let env = DiagnoseEnv::from_env(command)?;
            let payload = read_optional_stdin_payload(command)?;
            let limit = optional_usize_field(&payload, "limit", 50, 200, command)?;
            let path = format!("/api/conversations?limit={limit}");
            let data = request_json(client, &env, &path, command).await?;
            print_envelope(redact_value(data), meta_from_map(Map::new()), command)
        }
        DiagnoseConversationsCommand::Get => run_conversation_get(client).await,
        DiagnoseConversationsCommand::Messages => run_conversation_messages(client).await,
    }
}

async fn run_conversation_get(client: &reqwest::Client) -> Result<(), DiagnoseError> {
    let command = "diagnose conversations get";
    let env = DiagnoseEnv::from_env(command)?;
    let mut payload = read_stdin_payload(command)?;
    let mut selectors = SelectorMeta::default();
    resolve_conversation_selector(&env, command, &mut payload, &mut selectors)?;
    let conversation_id = required_string_field(&payload, "conversation_id", command)?;
    let path = format!("/api/conversations/{}", encode_path_segment(&conversation_id));
    let data = request_json(client, &env, &path, command).await?;
    print_envelope(
        redact_value(conversation_with_hints(data)),
        meta_from_map(selectors.into_map()),
        command,
    )
}

async fn run_conversation_messages(client: &reqwest::Client) -> Result<(), DiagnoseError> {
    let command = "diagnose conversations messages";
    let env = DiagnoseEnv::from_env(command)?;
    let mut payload = read_stdin_payload(command)?;
    let mut selectors = SelectorMeta::default();
    resolve_conversation_selector(&env, command, &mut payload, &mut selectors)?;
    let conversation_id = required_string_field(&payload, "conversation_id", command)?;
    let limit = optional_usize_field(&payload, "limit", 30, 200, command)?;
    let errors_only = optional_bool_field(&payload, "errors_only", false);
    let path = format!(
        "/api/conversations/{}/messages?limit={limit}&content_mode=compact",
        encode_path_segment(&conversation_id)
    );
    let mut data = request_json(client, &env, &path, command).await?;
    if errors_only {
        data = filter_error_messages(data);
    }
    print_envelope(redact_value(data), meta_from_map(selectors.into_map()), command)
}

async fn run_providers(client: &reqwest::Client, args: DiagnoseProvidersArgs) -> Result<(), DiagnoseError> {
    match args.command {
        DiagnoseSummaryCommand::Summary => {
            let command = "diagnose providers summary";
            let env = DiagnoseEnv::from_env(command)?;
            let data = request_json(client, &env, "/api/providers", command).await?;
            print_envelope(
                redact_value(provider_summary(&data)),
                meta_from_map(Map::new()),
                command,
            )
        }
    }
}

async fn run_mcp(client: &reqwest::Client, args: DiagnoseMcpArgs) -> Result<(), DiagnoseError> {
    match args.command {
        DiagnoseSummaryCommand::Summary => {
            let command = "diagnose mcp summary";
            let env = DiagnoseEnv::from_env(command)?;
            let data = request_json(client, &env, "/api/mcp/servers", command).await?;
            print_envelope(redact_value(mcp_summary(&data)), meta_from_map(Map::new()), command)
        }
    }
}

async fn run_cron(client: &reqwest::Client, args: DiagnoseCronArgs) -> Result<(), DiagnoseError> {
    match args.command {
        DiagnoseSummaryCommand::Summary => {
            let command = "diagnose cron summary";
            let env = DiagnoseEnv::from_env(command)?;
            let data = request_json(client, &env, "/api/cron/jobs", command).await?;
            print_envelope(redact_value(cron_summary(&data)), meta_from_map(Map::new()), command)
        }
    }
}

async fn run_teams(client: &reqwest::Client, args: DiagnoseTeamsArgs) -> Result<(), DiagnoseError> {
    match args.command {
        DiagnoseSummaryCommand::Summary => {
            let command = "diagnose teams summary";
            let env = DiagnoseEnv::from_env(command)?;
            let data = request_json(client, &env, "/api/teams", command).await?;
            let summary = teams_summary(client, &env, &data, command).await;
            print_envelope(redact_value(summary), meta_from_map(Map::new()), command)
        }
    }
}

fn run_logs(args: DiagnoseLogsArgs) -> Result<(), DiagnoseError> {
    match args.command {
        DiagnoseLogsCommand::Tail => run_logs_tail(),
    }
}

fn run_logs_tail() -> Result<(), DiagnoseError> {
    let command = "diagnose logs tail";
    let mut payload = read_optional_stdin_payload(command)?;
    let mut selectors = SelectorMeta::default();
    resolve_optional_log_conversation_selector(command, &mut payload, &mut selectors)?;
    let log_dir = optional_string_field(&payload, "log_dir")
        .or_else(|| std::env::var(ENV_LOG_DIR).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            DiagnoseError::new(
                DiagnoseErrorCode::EnvMissing,
                command,
                "missing log directory; set AIONUI_LOG_DIR or pass log_dir in stdin JSON",
            )
            .field("field", ENV_LOG_DIR)
        })?;
    let lines = optional_usize_field(&payload, "lines", 80, MAX_LOG_LINES, command)?;
    let errors_only = optional_bool_field(&payload, "errors_only", false);
    let conversation_id = optional_string_field(&payload, "conversation_id");
    let result = tail_latest_log(
        Path::new(&log_dir),
        lines,
        errors_only,
        conversation_id.as_deref(),
        command,
    )?;
    print_envelope(redact_value(result), meta_from_map(selectors.into_map()), command)
}

async fn run_http(client: &reqwest::Client, args: DiagnoseHttpArgs) -> Result<(), DiagnoseError> {
    match args.command {
        DiagnoseHttpCommand::Get => {
            let command = "diagnose http get";
            let env = DiagnoseEnv::from_env(command)?;
            let payload = read_stdin_payload(command)?;
            let path = required_string_field(&payload, "path", command)?;
            let path = validate_http_path(&path, command)?;
            let data = request_json(client, &env, &path, command).await?;
            let mut meta = Map::new();
            meta.insert("path".into(), Value::String(path));
            meta.insert("escape_hatch".into(), Value::Bool(true));
            if let Some(reason) = optional_string_field(&payload, "reason") {
                meta.insert("reason".into(), Value::String(reason));
            }
            print_envelope(truncate_large_json(redact_value(data)), meta_from_map(meta), command)
        }
    }
}

#[derive(Debug, Clone)]
struct DiagnoseEnv {
    base_url: String,
    conversation_id: String,
    user_id: String,
}

impl DiagnoseEnv {
    fn from_env(command: &str) -> Result<Self, DiagnoseError> {
        Ok(Self {
            base_url: required_env(command, ENV_BASE_URL)?.trim_end_matches('/').to_owned(),
            conversation_id: required_env(command, ENV_CONVERSATION_ID)?,
            user_id: required_env(command, ENV_USER_ID)?,
        })
    }
}

fn required_env(command: &str, name: &'static str) -> Result<String, DiagnoseError> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            DiagnoseError::new(
                DiagnoseErrorCode::EnvMissing,
                command,
                "missing required environment variable",
            )
            .field("field", name)
        })
}

async fn request_json(
    client: &reqwest::Client,
    env: &DiagnoseEnv,
    path: &str,
    command: &str,
) -> Result<Value, DiagnoseError> {
    let url = format!("{}{}", env.base_url, path);
    let response = client
        .get(&url)
        .header("content-type", "application/json")
        .header("x-aionui-conversation-id", &env.conversation_id)
        .header("x-aionui-user-id", &env.user_id)
        .send()
        .await
        .map_err(|_| {
            DiagnoseError::new(
                DiagnoseErrorCode::HttpRequestFailed,
                command,
                "failed to call AionUi backend",
            )
            .field("path", path)
        })?;

    let status = response.status();
    let text = response.text().await.map_err(|_| {
        DiagnoseError::new(
            DiagnoseErrorCode::ResponseReadFailed,
            command,
            "failed to read AionUi backend response",
        )
        .field("path", path)
    })?;

    if !status.is_success() {
        return Err(DiagnoseError::new(
            DiagnoseErrorCode::HttpStatusError,
            command,
            "AionUi backend returned an error status",
        )
        .field("path", path)
        .field("status", status.as_u16().to_string()));
    }

    if text.trim().is_empty() {
        return Ok(Value::Null);
    }

    let value: Value = serde_json::from_str(&text).map_err(|_| {
        DiagnoseError::new(
            DiagnoseErrorCode::ResponseJsonInvalid,
            command,
            "AionUi backend returned invalid JSON",
        )
        .field("path", path)
    })?;
    extract_api_data(value, command)
}

fn extract_api_data(value: Value, command: &str) -> Result<Value, DiagnoseError> {
    let Some(success) = value.get("success").and_then(Value::as_bool) else {
        return Ok(value);
    };

    if success {
        return Ok(value.get("data").cloned().unwrap_or(Value::Null));
    }

    Err(DiagnoseError::new(
        DiagnoseErrorCode::HttpStatusError,
        command,
        "AionUi backend returned an unsuccessful response",
    ))
}

fn read_stdin_payload(command: &str) -> Result<Value, DiagnoseError> {
    let payload = read_optional_stdin_payload(command)?;
    if payload.as_object().is_some_and(Map::is_empty) {
        return Err(DiagnoseError::new(
            DiagnoseErrorCode::PayloadMissing,
            command,
            "JSON payload is required on stdin",
        )
        .field("field", "stdin"));
    }
    Ok(payload)
}

fn read_optional_stdin_payload(command: &str) -> Result<Value, DiagnoseError> {
    let mut raw = String::new();
    io::stdin().read_to_string(&mut raw).map_err(|_| {
        DiagnoseError::new(
            DiagnoseErrorCode::PayloadInvalid,
            command,
            "failed to read JSON payload from stdin",
        )
        .field("field", "stdin")
    })?;
    if raw.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let value: Value = serde_json::from_str(&raw).map_err(|_| {
        DiagnoseError::new(
            DiagnoseErrorCode::PayloadInvalid,
            command,
            "invalid JSON payload on stdin",
        )
        .field("field", "stdin")
    })?;
    if !value.is_object() {
        return Err(DiagnoseError::new(
            DiagnoseErrorCode::PayloadInvalid,
            command,
            "JSON payload must be an object",
        )
        .field("field", "stdin"));
    }
    Ok(value)
}

fn print_envelope(data: Value, meta: Value, command: &str) -> Result<(), DiagnoseError> {
    let rendered = serde_json::to_string_pretty(&json!({
        "success": true,
        "data": data,
        "meta": meta,
    }))
    .map_err(|_| {
        DiagnoseError::new(
            DiagnoseErrorCode::StdoutWriteFailed,
            command,
            "failed to serialize JSON output",
        )
    })?;
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(rendered.as_bytes())
        .and_then(|_| stdout.write_all(b"\n"))
        .map_err(|_| {
            DiagnoseError::new(
                DiagnoseErrorCode::StdoutWriteFailed,
                command,
                "failed to write JSON output",
            )
        })?;
    Ok(())
}

fn meta_from_map(extra: Map<String, Value>) -> Value {
    let mut map = Map::new();
    map.insert("schema_version".into(), Value::Number(1.into()));
    for (key, value) in extra {
        map.insert(key, value);
    }
    Value::Object(map)
}

#[derive(Default)]
struct SelectorMeta {
    resolved: Map<String, Value>,
}

impl SelectorMeta {
    fn insert(&mut self, field: &'static str, value: impl Into<String>) {
        self.resolved.insert(field.into(), Value::String(value.into()));
    }

    fn into_map(self) -> Map<String, Value> {
        let mut map = Map::new();
        if !self.resolved.is_empty() {
            map.insert("resolved_selectors".into(), Value::Object(self.resolved));
        }
        map
    }
}

fn resolve_conversation_selector(
    env: &DiagnoseEnv,
    command: &str,
    payload: &mut Value,
    selectors: &mut SelectorMeta,
) -> Result<(), DiagnoseError> {
    let object = payload.as_object_mut().ok_or_else(|| {
        DiagnoseError::new(
            DiagnoseErrorCode::PayloadInvalid,
            command,
            "JSON payload must be an object",
        )
        .field("field", "stdin")
    })?;
    if object
        .get("conversation_id")
        .and_then(Value::as_str)
        .is_some_and(|value| value == "current")
    {
        object.insert("conversation_id".into(), Value::String(env.conversation_id.clone()));
        selectors.insert("conversation_id", env.conversation_id.clone());
    }
    Ok(())
}

fn resolve_optional_log_conversation_selector(
    command: &str,
    payload: &mut Value,
    selectors: &mut SelectorMeta,
) -> Result<(), DiagnoseError> {
    if payload
        .get("conversation_id")
        .and_then(Value::as_str)
        .is_some_and(|value| value == "current")
    {
        let conversation_id = required_env(command, ENV_CONVERSATION_ID)?;
        payload
            .as_object_mut()
            .expect("payload object checked by reader")
            .insert("conversation_id".into(), Value::String(conversation_id.clone()));
        selectors.insert("conversation_id", conversation_id);
    }
    Ok(())
}

fn required_string_field(payload: &Value, field: &'static str, command: &str) -> Result<String, DiagnoseError> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            DiagnoseError::new(DiagnoseErrorCode::PayloadInvalid, command, "missing required field")
                .field("field", field)
        })
}

fn optional_string_field(payload: &Value, field: &'static str) -> Option<String> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn optional_bool_field(payload: &Value, field: &'static str, default: bool) -> bool {
    payload.get(field).and_then(Value::as_bool).unwrap_or(default)
}

fn optional_usize_field(
    payload: &Value,
    field: &'static str,
    default: usize,
    max: usize,
    command: &str,
) -> Result<usize, DiagnoseError> {
    let Some(value) = payload.get(field) else {
        return Ok(default);
    };
    let parsed = value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            DiagnoseError::new(
                DiagnoseErrorCode::PayloadInvalid,
                command,
                "field must be a positive integer",
            )
            .field("field", field)
        })?;
    Ok(parsed.min(max))
}

fn validate_http_path(path: &str, command: &str) -> Result<String, DiagnoseError> {
    let path = path.trim();
    let allowed = path == "/health" || path.starts_with("/health?") || path.starts_with("/api/");
    if path.starts_with("http://")
        || path.starts_with("https://")
        || path.starts_with("//")
        || path.contains('\n')
        || path.contains('\r')
        || !allowed
    {
        return Err(DiagnoseError::new(
            DiagnoseErrorCode::PayloadInvalid,
            command,
            "path must start with /health or /api/",
        )
        .field("field", "path"));
    }
    Ok(path.to_owned())
}

fn encode_path_segment(input: &str) -> String {
    percent_encode(input)
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn conversation_with_hints(mut conversation: Value) -> Value {
    if let Value::Object(object) = &mut conversation {
        let hint = stuck_hint(object.get("runtime"));
        object.insert("stuck_hint".into(), hint.map(Value::String).unwrap_or(Value::Null));
    }
    conversation
}

fn stuck_hint(runtime: Option<&Value>) -> Option<String> {
    let runtime = runtime?;
    let state = runtime.get("state").and_then(Value::as_str);
    let is_processing = runtime.get("is_processing").and_then(Value::as_bool).unwrap_or(false);
    let pending_confirmations = runtime
        .get("pending_confirmations")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if state == Some("running") && is_processing {
        return Some(
            "state=running and is_processing=true; compare repeated checks, turn_id, messages, and logs before calling it stuck"
                .to_owned(),
        );
    }
    if state == Some("waiting_confirmation") || pending_confirmations > 0 {
        return Some(
            "state=waiting_confirmation or pending_confirmations>0; this is blocked on user approval, not hung"
                .to_owned(),
        );
    }
    None
}

fn filter_error_messages(mut data: Value) -> Value {
    if let Some(items) = data.get_mut("items").and_then(Value::as_array_mut) {
        items.retain(|item| item.get("status").and_then(Value::as_str) == Some("error"));
    } else if let Value::Array(items) = &mut data {
        items.retain(|item| item.get("status").and_then(Value::as_str) == Some("error"));
    }
    data
}

fn provider_summary(data: &Value) -> Value {
    let providers = value_items(data);
    Value::Array(
        providers
            .iter()
            .map(|provider| {
                json!({
                    "id": provider.get("id").cloned().unwrap_or(Value::Null),
                    "name": provider.get("name").cloned().unwrap_or(Value::Null),
                    "platform": provider.get("platform").cloned().unwrap_or(Value::Null),
                    "enabled": provider.get("enabled").cloned().unwrap_or(Value::Null),
                    "base_url": provider.get("base_url").cloned().unwrap_or(Value::Null),
                    "models": provider.get("models").cloned().unwrap_or(Value::Null),
                    "model_health": provider.get("model_health").cloned().unwrap_or(Value::Null),
                    "unhealthy_models": unhealthy_model_map(provider),
                })
            })
            .collect(),
    )
}

fn provider_overview(data: &Value) -> Value {
    let providers = value_items(data);
    let mut unhealthy = Vec::new();
    for provider in providers {
        let provider_name = provider.get("name").cloned().unwrap_or(Value::Null);
        if let Some(model_health) = provider.get("model_health").and_then(Value::as_object) {
            for (model, health) in model_health {
                if health
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status != "healthy")
                {
                    unhealthy.push(json!({
                        "provider": provider_name,
                        "model": model,
                        "status": health.get("status").cloned().unwrap_or(Value::Null),
                        "error": health.get("error").cloned().unwrap_or(Value::Null),
                    }));
                }
            }
        }
    }
    json!({
        "count": value_items(data).len(),
        "unhealthy": unhealthy,
    })
}

fn unhealthy_model_map(provider: &Value) -> Value {
    let Some(model_health) = provider.get("model_health").and_then(Value::as_object) else {
        return Value::Null;
    };
    let mut out = Map::new();
    for (model, health) in model_health {
        if health
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status != "healthy")
        {
            out.insert(model.clone(), health.clone());
        }
    }
    if out.is_empty() {
        Value::Null
    } else {
        Value::Object(out)
    }
}

fn mcp_summary(data: &Value) -> Value {
    Value::Array(
        value_items(data)
            .iter()
            .map(|server| {
                let tool_count = tool_count(server);
                let enabled = server.get("enabled").and_then(Value::as_bool).unwrap_or(false);
                json!({
                    "id": server.get("id").cloned().unwrap_or(Value::Null),
                    "name": server.get("name").cloned().unwrap_or(Value::Null),
                    "enabled": server.get("enabled").cloned().unwrap_or(Value::Null),
                    "builtin": server.get("builtin").cloned().unwrap_or(Value::Null),
                    "transport": server.get("transport").and_then(|value| value.get("type")).cloned().unwrap_or(Value::Null),
                    "tool_count": tool_count,
                    "warning": if enabled && tool_count == 0 {
                        Value::String("enabled but exposes 0 tools; check server startup and logs".to_owned())
                    } else {
                        Value::Null
                    },
                })
            })
            .collect(),
    )
}

fn mcp_overview(data: &Value) -> Value {
    let servers = value_items(data);
    let enabled_but_no_tools: Vec<Value> = servers
        .iter()
        .filter(|server| server.get("enabled").and_then(Value::as_bool).unwrap_or(false) && tool_count(server) == 0)
        .map(|server| {
            json!({
                "id": server.get("id").cloned().unwrap_or(Value::Null),
                "name": server.get("name").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    json!({
        "count": servers.len(),
        "enabled_but_no_tools": enabled_but_no_tools,
    })
}

fn tool_count(server: &Value) -> usize {
    match server.get("tools") {
        Some(Value::Array(tools)) => tools.len(),
        Some(Value::Number(number)) => number
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0),
        _ => 0,
    }
}

fn cron_summary(data: &Value) -> Value {
    let jobs = value_items(data);
    let failing: Vec<Value> = jobs
        .iter()
        .filter(|job| {
            matches!(
                job.get("last_status").and_then(Value::as_str),
                Some("error") | Some("missed")
            )
        })
        .cloned()
        .collect();
    json!({
        "total": jobs.len(),
        "failing": failing,
        "all": jobs,
    })
}

fn cron_overview(data: &Value) -> Value {
    let jobs = value_items(data);
    let failing: Vec<Value> = jobs
        .iter()
        .filter(|job| {
            matches!(
                job.get("last_status").and_then(Value::as_str),
                Some("error") | Some("missed")
            )
        })
        .map(|job| {
            json!({
                "id": job.get("id").cloned().unwrap_or(Value::Null),
                "name": job.get("name").cloned().unwrap_or(Value::Null),
                "last_status": job.get("last_status").cloned().unwrap_or(Value::Null),
                "last_error": job.get("last_error").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    json!({
        "total": jobs.len(),
        "failing": failing,
    })
}

async fn teams_summary(client: &reqwest::Client, env: &DiagnoseEnv, data: &Value, command: &str) -> Value {
    let mut teams = Vec::new();
    for team in value_items(data) {
        let mut members = Vec::new();
        let source_members = team
            .get("agents")
            .or_else(|| team.get("assistants"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for member in source_members {
            let conversation_id = member.get("conversation_id").and_then(Value::as_str);
            let runtime_state = if let Some(conversation_id) = conversation_id {
                let path = format!("/api/conversations/{}", encode_path_segment(conversation_id));
                match request_json(client, env, &path, command).await {
                    Ok(conversation) => conversation
                        .get("runtime")
                        .and_then(|runtime| runtime.get("state"))
                        .cloned()
                        .unwrap_or(Value::Null),
                    Err(_) => Value::Null,
                }
            } else {
                Value::Null
            };
            members.push(json!({
                "name": member.get("name").cloned().unwrap_or(Value::Null),
                "role": member.get("role").cloned().unwrap_or(Value::Null),
                "backend": member.get("backend").cloned().unwrap_or(Value::Null),
                "conversation_id": member.get("conversation_id").cloned().unwrap_or(Value::Null),
                "conv_state": runtime_state,
            }));
        }
        teams.push(json!({
            "id": team.get("id").cloned().unwrap_or(Value::Null),
            "name": team.get("name").cloned().unwrap_or(Value::Null),
            "members": members,
        }));
    }
    Value::Array(teams)
}

fn running_conversations(data: &Value) -> Value {
    Value::Array(
        value_items(data)
            .iter()
            .filter(|conversation| {
                conversation.get("status").and_then(Value::as_str) == Some("running")
                    || conversation
                        .get("runtime")
                        .and_then(|runtime| runtime.get("state"))
                        .and_then(Value::as_str)
                        == Some("running")
            })
            .map(|conversation| {
                json!({
                    "id": conversation.get("id").cloned().unwrap_or(Value::Null),
                    "name": conversation.get("name").cloned().unwrap_or(Value::Null),
                    "type": conversation.get("type").cloned().unwrap_or(Value::Null),
                    "status": conversation.get("status").cloned().unwrap_or(Value::Null),
                    "runtime": conversation.get("runtime").cloned().unwrap_or(Value::Null),
                })
            })
            .collect(),
    )
}

fn value_items(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        Value::Object(object) => object
            .get("items")
            .or_else(|| object.get("conversations"))
            .or_else(|| object.get("data"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn tail_latest_log(
    log_dir: &Path,
    lines: usize,
    errors_only: bool,
    conversation_id: Option<&str>,
    command: &str,
) -> Result<Value, DiagnoseError> {
    let mut files = Vec::new();
    collect_aioncore_logs(log_dir, 0, &mut files).map_err(|_| {
        DiagnoseError::new(
            DiagnoseErrorCode::LogReadFailed,
            command,
            "failed to read log directory",
        )
        .field("path", log_dir.display().to_string())
    })?;
    let latest = files
        .into_iter()
        .filter_map(|path| {
            let modified = path.metadata().and_then(|metadata| metadata.modified()).ok()?;
            Some((path, modified))
        })
        .max_by_key(|(_, modified)| *modified)
        .map(|(path, _)| path)
        .ok_or_else(|| {
            DiagnoseError::new(DiagnoseErrorCode::LogNotFound, command, "no *.aioncore.log files found")
                .field("path", log_dir.display().to_string())
        })?;
    let raw = std::fs::read_to_string(&latest).map_err(|_| {
        DiagnoseError::new(DiagnoseErrorCode::LogReadFailed, command, "failed to read log file")
            .field("path", latest.display().to_string())
    })?;
    let mut selected = Vec::new();
    for line in raw.lines().rev() {
        if let Some(conversation_id) = conversation_id
            && !line.contains(conversation_id)
        {
            continue;
        }
        if errors_only && !is_error_log_line(line) {
            continue;
        }
        selected.push(Value::String(redact_text(line)));
        if selected.len() >= lines {
            break;
        }
    }
    selected.reverse();
    Ok(json!({
        "log_dir": log_dir.display().to_string(),
        "file": latest.display().to_string(),
        "filters": {
            "errors_only": errors_only,
            "conversation_id": conversation_id,
        },
        "lines": selected,
    }))
}

fn collect_aioncore_logs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> io::Result<()> {
    if depth > MAX_LOG_SEARCH_DEPTH || !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_aioncore_logs(&path, depth + 1, out)?;
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".aioncore.log"))
        {
            out.push(path);
        }
    }
    Ok(())
}

fn is_error_log_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("\"error\"")
        || lower.contains("\"warn\"")
        || lower.contains("error")
        || lower.contains("panic")
        || lower.contains("warn")
}

fn redact_value(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let redacted = object
                .into_iter()
                .map(|(key, value)| {
                    let value = if should_redact_key(&key) {
                        redacted_secret_summary(value)
                    } else {
                        redact_value(value)
                    };
                    (key, value)
                })
                .collect();
            Value::Object(redacted)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(redact_value).collect()),
        Value::String(text) => Value::String(redact_text(&text)),
        other => other,
    }
}

fn should_redact_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "api_key"
            | "apikey"
            | "access_key"
            | "secret_key"
            | "aws_access_key_id"
            | "aws_secret_access_key"
            | "authorization"
            | "headers"
            | "env"
    ) || key.contains("secret")
        || key.contains("token")
        || key.contains("password")
}

fn redacted_secret_summary(value: Value) -> Value {
    match value {
        Value::String(text) => json!({
            "redacted": true,
            "chars": text.chars().count(),
        }),
        Value::Object(object) => json!({
            "redacted": true,
            "keys": object.len(),
        }),
        Value::Array(items) => json!({
            "redacted": true,
            "items": items.len(),
        }),
        _ => json!({
            "redacted": true,
        }),
    }
}

fn redact_text(text: &str) -> String {
    if let Ok(parsed @ (Value::Object(_) | Value::Array(_))) = serde_json::from_str::<Value>(text)
        && let Ok(redacted) = serde_json::to_string(&redact_value(parsed))
    {
        return redacted;
    }
    let text = redact_prefixed_secret(text, "sk-", "sk-REDACTED");
    let text = redact_prefixed_secret(&text, "Bearer ", "Bearer REDACTED");
    redact_prefixed_secret(&text, "bearer ", "bearer REDACTED")
}

fn redact_prefixed_secret(text: &str, prefix: &str, replacement: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut offset = 0;
    while let Some(relative_start) = text[offset..].find(prefix) {
        let start = offset + relative_start;
        output.push_str(&text[offset..start]);
        output.push_str(replacement);
        let secret_start = start + prefix.len();
        let secret_end = text[secret_start..]
            .char_indices()
            .find_map(|(index, ch)| is_secret_delimiter(ch).then_some(secret_start + index))
            .unwrap_or(text.len());
        offset = secret_end;
    }
    output.push_str(&text[offset..]);
    output
}

fn is_secret_delimiter(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '"' | '\'' | ',' | '}' | ']' | ';')
}

fn truncate_large_json(value: Value) -> Value {
    match serde_json::to_vec(&value) {
        Ok(bytes) if bytes.len() > MAX_HTTP_OUTPUT_BYTES => json!({
            "truncated": true,
            "original_bytes": bytes.len(),
            "limit_bytes": MAX_HTTP_OUTPUT_BYTES,
            "preview": String::from_utf8_lossy(&bytes[..MAX_HTTP_OUTPUT_BYTES]).to_string(),
        }),
        _ => value,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnoseErrorCode {
    EnvMissing,
    PayloadMissing,
    PayloadInvalid,
    HttpRequestFailed,
    HttpStatusError,
    ResponseReadFailed,
    ResponseJsonInvalid,
    LogNotFound,
    LogReadFailed,
    StdoutWriteFailed,
}

impl DiagnoseErrorCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::EnvMissing => "DIAGNOSE_ENV_MISSING",
            Self::PayloadMissing => "DIAGNOSE_PAYLOAD_MISSING",
            Self::PayloadInvalid => "DIAGNOSE_PAYLOAD_INVALID",
            Self::HttpRequestFailed => "DIAGNOSE_HTTP_REQUEST_FAILED",
            Self::HttpStatusError => "DIAGNOSE_HTTP_STATUS_ERROR",
            Self::ResponseReadFailed => "DIAGNOSE_RESPONSE_READ_FAILED",
            Self::ResponseJsonInvalid => "DIAGNOSE_RESPONSE_JSON_INVALID",
            Self::LogNotFound => "DIAGNOSE_LOG_NOT_FOUND",
            Self::LogReadFailed => "DIAGNOSE_LOG_READ_FAILED",
            Self::StdoutWriteFailed => "DIAGNOSE_STDOUT_WRITE_FAILED",
        }
    }

    fn exit_code(self) -> ExitCode {
        match self {
            Self::EnvMissing | Self::PayloadMissing | Self::PayloadInvalid => ExitCode::from(2),
            Self::HttpRequestFailed | Self::HttpStatusError | Self::LogNotFound => ExitCode::from(3),
            Self::ResponseReadFailed | Self::ResponseJsonInvalid | Self::LogReadFailed | Self::StdoutWriteFailed => {
                ExitCode::from(1)
            }
        }
    }
}

#[derive(Debug)]
struct DiagnoseError {
    code: DiagnoseErrorCode,
    command: String,
    message: &'static str,
    fields: BTreeMap<&'static str, String>,
}

impl DiagnoseError {
    fn new(code: DiagnoseErrorCode, command: &str, message: &'static str) -> Self {
        Self {
            code,
            command: command.to_owned(),
            message,
            fields: BTreeMap::new(),
        }
    }

    fn field(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.fields.insert(key, value.into());
        self
    }

    fn exit_code(&self) -> ExitCode {
        self.code.exit_code()
    }

    fn stderr_line(&self) -> String {
        let mut line = format!(
            "{} command=\"{}\"",
            self.code.as_str(),
            escape_stderr_field(&self.command)
        );
        for (key, value) in &self.fields {
            line.push_str(&format!(" {key}=\"{}\"", escape_stderr_field(value)));
        }
        line.push_str(": ");
        line.push_str(self.message);
        line
    }
}

fn escape_stderr_field(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnose_error_renders_quoted_stable_contract() {
        let error = DiagnoseError::new(
            DiagnoseErrorCode::EnvMissing,
            "diagnose context",
            "missing required environment variable",
        )
        .field("field", ENV_CONVERSATION_ID);

        assert_eq!(
            error.stderr_line(),
            "DIAGNOSE_ENV_MISSING command=\"diagnose context\" field=\"AIONUI_CONVERSATION_ID\": missing required environment variable"
        );
    }

    #[test]
    fn path_segments_are_percent_encoded() {
        assert_eq!(encode_path_segment("a/b c"), "a%2Fb%20c");
    }
}
