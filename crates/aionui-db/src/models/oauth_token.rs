use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `oauth_tokens` table.
///
/// Stores OAuth tokens keyed by MCP server URL.
/// Token values (`access_token`, `refresh_token`) should be stored
/// encrypted; callers handle encryption/decryption.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct OAuthTokenRow {
    pub user_id: String,
    /// MCP server URL (primary key).
    pub server_url: String,
    /// Encrypted OAuth access token.
    pub access_token: String,
    /// Encrypted OAuth refresh token (optional).
    pub refresh_token: Option<String>,
    /// Token type, typically "bearer".
    pub token_type: String,
    /// Token expiration timestamp (milliseconds).
    pub expires_at: Option<TimestampMs>,
    /// OAuth client id used for this server. `Some` when obtained through
    /// Dynamic Client Registration (RFC 7591); `None` when the static
    /// fallback client id was used. The refresh grant must present the same
    /// client that obtained the token, hence the persistence.
    pub client_id: Option<String>,
    /// Client secret issued by the authorization server at registration time.
    /// `None` for a public client (`token_endpoint_auth_method: "none"`),
    /// which is the normal case for a native app.
    pub client_secret: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
