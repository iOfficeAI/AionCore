//! `aioncore session` — the agent-facing cross-session messaging CLI.
//!
//! Shaped after `cmd_team.rs`, with two deliberate differences: `session` talks
//! to TWO dedicated endpoints instead of one generic `/call` (so `list` is a GET
//! whose stdin becomes a query string, and `send-message` is a POST whose stdin
//! is the body), and there is no `help` / `context` subcommand.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use aionui_api_types::{SessionCliEnvelope, SessionToolErrorCode, SessionToolErrorPayload, SessionToolName};
use serde_json::{Value, json};

use crate::cli::{SessionArgs, SessionCommand};
use crate::commands::session_capabilities;

const ENV_BASE_URL: &str = "AIONUI_BASE_URL";
const ENV_USER_ID: &str = "AIONUI_USER_ID";
const ENV_CONVERSATION_ID: &str = "AIONUI_CONVERSATION_ID";
const ENV_RUNTIME_TOKEN: &str = "AIONUI_RUNTIME_TOKEN";

pub(crate) async fn run_session(args: SessionArgs) -> ExitCode {
    match run_session_inner(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

async fn run_session_inner(args: SessionArgs) -> Result<(), ExitCode> {
    match args.command {
        // Always available: a static contract that touches no conversation data,
        // so it works with no runtime env and even when the feature is off
        // (spec §6.9.2).
        SessionCommand::Capabilities => print_json(&SessionCliEnvelope::success(
            session_capabilities::data(),
            Some("session capabilities".to_owned()),
        )),
        SessionCommand::List => list().await,
        SessionCommand::SendMessage => send_message().await,
        SessionCommand::Unknown(path) => Err(unknown_command("session", path, "unknown session command")),
    }
}

async fn list() -> Result<(), ExitCode> {
    let command = "session list";
    let env = runtime_env(command)?;
    let arguments = read_stdin_json_object(command, SessionToolName::SessionList)?;
    let query = query_string(&arguments);
    let url = format!(
        "{}/api/runtime/session-messages/targets{query}",
        env.base_url.trim_end_matches('/')
    );
    let response = reqwest::Client::new()
        .get(url)
        .headers(env.headers(command)?)
        .send()
        .await
        .map_err(|error| runtime_error(command, "SESSION_CLI_HTTP_BRIDGE_FAILED", error.to_string()))?;
    print_response(command, response).await
}

async fn send_message() -> Result<(), ExitCode> {
    let command = "session send-message";
    let env = runtime_env(command)?;
    let body = read_stdin_json_object(command, SessionToolName::SessionSendMessage)?;
    let url = format!(
        "{}/api/runtime/session-messages/send",
        env.base_url.trim_end_matches('/')
    );
    let response = reqwest::Client::new()
        .post(url)
        .headers(env.headers(command)?)
        .json(&body)
        .send()
        .await
        .map_err(|error| runtime_error(command, "SESSION_CLI_HTTP_BRIDGE_FAILED", error.to_string()))?;
    print_response(command, response).await
}

/// `list` is a GET, so its stdin object becomes a query string. Only scalars are
/// meaningful here; the descriptor schema already rejects anything else.
fn query_string(arguments: &Value) -> String {
    let Some(object) = arguments.as_object() else {
        return String::new();
    };
    let pairs: Vec<String> = object
        .iter()
        .filter_map(|(key, value)| {
            let rendered = match value {
                Value::String(text) => text.clone(),
                Value::Number(number) => number.to_string(),
                Value::Bool(flag) => flag.to_string(),
                _ => return None,
            };
            Some(format!("{}={}", urlencode(key), urlencode(&rendered)))
        })
        .collect();
    if pairs.is_empty() {
        String::new()
    } else {
        format!("?{}", pairs.join("&"))
    }
}

/// Minimal percent-encoding for query values. Deliberately not a new dependency:
/// the only inputs are a name filter, a project id, a page size and an opaque
/// cursor.
fn urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => encoded.push(byte as char),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

struct RuntimeEnv {
    base_url: String,
    user_id: String,
    conversation_id: String,
    runtime_token: String,
}

impl RuntimeEnv {
    /// Fallible on purpose. A header value rejects control characters and
    /// non-ASCII bytes, so a malformed `AIONUI_*` variable would panic here —
    /// and a panicking CLI prints no envelope at all, leaving the agent with a
    /// stack trace instead of the `transport_unavailable` it knows how to
    /// report. Every other failure in this file goes out as an envelope; this
    /// was the one exception.
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
                    "SESSION_CLI_HEADER_INVALID",
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
            "SESSION_CLI_ENV_MISSING",
            SessionToolErrorPayload::new(
                SessionToolErrorCode::TransportUnavailable,
                format!("missing required environment variable: {name}"),
            ),
        )
    })
}

fn read_stdin_json_object(command: &str, tool: SessionToolName) -> Result<Value, ExitCode> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).map_err(|error| {
        print_failure(
            command,
            "SESSION_CLI_STDIN_READ_FAILED",
            SessionToolErrorPayload::new(SessionToolErrorCode::SchemaValidationFailed, error.to_string()),
        )
    })?;
    let value = if input.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&input).map_err(|error| {
            print_failure(
                command,
                "SESSION_CLI_STDIN_JSON_INVALID",
                SessionToolErrorPayload::new(SessionToolErrorCode::SchemaValidationFailed, error.to_string()),
            )
        })?
    };
    validate_against_descriptor(command, tool, value)
}

/// Validate stdin against the registry descriptor before spending a round trip,
/// so a typo comes back as `schema_validation_failed` locally rather than as a
/// confusing server-side rejection.
fn validate_against_descriptor(command: &str, tool: SessionToolName, value: Value) -> Result<Value, ExitCode> {
    let Some(object) = value.as_object() else {
        return Err(print_failure(
            command,
            "SESSION_CLI_SCHEMA_VALIDATION_FAILED",
            SessionToolErrorPayload::new(
                SessionToolErrorCode::SchemaValidationFailed,
                "stdin JSON must be an object",
            ),
        ));
    };
    let descriptor = aionui_api_types::session_tool_descriptor(tool.as_str()).expect("descriptor for canonical tool");
    let properties = descriptor.input_schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    for key in object.keys() {
        if !properties.contains_key(key) {
            return Err(print_failure(
                command,
                "SESSION_CLI_SCHEMA_VALIDATION_FAILED",
                SessionToolErrorPayload::new(
                    SessionToolErrorCode::SchemaValidationFailed,
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
                    "SESSION_CLI_SCHEMA_VALIDATION_FAILED",
                    SessionToolErrorPayload::new(
                        SessionToolErrorCode::SchemaValidationFailed,
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
        .map_err(|error| runtime_error(command, "SESSION_CLI_HTTP_RESPONSE_FAILED", error.to_string()))?;
    if !status.is_success() {
        eprintln!(
            "SESSION_CLI_HTTP_STATUS_ERROR command={command} status={status}: runtime bridge returned non-success status"
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
        SessionToolErrorPayload::new(SessionToolErrorCode::TransportUnavailable, message),
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
        "SESSION_CLI_UNKNOWN_COMMAND",
        SessionToolErrorPayload::new(SessionToolErrorCode::SchemaValidationFailed, message),
    )
}

fn print_failure(command: &str, stderr_code: &'static str, error: SessionToolErrorPayload) -> ExitCode {
    eprintln!("{stderr_code} command={command}: {}", error.message);
    let _ = print_json(&SessionCliEnvelope::<Value>::failure(error, Some(command.to_owned())));
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
    use aionui_api_types::tool_name_for_session_cli_path;

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
        let headers = env("user_1", "conv_1", "tok_1")
            .headers("session list")
            .expect("ordinary ids and tokens are valid header values");
        assert_eq!(headers.get("x-aionui-user-id").unwrap(), "user_1");
        assert_eq!(headers.get("x-aionui-conversation-id").unwrap(), "conv_1");
        assert_eq!(headers.get("x-aionui-runtime-token").unwrap(), "tok_1");
    }

    /// A header value rejects control characters, so this used to panic and the
    /// agent got a stack trace instead of an envelope it could report.
    #[test]
    fn a_malformed_env_value_yields_an_exit_code_instead_of_panicking() {
        for broken in ["bad\nvalue", "bad\rvalue", "bad\0value"] {
            assert!(
                env(broken, "conv_1", "tok_1").headers("session list").is_err(),
                "{broken:?} must be rejected, not panic"
            );
            assert!(env("user_1", broken, "tok_1").headers("session list").is_err());
            assert!(env("user_1", "conv_1", broken).headers("session list").is_err());
        }
    }

    #[test]
    fn an_empty_stdin_object_produces_no_query_string() {
        assert_eq!(query_string(&json!({})), "");
    }

    #[test]
    fn scalar_arguments_become_query_pairs() {
        let query = query_string(&json!({ "q": "auth" }));
        assert_eq!(query, "?q=auth");
    }

    #[test]
    fn query_values_are_percent_encoded_so_a_name_filter_cannot_break_the_url() {
        let query = query_string(&json!({ "q": "重构 auth&x=1" }));
        assert!(query.starts_with("?q="), "{query}");
        assert!(!query.contains(' '), "{query}");
        assert!(
            !query.contains("&x=1"),
            "an unescaped & would forge a parameter: {query}"
        );
    }

    #[test]
    fn numbers_are_rendered_without_quotes() {
        assert_eq!(query_string(&json!({ "limit": 20 })), "?limit=20");
    }

    #[test]
    fn non_scalar_values_are_dropped_rather_than_serialised_into_the_url() {
        assert_eq!(query_string(&json!({ "q": ["a"] })), "");
    }

    #[test]
    fn send_message_requires_both_to_and_message() {
        // The descriptor is the source of truth, so a missing field fails
        // locally before a round trip.
        let missing_message = validate_against_descriptor(
            "session send-message",
            SessionToolName::SessionSendMessage,
            json!({ "to": "conv_1" }),
        );
        assert!(missing_message.is_err());

        let ok = validate_against_descriptor(
            "session send-message",
            SessionToolName::SessionSendMessage,
            json!({ "to": "conv_1", "message": "hi" }),
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn an_unknown_stdin_field_is_rejected_locally() {
        // `files` was deliberately cut from v1 (spec §6.1); passing it must fail
        // loudly rather than being silently ignored by the server.
        let result = validate_against_descriptor(
            "session send-message",
            SessionToolName::SessionSendMessage,
            json!({ "to": "conv_1", "message": "hi", "files": ["/tmp/a"] }),
        );
        assert!(result.is_err());
    }

    #[test]
    fn list_accepts_an_empty_object_because_every_field_is_optional() {
        let result = validate_against_descriptor("session list", SessionToolName::SessionList, json!({}));
        assert!(result.is_ok());
    }

    #[test]
    fn the_cli_paths_the_registry_advertises_resolve_to_tool_names() {
        // Guards the same contract as the `cli.rs` test, from this side: the
        // paths this file hard-codes must be the registry's.
        assert_eq!(
            tool_name_for_session_cli_path(&["list".to_owned()]),
            Some(SessionToolName::SessionList)
        );
        assert_eq!(
            tool_name_for_session_cli_path(&["send-message".to_owned()]),
            Some(SessionToolName::SessionSendMessage)
        );
    }
}
