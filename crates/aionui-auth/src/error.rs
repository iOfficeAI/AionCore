/// Authentication-layer errors.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Invalid credentials")]
    InvalidCredentials,

    #[error("Password validation failed: {0}")]
    WeakPassword(String),

    #[error("Username validation failed: {0}")]
    InvalidUsername(String),

    #[error("Token expired")]
    TokenExpired,

    #[error("Token invalid: {0}")]
    TokenInvalid(String),

    #[error("Token blacklisted")]
    TokenBlacklisted,

    #[error("Rate limit exceeded")]
    RateLimited,

    #[error("Password hash error: {0}")]
    HashError(String),
}
