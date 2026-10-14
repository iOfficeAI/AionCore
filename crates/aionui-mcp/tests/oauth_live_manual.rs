//! Manual, opt-in end-to-end check of the MCP OAuth flow against a REAL
//! authorization server. Ignored by default: it opens a browser and needs a
//! human to complete the login.
//!
//! ```sh
//! AIONUI_LIVE_MCP_URL=https://mcp.example.com/mcp \
//!   cargo test -p aionui-mcp --test oauth_live_manual -- --ignored --nocapture
//! ```
//!
//! What it proves, in order: protected resource metadata discovery (RFC 9728),
//! authorization server metadata (RFC 8414), dynamic client registration
//! (RFC 7591), the authorization code + PKCE exchange, the token reaching the
//! repository, and finally an authenticated MCP `initialize` call carrying the
//! bearer token.

use std::sync::Arc;

use aionui_db::{IOAuthTokenRepository, SqliteOAuthTokenRepository};
use aionui_mcp::{McpOAuthService, oauth_discovery};
use serde_json::json;

const TEST_USER_ID: &str = "system_default_user";

#[tokio::test]
#[ignore = "manual: opens a browser and requires a real MCP server (AIONUI_LIVE_MCP_URL)"]
async fn live_oauth_login_then_authenticated_mcp_call() {
    let Ok(mcp_url) = std::env::var("AIONUI_LIVE_MCP_URL") else {
        panic!("set AIONUI_LIVE_MCP_URL to the MCP server endpoint to run this test");
    };

    let http = reqwest::Client::new();

    // 1. Discovery.
    let discovered = oauth_discovery::discover(&http, &mcp_url)
        .await
        .expect("discovery must succeed");
    println!("authorization_endpoint: {}", discovered.metadata.authorization_endpoint);
    println!("token_endpoint:         {}", discovered.metadata.token_endpoint);
    println!(
        "registration_endpoint:  {:?}",
        discovered.metadata.registration_endpoint
    );
    println!("resource:               {:?}", discovered.resource);

    // 2. Login: registers the client, opens the browser, waits for the
    //    callback, exchanges the code.
    let db = aionui_db::init_database_memory().await.unwrap();
    let repo: Arc<dyn IOAuthTokenRepository> = Arc::new(SqliteOAuthTokenRepository::new(db.pool().clone()));
    let svc = McpOAuthService::new(repo.clone(), http.clone());

    println!("\nComplete the login in the browser window that just opened...");
    let login = svc.login(TEST_USER_ID, &mcp_url).await.expect("login must not error");
    assert!(login.success, "login failed: {:?}", login.error);

    let row = repo
        .get_by_url(TEST_USER_ID, &mcp_url)
        .await
        .unwrap()
        .expect("a token must be stored");
    println!("\nstored client_id: {:?}", row.client_id);
    println!("access token len: {}", row.access_token.len());
    assert!(
        row.client_id.is_some(),
        "the client must have been registered dynamically"
    );

    // 3. An authenticated MCP call with the stored token.
    let token = svc
        .get_token(TEST_USER_ID, &mcp_url)
        .await
        .unwrap()
        .expect("a token must be readable");

    let response = http
        .post(&mcp_url)
        .bearer_auth(&token)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "aionui-live-test", "version": "0"}
            }
        }))
        .send()
        .await
        .expect("MCP request must reach the server");

    let status = response.status();
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = response.text().await.unwrap_or_default();
    println!(
        "\nMCP initialize -> {status}\n{}",
        body.chars().take(400).collect::<String>()
    );
    assert!(status.is_success(), "authenticated MCP call failed: {status} {body}");

    // 4. A real tool round-trip on the same authenticated session: list the
    //    tools, then invoke the one named by AIONUI_LIVE_MCP_TOOL, if any.
    let session_id = session_id.expect("streamable HTTP server must return a session id");

    post_mcp(
        &http,
        &mcp_url,
        &token,
        &session_id,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;

    let tools = post_mcp(
        &http,
        &mcp_url,
        &token,
        &session_id,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await
    .expect("tools/list must return a response");
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    println!(
        "\ntools/list -> {} tools, e.g. {:?}",
        names.len(),
        &names[..names.len().min(5)]
    );
    assert!(!names.is_empty(), "an authenticated tools/list must not be empty");

    if let Ok(tool_name) = std::env::var("AIONUI_LIVE_MCP_TOOL") {
        let result = post_mcp(
            &http,
            &mcp_url,
            &token,
            &session_id,
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": tool_name, "arguments": {}}}),
        )
        .await
        .expect("tools/call must return a response");
        println!(
            "\ntools/call {tool_name} ->\n{}",
            serde_json::to_string(&result)
                .unwrap_or_default()
                .chars()
                .take(600)
                .collect::<String>()
        );
        assert!(result.get("error").is_none(), "tool call failed: {result}");
    }
}

/// POST one JSON-RPC message on the authenticated MCP session and return the
/// parsed response, if the server sent one (notifications get none).
async fn post_mcp(
    http: &reqwest::Client,
    mcp_url: &str,
    token: &str,
    session_id: &str,
    message: serde_json::Value,
) -> Option<serde_json::Value> {
    let response = http
        .post(mcp_url)
        .bearer_auth(token)
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Session-Id", session_id)
        .json(&message)
        .send()
        .await
        .expect("MCP request must reach the server");

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert!(status.is_success(), "MCP call failed: {status} {body}");
    parse_sse_or_json(&body)
}

/// The streamable HTTP transport answers either plain JSON or an SSE frame
/// (`event: message\ndata: {...}`); accept both.
fn parse_sse_or_json(body: &str) -> Option<serde_json::Value> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        return Some(value);
    }
    body.lines()
        .find_map(|line| line.strip_prefix("data: "))
        .and_then(|data| serde_json::from_str(data).ok())
}
