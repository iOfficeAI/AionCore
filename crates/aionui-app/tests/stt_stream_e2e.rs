//! End-to-end tests for the `GET /api/stt/stream` WebSocket endpoint.
//!
//! Exercises the full app stack: auth middleware on the upgrade request,
//! preference-backed config loading, the streaming session protocol, and the
//! Deepgram upstream against a local mock WebSocket server (wiremock cannot
//! speak WebSocket, so the mock is a small tokio-tungstenite server,
//! mirroring `aionui-shell/tests/stt_stream_deepgram_integration.rs`).
//!
//! Hang-safety: every potentially blocking await is wrapped in a 5s timeout.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aionui_app::{AppConfig, AppServices, create_router};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Await with a deadline so a regression fails fast instead of hanging the suite.
async fn within<F: std::future::Future>(fut: F) -> F::Output {
    tokio::time::timeout(TEST_TIMEOUT, fut)
        .await
        .expect("timed out after 5s")
}

// ---------------------------------------------------------------------------
// App harness
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

/// Sign a JWT for the seeded system user — the auth middleware verifies the
/// token AND looks the user up in the DB, so the subject must exist.
fn sign_token(app: &TestApp) -> String {
    app.services.jwt_service.sign("system_default_user", "admin").unwrap()
}

/// Seed `tools.speechToText` through the same `ClientPrefService` mechanism
/// the settings API uses (same DB pool the route's service reads from).
async fn seed_stt_prefs(app: &TestApp, config: Value) {
    let repo = Arc::new(aionui_db::SqliteClientPreferenceRepository::new(
        app.services.database.pool().clone(),
    ));
    let service = aionui_system::ClientPrefService::new(repo);
    let mut req = aionui_api_types::UpdateClientPreferencesRequest::new();
    req.insert("tools.speechToText".to_owned(), config);
    service.update_preferences(req).await.unwrap();
}

type ClientWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn upgrade_request(addr: SocketAddr, token: Option<&str>) -> tungstenite::http::Request<()> {
    let mut builder = tungstenite::http::Request::builder()
        .uri(format!("ws://{addr}/api/stt/stream"))
        .header("Host", addr.to_string())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key());
    if let Some(token) = token {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    builder.body(()).unwrap()
}

async fn connect_stream(addr: SocketAddr, token: &str) -> ClientWs {
    let (ws, _) = within(tokio_tungstenite::connect_async(upgrade_request(addr, Some(token))))
        .await
        .expect("websocket handshake failed");
    ws
}

/// Read the next protocol frame (server frames are always JSON text).
async fn read_frame(ws: &mut ClientWs) -> Value {
    loop {
        match within(ws.next()).await {
            Some(Ok(Message::Text(text))) => return serde_json::from_str(text.as_str()).unwrap(),
            Some(Ok(Message::Close(frame))) => panic!("unexpected close frame: {frame:?}"),
            Some(Ok(_)) => continue, // ping/pong
            other => panic!("unexpected websocket read result: {other:?}"),
        }
    }
}

/// Expect the server to close the connection as the next event.
async fn read_until_close(ws: &mut ClientWs) {
    match within(ws.next()).await {
        Some(Ok(Message::Close(_))) | None => (),
        Some(Ok(other)) => panic!("expected close frame, got {other:?}"),
        Some(Err(_)) => (), // server dropped the socket after Close
    }
}

fn start_frame() -> Message {
    Message::Text(r#"{"type":"start","format":"pcm16","sampleRate":16000,"channels":1}"#.into())
}

// ---------------------------------------------------------------------------
// Mock Deepgram live server (minimal copy of the aionui-shell integration
// test pattern; duplicated here to avoid a cross-crate test dependency)
// ---------------------------------------------------------------------------

fn results_frame(transcript: &str, is_final: bool) -> Message {
    Message::Text(
        json!({
            "type": "Results",
            "is_final": is_final,
            "channel": { "alternatives": [{ "transcript": transcript, "confidence": 0.98 }] },
        })
        .to_string()
        .into(),
    )
}

/// Start a one-connection mock Deepgram WS server; returns its HTTP base URL
/// and the server task handle (await it to propagate in-handler panics).
async fn spawn_deepgram_mock<F, Fut>(handler: F) -> (String, tokio::task::JoinHandle<()>)
where
    F: FnOnce(WebSocketStream<TcpStream>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        handler(ws).await;
    });
    (format!("http://{addr}"), handle)
}

// ===========================================================================
// Tests
// ===========================================================================

// 1. Unauthenticated upgrade requests must be rejected at the handshake by
//    the same auth middleware that guards POST /api/stt (GET bypasses CSRF,
//    so the auth middleware's 401 is what reaches the client).
#[tokio::test]
async fn unauthenticated_handshake_is_rejected() {
    let app = start_app().await;

    let err = within(tokio_tungstenite::connect_async(upgrade_request(app.addr, None)))
        .await
        .expect_err("handshake must be rejected without auth");
    match err {
        tungstenite::Error::Http(response) => assert_eq!(response.status(), 401),
        other => panic!("expected HTTP rejection, got {other:?}"),
    }
}

// 1b. An invalid token must be rejected the same way.
#[tokio::test]
async fn invalid_token_handshake_is_rejected() {
    let app = start_app().await;

    let err = within(tokio_tungstenite::connect_async(upgrade_request(
        app.addr,
        Some("not-a-valid-token"),
    )))
    .await
    .expect_err("handshake must be rejected with a bogus token");
    match err {
        tungstenite::Error::Http(response) => assert_eq!(response.status(), 401),
        other => panic!("expected HTTP rejection, got {other:?}"),
    }
}

// 2. Authed connect with STT disabled in prefs: the session answers the start
//    frame with an STT_DISABLED error frame, then the server closes.
#[tokio::test]
async fn disabled_stt_yields_error_frame_then_close() {
    let app = start_app().await;
    seed_stt_prefs(&app, json!({ "enabled": false, "provider": "openai" })).await;
    let token = sign_token(&app);

    let mut ws = connect_stream(app.addr, &token).await;
    within(ws.send(start_frame())).await.unwrap();

    let frame = read_frame(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["code"], "STT_DISABLED");
    assert!(frame["msg"].as_str().is_some());

    read_until_close(&mut ws).await;
}

// 3. Full happy path against a mock Deepgram live server:
//    start → ready → binary audio → partial → final → stop → done → close.
#[tokio::test]
async fn deepgram_full_flow_streams_exact_frame_sequence() {
    let (base_url, mock) = spawn_deepgram_mock(|mut ws| async move {
        // The audio chunk must arrive as a raw binary frame with exact bytes.
        match ws.next().await {
            Some(Ok(Message::Binary(bytes))) => assert_eq!(bytes.as_ref(), &[1u8, 2, 3][..]),
            other => panic!("expected binary audio frame, got {other:?}"),
        }
        ws.send(results_frame("hel", false)).await.unwrap();
        ws.send(results_frame("hello", true)).await.unwrap();
        // `stop` must arrive as Deepgram's CloseStream control message.
        match ws.next().await {
            Some(Ok(Message::Text(text))) => assert_eq!(text.as_str(), r#"{"type":"CloseStream"}"#),
            other => panic!("expected CloseStream text frame, got {other:?}"),
        }
        ws.send(Message::Close(None)).await.unwrap();
    })
    .await;

    let app = start_app().await;
    seed_stt_prefs(
        &app,
        json!({
            "enabled": true,
            "provider": "deepgram",
            "deepgram": { "api_key": "dg-test-key", "base_url": base_url, "model": "nova-3" }
        }),
    )
    .await;
    let token = sign_token(&app);

    let mut ws = connect_stream(app.addr, &token).await;
    within(ws.send(start_frame())).await.unwrap();

    assert_eq!(read_frame(&mut ws).await, json!({ "type": "ready" }));

    within(ws.send(Message::Binary(vec![1u8, 2, 3].into()))).await.unwrap();
    assert_eq!(read_frame(&mut ws).await, json!({ "type": "partial", "text": "hel" }));
    assert_eq!(read_frame(&mut ws).await, json!({ "type": "final", "text": "hello" }));

    within(ws.send(Message::Text(r#"{"type":"stop"}"#.into())))
        .await
        .unwrap();
    assert_eq!(read_frame(&mut ws).await, json!({ "type": "done" }));

    read_until_close(&mut ws).await;
    within(mock).await.unwrap();
}

// 4. Protocol violation: a binary frame before `start` is rejected with
//    STT_STREAM_PROTOCOL before any config/upstream work happens.
#[tokio::test]
async fn binary_first_frame_is_protocol_error() {
    let app = start_app().await;
    let token = sign_token(&app);

    let mut ws = connect_stream(app.addr, &token).await;
    within(ws.send(Message::Binary(vec![9u8, 9, 9].into()))).await.unwrap();

    let frame = read_frame(&mut ws).await;
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["code"], "STT_STREAM_PROTOCOL");

    read_until_close(&mut ws).await;
}
