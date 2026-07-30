//! End-to-end WebSocket integration tests through the full app stack.
//!
//! Tests exercise real JWT auth, token extraction from HTTP headers,
//! message routing, broadcast/unicast, and connection lifecycle.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::WebSocketMessage;
use aionui_app::{AppConfig, AppServices, create_router};
use aionui_realtime::{EventBroadcaster, WebSocketManager};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

struct TestApp {
    addr: SocketAddr,
    services: AppServices,
}

async fn start_app() -> TestApp {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let router = create_router(&services).await.expect("build router");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    TestApp { addr, services }
}

/// Seed an active Core user and sign a valid JWT for it.
///
/// The WS handshake resolves the token's user against the users table
/// (active row + matching session generation), so a bare signed token for
/// a nonexistent user is rejected with a realtime auth error.
async fn sign_token(app: &TestApp, user_id: &str) -> String {
    sqlx::query(
        "INSERT OR IGNORE INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES (?, ?, 'hash', 0, 0)",
    )
    .bind(user_id)
    .bind(user_id)
    .execute(app.services.database.pool())
    .await
    .unwrap();
    app.services.jwt_service.sign(user_id, "testuser").unwrap()
}

/// Connect with an Authorization: Bearer header.
async fn connect_bearer(
    addr: SocketAddr,
    token: &str,
) -> (
    futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        tungstenite::Message,
    >,
    futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    >,
) {
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
    ws.split()
}

/// Connect with a Cookie header.
async fn connect_cookie(
    addr: SocketAddr,
    token: &str,
) -> (
    futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        tungstenite::Message,
    >,
    futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    >,
) {
    let url = format!("ws://{addr}/ws");
    let request = tungstenite::http::Request::builder()
        .uri(&url)
        .header("Host", addr.to_string())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key())
        .header("Cookie", format!("aionui-session={token}"))
        .body(())
        .unwrap();

    let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws.split()
}

/// Connect with Sec-WebSocket-Protocol header (token as subprotocol).
async fn connect_protocol(
    addr: SocketAddr,
    token: &str,
) -> (
    futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        tungstenite::Message,
    >,
    futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    >,
) {
    let url = format!("ws://{addr}/ws");
    let request = tungstenite::http::Request::builder()
        .uri(&url)
        .header("Host", addr.to_string())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key())
        .header("Sec-WebSocket-Protocol", token)
        .body(())
        .unwrap();

    let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws.split()
}

/// Connect with no auth headers at all.
async fn connect_no_auth(
    addr: SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://{addr}/ws");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws
}

/// Read the next text message within a timeout, returning parsed JSON.
async fn read_text<S>(stream: &mut S) -> Value
where
    S: StreamExt<Item = Result<tungstenite::Message, tungstenite::Error>> + Unpin,
{
    let timeout = Duration::from_secs(5);
    tokio::time::timeout(timeout, async {
        loop {
            match stream.next().await {
                Some(Ok(tungstenite::Message::Text(t))) => {
                    return serde_json::from_str::<Value>(&t).unwrap();
                }
                Some(Ok(tungstenite::Message::Close(_))) => {
                    panic!("unexpected close frame while reading text");
                }
                Some(Err(e)) => {
                    panic!("read error: {e}");
                }
                None => {
                    panic!("stream ended");
                }
                _ => continue,
            }
        }
    })
    .await
    .expect("read_text timed out")
}

/// Read until a close frame is received, returning the close code.
async fn read_close<S>(stream: &mut S) -> Option<u16>
where
    S: StreamExt<Item = Result<tungstenite::Message, tungstenite::Error>> + Unpin,
{
    let timeout = Duration::from_secs(5);
    tokio::time::timeout(timeout, async {
        loop {
            match stream.next().await {
                Some(Ok(tungstenite::Message::Close(frame))) => {
                    return frame.map(|f| f.code.into());
                }
                Some(Ok(_)) => continue,
                Some(Err(_)) => return None,
                None => return None,
            }
        }
    })
    .await
    .expect("read_close timed out")
}

fn send_json(text: &str) -> tungstenite::Message {
    tungstenite::Message::Text(text.into())
}

fn assert_realtime_error(msg: &Value, code: &str, recoverable: bool) {
    assert_eq!(msg["name"], "realtime.error");
    assert_eq!(msg["data"]["code"], code);
    assert!(msg["data"]["message"].as_str().is_some());
    assert_eq!(msg["data"]["recoverable"], recoverable);
    assert!(msg["data"]["details"].is_object());
}

fn assert_invalid_message_error(msg: &Value) {
    assert_realtime_error(msg, "REALTIME_INVALID_MESSAGE", true);
    assert_eq!(
        msg["data"]["details"]["expected"],
        r#"{ "name": "event-name", "data": {...} }"#
    );
}

/// Wait (bounded) until the manager reports exactly `expected` clients.
///
/// Registration happens after the HTTP upgrade completes, so a fixed
/// post-connect sleep races under parallel test load; poll instead.
async fn wait_for_clients(app: &TestApp, expected: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if ws_manager(app).client_count() == expected {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            assert_eq!(
                ws_manager(app).client_count(),
                expected,
                "timed out waiting for websocket client count"
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn ws_manager(app: &TestApp) -> &Arc<WebSocketManager> {
    &app.services.ws_manager
}

// ===========================================================================
// T1 — Connection establishment and authentication
// ===========================================================================

#[tokio::test]
async fn t1_1_valid_bearer_token_connects() {
    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    let (_tx, _rx) = connect_bearer(app.addr, &token).await;
    wait_for_clients(&app, 1).await;
}

#[tokio::test]
async fn t1_2_no_token_closes_1008() {
    let app = start_app().await;
    let mut ws = connect_no_auth(app.addr).await;

    let msg = read_text(&mut ws).await;
    assert_realtime_error(&msg, "REALTIME_AUTH_MISSING", false);

    let code = read_close(&mut ws).await;
    assert_eq!(code, Some(1008));
}

#[tokio::test]
async fn t1_3_invalid_token_sends_auth_expired_then_closes() {
    let app = start_app().await;

    let (_, mut rx) = connect_bearer(app.addr, "invalid-token").await;

    let msg = read_text(&mut rx).await;
    assert_realtime_error(&msg, "REALTIME_AUTH_EXPIRED", false);

    let code = read_close(&mut rx).await;
    assert_eq!(code, Some(1008));
}

#[tokio::test]
async fn t1_4_token_from_cookie() {
    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    let (_tx, _rx) = connect_cookie(app.addr, &token).await;
    wait_for_clients(&app, 1).await;
}

#[tokio::test]
async fn t1_5_token_from_sec_websocket_protocol() {
    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    let (_tx, _rx) = connect_protocol(app.addr, &token).await;
    wait_for_clients(&app, 1).await;
}

// ===========================================================================
// T3 — Message format
// ===========================================================================

#[tokio::test]
async fn t3_1_valid_json_with_no_registered_route_returns_unsupported_error() {
    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    let (mut tx, mut rx) = connect_bearer(app.addr, &token).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let msg = json!({"name": "some-event", "data": {"key": "value"}});
    tx.send(send_json(&msg.to_string())).await.unwrap();

    let msg = read_text(&mut rx).await;
    assert_realtime_error(&msg, "REALTIME_UNSUPPORTED_MESSAGE", true);
}

#[tokio::test]
async fn t3_2_invalid_json_returns_error() {
    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    let (mut tx, mut rx) = connect_bearer(app.addr, &token).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    tx.send(send_json("not valid json")).await.unwrap();

    let msg = read_text(&mut rx).await;
    assert_invalid_message_error(&msg);
}

#[tokio::test]
async fn t3_3_missing_fields_returns_error() {
    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    let (mut tx, mut rx) = connect_bearer(app.addr, &token).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    tx.send(send_json(r#"{"foo": "bar"}"#)).await.unwrap();

    let msg = read_text(&mut rx).await;
    assert_invalid_message_error(&msg);
}

// ===========================================================================
// T4 — Event broadcast and unicast
// ===========================================================================

#[tokio::test]
async fn t4_1_broadcast_reaches_all_clients() {
    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    let (_, mut rx1) = connect_bearer(app.addr, &token).await;
    let (_, mut rx2) = connect_bearer(app.addr, &token).await;
    wait_for_clients(&app, 2).await;

    let event = WebSocketMessage::new("test-broadcast", json!({"seq": 1}));
    ws_manager(&app).broadcast_all(event);

    let msg1 = read_text(&mut rx1).await;
    let msg2 = read_text(&mut rx2).await;

    assert_eq!(msg1["name"], "test-broadcast");
    assert_eq!(msg2["name"], "test-broadcast");
}

#[tokio::test]
async fn t4_1_scoped_event_reaches_only_matching_user() {
    let app = start_app().await;
    let token_a = sign_token(&app, "user-a").await;
    let token_b = sign_token(&app, "user-b").await;

    let (_, mut rx_a) = connect_bearer(app.addr, &token_a).await;
    let (_, mut rx_b) = connect_bearer(app.addr, &token_b).await;
    wait_for_clients(&app, 2).await;
    assert_eq!(ws_manager(&app).client_count_for_user("user-a"), 1);
    assert_eq!(ws_manager(&app).client_count_for_user("user-b"), 1);

    app.services.event_bus.broadcast(WebSocketMessage::new(
        "scoped-broadcast",
        json!({"user_id": "user-a", "seq": 1}),
    ));

    let msg_a = read_text(&mut rx_a).await;
    assert_eq!(msg_a["name"], "scoped-broadcast");
    assert_eq!(msg_a["data"]["user_id"], "user-a");

    let timeout_result = tokio::time::timeout(Duration::from_millis(200), rx_b.next()).await;
    assert!(timeout_result.is_err(), "user-b should not receive user-a event");
}

#[tokio::test]
async fn t4_1_unscoped_business_event_is_dropped_by_bridge() {
    let app = start_app().await;
    let token = sign_token(&app, "user-a").await;

    let (_, mut rx) = connect_bearer(app.addr, &token).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    app.services.event_bus.broadcast(WebSocketMessage::new(
        "conversation.listChanged",
        json!({"conversation_id": "c1"}),
    ));

    let timeout_result = tokio::time::timeout(Duration::from_millis(200), rx.next()).await;
    assert!(timeout_result.is_err(), "unscoped business event should be dropped");
}

#[tokio::test]
async fn t4_1_whitelisted_global_event_reaches_all_users() {
    let app = start_app().await;
    let token_a = sign_token(&app, "user-a").await;
    let token_b = sign_token(&app, "user-b").await;

    let (_, mut rx_a) = connect_bearer(app.addr, &token_a).await;
    let (_, mut rx_b) = connect_bearer(app.addr, &token_b).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    app.services.event_bus.broadcast(WebSocketMessage::new(
        "runtime.statusChanged",
        json!({"phase": "ready"}),
    ));

    let msg_a = read_text(&mut rx_a).await;
    let msg_b = read_text(&mut rx_b).await;
    assert_eq!(msg_a["name"], "runtime.statusChanged");
    assert_eq!(msg_b["name"], "runtime.statusChanged");

    app.services.event_bus.broadcast(WebSocketMessage::new(
        "hub.state-changed",
        json!({"name": "demo-extension", "status": "installed"}),
    ));

    let msg_a = read_text(&mut rx_a).await;
    let msg_b = read_text(&mut rx_b).await;
    assert_eq!(msg_a["name"], "hub.state-changed");
    assert_eq!(msg_b["name"], "hub.state-changed");
}

#[tokio::test]
async fn t4_2_unicast_reaches_only_target() {
    use aionui_realtime::ConnectionId;

    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    let (_, mut rx1) = connect_bearer(app.addr, &token).await;
    let (_, mut rx2) = connect_bearer(app.addr, &token).await;
    wait_for_clients(&app, 2).await;

    let first_conn_id = ConnectionId(1);
    let msg = WebSocketMessage::new("unicast-test", json!({"target": true}));
    ws_manager(&app).send_to(first_conn_id, msg);

    let received = read_text(&mut rx1).await;
    assert_eq!(received["name"], "unicast-test");

    let timeout_result = tokio::time::timeout(Duration::from_millis(200), rx2.next()).await;
    assert!(timeout_result.is_err(), "rx2 should not receive the unicast");
}

#[tokio::test]
async fn t4_3_broadcast_after_disconnect_no_error() {
    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    let (mut tx1, _rx1) = connect_bearer(app.addr, &token).await;
    let (_, mut rx2) = connect_bearer(app.addr, &token).await;
    wait_for_clients(&app, 2).await;

    // Disconnect client 1
    tx1.send(tungstenite::Message::Close(None)).await.unwrap();
    wait_for_clients(&app, 1).await;

    // Broadcast — should not error even though client 1 is gone
    let event = WebSocketMessage::new("after-disconnect", json!({}));
    ws_manager(&app).broadcast_all(event);

    let msg = read_text(&mut rx2).await;
    assert_eq!(msg["name"], "after-disconnect");
}

// ===========================================================================
// T5 — Built-in message handling
// ===========================================================================

#[tokio::test]
async fn t5_1_pong_does_not_generate_response() {
    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    let (mut tx, mut rx) = connect_bearer(app.addr, &token).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let pong = json!({"name": "pong", "data": {}});
    tx.send(send_json(&pong.to_string())).await.unwrap();

    let timeout_result = tokio::time::timeout(Duration::from_millis(200), rx.next()).await;
    assert!(timeout_result.is_err(), "pong should not generate a response");
}

#[tokio::test]
async fn t5_2_subscribe_show_open_file_mode() {
    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    let (mut tx, mut rx) = connect_bearer(app.addr, &token).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let payload = json!({
        "name": "subscribe-show-open",
        "data": {"id": "req-file", "data": {"properties": ["openFile"]}}
    });
    tx.send(send_json(&payload.to_string())).await.unwrap();

    let msg = read_text(&mut rx).await;
    assert_eq!(msg["name"], "show-open-request");
    assert_eq!(msg["data"]["id"], "req-file");
    assert_eq!(msg["data"]["isFileMode"], true);
    assert_eq!(msg["data"]["properties"], json!(["openFile"]));
}

#[tokio::test]
async fn t5_3_subscribe_show_open_directory_mode() {
    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    let (mut tx, mut rx) = connect_bearer(app.addr, &token).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let payload = json!({
        "name": "subscribe-show-open",
        "data": {"id": "req-dir", "data": {"properties": ["openDirectory"]}}
    });
    tx.send(send_json(&payload.to_string())).await.unwrap();

    let msg = read_text(&mut rx).await;
    assert_eq!(msg["name"], "show-open-request");
    assert_eq!(msg["data"]["id"], "req-dir");
    assert_eq!(msg["data"]["isFileMode"], false);
}

#[tokio::test]
async fn t5_4_subscribe_show_open_mixed_mode() {
    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    let (mut tx, mut rx) = connect_bearer(app.addr, &token).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let payload = json!({
        "name": "subscribe-show-open",
        "data": {"id": "req-mixed", "data": {"properties": ["openFile", "openDirectory"]}}
    });
    tx.send(send_json(&payload.to_string())).await.unwrap();

    let msg = read_text(&mut rx).await;
    assert_eq!(msg["name"], "show-open-request");
    assert_eq!(msg["data"]["id"], "req-mixed");
    assert_eq!(msg["data"]["isFileMode"], false);
}

// ===========================================================================
// T6 — Connection close
// ===========================================================================

#[tokio::test]
async fn t6_1_client_close_removes_from_manager() {
    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    let (mut tx, _rx) = connect_bearer(app.addr, &token).await;
    wait_for_clients(&app, 1).await;

    tx.send(tungstenite::Message::Close(None)).await.unwrap();
    wait_for_clients(&app, 0).await;
}

// ===========================================================================
// T7 — Concurrent connections
// ===========================================================================

#[tokio::test]
async fn t7_1_multiple_concurrent_connections() {
    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    let mut handles = Vec::new();
    for _ in 0..10 {
        let addr = app.addr;
        let tok = token.clone();
        handles.push(tokio::spawn(async move { connect_bearer(addr, &tok).await }));
    }

    let mut connections = Vec::new();
    for h in handles {
        connections.push(h.await.unwrap());
    }

    wait_for_clients(&app, 10).await;
}

// ===========================================================================
// T7.2 — Blacklisted token rejected
// ===========================================================================

#[tokio::test]
async fn t7_2_blacklisted_token_rejected() {
    let app = start_app().await;
    let token = sign_token(&app, "user1").await;

    // Blacklist the token
    app.services.jwt_service.blacklist_token(&token);

    let (_, mut rx) = connect_bearer(app.addr, &token).await;

    let msg = read_text(&mut rx).await;
    assert_realtime_error(&msg, "REALTIME_AUTH_EXPIRED", false);

    let code = read_close(&mut rx).await;
    assert_eq!(code, Some(1008));
}
