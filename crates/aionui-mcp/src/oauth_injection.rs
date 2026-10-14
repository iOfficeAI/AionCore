//! Injection of OAuth access tokens into the MCP servers handed to a backend.
//!
//! Logging in to an MCP server is only half the job: the agent CLI that
//! actually speaks to the server must send the token. HTTP/SSE MCP servers
//! carry their credentials in request headers, so at session-build time every
//! URL-based server the user authenticated gets an `Authorization: Bearer`
//! header derived from the stored token.
//!
//! A header the user configured by hand always wins — an operator who pasted a
//! static token (or a different auth scheme) into the server's header list must
//! not have it silently replaced.

use std::collections::HashMap;

use aionui_api_types::{SessionMcpServer, SessionMcpTransport};
use tracing::{debug, info};

/// Standard HTTP authorization header name.
pub const AUTHORIZATION_HEADER: &str = "Authorization";

/// Supplies OAuth access tokens for MCP server URLs.
///
/// Implemented by [`crate::McpOAuthService`]; abstracted so the session-build
/// path can depend on the capability without owning the OAuth service.
#[async_trait::async_trait]
pub trait IMcpAccessTokenProvider: Send + Sync {
    /// Return a valid access token for `server_url`, refreshing it when needed.
    ///
    /// Returns `None` when the user has not authenticated against that server,
    /// or when a token could not be produced — injection is best effort and
    /// never fails a session build.
    async fn access_token(&self, user_id: &str, server_url: &str) -> Option<String>;
}

/// Add `Authorization: Bearer <token>` unless an authorization header is
/// already present (comparison is case-insensitive, per RFC 9110 §5.1).
///
/// Returns `true` when the header was added.
pub fn apply_bearer_header(headers: &mut HashMap<String, String>, token: &str) -> bool {
    if headers.keys().any(|k| k.eq_ignore_ascii_case(AUTHORIZATION_HEADER)) {
        return false;
    }
    headers.insert(AUTHORIZATION_HEADER.to_owned(), format!("Bearer {token}"));
    true
}

/// The URL an MCP server is reached at, when it is a URL-based transport.
pub fn server_url(server: &SessionMcpServer) -> Option<&str> {
    match &server.transport {
        SessionMcpTransport::Stdio { .. } => None,
        SessionMcpTransport::Http { url, .. }
        | SessionMcpTransport::Sse { url, .. }
        | SessionMcpTransport::StreamableHttp { url, .. } => Some(url.as_str()),
    }
}

/// Mutable access to a URL-based server's headers.
fn headers_mut(server: &mut SessionMcpServer) -> Option<&mut HashMap<String, String>> {
    match &mut server.transport {
        SessionMcpTransport::Stdio { .. } => None,
        SessionMcpTransport::Http { headers, .. }
        | SessionMcpTransport::Sse { headers, .. }
        | SessionMcpTransport::StreamableHttp { headers, .. } => Some(headers),
    }
}

/// Inject stored OAuth bearer tokens into every URL-based MCP server.
///
/// Best effort: a server with no stored token, or whose token cannot be
/// refreshed, is left untouched and still reaches the backend (it will simply
/// get the server's own 401, which is what the user sees today).
pub async fn inject_oauth_bearer_headers(
    servers: &mut [SessionMcpServer],
    user_id: &str,
    provider: &dyn IMcpAccessTokenProvider,
) {
    for server in servers.iter_mut() {
        let Some(url) = server_url(server).map(str::to_owned) else {
            continue;
        };
        let Some(token) = provider.access_token(user_id, &url).await else {
            continue;
        };
        let Some(headers) = headers_mut(server) else {
            continue;
        };
        if apply_bearer_header(headers, &token) {
            info!(
                mcp_server = %server.name,
                "mcp_oauth: injected bearer token into MCP server headers"
            );
        } else {
            debug!(
                mcp_server = %server.name,
                "mcp_oauth: authorization header already set by the user; leaving it untouched"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticProvider {
        token: Option<String>,
    }

    #[async_trait::async_trait]
    impl IMcpAccessTokenProvider for StaticProvider {
        async fn access_token(&self, _user_id: &str, _server_url: &str) -> Option<String> {
            self.token.clone()
        }
    }

    fn http_server(headers: HashMap<String, String>) -> SessionMcpServer {
        SessionMcpServer {
            id: "mcp_1".into(),
            name: "remote".into(),
            transport: SessionMcpTransport::StreamableHttp {
                url: "https://mcp.example.com/mcp".into(),
                headers,
            },
        }
    }

    fn stdio_server() -> SessionMcpServer {
        SessionMcpServer {
            id: "mcp_2".into(),
            name: "local".into(),
            transport: SessionMcpTransport::Stdio {
                command: "npx".into(),
                args: vec![],
                env: HashMap::new(),
            },
        }
    }

    fn headers_of(server: &SessionMcpServer) -> HashMap<String, String> {
        match &server.transport {
            SessionMcpTransport::Http { headers, .. }
            | SessionMcpTransport::Sse { headers, .. }
            | SessionMcpTransport::StreamableHttp { headers, .. } => headers.clone(),
            SessionMcpTransport::Stdio { .. } => HashMap::new(),
        }
    }

    #[test]
    fn apply_bearer_header_adds_header() {
        let mut headers = HashMap::new();
        assert!(apply_bearer_header(&mut headers, "tok123"));
        assert_eq!(headers.get("Authorization").unwrap(), "Bearer tok123");
    }

    #[test]
    fn apply_bearer_header_keeps_manual_header() {
        let mut headers = HashMap::from([("Authorization".to_owned(), "Bearer manual".to_owned())]);
        assert!(!apply_bearer_header(&mut headers, "tok123"));
        assert_eq!(headers.get("Authorization").unwrap(), "Bearer manual");
    }

    #[test]
    fn apply_bearer_header_keeps_manual_header_case_insensitively() {
        let mut headers = HashMap::from([("authorization".to_owned(), "Basic abc".to_owned())]);
        assert!(!apply_bearer_header(&mut headers, "tok123"));
        assert_eq!(headers.len(), 1);
        assert_eq!(headers.get("authorization").unwrap(), "Basic abc");
    }

    #[test]
    fn server_url_is_none_for_stdio() {
        assert!(server_url(&stdio_server()).is_none());
        assert_eq!(
            server_url(&http_server(HashMap::new())),
            Some("https://mcp.example.com/mcp")
        );
    }

    #[tokio::test]
    async fn injects_token_into_http_server() {
        let mut servers = vec![http_server(HashMap::new())];
        let provider = StaticProvider {
            token: Some("tok123".into()),
        };

        inject_oauth_bearer_headers(&mut servers, "user-1", &provider).await;

        assert_eq!(headers_of(&servers[0]).get("Authorization").unwrap(), "Bearer tok123");
    }

    #[tokio::test]
    async fn does_not_overwrite_user_configured_header() {
        let mut servers = vec![http_server(HashMap::from([(
            "Authorization".to_owned(),
            "Bearer manual".to_owned(),
        )]))];
        let provider = StaticProvider {
            token: Some("tok123".into()),
        };

        inject_oauth_bearer_headers(&mut servers, "user-1", &provider).await;

        assert_eq!(headers_of(&servers[0]).get("Authorization").unwrap(), "Bearer manual");
    }

    #[tokio::test]
    async fn leaves_server_untouched_without_token() {
        let mut servers = vec![http_server(HashMap::new())];
        let provider = StaticProvider { token: None };

        inject_oauth_bearer_headers(&mut servers, "user-1", &provider).await;

        assert!(headers_of(&servers[0]).is_empty());
    }

    #[tokio::test]
    async fn leaves_stdio_server_untouched() {
        let mut servers = vec![stdio_server()];
        let provider = StaticProvider {
            token: Some("tok123".into()),
        };

        inject_oauth_bearer_headers(&mut servers, "user-1", &provider).await;

        match &servers[0].transport {
            SessionMcpTransport::Stdio { env, .. } => assert!(env.is_empty()),
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn injects_into_every_url_server() {
        let mut servers = vec![
            http_server(HashMap::new()),
            stdio_server(),
            SessionMcpServer {
                id: "mcp_3".into(),
                name: "sse".into(),
                transport: SessionMcpTransport::Sse {
                    url: "https://mcp.example.com/sse".into(),
                    headers: HashMap::new(),
                },
            },
        ];
        let provider = StaticProvider {
            token: Some("tok123".into()),
        };

        inject_oauth_bearer_headers(&mut servers, "user-1", &provider).await;

        assert_eq!(headers_of(&servers[0]).get("Authorization").unwrap(), "Bearer tok123");
        assert_eq!(headers_of(&servers[2]).get("Authorization").unwrap(), "Bearer tok123");
    }
}
