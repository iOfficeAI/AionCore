use std::collections::HashMap;
use std::sync::Arc;

use aionui_api_types::{OAuthLoginResponse, OAuthStatusResponse};
use aionui_common::{TimestampMs, now_ms};
use aionui_db::{IOAuthTokenRepository, UpsertOAuthTokenParams};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RefreshToken,
    TokenResponse, TokenUrl,
};
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::error::McpError;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default OAuth client ID for MCP servers (public client, no secret).
const DEFAULT_CLIENT_ID: &str = "aionui";

/// Token expiry safety margin (refresh 5 minutes before expiration).
const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;

// ---------------------------------------------------------------------------
// Discovery response
// ---------------------------------------------------------------------------

/// OAuth Authorization Server Metadata (RFC 8414) — subset of fields we need.
#[derive(Debug, Deserialize)]
struct OAuthServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
}

// ---------------------------------------------------------------------------
// Pending login state
// ---------------------------------------------------------------------------

/// State held while waiting for the OAuth callback redirect.
///
/// Stores endpoint URLs rather than the typed `BasicClient` to avoid
/// complex generic type parameters from the `oauth2` crate. Keyed by
/// `(user_id, csrf_state)` in the map that holds these — that key match
/// itself is the CSRF check, so the CSRF token isn't stored again here.
struct PendingLogin {
    pkce_verifier: PkceCodeVerifier,
    auth_url: String,
    token_url: String,
    redirect_url: String,
    server_url: String,
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
    /// Returns immediately with an `authorize_url` for the caller to send a
    /// browser to — it does not wait for the user to complete authorization.
    /// The OAuth provider redirects back to `GET /api/mcp/oauth/callback` on
    /// this same server (see [`Self::handle_callback`]), which is reachable
    /// from anywhere this server itself is reachable from, unlike a
    /// per-login localhost listener. `redirect_base` is this server's own
    /// public origin (e.g. `https://host` or `http://127.0.0.1:port`),
    /// supplied by the HTTP layer from the inbound request.
    ///
    /// 1. Discover authorization/token endpoints
    /// 2. Generate PKCE challenge
    /// 3. Build the authorization URL, with this server's own callback route
    ///    as the redirect target
    /// 4. Stash PKCE/CSRF state keyed by (user, csrf state) for the callback
    ///    to pick up later
    pub async fn login(
        &self,
        user_id: &str,
        server_url: &str,
        redirect_base: &str,
    ) -> Result<OAuthLoginResponse, McpError> {
        match self.prepare_login_flow(user_id, server_url, redirect_base).await {
            Ok(authorize_url) => Ok(OAuthLoginResponse {
                success: true,
                authorize_url: Some(authorize_url),
                error: None,
            }),
            Err(e) => Ok(OAuthLoginResponse {
                success: false,
                authorize_url: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Handle the OAuth provider's redirect back to `GET /api/mcp/oauth/callback`.
    ///
    /// Looks up the pending login by `(user_id, state)`, exchanges the
    /// authorization code for tokens, and persists them. Clears the pending
    /// state on any failure so a retry starts a fresh login rather than
    /// reusing a burned code/verifier.
    pub async fn handle_callback(&self, user_id: &str, code: String, state: String) -> Result<(), McpError> {
        match self.exchange_code(user_id, code, state.clone()).await {
            Ok(()) => Ok(()),
            Err(e) => {
                self.clear_pending_for_user(user_id).await;
                Err(e)
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
                && let Some(ref refresh_token) = row.refresh_token
            {
                match self.refresh_token(user_id, server_url, refresh_token).await {
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

    /// Discover endpoints, build OAuth client, generate PKCE, store pending
    /// state, and return the authorization URL. The redirect target is this
    /// server's own `/api/mcp/oauth/callback` route under `redirect_base`
    /// (this server's public origin), not a per-login local listener.
    async fn prepare_login_flow(
        &self,
        user_id: &str,
        server_url: &str,
        redirect_base: &str,
    ) -> Result<String, McpError> {
        let metadata = self.discover_endpoints(server_url).await?;

        let auth_url_str = metadata.authorization_endpoint.clone();
        let token_url_str = metadata.token_endpoint.clone();

        let auth_url = AuthUrl::new(metadata.authorization_endpoint)
            .map_err(|e| McpError::OAuth(format!("Invalid auth URL: {e}")))?;
        let token_url =
            TokenUrl::new(metadata.token_endpoint).map_err(|e| McpError::OAuth(format!("Invalid token URL: {e}")))?;

        let redirect_url_str = format!("{}/api/mcp/oauth/callback", redirect_base.trim_end_matches('/'));
        let redirect = RedirectUrl::new(redirect_url_str.clone())
            .map_err(|e| McpError::OAuth(format!("Invalid redirect URL: {e}")))?;

        let client = BasicClient::new(ClientId::new(DEFAULT_CLIENT_ID.to_string()))
            .set_auth_uri(auth_url)
            .set_token_uri(token_url)
            .set_redirect_uri(redirect);

        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

        let (authorize_url, csrf_token) = client
            .authorize_url(CsrfToken::new_random)
            .set_pkce_challenge(pkce_challenge)
            .url();

        {
            let mut pending = self.pending.lock().await;
            let state = csrf_token.secret().clone();
            pending.insert(
                (user_id.to_string(), state),
                PendingLogin {
                    pkce_verifier,
                    auth_url: auth_url_str,
                    token_url: token_url_str,
                    redirect_url: redirect_url_str,
                    server_url: server_url.to_string(),
                },
            );
        }

        Ok(authorize_url.to_string())
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

    /// Discover OAuth authorization server metadata.
    ///
    /// Tries `.well-known/oauth-authorization-server` first,
    /// falls back to `.well-known/openid-configuration`.
    async fn discover_endpoints(&self, server_url: &str) -> Result<OAuthServerMetadata, McpError> {
        let base = server_url.trim_end_matches('/');

        let well_known_url = format!("{base}/.well-known/oauth-authorization-server");
        if let Ok(metadata) = self.fetch_metadata(&well_known_url).await {
            debug!(server_url, "Discovered OAuth metadata via RFC 8414");
            return Ok(metadata);
        }

        let oidc_url = format!("{base}/.well-known/openid-configuration");
        if let Ok(metadata) = self.fetch_metadata(&oidc_url).await {
            debug!(server_url, "Discovered OAuth metadata via OIDC");
            return Ok(metadata);
        }

        Err(McpError::OAuth(format!(
            "Failed to discover OAuth endpoints for '{server_url}': \
             no .well-known/oauth-authorization-server or \
             .well-known/openid-configuration found"
        )))
    }

    /// Fetch and parse OAuth server metadata from a URL.
    async fn fetch_metadata(&self, url: &str) -> Result<OAuthServerMetadata, McpError> {
        let resp = self
            .http_client
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

    /// Build a no-redirect reqwest client for OAuth token exchange.
    fn build_no_redirect_client() -> Result<reqwest::Client, McpError> {
        reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| McpError::OAuth(format!("Failed to build HTTP client: {e}")))
    }

    /// Exchange the authorization code for tokens and persist them.
    ///
    /// `server_url` is not a parameter — it comes from the `PendingLogin`
    /// stashed at login time, since the browser's callback redirect only
    /// carries `code` and `state`.
    async fn exchange_code(&self, user_id: &str, code: String, state: String) -> Result<(), McpError> {
        let (auth_url_str, token_url_str, redirect_url_str, pkce_verifier, server_url) = {
            let mut guard = self.pending.lock().await;
            let pending = guard
                .remove(&(user_id.to_string(), state))
                .ok_or_else(|| McpError::OAuth("No pending login state".to_string()))?;
            (
                pending.auth_url,
                pending.token_url,
                pending.redirect_url,
                pending.pkce_verifier,
                pending.server_url,
            )
        };

        let auth_url = AuthUrl::new(auth_url_str).map_err(|e| McpError::OAuth(format!("Invalid auth URL: {e}")))?;
        let token_url = TokenUrl::new(token_url_str).map_err(|e| McpError::OAuth(format!("Invalid token URL: {e}")))?;
        let redirect =
            RedirectUrl::new(redirect_url_str).map_err(|e| McpError::OAuth(format!("Invalid redirect URL: {e}")))?;

        let client = BasicClient::new(ClientId::new(DEFAULT_CLIENT_ID.to_string()))
            .set_auth_uri(auth_url)
            .set_token_uri(token_url)
            .set_redirect_uri(redirect);

        let http_client = Self::build_no_redirect_client()?;

        let token_result = client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(pkce_verifier)
            .request_async(&http_client)
            .await
            .map_err(|e| McpError::OAuth(format!("Token exchange failed: {e}")))?;

        self.persist_token(user_id, &server_url, &token_result).await?;
        debug!(server_url, "OAuth tokens stored successfully");
        Ok(())
    }

    /// Refresh an expired access token using the refresh token.
    async fn refresh_token(
        &self,
        user_id: &str,
        server_url: &str,
        refresh_token_value: &str,
    ) -> Result<String, McpError> {
        let metadata = self.discover_endpoints(server_url).await?;
        let token_url =
            TokenUrl::new(metadata.token_endpoint).map_err(|e| McpError::OAuth(format!("Invalid token URL: {e}")))?;

        let client = BasicClient::new(ClientId::new(DEFAULT_CLIENT_ID.to_string())).set_token_uri(token_url);

        let http_client = Self::build_no_redirect_client()?;

        let refresh_token = RefreshToken::new(refresh_token_value.to_string());
        let token_result = client
            .exchange_refresh_token(&refresh_token)
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
            })
            .await?;

        debug!(server_url, "OAuth token refreshed successfully");
        Ok(new_access_token)
    }

    /// Persist token response to DB.
    async fn persist_token<TR: TokenResponse>(
        &self,
        user_id: &str,
        server_url: &str,
        token_result: &TR,
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
// Query parameter parsing
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_USER_ID: &str = "user-1";

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

    // -- handle_callback -------------------------------------------------------
    //
    // Regression coverage for routing the OAuth callback through this
    // server's own HTTP router (keyed by (user_id, state) in `pending`)
    // instead of a per-login localhost TCP listener.

    #[tokio::test]
    async fn handle_callback_errors_for_unknown_state() {
        let svc = McpOAuthService::new(Arc::new(MockTokenRepo), reqwest::Client::new());

        let err = svc
            .handle_callback(TEST_USER_ID, "some-code".to_string(), "never-issued-state".to_string())
            .await
            .unwrap_err();

        assert!(err.to_string().contains("No pending login state"));
    }

    #[tokio::test]
    async fn handle_callback_rejects_another_users_state() {
        let svc = McpOAuthService::new(Arc::new(MockTokenRepo), reqwest::Client::new());
        insert_pending_login(&svc, "user-a", "state-a").await;

        // "user-b" trying user-a's state — the (user_id, state) key won't
        // match, so this must fail rather than authenticate as user-a.
        let err = svc
            .handle_callback("user-b", "some-code".to_string(), "state-a".to_string())
            .await
            .unwrap_err();

        assert!(err.to_string().contains("No pending login state"));
        // user-a's own pending state is untouched by user-b's failed attempt.
        let pending = svc.pending.lock().await;
        assert!(pending.contains_key(&("user-a".to_string(), "state-a".to_string())));
    }

    #[tokio::test]
    async fn handle_callback_clears_pending_state_on_failure() {
        let svc = McpOAuthService::new(Arc::new(MockTokenRepo), reqwest::Client::new());
        insert_pending_login(&svc, TEST_USER_ID, "state-x").await;

        // The stashed auth/token URLs point nowhere real, so the token
        // exchange itself will fail (network error) after the state lookup
        // succeeds. Either way, a failed callback must not leave stale
        // pending state behind for a retry to trip over.
        let _ = svc
            .handle_callback(TEST_USER_ID, "some-code".to_string(), "state-x".to_string())
            .await;

        let pending = svc.pending.lock().await;
        assert!(!pending.contains_key(&(TEST_USER_ID.to_string(), "state-x".to_string())));
    }

    // -- Mock repositories ---------------------------------------------------

    async fn insert_pending_login(svc: &McpOAuthService, user_id: &str, state: &str) {
        let (_, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let mut pending = svc.pending.lock().await;
        pending.insert(
            (user_id.to_string(), state.to_string()),
            PendingLogin {
                pkce_verifier,
                auth_url: "https://auth.example.com/authorize".to_string(),
                token_url: "https://auth.example.com/token".to_string(),
                redirect_url: "http://127.0.0.1/callback".to_string(),
                server_url: "https://mcp.example.com".to_string(),
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
