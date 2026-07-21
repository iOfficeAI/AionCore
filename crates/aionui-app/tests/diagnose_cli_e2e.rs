//! E2E coverage for the agent-facing `aioncore diagnose` CLI.

use axum::extract::{Path, State};
use axum::routing::get;
use serde_json::json;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::process::Command;

#[derive(Debug, Default)]
struct Capture {
    conversation_ids: Vec<String>,
}

type SharedCapture = Arc<Mutex<Capture>>;

fn diagnose_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aioncore"));
    command.arg("diagnose");
    command
}

async fn fake_health() -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "status": "ok",
        "version": "9.9.9-test",
        "build_time": "2026-07-08T00:00:00Z"
    }))
}

async fn fake_conversations() -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": {
            "items": [
                {
                    "id": "conv-running",
                    "name": "Running conversation",
                    "type": "acp",
                    "status": "running",
                    "runtime": {
                        "state": "running",
                        "is_processing": true
                    }
                },
                {
                    "id": "conv-idle",
                    "name": "Idle conversation",
                    "type": "aionrs",
                    "status": "idle"
                }
            ],
            "next_cursor": null
        }
    }))
}

async fn fake_conversation(
    State(capture): State<SharedCapture>,
    Path(id): Path<String>,
) -> axum::Json<serde_json::Value> {
    capture.lock().unwrap().conversation_ids.push(id.clone());
    axum::Json(json!({
        "success": true,
        "data": {
            "id": id,
            "name": "Current conversation",
            "type": "acp",
            "status": "running",
            "runtime": {
                "state": "running",
                "task_status": "running",
                "is_processing": true,
                "turn_id": "turn-1",
                "pending_confirmations": 0
            },
            "assistant": {
                "id": "assistant-current",
                "name": "Current Assistant"
            }
        }
    }))
}

async fn fake_messages(Path(id): Path<String>) -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": {
            "items": [
                {
                    "id": "msg-1",
                    "conversation_id": id,
                    "msg_id": "msg-1",
                    "type": "assistant",
                    "status": "error",
                    "content": { "text": "tool failed" },
                    "created_at": 1
                }
            ],
            "next_cursor": null
        }
    }))
}

async fn fake_providers() -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": [
            {
                "id": "provider-openai",
                "name": "OpenAI",
                "platform": "openai",
                "enabled": true,
                "base_url": "https://api.openai.example",
                "api_key": "sk-provider-secret",
                "model_health": {
                    "gpt-4.1": {
                        "status": "healthy",
                        "latency": 123,
                        "last_check": 1
                    },
                    "gpt-5": {
                        "status": "unhealthy",
                        "error": "401 invalid api key"
                    }
                }
            }
        ]
    }))
}

async fn fake_mcp_servers() -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": [
            {
                "id": "mcp-empty",
                "name": "Empty MCP",
                "enabled": true,
                "builtin": false,
                "transport": {
                    "type": "http",
                    "headers": {
                        "Authorization": "Bearer MCP_SECRET"
                    }
                },
                "tools": []
            }
        ]
    }))
}

async fn fake_cron_jobs() -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": [
            {
                "id": "cron-failing",
                "name": "Failing cron",
                "enabled": true,
                "last_status": "error",
                "last_error": "boom"
            },
            {
                "id": "cron-ok",
                "name": "OK cron",
                "enabled": true,
                "last_status": "success"
            }
        ]
    }))
}

async fn fake_teams() -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": [
            {
                "id": "team-1",
                "name": "Support Team",
                "agents": [
                    {
                        "name": "Reviewer",
                        "role": "review",
                        "backend": "codex",
                        "conversation_id": "member-conv"
                    }
                ]
            }
        ]
    }))
}

async fn spawn_diagnose_probe_server(capture: SharedCapture) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route("/health", get(fake_health))
        .route("/api/conversations", get(fake_conversations))
        .route("/api/conversations/{id}", get(fake_conversation))
        .route("/api/conversations/{id}/messages", get(fake_messages))
        .route("/api/providers", get(fake_providers))
        .route("/api/mcp/servers", get(fake_mcp_servers))
        .route("/api/cron/jobs", get(fake_cron_jobs))
        .route("/api/teams", get(fake_teams))
        .with_state(capture);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn diagnose_capabilities_prints_agent_readable_contract_without_runtime_env() {
    let output = diagnose_command()
        .arg("capabilities")
        .env_remove("AIONUI_BASE_URL")
        .env_remove("AIONUI_CONVERSATION_ID")
        .env_remove("AIONUI_USER_ID")
        .output()
        .await
        .unwrap();

    assert!(
        output.status.success(),
        "diagnose capabilities failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], true);
    assert_eq!(stdout["meta"]["schema_version"], 1);
    assert_eq!(stdout["data"]["contract"], "agent-facing-diagnose-cli");
    assert_eq!(stdout["data"]["input"]["default_mode"], "stdin_json");

    let commands = stdout["data"]["domains"]
        .as_array()
        .expect("domains should be an array")
        .iter()
        .flat_map(|domain| domain["commands"].as_array().into_iter().flatten())
        .collect::<Vec<_>>();
    assert!(commands.iter().any(|command| command["command"] == "diagnose overview"));
    assert!(
        commands
            .iter()
            .any(|command| command["command"] == "diagnose conversations get")
    );
    let http_get = commands
        .iter()
        .find(|command| command["command"] == "diagnose http get")
        .expect("http get escape hatch should be advertised");
    assert_eq!(http_get["escape_hatch"], true);
    assert_eq!(http_get["destructive"], false);
}

#[tokio::test]
async fn diagnose_health_reads_backend_health_from_runtime_base_url() {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let (base_url, handle) = spawn_diagnose_probe_server(capture).await;

    let output = diagnose_command()
        .arg("health")
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-health")
        .env("AIONUI_USER_ID", "user-health")
        .output()
        .await
        .unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "diagnose health failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["data"]["status"], "ok");
    assert_eq!(stdout["data"]["version"], "9.9.9-test");
}

#[tokio::test]
async fn diagnose_conversation_get_resolves_current_conversation_selector_and_adds_stuck_hint() {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let (base_url, handle) = spawn_diagnose_probe_server(capture.clone()).await;

    let mut child = diagnose_command()
        .args(["conversations", "get"])
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-current")
        .env("AIONUI_USER_ID", "user-current")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{ "conversation_id": "current" }"#)
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "diagnose conversation get failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        capture.lock().unwrap().conversation_ids,
        vec!["conv-current".to_owned()]
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["data"]["id"], "conv-current");
    assert_eq!(stdout["data"]["runtime"]["state"], "running");
    assert!(
        stdout["data"]["stuck_hint"]
            .as_str()
            .unwrap()
            .contains("repeated checks")
    );
    assert_eq!(stdout["meta"]["resolved_selectors"]["conversation_id"], "conv-current");
}

#[tokio::test]
async fn diagnose_http_get_is_get_only_redacted_escape_hatch_for_api_paths() {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let (base_url, handle) = spawn_diagnose_probe_server(capture).await;

    let mut child = diagnose_command()
        .args(["http", "get"])
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-http")
        .env("AIONUI_USER_ID", "user-http")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{ "path": "/api/providers", "reason": "inspect provider health" }"#)
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "diagnose http get failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout_text = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout_text.contains("sk-provider-secret"));
    assert!(!stdout_text.contains("MCP_SECRET"));
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["data"][0]["api_key"]["redacted"], true);
    assert_eq!(stdout["meta"]["path"], "/api/providers");
    assert_eq!(stdout["meta"]["escape_hatch"], true);
}

#[tokio::test]
async fn diagnose_http_get_rejects_paths_outside_health_and_api() {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let (base_url, handle) = spawn_diagnose_probe_server(capture).await;

    let mut child = diagnose_command()
        .args(["http", "get"])
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-http")
        .env("AIONUI_USER_ID", "user-http")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{ "path": "/metrics", "reason": "not allowed" }"#)
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    handle.abort();
    assert!(!output.status.success(), "diagnose http get should reject /metrics");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(
        "DIAGNOSE_PAYLOAD_INVALID command=\"diagnose http get\" field=\"path\": path must start with /health or /api/"
    ));
}

#[tokio::test]
async fn diagnose_overview_aggregates_common_failure_signals() {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let (base_url, handle) = spawn_diagnose_probe_server(capture).await;

    let output = diagnose_command()
        .arg("overview")
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-overview")
        .env("AIONUI_USER_ID", "user-overview")
        .output()
        .await
        .unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "diagnose overview failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["data"]["health"]["status"], "ok");
    assert_eq!(stdout["data"]["providers"]["count"], 1);
    assert_eq!(stdout["data"]["providers"]["unhealthy"][0]["model"], "gpt-5");
    assert_eq!(stdout["data"]["mcp"]["enabled_but_no_tools"][0]["id"], "mcp-empty");
    assert_eq!(stdout["data"]["cron"]["failing"][0]["id"], "cron-failing");
    assert_eq!(stdout["data"]["running_conversations"][0]["id"], "conv-running");
}

#[tokio::test]
async fn diagnose_logs_tail_reads_latest_aioncore_log_and_filters_errors() {
    let temp = tempfile::TempDir::new().unwrap();
    let log_dir = temp.path().join("2026").join("07").join("08");
    std::fs::create_dir_all(&log_dir).unwrap();
    std::fs::write(
        log_dir.join("2026-07-08.aioncore.log"),
        r#"{"level":"INFO","message":"boot","conversation_id":"conv-log"}
{"level":"WARN","message":"warn line","conversation_id":"conv-log"}
{"level":"ERROR","message":"other conversation","conversation_id":"other"}
{"level":"ERROR","message":"target failure","conversation_id":"conv-log","api_key":"sk-log-secret"}
"#,
    )
    .unwrap();

    let mut child = diagnose_command()
        .args(["logs", "tail"])
        .env("AIONUI_LOG_DIR", temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{ "lines": 10, "errors_only": true, "conversation_id": "conv-log" }"#)
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    assert!(
        output.status.success(),
        "diagnose logs tail failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout_text = String::from_utf8_lossy(&output.stdout);
    assert!(stdout_text.contains("target failure"));
    assert!(!stdout_text.contains("sk-log-secret"));
    assert!(!stdout_text.contains("boot"));
    assert!(!stdout_text.contains("other conversation"));
}

#[test]
fn builtin_troubleshooting_skill_uses_diagnose_cli_not_python_helper() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/builtin-skills/aionui-troubleshooting");
    let skill = std::fs::read_to_string(root.join("SKILL.md")).unwrap();

    for forbidden in ["python3", "aion_diag.py", "lsof", "ps -", "curl"] {
        assert!(
            !skill.contains(forbidden),
            "aionui-troubleshooting skill must not mention {forbidden}"
        );
    }
    assert!(skill.contains("\"$AIONUI_HELPER_BIN\" diagnose capabilities"));
    assert!(skill.contains("\"$AIONUI_HELPER_BIN\" diagnose overview"));
    assert!(skill.contains("\"$AIONUI_HELPER_BIN\" diagnose conversations get"));
    assert!(skill.contains("\"$AIONUI_HELPER_BIN\" diagnose http get"));
}
