//! E2E coverage for the agent-facing `aioncore config` CLI.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post, put};
use serde_json::json;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::process::Command;

#[derive(Debug, Default)]
struct Capture {
    conversation_id: String,
    user_id: String,
    job_id: Option<String>,
    resource_id: Option<String>,
    payload: Option<serde_json::Value>,
}

type SharedCapture = Arc<Mutex<Option<Capture>>>;

fn config_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aioncore"));
    command.arg("config");
    command
}

async fn fake_context_conversation(Path(id): Path<String>) -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": {
            "id": id,
            "name": "Current conversation",
            "assistant": {
                "id": "assistant-current",
                "source": "user",
                "name": "Current Assistant",
                "avatar": "",
                "backend": "codex"
            },
            "extra": {}
        }
    }))
}

async fn fake_conversation_rename(
    State(capture): State<SharedCapture>,
    Path(id): Path<String>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    *capture.lock().unwrap() = Some(Capture {
        resource_id: Some(id.clone()),
        payload: Some(payload),
        ..Capture::default()
    });
    axum::Json(json!({
        "success": true,
        "data": {
            "id": id,
            "name": "Renamed conversation",
            "extra": {}
        }
    }))
}

async fn fake_assistant_rule_read(
    State(capture): State<SharedCapture>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    *capture.lock().unwrap() = Some(Capture {
        payload: Some(payload),
        ..Capture::default()
    });
    axum::Json(json!({
        "success": true,
        "data": "current rule"
    }))
}

async fn fake_assistant_rule_write(
    axum::Json(_payload): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": true
    }))
}

async fn fake_conversation_cron_list(
    State(capture): State<SharedCapture>,
    headers: HeaderMap,
) -> axum::Json<serde_json::Value> {
    let mut capture = capture.lock().unwrap();
    if capture.is_none() {
        *capture = Some(Capture {
            conversation_id: header(&headers, "x-aionui-conversation-id"),
            user_id: header(&headers, "x-aionui-user-id"),
            job_id: None,
            resource_id: None,
            payload: None,
        });
    }
    axum::Json(json!({
        "success": true,
        "data": []
    }))
}

async fn fake_conversation_cron_update(
    State(capture): State<SharedCapture>,
    Path(job_id): Path<String>,
    headers: HeaderMap,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    *capture.lock().unwrap() = Some(Capture {
        conversation_id: header(&headers, "x-aionui-conversation-id"),
        user_id: header(&headers, "x-aionui-user-id"),
        job_id: Some(job_id),
        resource_id: None,
        payload: Some(payload),
    });
    axum::Json(json!({
        "success": true,
        "data": { "id": "cron_current_1", "name": "Updated task" }
    }))
}

async fn fake_mcp_server_get(Path(server_id): Path<String>) -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": {
            "id": server_id,
            "name": "MCP",
            "transport": {
                "type": "http",
                "url": "https://mcp.example.test",
                "headers": {
                    "Authorization": "Bearer SECRET_MCP_HEADER"
                }
            }
        }
    }))
}

async fn fake_mcp_server_update(
    State(capture): State<SharedCapture>,
    Path(server_id): Path<String>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    *capture.lock().unwrap() = Some(Capture {
        resource_id: Some(server_id.clone()),
        payload: Some(payload),
        ..Capture::default()
    });
    axum::Json(json!({
        "success": true,
        "data": {
            "id": server_id,
            "name": "Updated MCP",
            "transport": {
                "type": "http",
                "url": "https://mcp.example.test",
                "headers": {
                    "Authorization": "Bearer SECRET_MCP_HEADER"
                }
            }
        }
    }))
}

async fn fake_mcp_oauth_check_status(
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": {
            "server_url": payload["server_url"],
            "authenticated": true
        }
    }))
}

async fn fake_mcp_oauth_logout(
    State(capture): State<SharedCapture>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    *capture.lock().unwrap() = Some(Capture {
        payload: Some(payload),
        ..Capture::default()
    });
    axum::Json(json!({
        "success": true,
        "data": null
    }))
}

async fn fake_provider_create(axum::Json(_payload): axum::Json<serde_json::Value>) -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": {
            "id": "provider-openai",
            "name": "OpenAI",
            "api_key": "sk-provider-secret",
            "models": []
        }
    }))
}

async fn fake_provider_list() -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": [{
            "id": "provider-openai",
            "name": "OpenAI",
            "api_key": "sk-provider-secret",
            "models": []
        }]
    }))
}

async fn fake_provider_update(
    State(capture): State<SharedCapture>,
    Path(provider_id): Path<String>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    *capture.lock().unwrap() = Some(Capture {
        resource_id: Some(provider_id.clone()),
        payload: Some(payload),
        ..Capture::default()
    });
    axum::Json(json!({
        "success": true,
        "data": {
            "id": provider_id,
            "name": "Updated OpenAI",
            "api_key": "sk-provider-secret",
            "models": []
        }
    }))
}

async fn fake_external_paths_list() -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": [{
            "name": "Team Skills",
            "path": "/skills/team"
        }]
    }))
}

async fn fake_external_paths_add(
    State(capture): State<SharedCapture>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    *capture.lock().unwrap() = Some(Capture {
        payload: Some(payload),
        ..Capture::default()
    });
    axum::Json(json!({
        "success": true,
        "data": null
    }))
}

async fn fake_agent_custom_update(
    State(capture): State<SharedCapture>,
    Path(agent_id): Path<String>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    *capture.lock().unwrap() = Some(Capture {
        resource_id: Some(agent_id.clone()),
        payload: Some(payload),
        ..Capture::default()
    });
    axum::Json(json!({
        "success": true,
        "data": {
            "id": agent_id,
            "name": "Updated Agent"
        }
    }))
}

async fn fake_agent_management_list() -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": [{
            "id": "agent/custom",
            "name": "Updated Agent"
        }]
    }))
}

async fn fake_cron_job_skill_save(
    State(capture): State<SharedCapture>,
    Path(job_id): Path<String>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    *capture.lock().unwrap() = Some(Capture {
        job_id: Some(job_id),
        payload: Some(payload),
        ..Capture::default()
    });
    axum::Json(json!({
        "success": true,
        "data": null
    }))
}

async fn fake_cron_job_skill_get(Path(job_id): Path<String>) -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": {
            "job_id": job_id,
            "has_skill": true
        }
    }))
}

async fn fake_cron_job_create(
    State(capture): State<SharedCapture>,
    headers: HeaderMap,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    *capture.lock().unwrap() = Some(Capture {
        conversation_id: header(&headers, "x-aionui-conversation-id"),
        user_id: header(&headers, "x-aionui-user-id"),
        payload: Some(payload),
        ..Capture::default()
    });
    axum::Json(json!({
        "success": true,
        "data": {
            "id": "cron-global-1"
        }
    }))
}

async fn fake_cron_jobs_list() -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "success": true,
        "data": [{
            "id": "cron-global-1",
            "name": "Selector task"
        }]
    }))
}

fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

async fn spawn_config_probe_server(capture: SharedCapture) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            "/api/conversations/{id}",
            get(fake_context_conversation).patch(fake_conversation_rename),
        )
        .route("/api/skills/assistant-rule/read", post(fake_assistant_rule_read))
        .route("/api/skills/assistant-rule/write", post(fake_assistant_rule_write))
        .route("/api/internal/conversation-cron/list", get(fake_conversation_cron_list))
        .route(
            "/api/internal/conversation-cron/jobs/{job_id}",
            put(fake_conversation_cron_update),
        )
        .route(
            "/api/mcp/servers/{server_id}",
            get(fake_mcp_server_get).put(fake_mcp_server_update),
        )
        .route("/api/mcp/oauth/check-status", post(fake_mcp_oauth_check_status))
        .route("/api/mcp/oauth/logout", post(fake_mcp_oauth_logout))
        .route("/api/providers", get(fake_provider_list).post(fake_provider_create))
        .route("/api/providers/{provider_id}", put(fake_provider_update))
        .route(
            "/api/skills/external-paths",
            get(fake_external_paths_list).post(fake_external_paths_add),
        )
        .route("/api/agents/management", get(fake_agent_management_list))
        .route("/api/agents/custom/{agent_id}", put(fake_agent_custom_update))
        .route("/api/cron/jobs", get(fake_cron_jobs_list).post(fake_cron_job_create))
        .route(
            "/api/cron/jobs/{job_id}/skill",
            get(fake_cron_job_skill_get).post(fake_cron_job_skill_save),
        )
        .with_state(capture);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn config_mcp_oauth_logout_reads_status_before_and_after_write() {
    let capture = Arc::new(Mutex::new(None));
    let (base_url, handle) = spawn_config_probe_server(capture.clone()).await;

    let mut child = config_command()
        .args(["mcp", "oauth", "logout"])
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-mcp")
        .env("AIONUI_USER_ID", "user-mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{ "server_url": "https://mcp.example.test" }"#)
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "mcp oauth logout failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = capture
        .lock()
        .unwrap()
        .take()
        .expect("server should receive oauth logout");
    assert_eq!(captured.payload.unwrap()["server_url"], "https://mcp.example.test");
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["meta"]["before"]["authenticated"], true);
    assert_eq!(stdout["meta"]["after"]["authenticated"], true);
}

#[tokio::test]
async fn config_capabilities_prints_agent_readable_contract_without_runtime_env() {
    let output = config_command()
        .arg("capabilities")
        .env_remove("AIONUI_BASE_URL")
        .env_remove("AIONUI_CONVERSATION_ID")
        .env_remove("AIONUI_USER_ID")
        .output()
        .await
        .unwrap();

    assert!(
        output.status.success(),
        "config capabilities failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "capabilities should not need runtime env, stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], true);
    assert_eq!(stdout["meta"]["schema_version"], 1);
    assert_eq!(stdout["data"]["contract"], "agent-facing-config-cli");
    assert_eq!(stdout["data"]["input"]["default_mode"], "stdin_json");
    assert_eq!(
        stdout["data"]["input"]["selectors"]["assistant_id"]["current"],
        "resolve via AIONUI_CONVERSATION_ID"
    );

    let domains = stdout["data"]["domains"]
        .as_array()
        .expect("domains should be an array");
    let assistant_rule_write = domains
        .iter()
        .flat_map(|domain| domain["commands"].as_array().into_iter().flatten())
        .find(|command| command["command"] == "config assistants rule write")
        .expect("assistant rule write should be advertised");
    assert_eq!(assistant_rule_write["input"], "stdin_json");
    assert_eq!(assistant_rule_write["readback"], true);
    assert_eq!(assistant_rule_write["selectors"], json!(["assistant_id"]));

    let cron_current_update = domains
        .iter()
        .flat_map(|domain| domain["commands"].as_array().into_iter().flatten())
        .find(|command| command["command"] == "config cron current update")
        .expect("cron current update should be advertised");
    assert_eq!(cron_current_update["input"], "stdin_json");
    assert_eq!(cron_current_update["readback"], true);
    assert_eq!(cron_current_update["stdin_fields"], json!(["job_id"]));
}

#[tokio::test]
async fn config_provider_update_reads_collection_before_and_after_write() {
    let capture = Arc::new(Mutex::new(None));
    let (base_url, handle) = spawn_config_probe_server(capture.clone()).await;

    let mut child = config_command()
        .args(["providers", "update"])
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-provider")
        .env("AIONUI_USER_ID", "user-provider")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{ "provider_id": "provider-openai", "api_key": "sk-input-secret" }"#)
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "provider update failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = capture
        .lock()
        .unwrap()
        .take()
        .expect("server should receive provider update");
    assert_eq!(captured.resource_id.as_deref(), Some("provider-openai"));
    let payload = captured.payload.unwrap();
    assert!(payload.get("provider_id").is_none());
    let stdout_text = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout_text.contains("sk-input-secret"));
    assert!(!stdout_text.contains("sk-provider-secret"));
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(stdout["meta"]["before"].is_array());
    assert!(stdout["meta"]["after"].is_array());
}

#[tokio::test]
async fn config_conversation_rename_patches_name_and_reads_resource_before_and_after() {
    let capture = Arc::new(Mutex::new(None));
    let (base_url, handle) = spawn_config_probe_server(capture.clone()).await;

    let mut child = config_command()
        .args(["conversation", "rename"])
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-current")
        .env("AIONUI_USER_ID", "user-rename")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{ "conversation_id": "conv-target", "name": "New Title" }"#)
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "conversation rename failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = capture
        .lock()
        .unwrap()
        .take()
        .expect("server should receive conversation rename");
    // The id selector is extracted from the payload and moved into the path,
    // leaving only the update body forwarded to PATCH.
    assert_eq!(captured.resource_id.as_deref(), Some("conv-target"));
    let payload = captured.payload.unwrap();
    assert_eq!(payload.get("name").and_then(|v| v.as_str()), Some("New Title"));
    assert!(payload.get("conversation_id").is_none());
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(stdout["meta"]["before"].is_object());
    assert!(stdout["meta"]["after"].is_object());
}

#[tokio::test]
async fn config_external_paths_add_reads_collection_before_and_after_write() {
    let capture = Arc::new(Mutex::new(None));
    let (base_url, handle) = spawn_config_probe_server(capture.clone()).await;

    let mut child = config_command()
        .args(["skills", "external-paths", "add"])
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-skill")
        .env("AIONUI_USER_ID", "user-skill")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{ "name": "Team Skills", "path": "/skills/team" }"#)
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "external path add failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(stdout["meta"]["before"].is_array());
    assert!(stdout["meta"]["after"].is_array());
}

#[tokio::test]
async fn config_payload_commands_resolve_current_conversation_and_user_selectors() {
    let capture = Arc::new(Mutex::new(None));
    let (base_url, handle) = spawn_config_probe_server(capture.clone()).await;

    let mut child = config_command()
        .args(["cron", "jobs", "create"])
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-selector")
        .env("AIONUI_USER_ID", "user-selector")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            br#"{
  "name": "Selector task",
  "conversation_id": "current",
  "user_id": "current",
  "created_by": "user",
  "message": "Run selector task"
}"#,
        )
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "cron job create failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = capture
        .lock()
        .unwrap()
        .take()
        .expect("server should receive cron create");
    assert_eq!(captured.conversation_id, "conv-selector");
    assert_eq!(captured.user_id, "user-selector");
    let payload = captured.payload.unwrap();
    assert_eq!(payload["conversation_id"], "conv-selector");
    assert_eq!(payload["user_id"], "user-selector");
    assert_eq!(payload["created_by"], "user");
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["meta"]["resolved_selectors"]["conversation_id"], "conv-selector");
    assert_eq!(stdout["meta"]["resolved_selectors"]["user_id"], "user-selector");
}

#[tokio::test]
async fn config_mcp_server_update_reads_server_id_from_stdin_and_redacts_metadata() {
    let capture = Arc::new(Mutex::new(None));
    let (base_url, handle) = spawn_config_probe_server(capture.clone()).await;

    let mut child = config_command()
        .args(["mcp", "servers", "update"])
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-mcp")
        .env("AIONUI_USER_ID", "user-mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            br#"{
  "server_id": "server/one",
  "transport": {
    "type": "http",
    "url": "https://mcp.example.test",
    "headers": {
      "Authorization": "Bearer INPUT_SECRET"
    }
  }
}"#,
        )
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "mcp server update failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = capture
        .lock()
        .unwrap()
        .take()
        .expect("server should receive mcp update");
    assert_eq!(captured.resource_id.as_deref(), Some("server/one"));
    let payload = captured.payload.unwrap();
    assert!(payload.get("server_id").is_none());
    let stdout_text = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout_text.contains("SECRET_MCP_HEADER"));
    assert!(!stdout_text.contains("INPUT_SECRET"));
}

#[tokio::test]
async fn config_provider_create_redacts_api_key_from_stdout() {
    let capture = Arc::new(Mutex::new(None));
    let (base_url, handle) = spawn_config_probe_server(capture).await;

    let mut child = config_command()
        .args(["providers", "create"])
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-provider")
        .env("AIONUI_USER_ID", "user-provider")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{ "name": "OpenAI", "api_key": "sk-input-secret" }"#)
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "provider create failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout_text = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout_text.contains("sk-provider-secret"));
    assert!(!stdout_text.contains("sk-input-secret"));
}

#[tokio::test]
async fn config_agent_custom_update_reads_agent_id_from_stdin() {
    let capture = Arc::new(Mutex::new(None));
    let (base_url, handle) = spawn_config_probe_server(capture.clone()).await;

    let mut child = config_command()
        .args(["agents", "custom", "update"])
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-agent")
        .env("AIONUI_USER_ID", "user-agent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{ "agent_id": "agent/custom", "name": "Updated Agent", "command": "agent-cli" }"#)
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "agent custom update failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = capture
        .lock()
        .unwrap()
        .take()
        .expect("server should receive agent update");
    assert_eq!(captured.resource_id.as_deref(), Some("agent/custom"));
    let payload = captured.payload.unwrap();
    assert!(payload.get("agent_id").is_none());
}

#[tokio::test]
async fn config_cron_job_skill_save_reads_job_id_from_stdin() {
    let capture = Arc::new(Mutex::new(None));
    let (base_url, handle) = spawn_config_probe_server(capture.clone()).await;

    let mut child = config_command()
        .args(["cron", "jobs", "skill", "save"])
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-cron")
        .env("AIONUI_USER_ID", "user-cron")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br##"{ "job_id": "cron/job", "content": "# Cron skill" }"##)
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "cron job skill save failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = capture
        .lock()
        .unwrap()
        .take()
        .expect("server should receive cron skill save");
    assert_eq!(captured.job_id.as_deref(), Some("cron/job"));
    let payload = captured.payload.unwrap();
    assert!(payload.get("job_id").is_none());
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["meta"]["before"]["has_skill"], true);
    assert_eq!(stdout["meta"]["after"]["has_skill"], true);
}

#[tokio::test]
async fn config_context_resolves_current_conversation_assistant() {
    let capture = Arc::new(Mutex::new(None));
    let (base_url, handle) = spawn_config_probe_server(capture).await;

    let output = config_command()
        .arg("context")
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-current")
        .env("AIONUI_USER_ID", "user-current")
        .output()
        .await
        .unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "config context failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], true);
    assert_eq!(stdout["data"]["user_id"], "user-current");
    assert_eq!(stdout["data"]["conversation_id"], "conv-current");
    assert_eq!(stdout["data"]["assistant"]["id"], "assistant-current");
    assert_eq!(stdout["meta"]["schema_version"], 1);
}

#[tokio::test]
async fn config_assistant_rule_read_resolves_current_assistant_selector() {
    let capture = Arc::new(Mutex::new(None));
    let (base_url, handle) = spawn_config_probe_server(capture.clone()).await;

    let mut child = config_command()
        .args(["assistants", "rule", "read"])
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
        .write_all(br#"{ "assistant_id": "current", "locale": "zh-CN" }"#)
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "assistant rule read failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = capture
        .lock()
        .unwrap()
        .take()
        .expect("server should receive assistant rule payload");
    let payload = captured.payload.unwrap();
    assert_eq!(payload["assistant_id"], "assistant-current");
    assert_eq!(payload["locale"], "zh-CN");
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["data"], "current rule");
    assert_eq!(
        stdout["meta"]["resolved_selectors"]["assistant_id"],
        "assistant-current"
    );
}

#[tokio::test]
async fn config_assistant_rule_write_redacts_rule_content_from_metadata() {
    let capture = Arc::new(Mutex::new(None));
    let (base_url, handle) = spawn_config_probe_server(capture).await;

    let mut child = config_command()
        .args(["assistants", "rule", "write"])
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
        .write_all(
            br#"{
  "assistant_id": "current",
  "locale": "zh-CN",
  "content": "SECRET_RULE_BODY"
}"#,
        )
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "assistant rule write failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout_text = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout_text.contains("SECRET_RULE_BODY"));
    assert!(!stdout_text.contains("current rule"));
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["data"], true);
    assert_eq!(stdout["meta"]["before"]["redacted"], true);
    assert_eq!(stdout["meta"]["after"]["redacted"], true);
}

#[tokio::test]
async fn config_cron_current_update_reads_job_id_from_stdin_and_sends_runtime_headers() {
    let capture = Arc::new(Mutex::new(None));
    let (base_url, handle) = spawn_config_probe_server(capture.clone()).await;

    let mut child = config_command()
        .args(["cron", "current", "update"])
        .env("AIONUI_BASE_URL", &base_url)
        .env("AIONUI_CONVERSATION_ID", "conv-cron")
        .env("AIONUI_USER_ID", "user-cron")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            br#"{
  "job_id": "cron_current_1",
  "name": "Updated task",
  "schedule": "0 18 * * MON-FRI",
  "schedule_description": "Weekdays at 6:00 PM",
  "message": "Produce a concise end-of-day summary."
}"#,
        )
        .await
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().await.unwrap();

    handle.abort();
    assert!(
        output.status.success(),
        "cron current update failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = capture
        .lock()
        .unwrap()
        .take()
        .expect("server should receive cron update");
    assert_eq!(captured.conversation_id, "conv-cron");
    assert_eq!(captured.user_id, "user-cron");
    assert_eq!(captured.job_id.as_deref(), Some("cron_current_1"));
    let payload = captured.payload.unwrap();
    assert_eq!(payload["name"], "Updated task");
    assert!(
        payload.get("job_id").is_none(),
        "job_id belongs to the CLI path selector"
    );
}

#[tokio::test]
async fn config_context_fails_with_stable_error_when_conversation_env_missing() {
    let capture = Arc::new(Mutex::new(None));
    let (base_url, handle) = spawn_config_probe_server(capture).await;

    let output = config_command()
        .arg("context")
        .env("AIONUI_BASE_URL", &base_url)
        .env_remove("AIONUI_CONVERSATION_ID")
        .env("AIONUI_USER_ID", "user-current")
        .output()
        .await
        .unwrap();

    handle.abort();
    assert!(
        !output.status.success(),
        "config context should require conversation id"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(
        "CONFIG_ENV_MISSING command=\"config context\" field=\"AIONUI_CONVERSATION_ID\": missing required environment variable"
    ));
}

#[test]
fn builtin_config_skills_use_config_cli_not_python_or_cron_helper() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/builtin-skills/auto-inject");
    let aionui_config = std::fs::read_to_string(root.join("aionui-config/SKILL.md")).unwrap();
    let cron = std::fs::read_to_string(root.join("cron/SKILL.md")).unwrap();

    for forbidden in ["python3", "aionui_api.py", "lsof", "netstat", "curl"] {
        assert!(
            !aionui_config.contains(forbidden),
            "aionui-config skill must not mention {forbidden}"
        );
    }
    assert!(aionui_config.contains("\"$AIONUI_HELPER_BIN\" config context"));
    assert!(aionui_config.contains("\"$AIONUI_HELPER_BIN\" config capabilities"));
    assert!(aionui_config.contains("assistant_id\": \"current"));
    for command in [
        "\"$AIONUI_HELPER_BIN\" config mcp servers",
        "\"$AIONUI_HELPER_BIN\" config providers",
        "\"$AIONUI_HELPER_BIN\" config settings",
        "\"$AIONUI_HELPER_BIN\" config agents",
        "\"$AIONUI_HELPER_BIN\" config cron jobs",
        "\"$AIONUI_HELPER_BIN\" config skills external-paths",
    ] {
        assert!(
            aionui_config.contains(command),
            "aionui-config skill must document {command}"
        );
    }

    assert!(!cron.contains("cron-helper"));
    assert!(cron.contains("\"$AIONUI_HELPER_BIN\" config cron current list"));
    assert!(cron.contains("\"job_id\""));
}
