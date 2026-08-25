//! `aioncore skills` — the agent-facing runtime skills CLI (channel A).
//!
//! Shaped after `cmd_session.rs`: the same three runtime headers, the same
//! envelope-on-every-failure discipline, the same "a malformed env var must not
//! panic" handling. Differences: every subcommand is a GET (this domain is
//! read-only), and `cat` splits its stdin `path` into a skill name plus a
//! relative path.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use aionui_api_types::{SkillRuntimeEnvelope, SkillRuntimeErrorCode, SkillRuntimeErrorPayload};
use serde_json::{Value, json};

use crate::cli::{SkillsArgs, SkillsCommand};
use crate::commands::skills_capabilities;

const ENV_BASE_URL: &str = "AIONUI_BASE_URL";
const ENV_USER_ID: &str = "AIONUI_USER_ID";
const ENV_CONVERSATION_ID: &str = "AIONUI_CONVERSATION_ID";
const ENV_RUNTIME_TOKEN: &str = "AIONUI_RUNTIME_TOKEN";

pub(crate) async fn run_skills(args: SkillsArgs) -> ExitCode {
    match run_skills_inner(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

async fn run_skills_inner(args: SkillsArgs) -> Result<(), ExitCode> {
    match args.command {
        // Static contract: no conversation data, so it works with no runtime env.
        SkillsCommand::Capabilities => print_json(&SkillRuntimeEnvelope::success(
            skills_capabilities::data(),
            Some("skills capabilities".to_owned()),
        )),
        SkillsCommand::List => list().await,
        SkillsCommand::Show => show().await,
        SkillsCommand::Cat => cat().await,
        SkillsCommand::Unknown(path) => Err(unknown_command(path)),
    }
}

async fn list() -> Result<(), ExitCode> {
    let command = "skills list";
    let env = runtime_env(command)?;
    let url = format!("{}/api/runtime/skills", env.base_url.trim_end_matches('/'));
    get(command, &env, url).await
}

async fn show() -> Result<(), ExitCode> {
    let command = "skills show";
    let env = runtime_env(command)?;
    let name = required_string_field(command, "name")?;
    let url = format!(
        "{}/api/runtime/skills/{}",
        env.base_url.trim_end_matches('/'),
        urlencode(&name)
    );
    get(command, &env, url).await
}

async fn cat() -> Result<(), ExitCode> {
    let command = "skills cat";
    let env = runtime_env(command)?;
    let path = required_string_field(command, "path")?;
    let (name, rel) = split_skill_path(command, &path)?;
    let url = format!(
        "{}/api/runtime/skills/{}/file?path={}",
        env.base_url.trim_end_matches('/'),
        urlencode(&name),
        urlencode(&rel)
    );
    get(command, &env, url).await
}

/// Split `<skill-name>/<relative-path>` at the FIRST separator, so a nested
/// `references/sub/x.md` keeps its own slashes.
fn split_skill_path(command: &str, path: &str) -> Result<(String, String), ExitCode> {
    let trimmed = path.trim();
    match trimmed.split_once('/') {
        Some((name, rel)) if !name.is_empty() && !rel.is_empty() => Ok((name.to_owned(), rel.to_owned())),
        _ => Err(print_failure(
            command,
            "SKILLS_CLI_SCHEMA_VALIDATION_FAILED",
            SkillRuntimeErrorPayload::new(
                SkillRuntimeErrorCode::SchemaValidationFailed,
                "`path` must be of the form <skill-name>/<relative-path>",
            ),
        )),
    }
}

async fn get(command: &str, env: &RuntimeEnv, url: String) -> Result<(), ExitCode> {
    let response = reqwest::Client::new()
        .get(url)
        .headers(env.headers(command)?)
        .send()
        .await
        .map_err(|error| runtime_error(command, "SKILLS_CLI_HTTP_BRIDGE_FAILED", error.to_string()))?;
    print_response(command, response).await
}

struct RuntimeEnv {
    base_url: String,
    user_id: String,
    conversation_id: String,
    runtime_token: String,
}

impl RuntimeEnv {
    /// Fallible on purpose: a header value rejects control characters, so a
    /// malformed `AIONUI_*` variable would otherwise panic — and a panicking CLI
    /// prints no envelope at all, leaving the agent with a stack trace instead of
    /// something it can report.
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
                    "SKILLS_CLI_HEADER_INVALID",
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
            "SKILLS_CLI_ENV_MISSING",
            SkillRuntimeErrorPayload::new(
                SkillRuntimeErrorCode::TransportUnavailable,
                format!("missing required environment variable: {name}"),
            ),
        )
    })
}

fn read_stdin_json_object(command: &str) -> Result<Value, ExitCode> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).map_err(|error| {
        print_failure(
            command,
            "SKILLS_CLI_STDIN_READ_FAILED",
            SkillRuntimeErrorPayload::new(SkillRuntimeErrorCode::SchemaValidationFailed, error.to_string()),
        )
    })?;
    if input.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&input).map_err(|error| {
        print_failure(
            command,
            "SKILLS_CLI_STDIN_JSON_INVALID",
            SkillRuntimeErrorPayload::new(SkillRuntimeErrorCode::SchemaValidationFailed, error.to_string()),
        )
    })
}

fn required_string_field(command: &str, field: &'static str) -> Result<String, ExitCode> {
    let value = read_stdin_json_object(command)?;
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            print_failure(
                command,
                "SKILLS_CLI_SCHEMA_VALIDATION_FAILED",
                SkillRuntimeErrorPayload::new(
                    SkillRuntimeErrorCode::SchemaValidationFailed,
                    format!("missing required stdin field: {field}"),
                ),
            )
        })
}

/// Minimal percent-encoding. Deliberately not a new dependency: the only inputs
/// are a skill name and a relative path.
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

async fn print_response(command: &str, response: reqwest::Response) -> Result<(), ExitCode> {
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| runtime_error(command, "SKILLS_CLI_HTTP_RESPONSE_FAILED", error.to_string()))?;
    if !status.is_success() {
        eprintln!(
            "SKILLS_CLI_HTTP_STATUS_ERROR command={command} status={status}: runtime bridge returned non-success status"
        );
        // The body is already an envelope carrying a stable error code, so print
        // it verbatim rather than re-wrapping and losing the code.
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
        SkillRuntimeErrorPayload::new(SkillRuntimeErrorCode::TransportUnavailable, message),
    )
}

fn unknown_command(path: Vec<OsString>) -> ExitCode {
    let suffix = path
        .into_iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    let command = if suffix.is_empty() {
        "skills".to_owned()
    } else {
        format!("skills {suffix}")
    };
    print_failure(
        &command,
        "SKILLS_CLI_UNKNOWN_COMMAND",
        SkillRuntimeErrorPayload::new(
            SkillRuntimeErrorCode::SchemaValidationFailed,
            "unknown skills command; run `skills capabilities` for the contract",
        ),
    )
}

fn print_failure(command: &str, stderr_code: &'static str, error: SkillRuntimeErrorPayload) -> ExitCode {
    eprintln!("{stderr_code} command={command}: {}", error.message);
    let _ = print_json(&SkillRuntimeEnvelope::<Value>::failure(error, Some(command.to_owned())));
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

    #[test]
    fn a_nested_relative_path_keeps_its_own_separators() {
        let (name, rel) = split_skill_path("skills cat", "morph-ppt/reference/styles/INDEX.md").unwrap();
        assert_eq!(name, "morph-ppt");
        assert_eq!(
            rel, "reference/styles/INDEX.md",
            "only the FIRST separator splits, or nested files become unreachable"
        );
    }

    #[test]
    fn a_path_without_a_relative_part_is_refused_locally() {
        // Refused here rather than after a round trip, so a typo comes back as a
        // schema error instead of a confusing server-side rejection.
        for bad in ["cron", "cron/", "/notes.md", "", "   "] {
            assert!(split_skill_path("skills cat", bad).is_err(), "{bad:?} must be refused");
        }
    }

    /// A traversal attempt must survive encoding intact so the SERVER refuses it.
    /// Silently mangling it here would hide the attempt from the server's log.
    #[test]
    fn traversal_shapes_are_encoded_not_swallowed() {
        let (name, rel) = split_skill_path("skills cat", "cron/../../etc/passwd").unwrap();
        assert_eq!(name, "cron");
        assert_eq!(rel, "../../etc/passwd");
        let encoded = urlencode(&rel);
        assert!(!encoded.contains('/'), "separators must be escaped: {encoded}");
        // `.` is unreserved (RFC 3986) so `..` stays literal; only the separators
        // are escaped. Either way the traversal arrives intact and the SERVER
        // refuses it — mangling it here would hide the attempt from its log.
        assert!(
            encoded.starts_with("..%2F..%2F"),
            "the traversal must reach the server intact: {encoded}"
        );
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(urlencode("references/notes.md"), "references%2Fnotes.md");
        assert!(!urlencode("a b&c=1").contains(' '));
        assert!(!urlencode("a b&c=1").contains('&'));
    }
}
