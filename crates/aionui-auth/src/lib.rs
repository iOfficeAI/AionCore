#![warn(clippy::disallowed_types)]

//! JWT authentication, password hashing, CSRF protection, rate limiting, and auth middleware.
mod account;
mod cookie;
mod csrf;
mod error;
mod extract;
mod jwt;
pub mod middleware;
mod password;
pub mod qr_token;
mod rate_limit;
mod routes;
mod secret;
mod security;
mod service;
mod validation;

// Error type
pub use error::AuthError;

// Local-account password provisioning
pub use account::{AccountError, PasswordOutcome, create_local_user, set_local_password};

// Storage-encryption-secret provisioning and inspection
pub use secret::{
    SecretError, SecretSource, SecretStatus, SecretWriteOutcome, generate_secret, read_secret_status,
    secret_fingerprint, set_secret,
};

// JWT service
pub use jwt::{JwtService, TokenPayload, generate_random_secret_string, resolve_encryption_secret, resolve_jwt_secret};

// Password service
pub use password::{
    dummy_password_hash, generate_password, generate_user_credentials, hash_password, verify_password,
    verify_password_timed,
};

// Validation
pub use validation::{validate_password, validate_username};

// Rate limiting
pub use rate_limit::{
    RateLimiter, api_rate_limit_middleware, auth_rate_limit_middleware, authenticated_action_rate_limit_middleware,
};

// Token / IP extraction
pub use extract::{
    extract_client_ip, extract_client_ip_from_headers, extract_cookie_value, extract_token_from_headers,
    extract_token_from_ws_headers,
};

// Cookie configuration
pub use cookie::CookieConfig;

// Security headers
pub use security::security_headers_middleware;

// CSRF protection
pub use csrf::csrf_middleware;

// Auth middleware
pub use middleware::{
    AuthIdentityMode, AuthState, CurrentUser, IRuntimeTokenVerifier, RUNTIME_CONVERSATION_ID_HEADER,
    RUNTIME_TOKEN_HEADER, RUNTIME_USER_ID_HEADER, auth_middleware, local_auth_middleware,
};

// QR token store
pub use qr_token::QrTokenStore;

// Routes
pub use routes::{AuthRouterState, SessionRevokedHook, auth_routes};

pub use service::{AuthProvisionService, ProvisionError, SystemDefaultFilesystemAdopter};
