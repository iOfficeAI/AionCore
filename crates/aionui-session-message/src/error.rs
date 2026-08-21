//! Crate-owned error type. Mapped to `ApiError` / the CLI envelope only at the
//! route boundary (AGENTS.md: service code must not depend on
//! `aionui_common::ApiError`).

use aionui_api_types::SessionToolErrorCode;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionMessageError {
    #[error("target conversation not found: {id}")]
    TargetNotFound { id: String },

    #[error("target conversation is team-owned: {id}")]
    TargetIsTeam { id: String },

    #[error("sender conversation is team-owned: {id}")]
    SenderIsTeam { id: String },

    #[error("target conversation is the sender: {id}")]
    TargetIsSelf { id: String },

    #[error("pending-delivery queue is full")]
    QueueFull,

    #[error("cross-session message rate limit tripped")]
    RateLimited,

    #[error("cross-session messaging is disabled for this user")]
    FeatureDisabled,

    #[error("stdin payload does not match the schema: {reason}")]
    SchemaValidation { reason: String },

    #[error("delivery transport unavailable: {reason}")]
    TransportUnavailable { reason: String },
}

impl SessionMessageError {
    pub fn code(&self) -> SessionToolErrorCode {
        match self {
            Self::TargetNotFound { .. } => SessionToolErrorCode::TargetNotFound,
            Self::TargetIsTeam { .. } => SessionToolErrorCode::TargetIsTeam,
            Self::SenderIsTeam { .. } => SessionToolErrorCode::SenderIsTeam,
            Self::TargetIsSelf { .. } => SessionToolErrorCode::TargetIsSelf,
            Self::QueueFull => SessionToolErrorCode::QueueFull,
            Self::RateLimited => SessionToolErrorCode::RateLimited,
            Self::FeatureDisabled => SessionToolErrorCode::FeatureDisabled,
            Self::SchemaValidation { .. } => SessionToolErrorCode::SchemaValidationFailed,
            Self::TransportUnavailable { .. } => SessionToolErrorCode::TransportUnavailable,
        }
    }

    pub fn http_status(&self) -> u16 {
        match self {
            Self::TargetNotFound { .. } => 404,
            Self::TargetIsTeam { .. } | Self::SenderIsTeam { .. } | Self::FeatureDisabled => 403,
            Self::TargetIsSelf { .. } | Self::SchemaValidation { .. } => 400,
            Self::QueueFull | Self::TransportUnavailable { .. } => 409,
            Self::RateLimited => 429,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The status/code pairing is a wire contract the CLI and every bad-path
    /// test asserts on, so it is pinned here rather than re-derived per route.
    #[test]
    fn every_error_maps_to_the_status_the_spec_pins() {
        let cases: &[(SessionMessageError, SessionToolErrorCode, u16)] = &[
            (
                SessionMessageError::TargetNotFound { id: "c".into() },
                SessionToolErrorCode::TargetNotFound,
                404,
            ),
            (
                SessionMessageError::TargetIsTeam { id: "c".into() },
                SessionToolErrorCode::TargetIsTeam,
                403,
            ),
            (
                SessionMessageError::SenderIsTeam { id: "c".into() },
                SessionToolErrorCode::SenderIsTeam,
                403,
            ),
            (
                SessionMessageError::TargetIsSelf { id: "c".into() },
                SessionToolErrorCode::TargetIsSelf,
                400,
            ),
            (SessionMessageError::QueueFull, SessionToolErrorCode::QueueFull, 409),
            (SessionMessageError::RateLimited, SessionToolErrorCode::RateLimited, 429),
            (
                SessionMessageError::FeatureDisabled,
                SessionToolErrorCode::FeatureDisabled,
                403,
            ),
            (
                SessionMessageError::SchemaValidation { reason: "r".into() },
                SessionToolErrorCode::SchemaValidationFailed,
                400,
            ),
            (
                SessionMessageError::TransportUnavailable { reason: "r".into() },
                SessionToolErrorCode::TransportUnavailable,
                409,
            ),
        ];
        for (error, code, status) in cases {
            assert_eq!(error.code(), *code, "{error}");
            assert_eq!(error.http_status(), *status, "{error}");
        }
    }

    /// `runtime_auth_failed` has no `SessionMessageError` variant on purpose:
    /// token validation fails before any service call, so the route emits that
    /// code directly. Pinned so nobody "fixes" the apparent gap by mapping a
    /// service error onto 401.
    #[test]
    fn no_service_error_claims_runtime_auth_failed() {
        let all = [
            SessionMessageError::TargetNotFound { id: "c".into() },
            SessionMessageError::TargetIsTeam { id: "c".into() },
            SessionMessageError::SenderIsTeam { id: "c".into() },
            SessionMessageError::TargetIsSelf { id: "c".into() },
            SessionMessageError::QueueFull,
            SessionMessageError::RateLimited,
            SessionMessageError::FeatureDisabled,
            SessionMessageError::SchemaValidation { reason: "r".into() },
            SessionMessageError::TransportUnavailable { reason: "r".into() },
        ];
        for error in &all {
            assert_ne!(error.code(), SessionToolErrorCode::RuntimeAuthFailed, "{error}");
            assert_ne!(error.http_status(), 401, "{error}");
        }
    }
}
