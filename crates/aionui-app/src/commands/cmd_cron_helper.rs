//! `aioncore cron-helper` subcommand: HTTP helper for the built-in cron skill.
//!
//! The helper is invoked by agent shells through the auto-injected cron skill.
//! It intentionally depends only on the bundled `aioncore` binary and runtime
//! environment injected by AionUi; it does not require Python, curl, or PATH
//! discovery.

use std::io::{self, Read};
use std::process::ExitCode;

use reqwest::Method;
use serde_json::Value;

use crate::cli::{CronHelperArgs, CronHelperCommand};
use crate::commands::error::{CliBoundaryCode, CliBoundaryError};

const SUBCOMMAND: &str = "cron-helper";
const ENV_BASE_URL: &str = "AIONUI_BASE_URL";
const ENV_CONVERSATION_ID: &str = "AIONUI_CONVERSATION_ID";
const ENV_USER_ID: &str = "AIONUI_USER_ID";

pub async fn run_cron_helper(args: CronHelperArgs) -> ExitCode {
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let Some(detail) = error.detail.as_deref() {
                eprintln!("{detail}");
            }
            eprintln!("{}", error.boundary.stderr_line());
            error.boundary.exit_code()
        }
    }
}

async fn run(args: CronHelperArgs) -> Result<(), CronHelperError> {
    let env = HelperEnv::from_env()?;
    let client = reqwest::Client::new();

    match args.command {
        CronHelperCommand::Discover => {
            request_json(&client, &env, Method::GET, "/api/internal/conversation-cron/list", None).await?;
            println!("{}", env.base_url);
            Ok(())
        }
        CronHelperCommand::List => {
            let value = request_json(&client, &env, Method::GET, "/api/internal/conversation-cron/list", None).await?;
            print_json(&value)
        }
        CronHelperCommand::Create => {
            let payload = read_stdin_payload("create")?;
            let value = request_json(
                &client,
                &env,
                Method::POST,
                "/api/internal/conversation-cron/create",
                Some(payload),
            )
            .await?;
            print_json(&value)
        }
        CronHelperCommand::Update(update) => {
            let payload = read_stdin_payload("update")?;
            let path = format!("/api/internal/conversation-cron/jobs/{}", update.job_id);
            let value = request_json(&client, &env, Method::PUT, &path, Some(payload)).await?;
            print_json(&value)
        }
    }
}

#[derive(Debug)]
struct HelperEnv {
    base_url: String,
    conversation_id: String,
    user_id: String,
}

impl HelperEnv {
    fn from_env() -> Result<Self, CronHelperError> {
        Ok(Self {
            base_url: required_env(ENV_BASE_URL)?.trim_end_matches('/').to_owned(),
            conversation_id: required_env(ENV_CONVERSATION_ID)?,
            user_id: required_env(ENV_USER_ID)?,
        })
    }
}

fn required_env(name: &'static str) -> Result<String, CronHelperError> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            CronHelperError::new(CliBoundaryCode::CronEnvMissing, "missing required environment variable")
                .field("env", name)
        })
}

async fn request_json(
    client: &reqwest::Client,
    env: &HelperEnv,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value, CronHelperError> {
    let url = format!("{}{}", env.base_url, path);
    let mut request = client
        .request(method, &url)
        .header("content-type", "application/json")
        .header("x-aionui-conversation-id", &env.conversation_id)
        .header("x-aionui-user-id", &env.user_id);
    if let Some(body) = body {
        request = request.json(&body);
    }

    let response = request.send().await.map_err(|error| {
        CronHelperError::new(CliBoundaryCode::CronHttpRequestFailed, "failed to call AionUi backend")
            .field("path", path)
            .detail(format!("HTTP request failed for {url}: {error}"))
    })?;

    let status = response.status();
    let text = response.text().await.map_err(|error| {
        CronHelperError::new(
            CliBoundaryCode::CronResponseReadFailed,
            "failed to read AionUi backend response",
        )
        .field("path", path)
        .detail(format!("Failed to read response from {url}: {error}"))
    })?;

    if status == reqwest::StatusCode::NOT_FOUND && path.starts_with("/api/internal/conversation-cron") {
        return Err(CronHelperError::new(
            CliBoundaryCode::CronBackendUnavailable,
            "AionUi backend does not expose conversation cron helper routes",
        )
        .field("path", path)
        .detail(format!("AionUi backend not found at AIONUI_BASE_URL: {}", env.base_url)));
    }

    if !status.is_success() {
        return Err(CronHelperError::new(
            CliBoundaryCode::CronHttpStatusError,
            "AionUi backend returned an error status",
        )
        .field("path", path)
        .field("status", status.as_u16().to_string())
        .detail(format!("HTTP {}: {}", status.as_u16(), text)));
    }

    if text.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }

    serde_json::from_str(&text).map_err(|error| {
        CronHelperError::new(
            CliBoundaryCode::CronResponseJsonInvalid,
            "AionUi backend returned invalid JSON",
        )
        .field("path", path)
        .detail(format!("Invalid JSON response from {url}: {error}"))
    })
}

fn read_stdin_payload(command: &'static str) -> Result<Value, CronHelperError> {
    let mut raw = String::new();
    io::stdin().read_to_string(&mut raw).map_err(|error| {
        CronHelperError::new(
            CliBoundaryCode::CronPayloadInvalid,
            "failed to read JSON payload from stdin",
        )
        .field("command", command)
        .detail(format!("Failed to read stdin payload: {error}"))
    })?;
    if raw.trim().is_empty() {
        return Err(
            CronHelperError::new(CliBoundaryCode::CronPayloadMissing, "JSON payload is required on stdin")
                .field("command", command),
        );
    }
    serde_json::from_str(&raw).map_err(|error| {
        CronHelperError::new(CliBoundaryCode::CronPayloadInvalid, "invalid JSON payload on stdin")
            .field("command", command)
            .detail(format!("Invalid JSON payload on stdin: {error}"))
    })
}

fn print_json(value: &Value) -> Result<(), CronHelperError> {
    let rendered = serde_json::to_string_pretty(value).map_err(|error| {
        CronHelperError::new(
            CliBoundaryCode::CronResponseJsonInvalid,
            "failed to serialize JSON response",
        )
        .detail(format!("Failed to serialize JSON response: {error}"))
    })?;
    println!("{rendered}");
    Ok(())
}

#[derive(Debug)]
struct CronHelperError {
    boundary: CliBoundaryError,
    detail: Option<String>,
}

impl CronHelperError {
    fn new(code: CliBoundaryCode, message: &'static str) -> Self {
        Self {
            boundary: CliBoundaryError::new(code, SUBCOMMAND, message),
            detail: None,
        }
    }

    fn field(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.boundary = self.boundary.with_field(key, value);
        self
    }

    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_context_env_renders_stable_error() {
        let error = CronHelperError::new(CliBoundaryCode::CronEnvMissing, "missing required environment variable")
            .field("env", ENV_CONVERSATION_ID);

        assert_eq!(error.boundary.code(), CliBoundaryCode::CronEnvMissing);
        assert_eq!(
            error.boundary.stderr_line(),
            "CRON_ENV_MISSING subcommand=cron-helper env=AIONUI_CONVERSATION_ID: missing required environment variable"
        );
    }
}
