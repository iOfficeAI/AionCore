//! E2E coverage for the agent-facing `aioncore team` CLI fallback.

use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

fn team_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aioncore"));
    command.arg("team");
    command
}

#[tokio::test]
async fn team_capabilities_prints_contract_without_runtime_env() {
    let output = team_command()
        .arg("capabilities")
        .env_remove("AIONUI_BASE_URL")
        .env_remove("AIONUI_CONVERSATION_ID")
        .env_remove("AIONUI_USER_ID")
        .env_remove("AIONUI_RUNTIME_TOKEN")
        .output()
        .await
        .unwrap();

    assert!(
        output.status.success(),
        "team capabilities failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], true);
    assert_eq!(stdout["data"]["contract"], "agent-facing-team-cli");
    assert_eq!(stdout["data"]["tools"].as_array().unwrap().len(), 10);
    let spawn = stdout["data"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "team_spawn_agent")
        .unwrap();
    assert_eq!(spawn["lead_only"], true);
    assert!(spawn["stdin_json_schema"]["properties"]["assistant_id"].is_object());
}

#[tokio::test]
async fn team_help_prints_markdown_without_runtime_env() {
    let output = team_command()
        .arg("help")
        .env_remove("AIONUI_BASE_URL")
        .env_remove("AIONUI_CONVERSATION_ID")
        .env_remove("AIONUI_USER_ID")
        .env_remove("AIONUI_RUNTIME_TOKEN")
        .output()
        .await
        .unwrap();

    assert!(output.status.success());
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], true);
    assert_eq!(stdout["data"]["format"], "markdown");
    assert!(stdout["data"]["text"].as_str().unwrap().contains("team send-message"));
}

#[tokio::test]
async fn tool_command_rejects_forged_identity_fields_before_http_call() {
    let mut child = team_command()
        .args(["send-message"])
        .env("AIONUI_BASE_URL", "http://127.0.0.1:9")
        .env("AIONUI_CONVERSATION_ID", "conv-1")
        .env("AIONUI_USER_ID", "user-1")
        .env("AIONUI_RUNTIME_TOKEN", "token-1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{"to":"worker-1","message":"hi","team_id":"team-1","slot_id":"lead-1","role":"lead"}"#)
        .await
        .unwrap();
    let output = child.wait_with_output().await.unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("TEAM_CLI_SCHEMA_VALIDATION_FAILED"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], false);
    assert_eq!(stdout["error"]["code"], "schema_validation_failed");
    assert!(stdout["error"]["details"]["expected_schema"].is_object());
}

#[tokio::test]
async fn team_context_requires_runtime_env_and_prints_json_error() {
    let output = team_command()
        .arg("context")
        .env_remove("AIONUI_BASE_URL")
        .env_remove("AIONUI_CONVERSATION_ID")
        .env_remove("AIONUI_USER_ID")
        .env_remove("AIONUI_RUNTIME_TOKEN")
        .output()
        .await
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("TEAM_CLI_ENV_MISSING"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], false);
    assert_eq!(stdout["error"]["code"], "runtime_context_missing");
    assert_eq!(stdout["meta"]["command"], "team context");
}

#[tokio::test]
async fn unknown_team_command_returns_json_error_envelope() {
    let output = team_command().arg("does-not-exist").output().await.unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("TEAM_CLI_UNKNOWN_COMMAND"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], false);
    assert_eq!(stdout["error"]["code"], "unknown_tool");
    assert_eq!(stdout["meta"]["command"], "team does-not-exist");
}
