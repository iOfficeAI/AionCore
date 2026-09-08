//! `aioncore conversation` — the agent-facing conversation CLI.
//!
//! Shaped after `cmd_session.rs` and deliberately NOT sharing code with it: the
//! two command families have different envelope and error-code types, and a
//! generic layer to save a few dozen lines is not worth it until a third family
//! appears.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use aionui_api_types::{
    ConversationCliEnvelope, ConversationToolErrorCode, ConversationToolErrorPayload, ConversationToolName,
};
use serde_json::{Value, json};

use crate::cli::{ConversationArgs, ConversationCommand};
use crate::commands::conversation_capabilities;

const ENV_BASE_URL: &str = "AIONUI_BASE_URL";
const ENV_USER_ID: &str = "AIONUI_USER_ID";
const ENV_CONVERSATION_ID: &str = "AIONUI_CONVERSATION_ID";
const ENV_RUNTIME_TOKEN: &str = "AIONUI_RUNTIME_TOKEN";

pub(crate) async fn run_conversation(args: ConversationArgs) -> ExitCode {
    match run_conversation_inner(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

async fn run_conversation_inner(args: ConversationArgs) -> Result<(), ExitCode> {
    match args.command {
        // Static contract; needs no runtime env.
        ConversationCommand::Capabilities => print_json(&ConversationCliEnvelope::success(
            conversation_capabilities::data(),
            Some("conversation capabilities".to_owned()),
        )),
        ConversationCommand::Create => create().await,
        ConversationCommand::Unknown(path) => {
            Err(unknown_command("conversation", path, "unknown conversation command"))
        }
    }
}

async fn create() -> Result<(), ExitCode> {
    let command = "conversation create";
    let env = runtime_env(command)?;
    let body = read_stdin_json_object(command, ConversationToolName::ConversationCreate)?;
    let url = format!(
        "{}/api/runtime/conversations/create",
        env.base_url.trim_end_matches('/')
    );
    let response = reqwest::Client::new()
        .post(url)
        .headers(env.headers(command)?)
        .json(&body)
        .send()
        .await
        .map_err(|error| runtime_error(command, "CONVERSATION_CLI_HTTP_BRIDGE_FAILED", error.to_string()))?;
    print_response(command, response).await
}

struct RuntimeEnv {
    base_url: String,
    user_id: String,
    conversation_id: String,
    runtime_token: String,
}

impl RuntimeEnv {
    /// Fallible on purpose: a malformed `AIONUI_*` value must come back as an
    /// envelope, not a panic (same reasoning as `cmd_session.rs`).
    fn headers(&self, command: &str) -> Result<reqwest::header::HeaderMap, ExitCode> {
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in [
            ("x-aionui-user-id", &self.user_id),
            ("x-aionui-conversation-id", &self.conversation_id),
            ("x-aionui-runtime-token", &self.runtime_token),
        ] {
            let parsed = value.parse().map_err(|_| {
                // The NAME only — a token must never reach stdout or stderr.
                runtime_error(
                    command,
                    "CONVERSATION_CLI_HEADER_INVALID",
                    format!("environment variable for {name} is not a valid header value"),
                )
            })?;
            headers.insert(name, parsed);
        }
        Ok(headers)
    }
}

fn runtime_env(command: &str) -> Result<RuntimeEnv, ExitCode> {
    Ok(RuntimeEnv {
        base_url: required_env(command, ENV_BASE_URL)?,
        user_id: required_env(command, ENV_USER_ID)?,
        conversation_id: required_env(command, ENV_CONVERSATION_ID)?,
        runtime_token: required_env(command, ENV_RUNTIME_TOKEN)?,
    })
}

fn required_env(command: &str, name: &'static str) -> Result<String, ExitCode> {
    std::env::var(name).map_err(|_| {
        print_failure(
            command,
            "CONVERSATION_CLI_ENV_MISSING",
            ConversationToolErrorPayload::new(
                ConversationToolErrorCode::TransportUnavailable,
                format!("missing required environment variable: {name}"),
            ),
        )
    })
}

fn read_stdin_json_object(command: &str, tool: ConversationToolName) -> Result<Value, ExitCode> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).map_err(|error| {
        print_failure(
            command,
            "CONVERSATION_CLI_STDIN_READ_FAILED",
            ConversationToolErrorPayload::new(ConversationToolErrorCode::SchemaValidationFailed, error.to_string()),
        )
    })?;
    let value = if input.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&input).map_err(|error| {
            print_failure(
                command,
                "CONVERSATION_CLI_STDIN_JSON_INVALID",
                ConversationToolErrorPayload::new(ConversationToolErrorCode::SchemaValidationFailed, error.to_string()),
            )
        })?
    };
    validate_against_descriptor(command, tool, value)
}

/// Validate stdin against the registry descriptor before spending a round trip,
/// so a typo comes back as `schema_validation_failed` locally.
fn validate_against_descriptor(command: &str, tool: ConversationToolName, value: Value) -> Result<Value, ExitCode> {
    let Some(object) = value.as_object() else {
        return Err(print_failure(
            command,
            "CONVERSATION_CLI_SCHEMA_VALIDATION_FAILED",
            ConversationToolErrorPayload::new(
                ConversationToolErrorCode::SchemaValidationFailed,
                "stdin JSON must be an object",
            ),
        ));
    };
    let descriptor =
        aionui_api_types::conversation_tool_descriptor(tool.as_str()).expect("descriptor for canonical tool");
    let properties = descriptor.input_schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    for key in object.keys() {
        if !properties.contains_key(key) {
            return Err(print_failure(
                command,
                "CONVERSATION_CLI_SCHEMA_VALIDATION_FAILED",
                ConversationToolErrorPayload::new(
                    ConversationToolErrorCode::SchemaValidationFailed,
                    format!("unknown stdin field: {key}"),
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
                    "CONVERSATION_CLI_SCHEMA_VALIDATION_FAILED",
                    ConversationToolErrorPayload::new(
                        ConversationToolErrorCode::SchemaValidationFailed,
                        format!("missing required stdin field: {key}"),
                    )
                    .with_details(json!({ "expected_schema": descriptor.input_schema })),
                ));
            }
        }
    }
    Ok(value)
}

async fn print_response(command: &str, response: reqwest::Response) -> Result<(), ExitCode> {
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| runtime_error(command, "CONVERSATION_CLI_HTTP_RESPONSE_FAILED", error.to_string()))?;
    if !status.is_success() {
        eprintln!(
            "CONVERSATION_CLI_HTTP_STATUS_ERROR command={command} status={status}: runtime bridge returned non-success status"
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
        ConversationToolErrorPayload::new(ConversationToolErrorCode::TransportUnavailable, message),
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
        "CONVERSATION_CLI_UNKNOWN_COMMAND",
        ConversationToolErrorPayload::new(ConversationToolErrorCode::SchemaValidationFailed, message),
    )
}

fn print_failure(command: &str, stderr_code: &'static str, error: ConversationToolErrorPayload) -> ExitCode {
    eprintln!("{stderr_code} command={command}: {}", error.message);
    let _ = print_json(&ConversationCliEnvelope::<Value>::failure(
        error,
        Some(command.to_owned()),
    ));
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
    use aionui_api_types::tool_name_for_conversation_cli_path;

    use super::*;

    fn env(user_id: &str, conversation_id: &str, runtime_token: &str) -> RuntimeEnv {
        RuntimeEnv {
            base_url: "http://127.0.0.1:1".to_owned(),
            user_id: user_id.to_owned(),
            conversation_id: conversation_id.to_owned(),
            runtime_token: runtime_token.to_owned(),
        }
    }

    #[test]
    fn well_formed_runtime_env_produces_all_three_headers() {
        let headers = env("user_1", "conv_1", "tok_1").headers("conversation create").unwrap();
        assert_eq!(headers.get("x-aionui-user-id").unwrap(), "user_1");
        assert_eq!(headers.get("x-aionui-conversation-id").unwrap(), "conv_1");
        assert_eq!(headers.get("x-aionui-runtime-token").unwrap(), "tok_1");
    }

    #[test]
    fn a_malformed_env_value_yields_an_exit_code_instead_of_panicking() {
        for broken in ["bad\nvalue", "bad\rvalue", "bad\0value"] {
            assert!(env(broken, "conv_1", "tok_1").headers("conversation create").is_err());
            assert!(env("user_1", broken, "tok_1").headers("conversation create").is_err());
            assert!(env("user_1", "conv_1", broken).headers("conversation create").is_err());
        }
    }

    #[test]
    fn create_requires_name_and_rejects_unknown_fields_and_non_objects() {
        let tool = ConversationToolName::ConversationCreate;
        assert!(validate_against_descriptor("conversation create", tool, json!({ "workspace": "/tmp" })).is_err());
        assert!(validate_against_descriptor("conversation create", tool, json!({ "name": "x", "files": [] })).is_err());
        assert!(validate_against_descriptor("conversation create", tool, json!(["name"])).is_err());
        assert!(validate_against_descriptor("conversation create", tool, json!({ "name": "x" })).is_ok());
        assert!(
            validate_against_descriptor(
                "conversation create",
                tool,
                json!({ "name": "x", "workspace": "/abs", "assistant_id": "asst" })
            )
            .is_ok()
        );
    }

    #[test]
    fn the_cli_path_this_file_hard_codes_is_the_registrys() {
        assert_eq!(
            tool_name_for_conversation_cli_path(&["create".to_owned()]),
            Some(ConversationToolName::ConversationCreate)
        );
    }
}
