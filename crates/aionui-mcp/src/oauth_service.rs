use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::{OAuthLoginResponse, OAuthStatusResponse};
use aionui_common::{TimestampMs, now_ms};
use aionui_db::models::OAuthTokenRow;
use aionui_db::{IOAuthTokenRepository, UpsertOAuthTokenParams};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::error::McpError;
use crate::oauth_discovery::{self, DiscoveredAuthorization};
use crate::oauth_injection::IMcpAccessTokenProvider;
use crate::oauth_registration;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default timeout for the OAuth callback server waiting for the redirect.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(120);

/// Fallback OAuth client ID, used only against authorization servers that do
/// NOT advertise a dynamic client registration endpoint (public client, no
/// secret). A spec-compliant MCP authorization server registers the client
/// instead — see [`crate::oauth_registration`].
const DEFAULT_CLIENT_ID: &str = "aionui";

/// Token expiry safety margin (refresh 5 minutes before expiration).
const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;

// ---------------------------------------------------------------------------
// OAuth client credentials
// ---------------------------------------------------------------------------

/// The client identity used against one authorization server: either obtained
/// through dynamic client registration or the static fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OAuthClientCredentials {
    client_id: String,
    client_secret: Option<String>,
    /// `false` for the static fallback id, which must not be persisted as a
    /// registered client.
    registered: bool,
}

impl OAuthClientCredentials {
    fn fallback() -> Self {
        Self {
            client_id: DEFAULT_CLIENT_ID.to_owned(),
            client_secret: None,
            registered: false,
        }
    }

    /// The client id to persist with the token: `None` for the static
    /// fallback, so a later login against a newly spec-compliant server is not
    /// confused by a stale id.
    fn persisted_client_id(&self) -> Option<&str> {
        self.registered.then_some(self.client_id.as_str())
    }

    fn persisted_client_secret(&self) -> Option<&str> {
        self.registered.then_some(self.client_secret.as_deref()).flatten()
    }
}

// ---------------------------------------------------------------------------
// Pending login state
// ---------------------------------------------------------------------------

/// State held while waiting for the OAuth callback redirect.
///
/// Stores endpoint URLs rather than the typed `BasicClient` to avoid
/// complex generic type parameters from the `oauth2` crate.
struct PendingLogin {
    csrf_token: CsrfToken,
    pkce_verifier: PkceCodeVerifier,
    auth_url: String,
    token_url: String,
    redirect_url: String,
    credentials: OAuthClientCredentials,
    /// RFC 8707 resource indicator, echoed on the token request when the
    /// server published protected resource metadata.
    resource: Option<String>,
}

// ---------------------------------------------------------------------------
// McpOAuthService
// ---------------------------------------------------------------------------

/// Service for MCP server OAuth 2.0 PKCE authentication.
///
/// Manages the full lifecycle: discovery → authorize → callback → token
/// exchange → storage → refresh → logout.
#[derive(Clone)]
pub struct McpOAuthService {
    token_repo: Arc<dyn IOAuthTokenRepository>,
    http_client: reqwest::Client,
    /// Mutex protecting pending login state by (user_id, oauth_state).
    pending: Arc<Mutex<HashMap<(String, String), PendingLogin>>>,
}

impl McpOAuthService {
    pub fn new(token_repo: Arc<dyn IOAuthTokenRepository>, http_client: reqwest::Client) -> Self {
        Self {
            token_repo,
            http_client,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Check whether the given server URL has a valid (non-expired) OAuth token.
    pub async fn check_oauth_status(&self, user_id: &str, server_url: &str) -> Result<OAuthStatusResponse, McpError> {
        let authenticated = self.has_valid_token(user_id, server_url).await?;
        Ok(OAuthStatusResponse { authenticated })
    }

    /// Start the OAuth PKCE login flow for the given MCP server URL.
    ///
    /// 1. Discover the authorization server (RFC 9728 → RFC 8414)
    /// 2. Start local callback server on a random port
    /// 3. Register this client for that redirect URI (RFC 7591), when supported
    /// 4. Generate PKCE challenge
    /// 5. Build authorization URL and open it in the system browser
    /// 6. Wait for the redirect with the authorization code
    /// 7. Exchange code for tokens and persist them with the client identity
    pub async fn login(&self, user_id: &str, server_url: &str) -> Result<OAuthLoginResponse, McpError> {
        let (authorize_url, listener) = self.prepare_login_flow(user_id, server_url).await?;

        // Open browser.
        debug!(url = %authorize_url, "Opening browser for OAuth authorization");
        if let Err(e) = open::that(&authorize_url) {
            warn!("Failed to open browser: {e}");
        }

        // Wait for callback.
        let (code, state) = match self.wait_for_callback(user_id, listener).await {
            Ok(callback) => callback,
            Err(e) => {
                self.clear_pending_for_user(user_id).await;
                return Ok(OAuthLoginResponse {
                    success: false,
                    error: Some(e.to_string()),
                });
            }
        };

        // Exchange code for tokens.
        match self.exchange_code(user_id, server_url, code, state).await {
            Ok(()) => Ok(OAuthLoginResponse {
                success: true,
                error: None,
            }),
            Err(e) => {
                self.clear_pending_for_user(user_id).await;
                Ok(OAuthLoginResponse {
                    success: false,
                    error: Some(e.to_string()),
                })
            }
        }
    }

    /// Logout from the given MCP server URL (delete stored token).
    ///
    /// Idempotent: returns Ok even if no token was stored.
    pub async fn logout(&self, user_id: &str, server_url: &str) -> Result<(), McpError> {
        match self.token_repo.delete(user_id, server_url).await {
            Ok(()) => {
                debug!(server_url, "OAuth token deleted");
                Ok(())
            }
            Err(aionui_db::DbError::NotFound(_)) => {
                debug!(server_url, "No OAuth token to delete (idempotent)");
                Ok(())
            }
            Err(e) => Err(McpError::Database(e)),
        }
    }

    /// Return the list of server URLs that have stored OAuth tokens.
    pub async fn get_authenticated_servers(&self, user_id: &str) -> Result<Vec<String>, McpError> {
        let urls = self.token_repo.list_authenticated_urls(user_id).await?;
        Ok(urls)
    }

    /// Get a valid access token for the given server URL.
    ///
    /// If the stored token is expired and a refresh token is available,
    /// automatically refreshes before returning.
    /// Returns `None` if no token is stored for this URL.
    pub async fn get_token(&self, user_id: &str, server_url: &str) -> Result<Option<String>, McpError> {
        let row = match self.token_repo.get_by_url(user_id, server_url).await? {
            Some(row) => row,
            None => return Ok(None),
        };

        // Check if token is expired (with safety margin).
        if let Some(expires_at) = row.expires_at {
            let now = now_ms();
            if now >= expires_at - EXPIRY_MARGIN_MS
                && let Some(refresh_token) = row.refresh_token.clone()
            {
                match self.refresh_token(user_id, &row, &refresh_token).await {
                    Ok(new_token) => return Ok(Some(new_token)),
                    Err(e) => {
                        warn!(
                            server_url,
                            error = %e,
                            "Token refresh failed, returning expired token"
                        );
                    }
                }
            }
        }

        Ok(Some(row.access_token))
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Discover endpoints, build OAuth client, generate PKCE, bind callback
    /// server, store pending state, and return the authorization URL + listener.
    async fn prepare_login_flow(&self, user_id: &str, server_url: &str) -> Result<(String, TcpListener), McpError> {
        let discovered = oauth_discovery::discover(&self.http_client, server_url).await?;

        let auth_url_str = discovered.metadata.authorization_endpoint.clone();
        let token_url_str = discovered.metadata.token_endpoint.clone();

        let auth_url =
            AuthUrl::new(auth_url_str.clone()).map_err(|e| McpError::OAuth(format!("Invalid auth URL: {e}")))?;
        let token_url =
            TokenUrl::new(token_url_str.clone()).map_err(|e| McpError::OAuth(format!("Invalid token URL: {e}")))?;

        // Bind the callback server BEFORE registering: the redirect URI the
        // server binds the client to must be the exact one we will listen on.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| McpError::OAuth(format!("Failed to bind callback server: {e}")))?;
        let callback_port = listener
            .local_addr()
            .map_err(|e| McpError::OAuth(format!("Failed to get callback port: {e}")))?
            .port();

        let redirect_url_str = format!("http://127.0.0.1:{callback_port}/callback");
        let redirect = RedirectUrl::new(redirect_url_str.clone())
            .map_err(|e| McpError::OAuth(format!("Invalid redirect URL: {e}")))?;

        let credentials = self.resolve_client_credentials(&discovered, &redirect_url_str).await?;

        let client = build_client(&credentials)
            .set_auth_uri(auth_url)
            .set_token_uri(token_url)
            .set_redirect_uri(redirect);

        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

        let mut authorize_request = client
            .authorize_url(CsrfToken::new_random)
            .set_pkce_challenge(pkce_challenge);
        // RFC 8707: tell the authorization server which resource the token is
        // for, so it can audience-restrict it to this MCP server.
        if let Some(resource) = discovered.resource.as_deref() {
            authorize_request = authorize_request.add_extra_param("resource", resource);
        }
        for scope in discovered.scopes.iter().flatten() {
            authorize_request = authorize_request.add_scope(Scope::new(scope.clone()));
        }
        let (authorize_url, csrf_token) = authorize_request.url();

        {
            let mut pending = self.pending.lock().await;
            let state = csrf_token.secret().clone();
            pending.insert(
                (user_id.to_string(), state),
                PendingLogin {
                    csrf_token,
                    pkce_verifier,
                    auth_url: auth_url_str,
                    token_url: token_url_str,
                    redirect_url: redirect_url_str,
                    credentials,
                    resource: discovered.resource,
                },
            );
        }

        Ok((authorize_url.to_string(), listener))
    }

    /// Obtain the client identity for this authorization server.
    ///
    /// Registers dynamically (RFC 7591) when the server advertises a
    /// registration endpoint — the only way a loopback redirect URI on a random
    /// port can be accepted — and otherwise falls back to the static client id.
    async fn resolve_client_credentials(
        &self,
        discovered: &DiscoveredAuthorization,
        redirect_uri: &str,
    ) -> Result<OAuthClientCredentials, McpError> {
        let Some(registration_endpoint) = discovered.metadata.registration_endpoint.as_deref() else {
            debug!("Authorization server advertises no registration endpoint; using the static client id");
            return Ok(OAuthClientCredentials::fallback());
        };

        let registered = oauth_registration::register_client(
            &self.http_client,
            registration_endpoint,
            redirect_uri,
            discovered.scopes.as_deref(),
        )
        .await?;

        info!("mcp_oauth: registered a dynamic OAuth client for this MCP server");
        Ok(OAuthClientCredentials {
            client_id: registered.client_id,
            client_secret: registered.client_secret,
            registered: true,
        })
    }

    /// Check if a valid (non-expired) token exists for the URL.
    async fn has_valid_token(&self, user_id: &str, server_url: &str) -> Result<bool, McpError> {
        let row = match self.token_repo.get_by_url(user_id, server_url).await? {
            Some(row) => row,
            None => return Ok(false),
        };

        if let Some(expires_at) = row.expires_at
            && now_ms() >= expires_at
        {
            return Ok(false);
        }

        Ok(true)
    }

    /// Wait for the OAuth callback redirect on the given listener.
    async fn wait_for_callback(&self, user_id: &str, listener: TcpListener) -> Result<(String, String), McpError> {
        let (code_tx, code_rx) = tokio::sync::oneshot::channel::<Result<(String, String), McpError>>();
        let pending = self.pending.clone();
        let user_id = user_id.to_string();

        tokio::spawn(async move {
            let result = Self::handle_callback_connection(&user_id, listener, pending).await;
            let _ = code_tx.send(result);
        });

        match tokio::time::timeout(CALLBACK_TIMEOUT, code_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(McpError::OAuth("Callback channel closed unexpectedly".to_string())),
            Err(_) => Err(McpError::OAuth(
                "OAuth callback timed out — no redirect received within 120s".to_string(),
            )),
        }
    }

    /// Handle a single HTTP connection on the callback server.
    async fn handle_callback_connection(
        user_id: &str,
        listener: TcpListener,
        pending: Arc<Mutex<HashMap<(String, String), PendingLogin>>>,
    ) -> Result<(String, String), McpError> {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| McpError::OAuth(format!("Failed to accept connection: {e}")))?;

        let mut buf = vec![0u8; 4096];
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| McpError::OAuth(format!("Failed to read request: {e}")))?;

        let request = String::from_utf8_lossy(&buf[..n]);
        let (code, state) = parse_callback_query(&request)?;

        // Validate CSRF state.
        let guard = pending.lock().await;
        let pending_login = guard
            .get(&(user_id.to_string(), state.clone()))
            .ok_or_else(|| McpError::OAuth("No pending login state".to_string()))?;

        if state != *pending_login.csrf_token.secret() {
            return Err(McpError::OAuth("CSRF state mismatch".to_string()));
        }

        // Send a success response to the browser.
        let response = "HTTP/1.1 200 OK\r\n\
            Content-Type: text/html; charset=utf-8\r\n\
            Connection: close\r\n\r\n\
            <html><body><h1>Authorization successful!</h1>\
            <p>You can close this window and return to AionUi.</p>\
            </body></html>";

        let _ = stream.write_all(response.as_bytes()).await;

        Ok((code, state))
    }

    /// Build a no-redirect reqwest client for OAuth token exchange.
    fn build_no_redirect_client() -> Result<reqwest::Client, McpError> {
        reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| McpError::OAuth(format!("Failed to build HTTP client: {e}")))
    }

    /// Exchange the authorization code for tokens and persist them.
    async fn exchange_code(
        &self,
        user_id: &str,
        server_url: &str,
        code: String,
        state: String,
    ) -> Result<(), McpError> {
        let pending = {
            let mut guard = self.pending.lock().await;
            guard
                .remove(&(user_id.to_string(), state))
                .ok_or_else(|| McpError::OAuth("No pending login state".to_string()))?
        };

        let auth_url = AuthUrl::new(pending.auth_url).map_err(|e| McpError::OAuth(format!("Invalid auth URL: {e}")))?;
        let token_url =
            TokenUrl::new(pending.token_url).map_err(|e| McpError::OAuth(format!("Invalid token URL: {e}")))?;
        let redirect = RedirectUrl::new(pending.redirect_url)
            .map_err(|e| McpError::OAuth(format!("Invalid redirect URL: {e}")))?;

        let client = build_client(&pending.credentials)
            .set_auth_uri(auth_url)
            .set_token_uri(token_url)
            .set_redirect_uri(redirect);

        let http_client = Self::build_no_redirect_client()?;

        let mut token_request = client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(pending.pkce_verifier);
        if let Some(resource) = pending.resource.as_deref() {
            token_request = token_request.add_extra_param("resource", resource);
        }
        let token_result = token_request
            .request_async(&http_client)
            .await
            .map_err(|e| McpError::OAuth(format!("Token exchange failed: {e}")))?;

        self.persist_token(user_id, server_url, &token_result, &pending.credentials)
            .await?;
        debug!(server_url, "OAuth tokens stored successfully");
        Ok(())
    }

    /// Refresh an expired access token using the refresh token.
    ///
    /// The refresh grant must present the SAME client that obtained the token,
    /// so the client credentials come from the stored row rather than being
    /// re-registered.
    async fn refresh_token(
        &self,
        user_id: &str,
        row: &OAuthTokenRow,
        refresh_token_value: &str,
    ) -> Result<String, McpError> {
        let server_url = row.server_url.as_str();
        let discovered = oauth_discovery::discover(&self.http_client, server_url).await?;
        let token_url = TokenUrl::new(discovered.metadata.token_endpoint.clone())
            .map_err(|e| McpError::OAuth(format!("Invalid token URL: {e}")))?;

        let credentials = match row.client_id.as_deref() {
            Some(client_id) => OAuthClientCredentials {
                client_id: client_id.to_owned(),
                client_secret: row.client_secret.clone(),
                registered: true,
            },
            None => OAuthClientCredentials::fallback(),
        };

        let client = build_client(&credentials).set_token_uri(token_url);

        let http_client = Self::build_no_redirect_client()?;

        let refresh_token = RefreshToken::new(refresh_token_value.to_string());
        let mut refresh_request = client.exchange_refresh_token(&refresh_token);
        if let Some(resource) = discovered.resource.as_deref() {
            refresh_request = refresh_request.add_extra_param("resource", resource);
        }
        let token_result = refresh_request
            .request_async(&http_client)
            .await
            .map_err(|e| McpError::OAuth(format!("Token refresh failed: {e}")))?;

        let new_access_token = token_result.access_token().secret().clone();

        let expires_at: Option<TimestampMs> = token_result.expires_in().map(|d| now_ms() + d.as_millis() as i64);

        // Prefer new refresh_token if provided, otherwise keep the old one.
        let new_refresh = token_result
            .refresh_token()
            .map(|t| t.secret().as_str())
            .unwrap_or(refresh_token_value);

        self.token_repo
            .upsert(UpsertOAuthTokenParams {
                user_id,
                server_url,
                access_token: &new_access_token,
                refresh_token: Some(new_refresh),
                token_type: "bearer",
                expires_at,
                client_id: credentials.persisted_client_id(),
                client_secret: credentials.persisted_client_secret(),
            })
            .await?;

        debug!(server_url, "OAuth token refreshed successfully");
        Ok(new_access_token)
    }

    /// Persist token response to DB, together with the client that obtained it.
    async fn persist_token<TR: TokenResponse>(
        &self,
        user_id: &str,
        server_url: &str,
        token_result: &TR,
        credentials: &OAuthClientCredentials,
    ) -> Result<(), McpError> {
        let expires_at: Option<TimestampMs> = token_result.expires_in().map(|d| now_ms() + d.as_millis() as i64);

        self.token_repo
            .upsert(UpsertOAuthTokenParams {
                user_id,
                server_url,
                access_token: token_result.access_token().secret(),
                refresh_token: token_result.refresh_token().map(|t| t.secret().as_str()),
                token_type: "bearer",
                expires_at,
                client_id: credentials.persisted_client_id(),
                client_secret: credentials.persisted_client_secret(),
            })
            .await?;

        Ok(())
    }

    /// Clear the pending login state.
    async fn clear_pending_for_user(&self, user_id: &str) {
        let mut guard = self.pending.lock().await;
        guard.retain(|(pending_user_id, _), _| pending_user_id != user_id);
    }
}

// ---------------------------------------------------------------------------
// Access token provider
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl IMcpAccessTokenProvider for McpOAuthService {
    async fn access_token(&self, user_id: &str, server_url: &str) -> Option<String> {
        match self.get_token(user_id, server_url).await {
            Ok(token) => token,
            Err(e) => {
                warn!(server_url, error = %e, "mcp_oauth: failed to read access token for MCP server");
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// OAuth client construction
// ---------------------------------------------------------------------------

/// Build the `oauth2` client for the given credentials.
///
/// A dynamically registered client that received a secret authenticates with
/// `client_secret_post`, the method MCP authorization servers most commonly
/// advertise alongside `none`; a public client sends only its id.
fn build_client(
    credentials: &OAuthClientCredentials,
) -> oauth2::Client<
    oauth2::basic::BasicErrorResponse,
    oauth2::basic::BasicTokenResponse,
    oauth2::basic::BasicTokenIntrospectionResponse,
    oauth2::StandardRevocableToken,
    oauth2::basic::BasicRevocationErrorResponse,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
> {
    let client = BasicClient::new(ClientId::new(credentials.client_id.clone()));
    match credentials.client_secret.as_ref() {
        Some(secret) => client
            .set_client_secret(ClientSecret::new(secret.clone()))
            .set_auth_type(AuthType::RequestBody),
        None => client,
    }
}

// ---------------------------------------------------------------------------
// Query parameter parsing
// ---------------------------------------------------------------------------

/// Parse `code` and `state` from the first line of an HTTP request.
///
/// Expects: `GET /callback?code=xxx&state=yyy HTTP/1.1`
fn parse_callback_query(request: &str) -> Result<(String, String), McpError> {
    let first_line = request
        .lines()
        .next()
        .ok_or_else(|| McpError::OAuth("Empty HTTP request".to_string()))?;

    let path = first_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| McpError::OAuth("Malformed HTTP request line".to_string()))?;

    let query_str = path
        .split_once('?')
        .map(|(_, q)| q)
        .ok_or_else(|| McpError::OAuth("No query parameters in callback".to_string()))?;

    let mut code = None;
    let mut state = None;

    for pair in query_str.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "code" => code = Some(url_decode(value)),
                "state" => state = Some(url_decode(value)),
                _ => {}
            }
        }
    }

    let code = code.ok_or_else(|| McpError::OAuth("Missing 'code' in callback".to_string()))?;
    let state = state.ok_or_else(|| McpError::OAuth("Missing 'state' in callback".to_string()))?;

    Ok((code, state))
}

/// Minimal percent-decoding for query parameter values.
fn url_decode(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.bytes();

    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next();
            let lo = chars.next();
            if let (Some(h), Some(l)) = (hi, lo) {
                let hex = [h, l];
                if let Ok(s) = std::str::from_utf8(&hex)
                    && let Ok(byte) = u8::from_str_radix(s, 16)
                {
                    result.push(byte as char);
                    continue;
                }
                // Malformed percent-encoding: keep as-is.
                result.push('%');
                result.push(h as char);
                result.push(l as char);
            }
        } else if b == b'+' {
            result.push(' ');
        } else {
            result.push(b as char);
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_USER_ID: &str = "user-1";

    // -- parse_callback_query ------------------------------------------------

    #[test]
    fn parse_valid_callback_query() {
        let request = "GET /callback?code=abc123&state=xyz789 HTTP/1.1\r\nHost: localhost\r\n";
        let (code, state) = parse_callback_query(request).unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state, "xyz789");
    }

    #[test]
    fn parse_callback_query_reversed_params() {
        let request = "GET /callback?state=s1&code=c1 HTTP/1.1\r\n";
        let (code, state) = parse_callback_query(request).unwrap();
        assert_eq!(code, "c1");
        assert_eq!(state, "s1");
    }

    #[test]
    fn parse_callback_query_with_extra_params() {
        let request = "GET /callback?code=c&foo=bar&state=s HTTP/1.1\r\n";
        let (code, state) = parse_callback_query(request).unwrap();
        assert_eq!(code, "c");
        assert_eq!(state, "s");
    }

    #[test]
    fn parse_callback_query_missing_code() {
        let request = "GET /callback?state=s HTTP/1.1\r\n";
        let err = parse_callback_query(request).unwrap_err();
        assert!(err.to_string().contains("Missing 'code'"));
    }

    #[test]
    fn parse_callback_query_missing_state() {
        let request = "GET /callback?code=c HTTP/1.1\r\n";
        let err = parse_callback_query(request).unwrap_err();
        assert!(err.to_string().contains("Missing 'state'"));
    }

    #[test]
    fn parse_callback_query_no_query_string() {
        let request = "GET /callback HTTP/1.1\r\n";
        let err = parse_callback_query(request).unwrap_err();
        assert!(err.to_string().contains("No query parameters"));
    }

    #[test]
    fn parse_callback_query_empty_request() {
        let err = parse_callback_query("").unwrap_err();
        assert!(err.to_string().contains("Empty HTTP request"));
    }

    // -- url_decode ----------------------------------------------------------

    #[test]
    fn url_decode_no_encoding() {
        assert_eq!(url_decode("hello"), "hello");
    }

    #[test]
    fn url_decode_percent_encoded() {
        assert_eq!(url_decode("hello%20world"), "hello world");
    }

    #[test]
    fn url_decode_plus_sign() {
        assert_eq!(url_decode("hello+world"), "hello world");
    }

    #[test]
    fn url_decode_special_characters() {
        assert_eq!(url_decode("%3D%26%3F"), "=&?");
    }

    #[test]
    fn url_decode_mixed() {
        assert_eq!(url_decode("a%20b+c%3Dd"), "a b c=d");
    }

    // -- McpOAuthService construction ----------------------------------------

    #[test]
    fn service_clone_is_independent() {
        let repo: Arc<dyn IOAuthTokenRepository> = Arc::new(MockTokenRepo);
        let http = reqwest::Client::new();
        let svc = McpOAuthService::new(repo, http);
        let _clone = svc.clone();
    }

    #[tokio::test]
    async fn clear_pending_for_user_keeps_other_user_same_oauth_state() {
        let svc = McpOAuthService::new(Arc::new(MockTokenRepo), reqwest::Client::new());
        insert_pending_login(&svc, "user-a", "shared-state").await;
        insert_pending_login(&svc, "user-b", "shared-state").await;

        svc.clear_pending_for_user("user-a").await;

        let pending = svc.pending.lock().await;
        assert!(!pending.contains_key(&("user-a".to_string(), "shared-state".to_string())));
        assert!(pending.contains_key(&("user-b".to_string(), "shared-state".to_string())));
    }

    // -- Mock repositories ---------------------------------------------------

    async fn insert_pending_login(svc: &McpOAuthService, user_id: &str, state: &str) {
        let (_, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let mut pending = svc.pending.lock().await;
        pending.insert(
            (user_id.to_string(), state.to_string()),
            PendingLogin {
                csrf_token: CsrfToken::new(state.to_string()),
                pkce_verifier,
                auth_url: "https://auth.example.com/authorize".to_string(),
                token_url: "https://auth.example.com/token".to_string(),
                redirect_url: "http://127.0.0.1/callback".to_string(),
                credentials: OAuthClientCredentials::fallback(),
                resource: None,
            },
        );
    }

    struct MockTokenRepo;

    #[async_trait::async_trait]
    impl IOAuthTokenRepository for MockTokenRepo {
        async fn get_by_url(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Option<aionui_db::models::OAuthTokenRow>, aionui_db::DbError> {
            Ok(None)
        }

        async fn upsert(
            &self,
            _: UpsertOAuthTokenParams<'_>,
        ) -> Result<aionui_db::models::OAuthTokenRow, aionui_db::DbError> {
            unimplemented!()
        }

        async fn delete(&self, _: &str, _: &str) -> Result<(), aionui_db::DbError> {
            Ok(())
        }

        async fn list_authenticated_urls(&self, _: &str) -> Result<Vec<String>, aionui_db::DbError> {
            Ok(vec![])
        }
    }

    struct IdempotentDeleteRepo;

    #[async_trait::async_trait]
    impl IOAuthTokenRepository for IdempotentDeleteRepo {
        async fn get_by_url(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Option<aionui_db::models::OAuthTokenRow>, aionui_db::DbError> {
            Ok(None)
        }

        async fn upsert(
            &self,
            _: UpsertOAuthTokenParams<'_>,
        ) -> Result<aionui_db::models::OAuthTokenRow, aionui_db::DbError> {
            unimplemented!()
        }

        async fn delete(&self, _: &str, url: &str) -> Result<(), aionui_db::DbError> {
            Err(aionui_db::DbError::NotFound(format!(
                "OAuth token for '{url}' not found"
            )))
        }

        async fn list_authenticated_urls(&self, _: &str) -> Result<Vec<String>, aionui_db::DbError> {
            Ok(vec![])
        }
    }

    struct ValidTokenRepo;

    #[async_trait::async_trait]
    impl IOAuthTokenRepository for ValidTokenRepo {
        async fn get_by_url(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Option<aionui_db::models::OAuthTokenRow>, aionui_db::DbError> {
            Ok(Some(aionui_db::models::OAuthTokenRow {
                user_id: TEST_USER_ID.to_string(),
                server_url: "https://example.com".to_string(),
                access_token: "valid_access_token".to_string(),
                refresh_token: None,
                token_type: "bearer".to_string(),
                client_id: None,
                client_secret: None,
                expires_at: Some(now_ms() + 3_600_000),
                created_at: now_ms(),
                updated_at: now_ms(),
            }))
        }

        async fn upsert(
            &self,
            _: UpsertOAuthTokenParams<'_>,
        ) -> Result<aionui_db::models::OAuthTokenRow, aionui_db::DbError> {
            unimplemented!()
        }

        async fn delete(&self, _: &str, _: &str) -> Result<(), aionui_db::DbError> {
            Ok(())
        }

        async fn list_authenticated_urls(&self, _: &str) -> Result<Vec<String>, aionui_db::DbError> {
            Ok(vec!["https://example.com".to_string()])
        }
    }

    struct ExpiredTokenRepo;

    #[async_trait::async_trait]
    impl IOAuthTokenRepository for ExpiredTokenRepo {
        async fn get_by_url(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Option<aionui_db::models::OAuthTokenRow>, aionui_db::DbError> {
            Ok(Some(aionui_db::models::OAuthTokenRow {
                user_id: TEST_USER_ID.to_string(),
                server_url: "https://example.com".to_string(),
                access_token: "expired_token".to_string(),
                refresh_token: None,
                token_type: "bearer".to_string(),
                client_id: None,
                client_secret: None,
                expires_at: Some(1000),
                created_at: 500,
                updated_at: 500,
            }))
        }

        async fn upsert(
            &self,
            _: UpsertOAuthTokenParams<'_>,
        ) -> Result<aionui_db::models::OAuthTokenRow, aionui_db::DbError> {
            unimplemented!()
        }

        async fn delete(&self, _: &str, _: &str) -> Result<(), aionui_db::DbError> {
            Ok(())
        }

        async fn list_authenticated_urls(&self, _: &str) -> Result<Vec<String>, aionui_db::DbError> {
            Ok(vec![])
        }
    }

    struct NoExpiryTokenRepo;

    #[async_trait::async_trait]
    impl IOAuthTokenRepository for NoExpiryTokenRepo {
        async fn get_by_url(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Option<aionui_db::models::OAuthTokenRow>, aionui_db::DbError> {
            Ok(Some(aionui_db::models::OAuthTokenRow {
                user_id: TEST_USER_ID.to_string(),
                server_url: "https://example.com".to_string(),
                access_token: "no_expiry_token".to_string(),
                refresh_token: None,
                token_type: "bearer".to_string(),
                client_id: None,
                client_secret: None,
                expires_at: None,
                created_at: now_ms(),
                updated_at: now_ms(),
            }))
        }

        async fn upsert(
            &self,
            _: UpsertOAuthTokenParams<'_>,
        ) -> Result<aionui_db::models::OAuthTokenRow, aionui_db::DbError> {
            unimplemented!()
        }

        async fn delete(&self, _: &str, _: &str) -> Result<(), aionui_db::DbError> {
            Ok(())
        }

        async fn list_authenticated_urls(&self, _: &str) -> Result<Vec<String>, aionui_db::DbError> {
            Ok(vec!["https://example.com".to_string()])
        }
    }

    // -- Service behavior tests ----------------------------------------------

    #[tokio::test]
    async fn check_status_no_token_returns_false() {
        let svc = McpOAuthService::new(Arc::new(MockTokenRepo), reqwest::Client::new());
        let status = svc
            .check_oauth_status(TEST_USER_ID, "https://example.com")
            .await
            .unwrap();
        assert!(!status.authenticated);
    }

    #[tokio::test]
    async fn check_status_with_valid_token() {
        let svc = McpOAuthService::new(Arc::new(ValidTokenRepo), reqwest::Client::new());
        let status = svc
            .check_oauth_status(TEST_USER_ID, "https://example.com")
            .await
            .unwrap();
        assert!(status.authenticated);
    }

    #[tokio::test]
    async fn check_status_with_expired_token() {
        let svc = McpOAuthService::new(Arc::new(ExpiredTokenRepo), reqwest::Client::new());
        let status = svc
            .check_oauth_status(TEST_USER_ID, "https://example.com")
            .await
            .unwrap();
        assert!(!status.authenticated);
    }

    #[tokio::test]
    async fn check_status_no_expiry_treated_as_valid() {
        let svc = McpOAuthService::new(Arc::new(NoExpiryTokenRepo), reqwest::Client::new());
        let status = svc
            .check_oauth_status(TEST_USER_ID, "https://example.com")
            .await
            .unwrap();
        assert!(status.authenticated);
    }

    #[tokio::test]
    async fn logout_idempotent_for_nonexistent() {
        let svc = McpOAuthService::new(Arc::new(IdempotentDeleteRepo), reqwest::Client::new());
        svc.logout(TEST_USER_ID, "https://nonexistent.example.com")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn get_authenticated_servers_empty() {
        let svc = McpOAuthService::new(Arc::new(MockTokenRepo), reqwest::Client::new());
        let urls = svc.get_authenticated_servers(TEST_USER_ID).await.unwrap();
        assert!(urls.is_empty());
    }

    #[tokio::test]
    async fn get_authenticated_servers_returns_urls() {
        let svc = McpOAuthService::new(Arc::new(ValidTokenRepo), reqwest::Client::new());
        let urls = svc.get_authenticated_servers(TEST_USER_ID).await.unwrap();
        assert_eq!(urls, vec!["https://example.com"]);
    }

    #[tokio::test]
    async fn get_token_returns_none_when_no_token() {
        let svc = McpOAuthService::new(Arc::new(MockTokenRepo), reqwest::Client::new());
        let token = svc.get_token(TEST_USER_ID, "https://example.com").await.unwrap();
        assert!(token.is_none());
    }

    #[tokio::test]
    async fn get_token_returns_access_token() {
        let svc = McpOAuthService::new(Arc::new(ValidTokenRepo), reqwest::Client::new());
        let token = svc.get_token(TEST_USER_ID, "https://example.com").await.unwrap();
        assert_eq!(token.as_deref(), Some("valid_access_token"));
    }

    #[tokio::test]
    async fn get_token_returns_expired_when_no_refresh() {
        let svc = McpOAuthService::new(Arc::new(ExpiredTokenRepo), reqwest::Client::new());
        // Expired token with no refresh_token: returns the expired token as-is.
        let token = svc.get_token(TEST_USER_ID, "https://example.com").await.unwrap();
        assert_eq!(token.as_deref(), Some("expired_token"));
    }
}
