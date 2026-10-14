//! OAuth discovery for MCP servers.
//!
//! Implements the discovery chain the MCP authorization specification
//! prescribes:
//!
//! 1. Fetch the server's **Protected Resource Metadata** (RFC 9728) to learn
//!    which authorization server(s) guard it and the canonical `resource`
//!    identifier to use as an RFC 8707 resource indicator.
//! 2. Fetch the **Authorization Server Metadata** (RFC 8414 / OIDC Discovery)
//!    of the first authorization server that answers, to learn the authorize,
//!    token and — when the server supports it — registration endpoints.
//!
//! Servers that publish neither are still supported: the caller falls back to
//! probing the MCP server URL itself for authorization server metadata, which
//! is what AionUi did before this module existed.

use serde::Deserialize;
use tracing::debug;

use crate::error::McpError;

/// Protected Resource Metadata (RFC 9728 §2) — the subset we consume.
#[derive(Debug, Clone, Deserialize)]
pub struct ProtectedResourceMetadata {
    /// Canonical identifier of the protected resource. Sent back as the
    /// RFC 8707 `resource` parameter on authorize and token requests.
    pub resource: Option<String>,
    /// Issuer identifiers of the authorization servers that can issue tokens
    /// for this resource.
    #[serde(default)]
    pub authorization_servers: Vec<String>,
    /// Scopes the resource understands, when advertised.
    #[serde(default)]
    pub scopes_supported: Option<Vec<String>>,
}

/// Authorization Server Metadata (RFC 8414 §2) — the subset we consume.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizationServerMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    /// RFC 7591 dynamic client registration endpoint. `None` means the server
    /// expects a pre-provisioned client id.
    #[serde(default)]
    pub registration_endpoint: Option<String>,
}

/// Everything a login flow needs after discovery.
#[derive(Debug, Clone)]
pub struct DiscoveredAuthorization {
    pub metadata: AuthorizationServerMetadata,
    /// RFC 8707 resource indicator, present only when the server published
    /// protected resource metadata.
    pub resource: Option<String>,
    /// Scopes advertised by the protected resource, when any.
    pub scopes: Option<Vec<String>>,
}

/// Build the candidate Protected Resource Metadata URLs for an MCP server URL.
///
/// RFC 9728 §3.1 inserts `.well-known/oauth-protected-resource` between the
/// origin and the resource path, so `https://host/mcp` is described by
/// `https://host/.well-known/oauth-protected-resource/mcp`. Servers that host a
/// single resource commonly publish the path-less form instead, so that is
/// tried as well.
pub fn protected_resource_metadata_urls(server_url: &str) -> Vec<String> {
    let Ok(url) = reqwest::Url::parse(server_url) else {
        return Vec::new();
    };
    well_known_candidates(&url, &["oauth-protected-resource"])
}

/// Build the candidate Authorization Server Metadata URLs for an issuer.
///
/// Tries the RFC 8414 path-aware form first, then the OIDC Discovery forms,
/// matching the fallback order the MCP specification describes.
pub fn authorization_server_metadata_urls(issuer: &str) -> Vec<String> {
    let Ok(url) = reqwest::Url::parse(issuer) else {
        return Vec::new();
    };
    well_known_candidates(&url, &["oauth-authorization-server", "openid-configuration"])
}

/// Expand `{origin}/.well-known/{suffix}{path}` + `{origin}{path}/.well-known/{suffix}`
/// candidates for each suffix, de-duplicated and in probe order.
fn well_known_candidates(url: &reqwest::Url, suffixes: &[&str]) -> Vec<String> {
    let origin = format!(
        "{}://{}",
        url.scheme(),
        url.host_str().map(str::to_owned).unwrap_or_default()
            + &url.port().map(|p| format!(":{p}")).unwrap_or_default()
    );
    let path = url.path().trim_end_matches('/').to_owned();

    let mut candidates = Vec::new();
    for suffix in suffixes {
        candidates.push(format!("{origin}/.well-known/{suffix}{path}"));
        if !path.is_empty() {
            // Non-compliant but widespread: the well-known segment appended
            // after the resource/issuer path.
            candidates.push(format!("{origin}{path}/.well-known/{suffix}"));
            // Equally widespread: a deployment hosting a single resource
            // publishes the document at the origin, ignoring the path.
            candidates.push(format!("{origin}/.well-known/{suffix}"));
        }
    }
    candidates
}

/// Fetch and parse JSON metadata from a URL.
pub async fn fetch_json<T: serde::de::DeserializeOwned>(
    http_client: &reqwest::Client,
    url: &str,
) -> Result<T, McpError> {
    let resp = http_client
        .get(url)
        .send()
        .await
        .map_err(|e| McpError::OAuth(format!("HTTP request failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(McpError::OAuth(format!("Metadata endpoint returned {}", resp.status())));
    }

    resp.json()
        .await
        .map_err(|e| McpError::OAuth(format!("Failed to parse metadata: {e}")))
}

/// Run the full discovery chain for an MCP server URL.
///
/// Returns the authorization server metadata plus the resource indicator when
/// the server published protected resource metadata. Falls back to probing the
/// MCP server URL itself as an authorization server, which keeps servers that
/// predate RFC 9728 working.
pub async fn discover(http_client: &reqwest::Client, server_url: &str) -> Result<DiscoveredAuthorization, McpError> {
    if let Some(prm) = fetch_protected_resource_metadata(http_client, server_url).await {
        for issuer in &prm.authorization_servers {
            if let Some(metadata) = fetch_authorization_server_metadata(http_client, issuer).await {
                debug!(server_url, issuer, "Discovered authorization server via RFC 9728");
                return Ok(DiscoveredAuthorization {
                    metadata,
                    resource: prm.resource.clone(),
                    scopes: prm.scopes_supported.clone(),
                });
            }
        }
    }

    // Fallback: treat the MCP server URL itself as the authorization server.
    if let Some(metadata) = fetch_authorization_server_metadata(http_client, server_url).await {
        debug!(
            server_url,
            "Discovered authorization server metadata on the MCP server URL"
        );
        return Ok(DiscoveredAuthorization {
            metadata,
            resource: None,
            scopes: None,
        });
    }

    Err(McpError::OAuth(format!(
        "Failed to discover OAuth endpoints for '{server_url}': no \
         .well-known/oauth-protected-resource, .well-known/oauth-authorization-server \
         or .well-known/openid-configuration found"
    )))
}

/// Fetch protected resource metadata, trying each candidate URL in order.
async fn fetch_protected_resource_metadata(
    http_client: &reqwest::Client,
    server_url: &str,
) -> Option<ProtectedResourceMetadata> {
    for url in protected_resource_metadata_urls(server_url) {
        match fetch_json::<ProtectedResourceMetadata>(http_client, &url).await {
            Ok(prm) if !prm.authorization_servers.is_empty() => return Some(prm),
            Ok(_) => debug!(url, "Protected resource metadata listed no authorization server"),
            Err(e) => debug!(url, error = %e, "Protected resource metadata not available"),
        }
    }
    None
}

/// Fetch authorization server metadata, trying each candidate URL in order.
async fn fetch_authorization_server_metadata(
    http_client: &reqwest::Client,
    issuer: &str,
) -> Option<AuthorizationServerMetadata> {
    for url in authorization_server_metadata_urls(issuer) {
        match fetch_json::<AuthorizationServerMetadata>(http_client, &url).await {
            Ok(metadata) => return Some(metadata),
            Err(e) => debug!(url, error = %e, "Authorization server metadata not available"),
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prm_urls_for_path_resource() {
        let urls = protected_resource_metadata_urls("https://example.com/mcp");
        assert_eq!(
            urls,
            vec![
                "https://example.com/.well-known/oauth-protected-resource/mcp",
                "https://example.com/mcp/.well-known/oauth-protected-resource",
                // Single-resource deployments publish the document at the origin.
                "https://example.com/.well-known/oauth-protected-resource",
            ]
        );
    }

    #[test]
    fn prm_urls_for_root_resource() {
        let urls = protected_resource_metadata_urls("https://example.com/");
        assert_eq!(urls, vec!["https://example.com/.well-known/oauth-protected-resource"]);
    }

    #[test]
    fn prm_urls_keep_port() {
        let urls = protected_resource_metadata_urls("http://127.0.0.1:8931/mcp");
        assert_eq!(
            urls[0],
            "http://127.0.0.1:8931/.well-known/oauth-protected-resource/mcp"
        );
    }

    #[test]
    fn prm_urls_invalid_input_is_empty() {
        assert!(protected_resource_metadata_urls("not a url").is_empty());
    }

    #[test]
    fn as_urls_for_issuer_without_path() {
        let urls = authorization_server_metadata_urls("https://auth.example.com");
        assert_eq!(
            urls,
            vec![
                "https://auth.example.com/.well-known/oauth-authorization-server",
                "https://auth.example.com/.well-known/openid-configuration",
            ]
        );
    }

    #[test]
    fn as_urls_for_issuer_with_tenant_path() {
        let urls = authorization_server_metadata_urls("https://auth.example.com/tenant-a");
        assert_eq!(
            urls,
            vec![
                "https://auth.example.com/.well-known/oauth-authorization-server/tenant-a",
                "https://auth.example.com/tenant-a/.well-known/oauth-authorization-server",
                "https://auth.example.com/.well-known/oauth-authorization-server",
                "https://auth.example.com/.well-known/openid-configuration/tenant-a",
                "https://auth.example.com/tenant-a/.well-known/openid-configuration",
                "https://auth.example.com/.well-known/openid-configuration",
            ]
        );
    }

    #[test]
    fn parse_protected_resource_metadata() {
        let prm: ProtectedResourceMetadata = serde_json::from_str(
            r#"{
                "resource": "https://example.com/mcp",
                "authorization_servers": ["https://auth.example.com"],
                "bearer_methods_supported": ["header"]
            }"#,
        )
        .unwrap();

        assert_eq!(prm.resource.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(prm.authorization_servers, vec!["https://auth.example.com"]);
        assert!(prm.scopes_supported.is_none());
    }

    #[test]
    fn parse_protected_resource_metadata_with_scopes() {
        let prm: ProtectedResourceMetadata =
            serde_json::from_str(r#"{"authorization_servers": ["https://a"], "scopes_supported": ["read", "write"]}"#)
                .unwrap();

        assert!(prm.resource.is_none());
        assert_eq!(prm.scopes_supported.unwrap(), vec!["read", "write"]);
    }

    #[test]
    fn parse_authorization_server_metadata_with_registration() {
        let metadata: AuthorizationServerMetadata = serde_json::from_str(
            r#"{
                "issuer": "https://auth.example.com",
                "authorization_endpoint": "https://auth.example.com/oauth/authorize",
                "token_endpoint": "https://auth.example.com/oauth/token",
                "registration_endpoint": "https://auth.example.com/oauth/register",
                "code_challenge_methods_supported": ["S256"]
            }"#,
        )
        .unwrap();

        assert_eq!(
            metadata.authorization_endpoint,
            "https://auth.example.com/oauth/authorize"
        );
        assert_eq!(metadata.token_endpoint, "https://auth.example.com/oauth/token");
        assert_eq!(
            metadata.registration_endpoint.as_deref(),
            Some("https://auth.example.com/oauth/register")
        );
    }

    #[test]
    fn parse_authorization_server_metadata_without_registration() {
        let metadata: AuthorizationServerMetadata = serde_json::from_str(
            r#"{
                "authorization_endpoint": "https://auth.example.com/authorize",
                "token_endpoint": "https://auth.example.com/token"
            }"#,
        )
        .unwrap();

        assert!(metadata.registration_endpoint.is_none());
    }

    #[test]
    fn parse_authorization_server_metadata_missing_endpoint_fails() {
        let err = serde_json::from_str::<AuthorizationServerMetadata>(
            r#"{"authorization_endpoint": "https://auth.example.com/authorize"}"#,
        );
        assert!(err.is_err());
    }
}
