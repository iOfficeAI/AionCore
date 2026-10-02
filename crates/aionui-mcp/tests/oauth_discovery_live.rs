//! Live discovery checks against real public MCP servers.
//!
//! Ignored by default (network-dependent). Run with:
//! `cargo test -p aionui-mcp --test oauth_discovery_live -- --ignored`
//!
//! These guard the exact regression from the bug report: the old code appended
//! `.well-known/...` to the full URL path, which these servers answer with 401.

use aionui_mcp::oauth_discovery::OAuthDiscovery;

#[tokio::test]
#[ignore = "requires network access to mcp.monday.com"]
async fn discovers_monday_sse_endpoints() {
    let client = reqwest::Client::new();
    let metadata = OAuthDiscovery::new(&client)
        .discover("https://mcp.monday.com/sse")
        .await
        .expect("discovery must succeed for monday.com MCP server");

    assert!(
        metadata.authorization_endpoint.starts_with("https://"),
        "unexpected authorization_endpoint: {}",
        metadata.authorization_endpoint
    );
    assert!(
        metadata.token_endpoint.starts_with("https://"),
        "unexpected token_endpoint: {}",
        metadata.token_endpoint
    );
    // monday requires dynamic client registration.
    assert!(
        metadata.registration_endpoint.is_some(),
        "expected a registration endpoint to be advertised"
    );
}
