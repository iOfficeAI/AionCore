//! OAuth endpoint discovery and dynamic client registration for MCP servers.
//!
//! Implements the discovery chain the MCP authorization spec requires:
//!
//! 1. RFC 9728 protected-resource metadata to locate the authorization server
//! 2. RFC 8414 / OIDC authorization-server metadata for the endpoints
//! 3. RFC 7591 dynamic client registration when the server advertises it
//!
//! The well-known probe order matters: RFC 8414 inserts `.well-known/...`
//! between the origin and the resource path (`/sse` -> `/.well-known/x/sse`),
//! which is *not* the same as appending it to the full URL. Appending to the
//! full path is what many public MCP servers reject, so both spellings are
//! tried, origin-relative first.

use serde::Deserialize;
use url::Url;

use crate::error::McpError;

/// OAuth Authorization Server Metadata (RFC 8414) — subset of fields we need.
#[derive(Debug, Clone, Deserialize)]
pub struct OAuthServerMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    /// RFC 7591 dynamic client registration endpoint, when supported.
    #[serde(default)]
    pub registration_endpoint: Option<String>,
}

/// OAuth Protected Resource Metadata (RFC 9728) — subset of fields we need.
#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
}

/// Credentials for the OAuth client used against one MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCredentials {
    pub client_id: String,
    pub client_secret: Option<String>,
}

/// Successful RFC 7591 registration response — subset of fields we need.
#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
}

/// Build the candidate well-known URLs for a metadata document, in probe order.
///
/// For `https://host/sse` and suffix `oauth-authorization-server` this yields:
///
/// 1. `https://host/.well-known/oauth-authorization-server/sse` (RFC 8414)
/// 2. `https://host/.well-known/oauth-authorization-server` (path-less form)
/// 3. `https://host/sse/.well-known/oauth-authorization-server` (legacy)
///
/// Returns an empty vec when `server_url` cannot be parsed as a URL.
pub fn well_known_candidates(server_url: &str, suffix: &str) -> Vec<String> {
    let Ok(url) = Url::parse(server_url) else {
        return Vec::new();
    };
    let Some(origin) = url_origin(&url) else {
        return Vec::new();
    };

    let path = url.path().trim_end_matches('/');
    let mut candidates = Vec::new();

    if !path.is_empty() {
        candidates.push(format!("{origin}/.well-known/{suffix}{path}"));
    }
    candidates.push(format!("{origin}/.well-known/{suffix}"));
    if !path.is_empty() {
        candidates.push(format!("{origin}{path}/.well-known/{suffix}"));
    }

    candidates
}

/// Serialize a URL's origin as `scheme://host[:port]`.
///
/// `Url::origin()` is not used because it opaquefies non-special schemes; we
/// only ever probe http(s) MCP endpoints and want a plain string.
fn url_origin(url: &Url) -> Option<String> {
    let scheme = url.scheme();
    let host = url.host_str()?;
    match url.port() {
        Some(port) => Some(format!("{scheme}://{host}:{port}")),
        None => Some(format!("{scheme}://{host}")),
    }
}

/// Discovery and registration over a shared HTTP client.
pub struct OAuthDiscovery<'a> {
    http_client: &'a reqwest::Client,
}

impl<'a> OAuthDiscovery<'a> {
    pub fn new(http_client: &'a reqwest::Client) -> Self {
        Self { http_client }
    }

    /// Discover the authorization server metadata for an MCP server URL.
    ///
    /// Follows the RFC 9728 pointer when present, then probes RFC 8414 and
    /// OIDC well-known locations. Returns the first document that parses.
    pub async fn discover(&self, server_url: &str) -> Result<OAuthServerMetadata, McpError> {
        // RFC 9728: the resource may name its authorization server(s).
        let mut issuers: Vec<String> = Vec::new();
        for candidate in well_known_candidates(server_url, "oauth-protected-resource") {
            if let Some(resource) = self.fetch::<ProtectedResourceMetadata>(&candidate).await {
                issuers = resource.authorization_servers;
                break;
            }
        }

        // Probe the advertised issuers first, then the resource URL itself.
        let mut probe_bases: Vec<String> = issuers;
        probe_bases.push(server_url.to_string());

        for base in &probe_bases {
            for suffix in ["oauth-authorization-server", "openid-configuration"] {
                for candidate in well_known_candidates(base, suffix) {
                    if let Some(metadata) = self.fetch::<OAuthServerMetadata>(&candidate).await {
                        return Ok(metadata);
                    }
                }
            }
        }

        Err(McpError::OAuthDiscovery(format!(
            "Could not discover OAuth endpoints for '{server_url}'. \
             The server did not publish RFC 8414 authorization-server metadata, \
             OpenID configuration, or RFC 9728 protected-resource metadata at any \
             well-known location."
        )))
    }

    /// Register a public OAuth client via RFC 7591 dynamic client registration.
    pub async fn register_client(
        &self,
        registration_endpoint: &str,
        redirect_uri: &str,
    ) -> Result<ClientCredentials, McpError> {
        let body = serde_json::json!({
            "client_name": "AionUi",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        });

        let resp = self
            .http_client
            .post(registration_endpoint)
            .json(&body)
            .send()
            .await
            .map_err(|e| McpError::OAuth(format!("Client registration request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(McpError::OAuth(format!(
                "Client registration at '{registration_endpoint}' returned {status}"
            )));
        }

        let registered: RegistrationResponse = resp
            .json()
            .await
            .map_err(|e| McpError::OAuth(format!("Failed to parse client registration response: {e}")))?;

        Ok(ClientCredentials {
            client_id: registered.client_id,
            client_secret: registered.client_secret,
        })
    }

    /// GET and deserialize a metadata document, returning `None` on any failure.
    async fn fetch<T: serde::de::DeserializeOwned>(&self, url: &str) -> Option<T> {
        let resp = self.http_client.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<T>().await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_put_origin_relative_path_suffix_first() {
        let candidates = well_known_candidates("https://mcp.monday.com/sse", "oauth-authorization-server");
        assert_eq!(
            candidates,
            vec![
                "https://mcp.monday.com/.well-known/oauth-authorization-server/sse",
                "https://mcp.monday.com/.well-known/oauth-authorization-server",
                "https://mcp.monday.com/sse/.well-known/oauth-authorization-server",
            ]
        );
    }

    /// Regression guard for the reported bug: the legacy "append to full path"
    /// spelling must not be the only thing we try, because public MCP servers
    /// answer 401 there.
    #[test]
    fn candidates_are_not_only_the_legacy_appended_form() {
        let legacy = "https://mcp.monday.com/sse/.well-known/oauth-authorization-server";
        let candidates = well_known_candidates("https://mcp.monday.com/sse", "oauth-authorization-server");
        assert!(candidates.len() > 1);
        assert_ne!(candidates[0], legacy);
        assert!(candidates.contains(&legacy.to_string()));
    }

    #[test]
    fn candidates_for_root_url_have_no_duplicate_path_forms() {
        let candidates = well_known_candidates("https://example.com/", "openid-configuration");
        assert_eq!(candidates, vec!["https://example.com/.well-known/openid-configuration"]);
    }

    #[test]
    fn candidates_preserve_non_default_port() {
        let candidates = well_known_candidates("http://127.0.0.1:8931/mcp", "oauth-authorization-server");
        assert_eq!(
            candidates[0],
            "http://127.0.0.1:8931/.well-known/oauth-authorization-server/mcp"
        );
    }

    #[test]
    fn candidates_for_nested_path() {
        let candidates = well_known_candidates("https://example.com/api/v1/mcp", "oauth-protected-resource");
        assert_eq!(
            candidates[0],
            "https://example.com/.well-known/oauth-protected-resource/api/v1/mcp"
        );
    }

    #[test]
    fn candidates_empty_for_unparseable_url() {
        assert!(well_known_candidates("not a url", "oauth-authorization-server").is_empty());
    }

    #[test]
    fn metadata_parses_without_registration_endpoint() {
        let metadata: OAuthServerMetadata = serde_json::from_str(
            r#"{"authorization_endpoint":"https://a.example/auth","token_endpoint":"https://a.example/token"}"#,
        )
        .unwrap();
        assert_eq!(metadata.registration_endpoint, None);
    }

    #[test]
    fn metadata_parses_with_registration_endpoint_and_extra_fields() {
        let metadata: OAuthServerMetadata = serde_json::from_str(
            r#"{"issuer":"https://a.example","authorization_endpoint":"https://a.example/auth",
                "token_endpoint":"https://a.example/token",
                "registration_endpoint":"https://a.example/register"}"#,
        )
        .unwrap();
        assert_eq!(
            metadata.registration_endpoint.as_deref(),
            Some("https://a.example/register")
        );
    }
}
