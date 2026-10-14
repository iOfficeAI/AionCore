//! OAuth 2.0 Dynamic Client Registration (RFC 7591) for MCP servers.
//!
//! MCP authorization servers are per-deployment: there is no way for AionUi to
//! ship a client id that they would recognise, and the loopback redirect URI it
//! listens on cannot be allow-listed up front. RFC 7591 is the mechanism the
//! MCP specification prescribes for exactly this — the client registers itself
//! at login time and receives a client id bound to the redirect URI it is about
//! to use.

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::McpError;

/// Client name AionUi registers itself under.
const CLIENT_NAME: &str = "AionUi";

/// Client registration request (RFC 7591 §2) — the metadata we send.
#[derive(Debug, Clone, Serialize)]
pub struct ClientRegistrationRequest {
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    /// `none` — AionUi is a native public client authenticating with PKCE.
    pub token_endpoint_auth_method: String,
    pub application_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl ClientRegistrationRequest {
    /// Build the registration request for a loopback redirect URI.
    pub fn for_redirect_uri(redirect_uri: &str, scopes: Option<&[String]>) -> Self {
        Self {
            client_name: CLIENT_NAME.to_owned(),
            redirect_uris: vec![redirect_uri.to_owned()],
            grant_types: vec!["authorization_code".to_owned(), "refresh_token".to_owned()],
            response_types: vec!["code".to_owned()],
            token_endpoint_auth_method: "none".to_owned(),
            application_type: "native".to_owned(),
            scope: scopes.filter(|s| !s.is_empty()).map(|scopes| scopes.join(" ")),
        }
    }
}

/// Client registration response (RFC 7591 §3.2.1) — the subset we persist.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientRegistrationResponse {
    pub client_id: String,
    /// Issued only for confidential clients; absent for a public client.
    #[serde(default)]
    pub client_secret: Option<String>,
}

/// Register this client with the authorization server.
///
/// Returns the issued client id (and secret, when the server issues one).
pub async fn register_client(
    http_client: &reqwest::Client,
    registration_endpoint: &str,
    redirect_uri: &str,
    scopes: Option<&[String]>,
) -> Result<ClientRegistrationResponse, McpError> {
    let request = ClientRegistrationRequest::for_redirect_uri(redirect_uri, scopes);

    let response = http_client
        .post(registration_endpoint)
        .json(&request)
        .send()
        .await
        .map_err(|e| McpError::OAuth(format!("Client registration request failed: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        // RFC 7591 §3.2.2 returns a JSON error body; surface it as-is since it
        // is the server's own diagnostic (e.g. `invalid_redirect_uri`).
        let body = response.text().await.unwrap_or_default();
        return Err(McpError::OAuth(format!(
            "Client registration failed with {status}: {}",
            body.trim()
        )));
    }

    let registered: ClientRegistrationResponse = response
        .json()
        .await
        .map_err(|e| McpError::OAuth(format!("Failed to parse client registration response: {e}")))?;

    debug!(
        registration_endpoint,
        client_id = %registered.client_id,
        "Registered OAuth client dynamically (RFC 7591)"
    );
    Ok(registered)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_declares_public_native_client() {
        let request = ClientRegistrationRequest::for_redirect_uri("http://127.0.0.1:5173/callback", None);

        assert_eq!(request.client_name, "AionUi");
        assert_eq!(request.redirect_uris, vec!["http://127.0.0.1:5173/callback"]);
        assert_eq!(request.grant_types, vec!["authorization_code", "refresh_token"]);
        assert_eq!(request.response_types, vec!["code"]);
        assert_eq!(request.token_endpoint_auth_method, "none");
        assert_eq!(request.application_type, "native");
        assert!(request.scope.is_none());
    }

    #[test]
    fn request_joins_scopes_with_spaces() {
        let scopes = vec!["read".to_owned(), "write".to_owned()];
        let request = ClientRegistrationRequest::for_redirect_uri("http://127.0.0.1:1/callback", Some(&scopes));
        assert_eq!(request.scope.as_deref(), Some("read write"));
    }

    #[test]
    fn request_omits_empty_scope_list() {
        let request = ClientRegistrationRequest::for_redirect_uri("http://127.0.0.1:1/callback", Some(&[]));
        assert!(request.scope.is_none());
    }

    #[test]
    fn request_serializes_without_scope_key_when_absent() {
        let request = ClientRegistrationRequest::for_redirect_uri("http://127.0.0.1:1/callback", None);
        let json = serde_json::to_value(&request).unwrap();
        assert!(json.get("scope").is_none());
        assert_eq!(json["redirect_uris"][0], "http://127.0.0.1:1/callback");
    }

    #[test]
    fn parse_registration_response_public_client() {
        let parsed: ClientRegistrationResponse = serde_json::from_str(
            r#"{"client_id": "abc123", "client_id_issued_at": 1700000000, "redirect_uris": ["http://127.0.0.1:1/callback"]}"#,
        )
        .unwrap();

        assert_eq!(parsed.client_id, "abc123");
        assert!(parsed.client_secret.is_none());
    }

    #[test]
    fn parse_registration_response_confidential_client() {
        let parsed: ClientRegistrationResponse =
            serde_json::from_str(r#"{"client_id": "abc123", "client_secret": "s3cret"}"#).unwrap();

        assert_eq!(parsed.client_secret.as_deref(), Some("s3cret"));
    }

    #[test]
    fn parse_registration_response_without_client_id_fails() {
        assert!(serde_json::from_str::<ClientRegistrationResponse>(r#"{"client_secret": "s3cret"}"#).is_err());
    }
}
