#![allow(clippy::disallowed_types)]

use std::fs;
use std::path::Path;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};

/// API boundary error with HTTP status code mapping.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Rate limited")]
    RateLimited,

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Bad gateway: {0}")]
    BadGateway(String),

    #[error("Request timeout: {0}")]
    Timeout(String),

    #[error("Unprocessable entity: {0}")]
    UnprocessableEntity(String),

    /// The conversation exists but is archived and cannot be operated on.
    /// Example: legacy Gemini runtime conversations after the runtime was
    /// removed — the row stays readable (list + history) but send_message /
    /// resume should 410 Gone with this code so the client renders a
    /// dedicated "this conversation is archived" UI instead of a generic
    /// bad-request banner.
    #[error("Conversation archived: {0}")]
    ConversationArchived(String),

    #[error("Workspace path is unavailable: {0}")]
    WorkspacePathUnavailable(String),

    #[error("Workspace path is unavailable during execution: {0}")]
    WorkspacePathRuntimeUnavailable(String),
}

/// Internal error response body matching the `ErrorResponse` format from `aionui-api-types`.
#[derive(Serialize)]
struct ErrorBody {
    success: bool,
    error: String,
    code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
}

impl ApiError {
    /// HTTP status code for this error variant.
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::BadGateway(_) => StatusCode::BAD_GATEWAY,
            Self::Timeout(_) => StatusCode::BAD_GATEWAY,
            Self::UnprocessableEntity(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::ConversationArchived(_) => StatusCode::GONE,
            Self::WorkspacePathUnavailable(_) => StatusCode::BAD_REQUEST,
            Self::WorkspacePathRuntimeUnavailable(_) => StatusCode::BAD_REQUEST,
        }
    }

    /// Machine-readable error code string.
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "NOT_FOUND",
            Self::BadRequest(_) => "BAD_REQUEST",
            Self::Unauthorized(_) => "UNAUTHORIZED",
            Self::Forbidden(message) => {
                if message.contains("outside the allowed sandbox") {
                    "PATH_OUTSIDE_SANDBOX"
                } else {
                    "FORBIDDEN"
                }
            }
            Self::Conflict(_) => "CONFLICT",
            Self::RateLimited => "RATE_LIMITED",
            Self::Internal(_) => "INTERNAL_ERROR",
            Self::BadGateway(_) => "BAD_GATEWAY",
            Self::Timeout(_) => "TIMEOUT",
            Self::UnprocessableEntity(_) => "UNPROCESSABLE_ENTITY",
            Self::ConversationArchived(_) => "CONVERSATION_ARCHIVED",
            Self::WorkspacePathUnavailable(_) => "WORKSPACE_PATH_UNAVAILABLE",
            Self::WorkspacePathRuntimeUnavailable(_) => "WORKSPACE_PATH_RUNTIME_UNAVAILABLE",
        }
    }

    /// Structured error metadata for clients that need stable machine-readable
    /// context in addition to the top-level error code.
    pub fn error_details(&self) -> Option<Value> {
        match self {
            Self::WorkspacePathUnavailable(path) => Some(workspace_path_details(path, "create")),
            Self::WorkspacePathRuntimeUnavailable(path) => Some(workspace_path_details(path, "runtime")),
            _ => None,
        }
    }
}

fn workspace_path_details(path: &str, operation: &str) -> Value {
    json!({
        "field": "workspace",
        "workspace_path": path,
        "operation": operation,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspacePathValidationError {
    Empty,
    DoesNotExist(String),
    NotDirectory(String),
    NotAccessible { path: String, reason: String },
}

pub fn validate_workspace_path_availability(workspace: &str) -> Result<String, WorkspacePathValidationError> {
    if workspace.trim().is_empty() {
        return Err(WorkspacePathValidationError::Empty);
    }

    let path = Path::new(workspace);
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(workspace.to_owned()),
        Ok(_) => Err(WorkspacePathValidationError::NotDirectory(workspace.to_owned())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Err(WorkspacePathValidationError::DoesNotExist(workspace.to_owned()))
        }
        Err(err) => Err(WorkspacePathValidationError::NotAccessible {
            path: workspace.to_owned(),
            reason: err.to_string(),
        }),
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = ErrorBody {
            success: false,
            error: self.to_string(),
            code: self.error_code().to_owned(),
            details: self.error_details(),
        };
        (status, axum::Json(body)).into_response()
    }
}

/// Wrap an error to display its full `source()` chain as "outer: inner1: inner2" in a single log line.
pub struct ErrorChain<'a>(pub &'a (dyn std::error::Error + 'static));

impl std::fmt::Display for ErrorChain<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)?;
        let mut src = self.0.source();
        while let Some(inner) = src {
            write!(f, ": {inner}")?;
            src = inner.source();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn test_status_codes() {
        assert_eq!(ApiError::NotFound("x".into()).status_code(), StatusCode::NOT_FOUND);
        assert_eq!(ApiError::BadRequest("x".into()).status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(
            ApiError::Unauthorized("x".into()).status_code(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(ApiError::Forbidden("x".into()).status_code(), StatusCode::FORBIDDEN);
        assert_eq!(ApiError::Conflict("x".into()).status_code(), StatusCode::CONFLICT);
        assert_eq!(ApiError::RateLimited.status_code(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            ApiError::Internal("x".into()).status_code(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(ApiError::BadGateway("x".into()).status_code(), StatusCode::BAD_GATEWAY);
        assert_eq!(ApiError::Timeout("x".into()).status_code(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            ApiError::UnprocessableEntity("x".into()).status_code(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            ApiError::WorkspacePathUnavailable("x".into()).status_code(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ApiError::WorkspacePathRuntimeUnavailable("x".into()).status_code(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_error_codes() {
        assert_eq!(ApiError::NotFound("x".into()).error_code(), "NOT_FOUND");
        assert_eq!(ApiError::BadRequest("x".into()).error_code(), "BAD_REQUEST");
        assert_eq!(ApiError::Unauthorized("x".into()).error_code(), "UNAUTHORIZED");
        assert_eq!(ApiError::Forbidden("x".into()).error_code(), "FORBIDDEN");
        assert_eq!(
            ApiError::Forbidden("path '/tmp/x' is outside the allowed sandbox".into()).error_code(),
            "PATH_OUTSIDE_SANDBOX"
        );
        assert_eq!(ApiError::Conflict("x".into()).error_code(), "CONFLICT");
        assert_eq!(ApiError::RateLimited.error_code(), "RATE_LIMITED");
        assert_eq!(ApiError::Internal("x".into()).error_code(), "INTERNAL_ERROR");
        assert_eq!(ApiError::BadGateway("x".into()).error_code(), "BAD_GATEWAY");
        assert_eq!(ApiError::Timeout("x".into()).error_code(), "TIMEOUT");
        assert_eq!(
            ApiError::UnprocessableEntity("x".into()).error_code(),
            "UNPROCESSABLE_ENTITY"
        );
        assert_eq!(
            ApiError::WorkspacePathUnavailable("x".into()).error_code(),
            "WORKSPACE_PATH_UNAVAILABLE"
        );
        assert_eq!(
            ApiError::WorkspacePathRuntimeUnavailable("x".into()).error_code(),
            "WORKSPACE_PATH_RUNTIME_UNAVAILABLE"
        );
    }

    #[test]
    fn test_error_display() {
        assert_eq!(ApiError::NotFound("user 123".into()).to_string(), "Not found: user 123");
        assert_eq!(ApiError::RateLimited.to_string(), "Rate limited");
    }

    #[test]
    fn test_into_response_status() {
        let resp = ApiError::NotFound("test".into()).into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_into_response_body_format() {
        let resp = ApiError::NotFound("user 42".into()).into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["success"], false);
        assert_eq!(json["error"], "Not found: user 42");
        assert_eq!(json["code"], "NOT_FOUND");
    }

    #[tokio::test]
    async fn test_rate_limited_response_body() {
        let resp = ApiError::RateLimited.into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["success"], false);
        assert_eq!(json["error"], "Rate limited");
        assert_eq!(json["code"], "RATE_LIMITED");
        assert!(json.get("details").is_none());
    }

    #[tokio::test]
    async fn test_workspace_unavailable_response_contains_details() {
        let resp = ApiError::WorkspacePathUnavailable("/tmp/Archive ".into()).into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["code"], "WORKSPACE_PATH_UNAVAILABLE");
        assert_eq!(json["details"]["field"], "workspace");
        assert_eq!(json["details"]["workspace_path"], "/tmp/Archive ");
        assert_eq!(json["details"]["operation"], "create");
    }

    #[tokio::test]
    async fn test_workspace_runtime_unavailable_response_contains_details() {
        let resp = ApiError::WorkspacePathRuntimeUnavailable("/tmp/Archive ".into()).into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["code"], "WORKSPACE_PATH_RUNTIME_UNAVAILABLE");
        assert_eq!(json["details"]["field"], "workspace");
        assert_eq!(json["details"]["workspace_path"], "/tmp/Archive ");
        assert_eq!(json["details"]["operation"], "runtime");
    }

    #[test]
    fn test_validate_workspace_path_availability() {
        let dir = std::env::temp_dir().join(format!("aionui-common-{}", crate::generate_short_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let workspace = dir.join("my project");
        std::fs::create_dir_all(&workspace).unwrap();
        let file = dir.join("file.txt");
        std::fs::write(&file, "x").unwrap();

        assert_eq!(
            validate_workspace_path_availability(&workspace.to_string_lossy()),
            Ok(workspace.to_string_lossy().to_string())
        );
        assert_eq!(
            validate_workspace_path_availability("   "),
            Err(WorkspacePathValidationError::Empty)
        );
        assert!(matches!(
            validate_workspace_path_availability(&dir.join("missing").to_string_lossy()),
            Err(WorkspacePathValidationError::DoesNotExist(_))
        ));
        assert!(matches!(
            validate_workspace_path_availability(&file.to_string_lossy()),
            Err(WorkspacePathValidationError::NotDirectory(_))
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[derive(Debug, thiserror::Error)]
    #[error("inner cause")]
    struct Inner;

    #[derive(Debug, thiserror::Error)]
    #[error("outer: {message}")]
    struct Outer {
        message: String,
        #[source]
        source: Inner,
    }

    #[test]
    fn test_error_chain_single_error() {
        let err = ApiError::NotFound("x".into());
        assert_eq!(format!("{}", ErrorChain(&err)), err.to_string());
    }

    #[test]
    fn test_error_chain_nested() {
        let err = Outer {
            message: "boom".into(),
            source: Inner,
        };
        assert_eq!(format!("{}", ErrorChain(&err)), "outer: boom: inner cause");
    }
}
