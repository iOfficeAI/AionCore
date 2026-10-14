//! Integration tests for the MCP OAuth discovery + dynamic client registration
//! chain, exercised against a fake authorization server running in-process.
//!
//! The browser leg of the authorization code flow cannot run headless, so what
//! is covered here is everything around it: RFC 9728 protected resource
//! metadata discovery, RFC 8414 authorization server metadata, RFC 7591 dynamic
//! client registration, and the refresh grant presenting the registered client
//! plus the RFC 8707 resource indicator.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use aionui_db::{IOAuthTokenRepository, SqliteOAuthTokenRepository, UpsertOAuthTokenParams};
use aionui_mcp::McpOAuthService;
use aionui_mcp::oauth_discovery;
use aionui_mcp::oauth_registration;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

const TEST_USER_ID: &str = "system_default_user";

// ---------------------------------------------------------------------------
// Fake authorization server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FakeServerState {
    base_url: String,
    /// When false the authorization server metadata omits
    /// `registration_endpoint`, i.e. a server that predates RFC 7591 support.
    supports_registration: bool,
    /// Registration responses fail with 400 when true.
    registration_fails: bool,
    registration_requests: Arc<Mutex<Vec<Value>>>,
    token_requests: Arc<Mutex<Vec<HashMap<String, String>>>>,
}

struct FakeServer {
    base_url: String,
    state: FakeServerState,
}

impl FakeServer {
    async fn start(supports_registration: bool, registration_fails: bool) -> Self {
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());

        let state = FakeServerState {
            base_url: base_url.clone(),
            supports_registration,
            registration_fails,
            registration_requests: Arc::new(Mutex::new(Vec::new())),
            token_requests: Arc::new(Mutex::new(Vec::new())),
        };

        let app = Router::new()
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                get(protected_resource_metadata),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(authorization_server_metadata),
            )
            .route("/oauth/register", post(register))
            .route("/oauth/token", post(token))
            .with_state(state.clone());

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self { base_url, state }
    }

    fn mcp_url(&self) -> String {
        format!("{}/mcp", self.base_url)
    }

    fn registration_requests(&self) -> Vec<Value> {
        self.state.registration_requests.lock().unwrap().clone()
    }

    fn token_requests(&self) -> Vec<HashMap<String, String>> {
        self.state.token_requests.lock().unwrap().clone()
    }
}

async fn protected_resource_metadata(State(state): State<FakeServerState>) -> Json<Value> {
    Json(json!({
        "resource": format!("{}/mcp", state.base_url),
        "authorization_servers": [state.base_url],
        "bearer_methods_supported": ["header"],
    }))
}

async fn authorization_server_metadata(State(state): State<FakeServerState>) -> Json<Value> {
    let mut metadata = json!({
        "issuer": state.base_url,
        "authorization_endpoint": format!("{}/oauth/authorize", state.base_url),
        "token_endpoint": format!("{}/oauth/token", state.base_url),
        "response_types_supported": ["code"],
        "code_challenge_methods_supported": ["S256"],
    });
    if state.supports_registration {
        metadata["registration_endpoint"] = json!(format!("{}/oauth/register", state.base_url));
    }
    Json(metadata)
}

async fn register(
    State(state): State<FakeServerState>,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    state.registration_requests.lock().unwrap().push(body.clone());

    if state.registration_fails {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_redirect_uri"})),
        );
    }

    (
        axum::http::StatusCode::CREATED,
        Json(json!({
            "client_id": "registered-client-id",
            "client_id_issued_at": 1_700_000_000,
            "redirect_uris": body.get("redirect_uris").cloned().unwrap_or(json!([])),
        })),
    )
}

async fn token(State(state): State<FakeServerState>, body: String) -> Json<Value> {
    let params: HashMap<String, String> = url_decode_form(&body);
    state.token_requests.lock().unwrap().push(params);

    Json(json!({
        "access_token": "refreshed-access-token",
        "token_type": "bearer",
        "expires_in": 3600,
        "refresh_token": "refreshed-refresh-token",
    }))
}

/// Minimal `application/x-www-form-urlencoded` parsing for the fake token endpoint.
fn url_decode_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (percent_decode(k), percent_decode(v)))
        .collect()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn make_service() -> (McpOAuthService, Arc<dyn IOAuthTokenRepository>) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let repo: Arc<dyn IOAuthTokenRepository> = Arc::new(SqliteOAuthTokenRepository::new(db.pool().clone()));
    let svc = McpOAuthService::new(repo.clone(), reqwest::Client::new());
    std::mem::forget(db);
    (svc, repo)
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discovery_follows_protected_resource_metadata_to_the_authorization_server() {
    let server = FakeServer::start(true, false).await;
    let http = reqwest::Client::new();

    let discovered = oauth_discovery::discover(&http, &server.mcp_url()).await.unwrap();

    assert_eq!(
        discovered.metadata.authorization_endpoint,
        format!("{}/oauth/authorize", server.base_url)
    );
    assert_eq!(
        discovered.metadata.token_endpoint,
        format!("{}/oauth/token", server.base_url)
    );
    assert_eq!(
        discovered.metadata.registration_endpoint,
        Some(format!("{}/oauth/register", server.base_url))
    );
    // RFC 8707 resource indicator comes from the protected resource metadata.
    assert_eq!(discovered.resource, Some(server.mcp_url()));
}

#[tokio::test]
async fn discovery_omits_registration_endpoint_when_the_server_does_not_support_it() {
    let server = FakeServer::start(false, false).await;
    let http = reqwest::Client::new();

    let discovered = oauth_discovery::discover(&http, &server.mcp_url()).await.unwrap();

    assert!(discovered.metadata.registration_endpoint.is_none());
}

#[tokio::test]
async fn discovery_fails_with_a_descriptive_error_when_nothing_is_published() {
    let http = reqwest::Client::new();
    // Port 1 on loopback: nothing listens there.
    let err = oauth_discovery::discover(&http, "http://127.0.0.1:1/mcp")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Failed to discover OAuth endpoints"));
}

// ---------------------------------------------------------------------------
// Dynamic client registration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registration_sends_the_loopback_redirect_and_returns_the_client_id() {
    let server = FakeServer::start(true, false).await;
    let http = reqwest::Client::new();

    let registered = oauth_registration::register_client(
        &http,
        &format!("{}/oauth/register", server.base_url),
        "http://127.0.0.1:54321/callback",
        None,
    )
    .await
    .unwrap();

    assert_eq!(registered.client_id, "registered-client-id");
    assert!(registered.client_secret.is_none());

    let requests = server.registration_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["redirect_uris"][0], "http://127.0.0.1:54321/callback");
    assert_eq!(requests[0]["token_endpoint_auth_method"], "none");
    assert_eq!(requests[0]["grant_types"][0], "authorization_code");
    assert_eq!(requests[0]["grant_types"][1], "refresh_token");
    assert_eq!(requests[0]["client_name"], "AionUi");
}

#[tokio::test]
async fn registration_failure_surfaces_the_server_error() {
    let server = FakeServer::start(true, true).await;
    let http = reqwest::Client::new();

    let err = oauth_registration::register_client(
        &http,
        &format!("{}/oauth/register", server.base_url),
        "http://127.0.0.1:54321/callback",
        None,
    )
    .await
    .unwrap_err();

    assert!(err.to_string().contains("invalid_redirect_uri"), "got: {err}");
}

#[tokio::test]
async fn login_registers_a_client_before_opening_the_browser() {
    // The login flow registers while preparing the authorization URL, so a
    // registration failure aborts the login before any browser is opened —
    // which is what makes this assertable headlessly.
    let server = FakeServer::start(true, true).await;
    let (svc, _repo) = make_service().await;

    let err = svc.login(TEST_USER_ID, &server.mcp_url()).await.unwrap_err();

    assert!(err.to_string().contains("Client registration failed"), "got: {err}");
    let requests = server.registration_requests();
    assert_eq!(requests.len(), 1);
    // The redirect URI registered is the loopback callback the flow listens on.
    let redirect = requests[0]["redirect_uris"][0].as_str().unwrap();
    assert!(redirect.starts_with("http://127.0.0.1:"), "got: {redirect}");
    assert!(redirect.ends_with("/callback"), "got: {redirect}");
}

// ---------------------------------------------------------------------------
// Refresh grant
// ---------------------------------------------------------------------------

#[tokio::test]
async fn refresh_presents_the_registered_client_and_the_resource_indicator() {
    let server = FakeServer::start(true, false).await;
    let (svc, repo) = make_service().await;
    let mcp_url = server.mcp_url();

    // An expired token obtained earlier by a dynamically registered client.
    repo.upsert(UpsertOAuthTokenParams {
        user_id: TEST_USER_ID,
        server_url: &mcp_url,
        access_token: "expired-access-token",
        refresh_token: Some("stored-refresh-token"),
        token_type: "bearer",
        expires_at: Some(1000),
        client_id: Some("registered-client-id"),
        client_secret: None,
    })
    .await
    .unwrap();

    let token = svc.get_token(TEST_USER_ID, &mcp_url).await.unwrap();
    assert_eq!(token.as_deref(), Some("refreshed-access-token"));

    let requests = server.token_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].get("grant_type").map(String::as_str), Some("refresh_token"));
    assert_eq!(
        requests[0].get("refresh_token").map(String::as_str),
        Some("stored-refresh-token")
    );
    assert_eq!(
        requests[0].get("client_id").map(String::as_str),
        Some("registered-client-id")
    );
    assert_eq!(requests[0].get("resource").map(String::as_str), Some(mcp_url.as_str()));

    // The refreshed token is persisted together with the same client id.
    let row = repo.get_by_url(TEST_USER_ID, &mcp_url).await.unwrap().unwrap();
    assert_eq!(row.access_token, "refreshed-access-token");
    assert_eq!(row.refresh_token.as_deref(), Some("refreshed-refresh-token"));
    assert_eq!(row.client_id.as_deref(), Some("registered-client-id"));
}

#[tokio::test]
async fn refresh_falls_back_to_the_static_client_id_without_a_registration() {
    let server = FakeServer::start(false, false).await;
    let (svc, repo) = make_service().await;
    let mcp_url = server.mcp_url();

    repo.upsert(UpsertOAuthTokenParams {
        user_id: TEST_USER_ID,
        server_url: &mcp_url,
        access_token: "expired-access-token",
        refresh_token: Some("stored-refresh-token"),
        token_type: "bearer",
        expires_at: Some(1000),
        client_id: None,
        client_secret: None,
    })
    .await
    .unwrap();

    let token = svc.get_token(TEST_USER_ID, &mcp_url).await.unwrap();
    assert_eq!(token.as_deref(), Some("refreshed-access-token"));

    let requests = server.token_requests();
    assert_eq!(requests[0].get("client_id").map(String::as_str), Some("aionui"));

    let row = repo.get_by_url(TEST_USER_ID, &mcp_url).await.unwrap().unwrap();
    assert!(row.client_id.is_none());
}
