use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

use aionui_api_types::{
    TeamToolCliEnvelope, TeamToolErrorCode, TeamToolErrorPayload, TeamToolName, TeamToolRuntimeCallRequest,
    tool_name_for_cli_path,
};
use serde_json::{Value, json};

use crate::cli::{TeamArgs, TeamCommand, TeamTaskCommand};
use crate::commands::json_input::{JsonInputArgs, JsonInputError, JsonInputFailure, JsonInputSource, read_json_input};
use crate::commands::team_capabilities;

const ENV_BASE_URL: &str = "AIONUI_BASE_URL";
const ENV_USER_ID: &str = "AIONUI_USER_ID";
const ENV_CONVERSATION_ID: &str = "AIONUI_CONVERSATION_ID";
const ENV_RUNTIME_TOKEN: &str = "AIONUI_RUNTIME_TOKEN";

pub(crate) async fn run_team(args: TeamArgs) -> ExitCode {
    let input = args.json_input;
    match run_team_inner(args.command, &input).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

async fn run_team_inner(command: TeamCommand, input: &JsonInputArgs) -> Result<(), ExitCode> {
    match command {
        TeamCommand::Capabilities => print_json(&TeamToolCliEnvelope::success(
            team_capabilities::data(),
            Some("team capabilities".to_owned()),
        )),
        TeamCommand::Help => print_json(&TeamToolCliEnvelope::success(
            json!({ "format": "markdown", "text": team_capabilities::help_markdown() }),
            Some("team help".to_owned()),
        )),
        TeamCommand::Context => {
            let env = runtime_env("team context")?;
            let url = format!("{}/api/runtime/team-tools/context", env.base_url.trim_end_matches('/'));
            let response = reqwest::Client::new()
                .get(url)
                .headers(env.headers())
                .send()
                .await
                .map_err(|error| runtime_error("team context", "TEAM_CLI_HTTP_BRIDGE_FAILED", error.to_string()))?;
            print_response(response).await
        }
        TeamCommand::Members => call_tool(vec!["members"], input).await,
        TeamCommand::ReadMessages => call_tool(vec!["read-messages"], input).await,
        TeamCommand::SendMessage => call_tool(vec!["send-message"], input).await,
        TeamCommand::InterruptAgent => call_tool(vec!["interrupt-agent"], input).await,
        TeamCommand::Task(task) => match task.command {
            TeamTaskCommand::Create => call_tool(vec!["task", "create"], input).await,
            TeamTaskCommand::Update => call_tool(vec!["task", "update"], input).await,
            TeamTaskCommand::List => call_tool(vec!["task", "list"], input).await,
            TeamTaskCommand::Unknown(path) => Err(unknown_command("team task", path, "unknown team task command")),
        },
        TeamCommand::ListAssistants => call_tool(vec!["list-assistants"], input).await,
        TeamCommand::DescribeAssistant => call_tool(vec!["describe-assistant"], input).await,
        TeamCommand::SpawnAgent => call_tool(vec!["spawn-agent"], input).await,
        TeamCommand::RenameAgent => call_tool(vec!["rename-agent"], input).await,
        TeamCommand::ClearAgentContext => call_tool(vec!["clear-agent-context"], input).await,
        TeamCommand::ShutdownAgent => call_tool(vec!["shutdown-agent"], input).await,
        TeamCommand::Unknown(path) => Err(unknown_command("team", path, "unknown team command")),
    }
}

async fn call_tool(path: Vec<&'static str>, input: &JsonInputArgs) -> Result<(), ExitCode> {
    let command = format!("team {}", path.join(" "));
    let env = runtime_env(&command)?;
    let tool =
        tool_name_for_cli_path(&path.iter().map(|part| (*part).to_owned()).collect::<Vec<_>>()).ok_or_else(|| {
            print_failure(
                &command,
                "TEAM_CLI_UNKNOWN_COMMAND",
                TeamToolErrorPayload::new(TeamToolErrorCode::UnknownTool, "unknown team command"),
            )
        })?;
    let arguments = read_stdin_json_object(&command, tool, input)?;
    let url = format!("{}/api/runtime/team-tools/call", env.base_url.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .post(url)
        .headers(env.headers())
        .json(&TeamToolRuntimeCallRequest { tool, arguments })
        .send()
        .await
        .map_err(|error| runtime_error(&command, "TEAM_CLI_HTTP_BRIDGE_FAILED", error.to_string()))?;
    print_response(response).await
}

struct RuntimeEnv {
    base_url: String,
    user_id: String,
    conversation_id: String,
    runtime_token: String,
}

impl RuntimeEnv {
    fn headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-aionui-user-id", self.user_id.parse().unwrap());
        headers.insert("x-aionui-conversation-id", self.conversation_id.parse().unwrap());
        headers.insert("x-aionui-runtime-token", self.runtime_token.parse().unwrap());
        headers
    }
}

fn runtime_env(command: &str) -> Result<RuntimeEnv, ExitCode> {
    let base_url = required_env(command, ENV_BASE_URL)?;
    let user_id = required_env(command, ENV_USER_ID)?;
    let conversation_id = required_env(command, ENV_CONVERSATION_ID)?;
    let runtime_token = required_env(command, ENV_RUNTIME_TOKEN)?;
    Ok(RuntimeEnv {
        base_url,
        user_id,
        conversation_id,
        runtime_token,
    })
}

fn required_env(command: &str, name: &'static str) -> Result<String, ExitCode> {
    std::env::var(name).map_err(|_| {
        print_failure(
            command,
            "TEAM_CLI_ENV_MISSING",
            TeamToolErrorPayload::new(
                TeamToolErrorCode::RuntimeContextMissing,
                format!("missing required environment variable: {name}"),
            ),
        )
    })
}

fn read_stdin_json_object(command: &str, tool: TeamToolName, input: &JsonInputArgs) -> Result<Value, ExitCode> {
    let document = read_json_input(input).map_err(|error| {
        let code = json_input_error_code(&error);
        print_failure(
            command,
            code,
            TeamToolErrorPayload::new(TeamToolErrorCode::SchemaValidationFailed, error.failure.message())
                .with_details(error.metadata.details()),
        )
    })?;
    validate_against_descriptor(command, tool, document.value)
}

fn json_input_error_code(error: &JsonInputError) -> &'static str {
    match (error.failure, error.metadata.source) {
        (JsonInputFailure::ReadTimeout, JsonInputSource::Stdin) => "TEAM_CLI_STDIN_TIMEOUT",
        (JsonInputFailure::ReadTimeout, _) => "TEAM_CLI_JSON_INPUT_TIMEOUT",
        (JsonInputFailure::ReadFailed, _) => "TEAM_CLI_JSON_INPUT_READ_FAILED",
        (JsonInputFailure::TooLarge, _) => "TEAM_CLI_JSON_INPUT_TOO_LARGE",
        (JsonInputFailure::InvalidEncoding, _) => "TEAM_CLI_JSON_INPUT_ENCODING_INVALID",
        (JsonInputFailure::InvalidJson, _) => "TEAM_CLI_JSON_INPUT_INVALID",
    }
}

fn validate_against_descriptor(command: &str, tool: TeamToolName, value: Value) -> Result<Value, ExitCode> {
    let Some(object) = value.as_object() else {
        return Err(print_failure(
            command,
            "TEAM_CLI_SCHEMA_VALIDATION_FAILED",
            TeamToolErrorPayload::new(
                TeamToolErrorCode::SchemaValidationFailed,
                "JSON input must be an object",
            ),
        ));
    };
    let descriptor = aionui_api_types::team_tool_descriptor(tool.as_str()).expect("descriptor for canonical tool");
    let properties = descriptor.input_schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    for key in object.keys() {
        if !properties.contains_key(key) {
            return Err(print_failure(
                command,
                "TEAM_CLI_SCHEMA_VALIDATION_FAILED",
                TeamToolErrorPayload::new(
                    TeamToolErrorCode::SchemaValidationFailed,
                    format!("unknown JSON input field: {key}"),
                )
                .with_details(json!({ "expected_schema": descriptor.input_schema })),
            ));
        }
    }
    if let Some(required) = descriptor.input_schema["required"].as_array() {
        for key in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(key) {
                return Err(print_failure(
                    command,
                    "TEAM_CLI_SCHEMA_VALIDATION_FAILED",
                    TeamToolErrorPayload::new(
                        TeamToolErrorCode::SchemaValidationFailed,
                        format!("missing required JSON input field: {key}"),
                    )
                    .with_details(json!({ "expected_schema": descriptor.input_schema })),
                ));
            }
        }
    }
    Ok(value)
}

async fn print_response(response: reqwest::Response) -> Result<(), ExitCode> {
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| runtime_error("team", "TEAM_CLI_HTTP_RESPONSE_FAILED", error.to_string()))?;
    if !status.is_success() {
        eprintln!(
            "TEAM_CLI_HTTP_STATUS_ERROR command=team status={status}: runtime bridge returned non-success status"
        );
        println!("{text}");
        return Err(ExitCode::from(3));
    }
    println!("{text}");
    Ok(())
}

fn runtime_error(command: &str, code: &'static str, message: String) -> ExitCode {
    print_failure(
        command,
        code,
        TeamToolErrorPayload::new(TeamToolErrorCode::TransportUnavailable, message),
    )
}

fn unknown_command(prefix: &str, path: Vec<OsString>, message: &'static str) -> ExitCode {
    let suffix = path
        .into_iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    let command = if suffix.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix} {suffix}")
    };
    print_failure(
        &command,
        "TEAM_CLI_UNKNOWN_COMMAND",
        TeamToolErrorPayload::new(TeamToolErrorCode::UnknownTool, message),
    )
}

fn print_failure(command: &str, stderr_code: &'static str, error: TeamToolErrorPayload) -> ExitCode {
    eprintln!("{stderr_code} command={command}: {}", error.message);
    let _ = print_json(&TeamToolCliEnvelope::<Value>::failure(error, Some(command.to_owned())));
    ExitCode::from(2)
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), ExitCode> {
    let rendered = serde_json::to_string_pretty(value).map_err(|_| ExitCode::from(1))?;
    let mut stdout = io::stdout();
    stdout
        .write_all(rendered.as_bytes())
        .and_then(|_| stdout.write_all(b"\n"))
        .map_err(|_| ExitCode::from(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::json_input::JsonInputMetadata;

    #[test]
    fn a_stalled_stdin_uses_the_stable_team_timeout_code() {
        let error = JsonInputError {
            failure: JsonInputFailure::ReadTimeout,
            metadata: JsonInputMetadata {
                source: JsonInputSource::Stdin,
                bytes_read: 1,
                prefix_hex: "7b".to_owned(),
            },
        };
        assert_eq!(json_input_error_code(&error), "TEAM_CLI_STDIN_TIMEOUT");
        assert_eq!(error.metadata.details().as_object().unwrap().len(), 3);
    }
}
