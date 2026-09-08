//! E2E for the agent-facing `aioncore conversation` CLI: the real binary against
//! the real app router served on a loopback port. Covers the success path, the
//! `caller_is_team` rejection (exit 3 + envelope), and the runtime-free
//! `capabilities` / local-contract errors (exit 2).

mod common;

use std::process::Stdio;

use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use common::build_app;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::process::Command;

const USER: &str = "system_default_user";

fn conversation_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aioncore"));
    command.arg("conversation");
    command
}

async fn serve(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

async fn insert_caller(services: &aionui_app::AppServices, id: &str, extra: &str) {
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES (?, ?, 'caller', 'acp', ?, 'finished', 1, 1)",
    )
    .bind(id)
    .bind(USER)
    .bind(extra)
    .execute(services.database.pool())
    .await
    .unwrap();
}

async fn run_create(base_url: &str, conversation_id: &str, token: &str, stdin: &str) -> std::process::Output {
    let mut child = conversation_command()
        .arg("create")
        .env("AIONUI_BASE_URL", base_url)
        .env("AIONUI_USER_ID", USER)
        .env("AIONUI_CONVERSATION_ID", conversation_id)
        .env("AIONUI_RUNTIME_TOKEN", token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(stdin.as_bytes()).await.unwrap();
    drop(child.stdin.take());
    child.wait_with_output().await.unwrap()
}

#[tokio::test]
async fn create_through_the_binary_returns_the_new_conversation_id() {
    let (app, services) = build_app().await;
    let workspace = std::env::temp_dir().join("aionui-conversation-cli-e2e");
    std::fs::create_dir_all(&workspace).unwrap();
    insert_caller(
        &services,
        "conv-cli-caller",
        &serde_json::json!({ "workspace": workspace, "backend": "claude" }).to_string(),
    )
    .await;
    let token = services
        .runtime_token_service
        .issue(
            USER,
            "conv-cli-caller",
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            [RuntimeTokenScope::ConversationHelper],
        )
        .token;
    let (base_url, handle) = serve(app).await;

    let output = run_create(&base_url, "conv-cli-caller", &token, r#"{ "name": "重构鉴权模块" }"#).await;
    handle.abort();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], true);
    assert_eq!(stdout["meta"]["command"], "conversation create");
    assert_eq!(stdout["data"]["name"], "重构鉴权模块");
    assert_eq!(
        stdout["data"]["workspace"],
        serde_json::json!(workspace.to_string_lossy())
    );
    assert!(stdout["data"]["id"].as_str().is_some_and(|id| !id.is_empty()));
    services.database.close().await;
}

#[tokio::test]
async fn a_team_caller_gets_exit_3_and_the_caller_is_team_envelope() {
    let (app, services) = build_app().await;
    insert_caller(&services, "conv-cli-team", r#"{"teamId":"team-1"}"#).await;
    let token = services
        .runtime_token_service
        .issue(
            USER,
            "conv-cli-team",
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            [RuntimeTokenScope::ConversationHelper],
        )
        .token;
    let (base_url, handle) = serve(app).await;

    let output = run_create(&base_url, "conv-cli-team", &token, r#"{ "name": "x" }"#).await;
    handle.abort();

    assert_eq!(output.status.code(), Some(3));
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], false);
    assert_eq!(stdout["error"]["code"], "caller_is_team");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("CONVERSATION_CLI_HTTP_STATUS_ERROR command=conversation create"),
        "{stderr}"
    );
    assert!(!stderr.contains(&token), "the token must never reach stderr");
    services.database.close().await;
}

#[tokio::test]
async fn capabilities_needs_no_runtime_env() {
    let output = conversation_command()
        .arg("capabilities")
        .env_remove("AIONUI_BASE_URL")
        .env_remove("AIONUI_USER_ID")
        .env_remove("AIONUI_CONVERSATION_ID")
        .env_remove("AIONUI_RUNTIME_TOKEN")
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["data"]["contract"], "agent-facing-conversation-cli");
    assert_eq!(stdout["data"]["tools"][0]["cli_command"], serde_json::json!(["create"]));
}

#[tokio::test]
async fn missing_runtime_env_is_exit_2_transport_unavailable() {
    let output = conversation_command()
        .arg("create")
        .env_remove("AIONUI_BASE_URL")
        .env_remove("AIONUI_USER_ID")
        .env_remove("AIONUI_CONVERSATION_ID")
        .env_remove("AIONUI_RUNTIME_TOKEN")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["error"]["code"], "transport_unavailable");
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("CONVERSATION_CLI_ENV_MISSING command=conversation create")
    );
}

#[tokio::test]
async fn an_unknown_subcommand_is_a_structured_envelope_not_clap_help() {
    let output = conversation_command().arg("delete").output().await.unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["error"]["code"], "schema_validation_failed");
    assert_eq!(stdout["meta"]["command"], "conversation delete");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Usage:"));
}
