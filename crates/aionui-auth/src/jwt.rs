use std::fmt::Write as _;
use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use dashmap::DashMap;
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use aionui_common::constants::REFRESH_COOKIE_MAX_AGE_DAYS;

use crate::error::AuthError;

/// Access-token lifetime.
///
/// Still 30 days as a stop-gap for #4124: before the refresh flow exists,
/// shortening it would reintroduce the 401/reconnect loop on remote WebUI (a
/// dead token on every request with no way to renew). It drops to a short
/// lifetime only once clients consume the refresh flow end-to-end — an
/// intentional, later contract change, not a regression.
const ACCESS_TOKEN_EXPIRY: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Refresh-token lifetime — the window a session can be renewed without
/// re-authenticating. Sliding: each refresh issues a fresh refresh token.
/// Kept in sync with the refresh cookie's `Max-Age` so the token never outlives
/// (or is outlived by) its container.
const REFRESH_TOKEN_EXPIRY: Duration = Duration::from_secs(REFRESH_COOKIE_MAX_AGE_DAYS as u64 * 24 * 60 * 60);

/// JWT issuer claim value.
const JWT_ISSUER: &str = "aionui";

/// JWT audience claim value.
const JWT_AUDIENCE: &str = "aionui-webui";

/// Which credential role a token fills.
///
/// Access tokens authenticate ordinary API/WebSocket requests; refresh tokens
/// are accepted only at the refresh endpoint to mint new access tokens. Keeping
/// the roles distinct means a leaked short-lived access token can never be
/// replayed as a long-lived refresh credential, and a refresh token can never
/// be used to call protected endpoints directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TokenType {
    /// Short-lived credential for authenticating requests. Default so tokens
    /// issued before this field existed deserialize as access tokens.
    #[default]
    Access,
    /// Long-lived credential accepted only by the refresh endpoint.
    Refresh,
}

/// JWT payload (claims embedded in the token).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenPayload {
    /// User ID.
    pub user_id: String,
    /// Username.
    pub username: String,
    /// Issued-at timestamp (seconds since UNIX epoch).
    pub iat: u64,
    /// Expiration timestamp (seconds since UNIX epoch).
    pub exp: u64,
    /// Issuer (standard JWT claim).
    pub iss: String,
    /// Audience (standard JWT claim).
    pub aud: String,
    /// User session generation at token issuance time.
    #[serde(default)]
    pub session_generation: i64,
    /// Credential role: access (default) vs refresh.
    #[serde(default)]
    pub token_type: TokenType,
}

/// JWT service for signing, verification, and token blacklisting.
///
/// Thread-safe: the secret is behind a `RwLock` and the blacklist uses `DashMap`.
pub struct JwtService {
    /// Current signing/verification secret (rotatable).
    secret: RwLock<String>,
    /// Blacklisted token hashes -> expiry timestamps.
    blacklist: DashMap<String, u64>,
}

impl JwtService {
    /// Create a new JWT service with the given secret string.
    ///
    /// The secret's bytes are used as the HMAC-SHA256 key.
    pub fn new(secret: String) -> Self {
        Self {
            secret: RwLock::new(secret),
            blacklist: DashMap::new(),
        }
    }

    /// Sign a new access JWT for the given user. Convenience for callers that
    /// do not track a session generation (tests, local mode).
    pub fn sign(&self, user_id: &str, username: &str) -> Result<String, AuthError> {
        self.sign_with_session_generation(user_id, username, 0)
    }

    /// Sign an access JWT bound to the user's current session generation.
    ///
    /// Access tokens authenticate ordinary requests and carry the
    /// [`ACCESS_TOKEN_EXPIRY`]. Pair with [`JwtService::sign_refresh`] to also
    /// issue the long-lived refresh token used to renew this one.
    pub fn sign_with_session_generation(
        &self,
        user_id: &str,
        username: &str,
        session_generation: i64,
    ) -> Result<String, AuthError> {
        self.sign_claims(
            user_id,
            username,
            session_generation,
            TokenType::Access,
            ACCESS_TOKEN_EXPIRY,
        )
    }

    /// Sign a refresh JWT bound to the user's current session generation.
    ///
    /// Accepted only by the refresh endpoint (enforced via
    /// [`JwtService::verify_refresh`]), never as a request credential. Carries
    /// the long [`REFRESH_TOKEN_EXPIRY`].
    pub fn sign_refresh(&self, user_id: &str, username: &str, session_generation: i64) -> Result<String, AuthError> {
        self.sign_claims(
            user_id,
            username,
            session_generation,
            TokenType::Refresh,
            REFRESH_TOKEN_EXPIRY,
        )
    }

    /// Encode a signed JWT for the given credential role and lifetime.
    fn sign_claims(
        &self,
        user_id: &str,
        username: &str,
        session_generation: i64,
        token_type: TokenType,
        ttl: Duration,
    ) -> Result<String, AuthError> {
        let now = now_secs()?;
        let exp = now + ttl.as_secs();

        let claims = TokenPayload {
            user_id: user_id.to_owned(),
            username: username.to_owned(),
            iat: now,
            exp,
            iss: JWT_ISSUER.to_owned(),
            aud: JWT_AUDIENCE.to_owned(),
            session_generation,
            token_type,
        };

        let secret = self
            .secret
            .read()
            .map_err(|e| AuthError::TokenInvalid(format!("Secret lock poisoned: {e}")))?;

        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .map_err(|e| AuthError::TokenInvalid(format!("JWT encoding failed: {e}")))
    }

    /// Verify a JWT and return its payload.
    ///
    /// Checks: blacklist, signature, expiration, issuer, audience.
    pub fn verify(&self, token: &str) -> Result<TokenPayload, AuthError> {
        let hash = token_hash(token);
        if self.blacklist.contains_key(&hash) {
            return Err(AuthError::TokenBlacklisted);
        }

        let secret = self
            .secret
            .read()
            .map_err(|e| AuthError::TokenInvalid(format!("Secret lock poisoned: {e}")))?;

        let mut validation = Validation::default();
        validation.set_issuer(&[JWT_ISSUER]);
        validation.set_audience(&[JWT_AUDIENCE]);

        let token_data = decode::<TokenPayload>(token, &DecodingKey::from_secret(secret.as_bytes()), &validation)
            .map_err(|e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::TokenExpired,
                _ => AuthError::TokenInvalid(format!("JWT verification failed: {e}")),
            })?;

        Ok(token_data.claims)
    }

    /// Verify a token and require it to be a [`TokenType::Access`] token.
    ///
    /// Used on the request-authentication path so a refresh token cannot be
    /// replayed as a request credential.
    pub fn verify_access(&self, token: &str) -> Result<TokenPayload, AuthError> {
        let payload = self.verify(token)?;
        if payload.token_type != TokenType::Access {
            return Err(AuthError::TokenInvalid("expected an access token".into()));
        }
        Ok(payload)
    }

    /// Verify a token and require it to be a [`TokenType::Refresh`] token.
    ///
    /// Used by the refresh endpoint so an access token cannot be exchanged for
    /// a renewed session.
    pub fn verify_refresh(&self, token: &str) -> Result<TokenPayload, AuthError> {
        let payload = self.verify(token)?;
        if payload.token_type != TokenType::Refresh {
            return Err(AuthError::TokenInvalid("expected a refresh token".into()));
        }
        Ok(payload)
    }

    /// Add a token to the blacklist.
    ///
    /// Stores the token's SHA-256 hash with its expiry time for automatic cleanup.
    pub fn blacklist_token(&self, token: &str) {
        let hash = token_hash(token);
        let exp = self
            .extract_expiry(token)
            // Fallback when the token cannot be parsed: keep the entry at least
            // as long as the longest-lived token could survive.
            .unwrap_or_else(|| now_secs().unwrap_or(0) + REFRESH_TOKEN_EXPIRY.as_secs());
        self.blacklist.insert(hash, exp);
    }

    /// Rotate the JWT secret, invalidating all previously issued tokens.
    ///
    /// Returns the new secret string for database persistence.
    pub fn rotate_secret(&self) -> Result<String, AuthError> {
        let new_secret = generate_random_secret_string();
        let mut secret = self
            .secret
            .write()
            .map_err(|e| AuthError::TokenInvalid(format!("Secret lock poisoned: {e}")))?;
        *secret = new_secret.clone();
        // All old tokens are invalid with the new secret; clear the blacklist
        self.blacklist.clear();
        tracing::info!("JWT secret rotated; all existing tokens invalidated");
        Ok(new_secret)
    }

    /// Remove expired entries from the blacklist.
    pub fn cleanup_blacklist(&self) {
        let now = now_secs().unwrap_or(0);
        self.blacklist.retain(|_, exp| *exp > now);
    }

    /// Number of entries in the blacklist (for monitoring/testing).
    pub fn blacklist_size(&self) -> usize {
        self.blacklist.len()
    }

    /// Try to extract the expiry time from a token without rejecting expired tokens.
    fn extract_expiry(&self, token: &str) -> Option<u64> {
        let secret = self.secret.read().ok()?;
        let mut validation = Validation::default();
        validation.validate_exp = false;
        validation.set_issuer(&[JWT_ISSUER]);
        validation.set_audience(&[JWT_AUDIENCE]);

        decode::<TokenPayload>(token, &DecodingKey::from_secret(secret.as_bytes()), &validation)
            .ok()
            .map(|data| data.claims.exp)
    }
}

/// Resolve the JWT secret from available sources.
///
/// Priority: environment variable -> database value -> random generation.
/// Returns `(secret_string, is_newly_generated)`.
pub fn resolve_jwt_secret(env_secret: Option<&str>, db_secret: Option<&str>) -> (String, bool) {
    // An empty value from either source is treated as absent. An empty string is
    // never a usable secret, and because this value is the storage-encryption
    // root, silently deriving an AES key from "" (e.g. a `JWT_SECRET=` empty-env
    // footgun) would encrypt every credential under a key that cannot be
    // reproduced. Normalizing here — rather than only at the composition-layer
    // call site — keeps the running server, the `secret` overwrite guard, and the
    // `secret status` report in agreement on what counts as "present".
    if let Some(s) = env_secret.filter(|s| !s.is_empty()) {
        return (s.to_owned(), false);
    }
    if let Some(s) = db_secret.filter(|s| !s.is_empty()) {
        return (s.to_owned(), false);
    }
    (generate_random_secret_string(), true)
}

/// Resolve the storage-encryption secret (the root the at-rest AES-256-GCM key
/// is derived from), decoupled from the JWT signing secret.
///
/// Priority: `AIONUI_ENCRYPTION_SECRET` env → `users.encryption_secret` db →
/// seed from `jwt_secret_seed` (the already-resolved effective JWT secret).
/// Returns `(secret_string, is_newly_seeded)`; when `true`, the caller should
/// persist it so it survives restarts and stays stable across signing-secret
/// rotations.
///
/// Seeding from the JWT secret is the zero-re-encrypt upgrade path: before this
/// split the JWT secret WAS the encryption root, so a database created under the
/// old scheme still decrypts under it. `jwt_secret_seed` is always non-empty
/// (`resolve_jwt_secret` never yields an empty secret), so the seeded root is
/// always usable. An empty value from env or db is treated as absent, mirroring
/// `resolve_jwt_secret` — an empty string is never a usable encryption root.
pub fn resolve_encryption_secret(
    env_secret: Option<&str>,
    db_secret: Option<&str>,
    jwt_secret_seed: &str,
) -> (String, bool) {
    if let Some(s) = env_secret.filter(|s| !s.is_empty()) {
        return (s.to_owned(), false);
    }
    if let Some(s) = db_secret.filter(|s| !s.is_empty()) {
        return (s.to_owned(), false);
    }
    (jwt_secret_seed.to_owned(), true)
}

/// Generate a cryptographically random 64-byte secret, base64-encoded.
pub fn generate_random_secret_string() -> String {
    let mut buf = [0u8; 64];
    // getrandom failure is fatal — mirrors aionui-common's UUID generation.
    getrandom::getrandom(&mut buf).expect("OS entropy source unavailable");
    base64::engine::general_purpose::STANDARD.encode(buf)
}

/// Current time in seconds since UNIX epoch.
fn now_secs() -> Result<u64, AuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| AuthError::TokenInvalid(format!("System clock error: {e}")))
}

/// Compute the SHA-256 hash of a token string, returned as hex.
fn token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let result = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in result {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_service() -> JwtService {
        JwtService::new("test_secret_key_for_testing".into())
    }

    #[test]
    fn sign_produces_valid_jwt_format() {
        let service = test_service();
        let token = service.sign("user_1", "admin").unwrap();
        assert!(!token.is_empty());
        assert_eq!(token.split('.').count(), 3);
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let service = test_service();
        let token = service.sign("user_1", "admin").unwrap();
        let payload = service.verify(&token).unwrap();
        assert_eq!(payload.user_id, "user_1");
        assert_eq!(payload.username, "admin");
        assert_eq!(payload.session_generation, 0);
        assert_eq!(payload.iss, JWT_ISSUER);
        assert_eq!(payload.aud, JWT_AUDIENCE);
        assert!(payload.exp > payload.iat);
    }

    #[test]
    fn token_lifetime_is_30_days() {
        // Stop-gap: the JWT lifetime is aligned with the session cookie's
        // 30-day Max-Age so the cookie no longer outlives the token. Returns to
        // a short lifetime once token refresh lands; updating it then is an
        // intentional contract change, not a regression.
        let service = test_service();
        let token = service.sign("user_1", "admin").unwrap();
        let payload = service.verify(&token).unwrap();
        assert_eq!(ACCESS_TOKEN_EXPIRY.as_secs(), 30 * 24 * 60 * 60);
        assert_eq!(payload.exp - payload.iat, 30 * 24 * 60 * 60);
    }

    #[test]
    fn sign_with_session_generation_roundtrip() {
        let service = test_service();
        let token = service.sign_with_session_generation("user_1", "admin", 7).unwrap();
        let payload = service.verify(&token).unwrap();
        assert_eq!(payload.user_id, "user_1");
        assert_eq!(payload.session_generation, 7);
    }

    #[test]
    fn verify_tampered_token_fails() {
        let service = test_service();
        let token = service.sign("user_1", "admin").unwrap();
        let tampered = format!("{token}x");
        assert!(matches!(service.verify(&tampered), Err(AuthError::TokenInvalid(_))));
    }

    #[test]
    fn verify_wrong_secret_fails() {
        let service1 = JwtService::new("secret_1".into());
        let service2 = JwtService::new("secret_2".into());
        let token = service1.sign("user_1", "admin").unwrap();
        assert!(service2.verify(&token).is_err());
    }

    #[test]
    fn verify_expired_token() {
        let service = test_service();
        let secret = service.secret.read().unwrap();

        let claims = TokenPayload {
            user_id: "user_1".into(),
            username: "admin".into(),
            iat: 1000,
            exp: 1001,
            iss: JWT_ISSUER.into(),
            aud: JWT_AUDIENCE.into(),
            session_generation: 0,
            token_type: TokenType::Access,
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();
        drop(secret);

        assert!(matches!(service.verify(&token), Err(AuthError::TokenExpired)));
    }

    #[test]
    fn blacklist_token_then_verify_fails() {
        let service = test_service();
        let token = service.sign("user_1", "admin").unwrap();
        assert!(service.verify(&token).is_ok());

        service.blacklist_token(&token);
        assert!(matches!(service.verify(&token), Err(AuthError::TokenBlacklisted)));
    }

    #[test]
    fn blacklist_size_tracking() {
        let service = test_service();
        assert_eq!(service.blacklist_size(), 0);

        let token1 = service.sign("user_1", "admin").unwrap();
        let token2 = service.sign("user_2", "user").unwrap();

        service.blacklist_token(&token1);
        assert_eq!(service.blacklist_size(), 1);

        service.blacklist_token(&token2);
        assert_eq!(service.blacklist_size(), 2);
    }

    #[test]
    fn rotate_secret_invalidates_old_tokens() {
        let service = test_service();
        let token = service.sign("user_1", "admin").unwrap();
        assert!(service.verify(&token).is_ok());

        service.rotate_secret().unwrap();
        assert!(service.verify(&token).is_err());
    }

    #[test]
    fn rotate_secret_clears_blacklist() {
        let service = test_service();
        let token = service.sign("user_1", "admin").unwrap();
        service.blacklist_token(&token);
        assert_eq!(service.blacklist_size(), 1);

        service.rotate_secret().unwrap();
        assert_eq!(service.blacklist_size(), 0);
    }

    #[test]
    fn rotate_secret_allows_new_tokens() {
        let service = test_service();
        service.rotate_secret().unwrap();

        let token = service.sign("user_1", "admin").unwrap();
        let payload = service.verify(&token).unwrap();
        assert_eq!(payload.user_id, "user_1");
    }

    #[test]
    fn cleanup_removes_expired_entries() {
        let service = test_service();
        let secret = service.secret.read().unwrap();

        // Create a token with an already-past expiry
        let claims = TokenPayload {
            user_id: "user_1".into(),
            username: "admin".into(),
            iat: 1000,
            exp: 1001,
            iss: JWT_ISSUER.into(),
            aud: JWT_AUDIENCE.into(),
            session_generation: 0,
            token_type: TokenType::Access,
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();
        drop(secret);

        service.blacklist_token(&token);
        assert_eq!(service.blacklist_size(), 1);

        service.cleanup_blacklist();
        assert_eq!(service.blacklist_size(), 0);
    }

    #[test]
    fn cleanup_keeps_valid_entries() {
        let service = test_service();
        let token = service.sign("user_1", "admin").unwrap();
        service.blacklist_token(&token);
        assert_eq!(service.blacklist_size(), 1);

        service.cleanup_blacklist();
        // Token just signed with 30d expiry should still be in blacklist
        assert_eq!(service.blacklist_size(), 1);
    }

    #[test]
    fn resolve_jwt_secret_env_priority() {
        let (secret, generated) = resolve_jwt_secret(Some("env_secret"), Some("db_secret"));
        assert_eq!(secret, "env_secret");
        assert!(!generated);
    }

    #[test]
    fn resolve_jwt_secret_db_fallback() {
        let (secret, generated) = resolve_jwt_secret(None, Some("db_secret"));
        assert_eq!(secret, "db_secret");
        assert!(!generated);
    }

    #[test]
    fn resolve_jwt_secret_generates_new() {
        let (secret, generated) = resolve_jwt_secret(None, None);
        assert!(!secret.is_empty());
        assert!(generated);
    }

    #[test]
    fn resolve_jwt_secret_treats_empty_env_as_absent() {
        // A set-but-empty JWT_SECRET (a docker-compose footgun) must not become
        // the encryption root; resolution falls through to the database value.
        let (secret, generated) = resolve_jwt_secret(Some(""), Some("db_secret"));
        assert_eq!(secret, "db_secret");
        assert!(!generated);
    }

    #[test]
    fn resolve_jwt_secret_treats_empty_sources_as_absent() {
        // Empty from both sources → fall through to random generation rather than
        // deriving a key from "".
        let (secret, generated) = resolve_jwt_secret(Some(""), Some(""));
        assert!(!secret.is_empty());
        assert!(generated);
    }

    #[test]
    fn resolve_encryption_secret_env_priority() {
        let (secret, seeded) = resolve_encryption_secret(Some("env_enc"), Some("db_enc"), "jwt_seed");
        assert_eq!(secret, "env_enc");
        assert!(!seeded);
    }

    #[test]
    fn resolve_encryption_secret_db_over_seed() {
        let (secret, seeded) = resolve_encryption_secret(None, Some("db_enc"), "jwt_seed");
        assert_eq!(secret, "db_enc");
        assert!(!seeded);
    }

    #[test]
    fn resolve_encryption_secret_seeds_from_jwt_when_absent() {
        // The upgrade path: no dedicated encryption secret yet, so it is seeded
        // from the effective JWT secret and flagged for persistence.
        let (secret, seeded) = resolve_encryption_secret(None, None, "jwt_seed");
        assert_eq!(secret, "jwt_seed");
        assert!(seeded);
    }

    #[test]
    fn resolve_encryption_secret_treats_empty_env_as_absent() {
        // A set-but-empty AIONUI_ENCRYPTION_SECRET must not become the root;
        // resolution falls through to the persisted database value.
        let (secret, seeded) = resolve_encryption_secret(Some(""), Some("db_enc"), "jwt_seed");
        assert_eq!(secret, "db_enc");
        assert!(!seeded);
    }

    #[test]
    fn resolve_encryption_secret_empty_sources_seed_from_jwt() {
        let (secret, seeded) = resolve_encryption_secret(Some(""), Some(""), "jwt_seed");
        assert_eq!(secret, "jwt_seed");
        assert!(seeded);
    }

    #[test]
    fn generate_random_secret_is_unique() {
        let s1 = generate_random_secret_string();
        let s2 = generate_random_secret_string();
        assert_ne!(s1, s2);
    }

    #[test]
    fn token_hash_is_deterministic() {
        let h1 = token_hash("test_token");
        let h2 = token_hash("test_token");
        assert_eq!(h1, h2);
    }

    #[test]
    fn token_hash_differs_for_different_inputs() {
        let h1 = token_hash("token_1");
        let h2 = token_hash("token_2");
        assert_ne!(h1, h2);
    }

    #[test]
    fn token_hash_is_64_hex_chars() {
        let h = token_hash("test");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn access_token_carries_access_type() {
        let service = test_service();
        let token = service.sign_with_session_generation("user_1", "admin", 0).unwrap();
        let payload = service.verify(&token).unwrap();
        assert_eq!(payload.token_type, TokenType::Access);
    }

    #[test]
    fn sign_refresh_produces_refresh_token() {
        let service = test_service();
        let token = service.sign_refresh("user_1", "admin", 3).unwrap();
        let payload = service.verify(&token).unwrap();
        assert_eq!(payload.token_type, TokenType::Refresh);
        assert_eq!(payload.user_id, "user_1");
        assert_eq!(payload.session_generation, 3);
        // Refresh lifetime is kept in sync with the refresh cookie's Max-Age.
        assert_eq!(payload.exp - payload.iat, REFRESH_TOKEN_EXPIRY.as_secs());
    }

    #[test]
    fn verify_access_accepts_access_but_rejects_refresh() {
        let service = test_service();
        let access = service.sign("user_1", "admin").unwrap();
        let refresh = service.sign_refresh("user_1", "admin", 0).unwrap();

        // An access token is accepted on the request path.
        assert!(service.verify_access(&access).is_ok());

        // A refresh token must never authenticate an ordinary request.
        let err = service.verify_access(&refresh).unwrap_err();
        assert!(
            matches!(&err, AuthError::TokenInvalid(msg) if msg == "expected an access token"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn verify_refresh_accepts_refresh_but_rejects_access() {
        let service = test_service();
        let access = service.sign("user_1", "admin").unwrap();
        let refresh = service.sign_refresh("user_1", "admin", 0).unwrap();

        // A refresh token is accepted at the refresh endpoint.
        assert!(service.verify_refresh(&refresh).is_ok());

        // An access token cannot be exchanged for a renewed session.
        let err = service.verify_refresh(&access).unwrap_err();
        assert!(
            matches!(&err, AuthError::TokenInvalid(msg) if msg == "expected a refresh token"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn legacy_token_without_type_defaults_to_access() {
        // A token minted before `token_type` existed carries no such claim. It
        // must deserialize as an access token (serde default) so pre-upgrade
        // sessions keep authenticating instead of being rejected as "not an
        // access token".
        #[derive(Serialize)]
        struct LegacyClaims {
            user_id: String,
            username: String,
            iat: u64,
            exp: u64,
            iss: String,
            aud: String,
            session_generation: i64,
        }

        let service = test_service();
        let now = now_secs().unwrap();
        let secret = service.secret.read().unwrap();
        let token = encode(
            &Header::default(),
            &LegacyClaims {
                user_id: "user_1".into(),
                username: "admin".into(),
                iat: now,
                exp: now + 3600,
                iss: JWT_ISSUER.into(),
                aud: JWT_AUDIENCE.into(),
                session_generation: 0,
            },
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();
        drop(secret);

        let payload = service.verify(&token).unwrap();
        assert_eq!(payload.token_type, TokenType::Access);
        // And it is therefore usable on the request-authentication path.
        assert!(service.verify_access(&token).is_ok());
    }
}
