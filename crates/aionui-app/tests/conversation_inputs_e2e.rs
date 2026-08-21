//! Route-level coverage for conversation inputs and capabilities.
//!
//! Covers unauthenticated access, CSRF, cross-user isolation, idempotent
//! client keys, illegal mode/status transitions, content/attachment bounds,
//! and WebSocket delivery only to the owning user.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite;
use tower::ServiceExt;

use common::{
    body_json, build_app_with_mock_agents, delete_with_token, get_request, get_with_token, json_with_token,
    setup_and_login,
};

fn create_body(name: &str, agent_type: &str) -> serde_json::Value {
    json!({
        "type": agent_type,
        "name": name,
        "extra": {}
    })
}

fn followup_body(content: &str, client_key: &str) -> serde_json::Value {
    json!({
        "mode": "followup",
        "content": content,
        "client_key": client_key,
    })
}

async fn create_conversation(app: &mut axum::Router, token: &str, csrf: &str, name: &str, agent_type: &str) -> String {
    let req = json_with_token("POST", "/api/conversations", create_body(name, agent_type), token, csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    body_json(resp).await["data"]["id"].as_str().unwrap().to_owned()
}

async fn submit_input(
    app: &mut axum::Router,
    token: &str,
    csrf: &str,
    conversation_id: &str,
    body: serde_json::Value,
) -> axum::response::Response {
    app.clone()
        .oneshot(json_with_token(
            "POST",
            &format!("/api/conversations/{conversation_id}/inputs"),
            body,
            token,
            csrf,
        ))
        .await
        .unwrap()
}

fn json_with_token_no_csrf(method: &str, uri: &str, body: serde_json::Value, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

async fn connect_bearer(
    addr: SocketAddr,
    token: &str,
) -> futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    let url = format!("ws://{addr}/ws");
    let request = tungstenite::http::Request::builder()
        .uri(&url)
        .header("Host", addr.to_string())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key())
        .header("Authorization", format!("Bearer {token}"))
        .body(())
        .unwrap();
    let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws.split().1
}

async fn read_named_event(
    stream: &mut (impl StreamExt<Item = Result<tungstenite::Message, tungstenite::Error>> + Unpin),
    name: &str,
) -> serde_json::Value {
    let timeout = Duration::from_secs(5);
    tokio::time::timeout(timeout, async {
        loop {
            match stream.next().await {
                Some(Ok(tungstenite::Message::Text(text))) => {
                    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                    if value["name"] == name {
                        return value;
                    }
                }
                Some(Ok(tungstenite::Message::Close(_))) => panic!("websocket closed before {name}"),
                Some(Err(error)) => panic!("websocket read failed: {error}"),
                None => panic!("websocket ended before {name}"),
                _ => continue,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {name}"))
}

#[tokio::test]
async fn list_inputs_requires_auth() {
    let (app, _services) = build_app_with_mock_agents().await;
    let resp = app
        .oneshot(get_request("/api/conversations/conv-1/inputs"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(resp).await["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn submit_input_requires_auth() {
    let (app, _services) = build_app_with_mock_agents().await;
    let csrf = "csrf-test";
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/conversations/conv-1/inputs")
                .header("content-type", "application/json")
                .header("x-csrf-token", csrf)
                .header("cookie", format!("aionui-csrf-token={csrf}"))
                .body(Body::from(
                    serde_json::to_vec(&followup_body("hello", "key-1")).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(resp).await["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn cancel_input_requires_auth() {
    let (app, _services) = build_app_with_mock_agents().await;
    let csrf = "csrf-test";
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/conversations/conv-1/inputs/input-1")
                .header("x-csrf-token", csrf)
                .header("cookie", format!("aionui-csrf-token={csrf}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(resp).await["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn capabilities_requires_auth() {
    let (app, _services) = build_app_with_mock_agents().await;
    let resp = app
        .oneshot(get_request("/api/conversations/conv-1/capabilities"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(resp).await["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn submit_input_requires_csrf() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let id = create_conversation(&mut app, &token, &csrf, "Queue", "acp").await;

    let resp = app
        .oneshot(json_with_token_no_csrf(
            "POST",
            &format!("/api/conversations/{id}/inputs"),
            followup_body("hello", "key-1"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn submit_input_rejects_mismatched_csrf() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let id = create_conversation(&mut app, &token, &csrf, "Queue", "acp").await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/conversations/{id}/inputs"))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .header("x-csrf-token", "wrong-csrf")
                .header("cookie", format!("aionui-csrf-token={csrf}"))
                .body(Body::from(
                    serde_json::to_vec(&followup_body("hello", "key-1")).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn cancel_input_requires_csrf() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let id = create_conversation(&mut app, &token, &csrf, "Queue", "acp").await;
    let created = submit_input(&mut app, &token, &csrf, &id, followup_body("hello", "key-1")).await;
    let input_id = body_json(created).await["data"]["input"]["input_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/conversations/{id}/inputs/{input_id}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn list_inputs_and_capabilities_skip_csrf() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let id = create_conversation(&mut app, &token, &csrf, "Queue", "acp").await;

    let list = app
        .clone()
        .oneshot(get_with_token(&format!("/api/conversations/{id}/inputs"), &token))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);

    let capabilities = app
        .oneshot(get_with_token(&format!("/api/conversations/{id}/capabilities"), &token))
        .await
        .unwrap();
    assert_eq!(capabilities.status(), StatusCode::OK);
    let json = body_json(capabilities).await;
    assert_eq!(json["data"]["followup"], true);
    assert_eq!(json["data"]["steer"], false);
    assert_eq!(json["data"]["inject"], false);
}

#[tokio::test]
async fn other_user_cannot_read_or_mutate_inputs() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token_a, csrf_a) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let (token_b, csrf_b) = setup_and_login(&mut app, &services, "bob", "StrongP@ss2").await;
    let id = create_conversation(&mut app, &token_a, &csrf_a, "Owner Queue", "acp").await;
    let created = submit_input(&mut app, &token_a, &csrf_a, &id, followup_body("hello", "key-1")).await;
    let input_id = body_json(created).await["data"]["input"]["input_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let list = app
        .clone()
        .oneshot(get_with_token(&format!("/api/conversations/{id}/inputs"), &token_b))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::NOT_FOUND);

    let capabilities = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/conversations/{id}/capabilities"),
            &token_b,
        ))
        .await
        .unwrap();
    assert_eq!(capabilities.status(), StatusCode::NOT_FOUND);

    let submit = submit_input(&mut app, &token_b, &csrf_b, &id, followup_body("intrude", "key-b")).await;
    assert_eq!(submit.status(), StatusCode::NOT_FOUND);

    let cancel = app
        .oneshot(delete_with_token(
            &format!("/api/conversations/{id}/inputs/{input_id}"),
            &token_b,
            &csrf_b,
        ))
        .await
        .unwrap();
    assert_eq!(cancel.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn duplicate_client_key_is_idempotent_for_the_same_payload() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let id = create_conversation(&mut app, &token, &csrf, "Queue", "acp").await;
    let body = followup_body("same payload", "stable-key");

    let first = submit_input(&mut app, &token, &csrf, &id, body.clone()).await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let first_id = body_json(first).await["data"]["input"]["input_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let second = submit_input(&mut app, &token, &csrf, &id, body).await;
    assert_eq!(second.status(), StatusCode::ACCEPTED);
    assert_eq!(
        body_json(second).await["data"]["input"]["input_id"].as_str().unwrap(),
        first_id
    );
}

#[tokio::test]
async fn duplicate_client_key_rejects_a_different_payload() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let id = create_conversation(&mut app, &token, &csrf, "Queue", "acp").await;

    let first = submit_input(&mut app, &token, &csrf, &id, followup_body("first", "reuse-key")).await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);

    let second = submit_input(&mut app, &token, &csrf, &id, followup_body("changed", "reuse-key")).await;
    assert_eq!(second.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(second).await["code"], "BAD_REQUEST");
}

#[tokio::test]
async fn submit_input_rejects_empty_content_and_client_key() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let id = create_conversation(&mut app, &token, &csrf, "Queue", "acp").await;

    let empty_content = submit_input(&mut app, &token, &csrf, &id, followup_body("   ", "key-1")).await;
    assert_eq!(empty_content.status(), StatusCode::BAD_REQUEST);

    let empty_key = submit_input(&mut app, &token, &csrf, &id, followup_body("hello", "")).await;
    assert_eq!(empty_key.status(), StatusCode::BAD_REQUEST);

    let oversized_key = submit_input(&mut app, &token, &csrf, &id, followup_body("hello", &"k".repeat(257))).await;
    assert_eq!(oversized_key.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn submit_input_rejects_invalid_files_and_keeps_valid_attachments() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let id = create_conversation(&mut app, &token, &csrf, "Queue", "acp").await;

    let invalid = submit_input(
        &mut app,
        &token,
        &csrf,
        &id,
        json!({
            "mode": "followup",
            "content": "with files",
            "client_key": "bad-files",
            "files": "not-an-array",
        }),
    )
    .await;
    assert!(
        invalid.status() == StatusCode::BAD_REQUEST || invalid.status() == StatusCode::UNPROCESSABLE_ENTITY,
        "unexpected status {}",
        invalid.status()
    );

    let valid = submit_input(
        &mut app,
        &token,
        &csrf,
        &id,
        json!({
            "mode": "followup",
            "content": "with files",
            "client_key": "good-files",
            "files": [{ "kind": "upload", "path": "/tmp/report.txt" }],
        }),
    )
    .await;
    assert_eq!(valid.status(), StatusCode::ACCEPTED);
    let json = body_json(valid).await;
    assert_eq!(json["data"]["input"]["files"][0]["kind"], "upload");
    assert_eq!(json["data"]["input"]["files"][0]["path"], "/tmp/report.txt");
}

#[tokio::test]
async fn unsupported_steer_and_inject_fail_closed_on_acp() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let id = create_conversation(&mut app, &token, &csrf, "ACP Queue", "acp").await;

    let steer = submit_input(
        &mut app,
        &token,
        &csrf,
        &id,
        json!({
            "mode": "steer",
            "content": "steer now",
            "client_key": "steer-1",
        }),
    )
    .await;
    assert_eq!(steer.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(steer).await["code"], "capability_unsupported");

    let inject = submit_input(
        &mut app,
        &token,
        &csrf,
        &id,
        json!({
            "mode": "inject",
            "content": "inject now",
            "client_key": "inject-1",
        }),
    )
    .await;
    assert_eq!(inject.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(inject).await["code"], "capability_unsupported");
}

#[tokio::test]
async fn cancel_rejects_an_already_terminal_input() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let id = create_conversation(&mut app, &token, &csrf, "Queue", "acp").await;
    let created = submit_input(&mut app, &token, &csrf, &id, followup_body("hello", "cancel-me")).await;
    assert_eq!(created.status(), StatusCode::ACCEPTED);
    let input_id = body_json(created).await["data"]["input"]["input_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let first = app
        .clone()
        .oneshot(delete_with_token(
            &format!("/api/conversations/{id}/inputs/{input_id}"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    if first.status() == StatusCode::OK {
        let second = app
            .oneshot(delete_with_token(
                &format!("/api/conversations/{id}/inputs/{input_id}"),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::CONFLICT);
        assert_eq!(body_json(second).await["code"], "CONFLICT");
    } else {
        assert_eq!(first.status(), StatusCode::CONFLICT);
        assert_eq!(body_json(first).await["code"], "CONFLICT");
    }
}

#[tokio::test]
async fn list_inputs_caps_terminal_records() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let id = create_conversation(&mut app, &token, &csrf, "Queue", "acp").await;

    for index in 1..=3 {
        let created = submit_input(
            &mut app,
            &token,
            &csrf,
            &id,
            followup_body(&format!("msg-{index}"), &format!("key-{index}")),
        )
        .await;
        assert_eq!(created.status(), StatusCode::ACCEPTED);
        let input_id = body_json(created).await["data"]["input"]["input_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let _ = app
            .clone()
            .oneshot(delete_with_token(
                &format!("/api/conversations/{id}/inputs/{input_id}"),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
    }

    let listed = app
        .oneshot(get_with_token(
            &format!("/api/conversations/{id}/inputs?terminal_limit=1"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let items = body_json(listed).await["data"].as_array().cloned().unwrap_or_default();
    let terminal = items
        .iter()
        .filter(|item| matches!(item["status"].as_str(), Some("applied" | "canceled" | "failed")))
        .count();
    assert!(terminal <= 1, "terminal_limit=1 leaked {terminal} terminal records");
}

#[tokio::test]
async fn input_changed_events_reach_only_the_owning_user() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_app = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, serve_app).await.unwrap();
    });

    let (token_a, csrf_a) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let (token_b, _csrf_b) = setup_and_login(&mut app, &services, "bob", "StrongP@ss2").await;
    let id = create_conversation(&mut app, &token_a, &csrf_a, "Owner Queue", "acp").await;

    let mut rx_a = connect_bearer(addr, &token_a).await;
    let mut rx_b = connect_bearer(addr, &token_b).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let created = submit_input(&mut app, &token_a, &csrf_a, &id, followup_body("hello", "ws-key")).await;
    assert_eq!(created.status(), StatusCode::ACCEPTED);

    let event = read_named_event(&mut rx_a, "conversation.inputChanged").await;
    assert_eq!(event["data"]["input"]["content"], "hello");

    let leaked = tokio::time::timeout(Duration::from_millis(300), rx_b.next()).await;
    assert!(leaked.is_err(), "non-owner received a conversation.inputChanged event");
}
