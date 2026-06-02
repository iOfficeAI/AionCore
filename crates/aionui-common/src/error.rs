use std::path::{Component, Path};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};

/// Application-level error with HTTP status code mapping.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
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

    #[error(
        "Workspace path contains whitespace in one or more directory names: {0}. Rename the affected directory or choose a path without whitespace in any directory name."
    )]
    WorkspacePathContainsWhitespace(String),

    #[error(
        "Workspace path contains whitespace in one or more directory names and is no longer supported for send or warmup: {0}. Rename the affected directory, then update this conversation or task to use a path without whitespace in any directory name."
    )]
    WorkspacePathContainsWhitespaceRuntimeUnsupported(String),
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

impl AppError {
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
            Self::WorkspacePathContainsWhitespace(_) => StatusCode::BAD_REQUEST,
            Self::WorkspacePathContainsWhitespaceRuntimeUnsupported(_) => StatusCode::BAD_REQUEST,
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
            Self::WorkspacePathContainsWhitespace(_) => "WORKSPACE_PATH_CONTAINS_WHITESPACE_UNSUPPORTED",
            Self::WorkspacePathContainsWhitespaceRuntimeUnsupported(_) => {
                "WORKSPACE_PATH_CONTAINS_WHITESPACE_RUNTIME_UNSUPPORTED"
            }
        }
    }

    /// Structured error metadata for clients that need stable machine-readable
    /// context in addition to the top-level error code.
    pub fn error_details(&self) -> Option<Value> {
        match self {
            Self::WorkspacePathContainsWhitespace(path) => Some(workspace_path_whitespace_details(path, "create")),
            Self::WorkspacePathContainsWhitespaceRuntimeUnsupported(path) => {
                Some(workspace_path_whitespace_details(path, "runtime"))
            }
            _ => None,
        }
    }
}

fn workspace_path_whitespace_details(path: &str, operation: &str) -> Value {
    json!({
        "field": "workspace",
        "workspace_path": path,
        "offending_segments": workspace_path_whitespace_segments(Path::new(path)),
        "operation": operation,
    })
}

/// Return true when any normal directory/file name component in `path`
/// contains a Unicode whitespace character.
pub fn workspace_path_has_whitespace_segment(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(segment) => segment.to_string_lossy().chars().any(char::is_whitespace),
        _ => false,
    })
}

fn workspace_path_whitespace_segments(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(segment) => {
                let value = segment.to_string_lossy().to_string();
                if value.chars().any(char::is_whitespace) {
                    Some(value)
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect()
}

impl IntoResponse for AppError {
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
        assert_eq!(AppError::NotFound("x".into()).status_code(), StatusCode::NOT_FOUND);
        assert_eq!(AppError::BadRequest("x".into()).status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(
            AppError::Unauthorized("x".into()).status_code(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(AppError::Forbidden("x".into()).status_code(), StatusCode::FORBIDDEN);
        assert_eq!(AppError::Conflict("x".into()).status_code(), StatusCode::CONFLICT);
        assert_eq!(AppError::RateLimited.status_code(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            AppError::Internal("x".into()).status_code(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(AppError::BadGateway("x".into()).status_code(), StatusCode::BAD_GATEWAY);
        assert_eq!(AppError::Timeout("x".into()).status_code(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            AppError::UnprocessableEntity("x".into()).status_code(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            AppError::WorkspacePathContainsWhitespace("x".into()).status_code(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            AppError::WorkspacePathContainsWhitespaceRuntimeUnsupported("x".into()).status_code(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_error_codes() {
        assert_eq!(AppError::NotFound("x".into()).error_code(), "NOT_FOUND");
        assert_eq!(AppError::BadRequest("x".into()).error_code(), "BAD_REQUEST");
        assert_eq!(AppError::Unauthorized("x".into()).error_code(), "UNAUTHORIZED");
        assert_eq!(AppError::Forbidden("x".into()).error_code(), "FORBIDDEN");
        assert_eq!(
            AppError::Forbidden("path '/tmp/x' is outside the allowed sandbox".into()).error_code(),
            "PATH_OUTSIDE_SANDBOX"
        );
        assert_eq!(AppError::Conflict("x".into()).error_code(), "CONFLICT");
        assert_eq!(AppError::RateLimited.error_code(), "RATE_LIMITED");
        assert_eq!(AppError::Internal("x".into()).error_code(), "INTERNAL_ERROR");
        assert_eq!(AppError::BadGateway("x".into()).error_code(), "BAD_GATEWAY");
        assert_eq!(AppError::Timeout("x".into()).error_code(), "TIMEOUT");
        assert_eq!(
            AppError::UnprocessableEntity("x".into()).error_code(),
            "UNPROCESSABLE_ENTITY"
        );
        assert_eq!(
            AppError::WorkspacePathContainsWhitespace("x".into()).error_code(),
            "WORKSPACE_PATH_CONTAINS_WHITESPACE_UNSUPPORTED"
        );
        assert_eq!(
            AppError::WorkspacePathContainsWhitespaceRuntimeUnsupported("x".into()).error_code(),
            "WORKSPACE_PATH_CONTAINS_WHITESPACE_RUNTIME_UNSUPPORTED"
        );
    }

    #[test]
    fn test_error_display() {
        assert_eq!(AppError::NotFound("user 123".into()).to_string(), "Not found: user 123");
        assert_eq!(AppError::RateLimited.to_string(), "Rate limited");
    }

    #[test]
    fn test_into_response_status() {
        let resp = AppError::NotFound("test".into()).into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_into_response_body_format() {
        let resp = AppError::NotFound("user 42".into()).into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["success"], false);
        assert_eq!(json["error"], "Not found: user 42");
        assert_eq!(json["code"], "NOT_FOUND");
    }

    #[tokio::test]
    async fn test_rate_limited_response_body() {
        let resp = AppError::RateLimited.into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["success"], false);
        assert_eq!(json["error"], "Rate limited");
        assert_eq!(json["code"], "RATE_LIMITED");
        assert!(json.get("details").is_none());
    }

    #[tokio::test]
    async fn test_workspace_whitespace_response_contains_details() {
        let resp = AppError::WorkspacePathContainsWhitespace("/tmp/Archive ".into()).into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["code"], "WORKSPACE_PATH_CONTAINS_WHITESPACE_UNSUPPORTED");
        assert_eq!(json["details"]["field"], "workspace");
        assert_eq!(json["details"]["workspace_path"], "/tmp/Archive ");
        assert_eq!(json["details"]["offending_segments"], serde_json::json!(["Archive "]));
        assert_eq!(json["details"]["operation"], "create");
    }

    #[tokio::test]
    async fn test_workspace_runtime_whitespace_response_contains_details() {
        let resp = AppError::WorkspacePathContainsWhitespaceRuntimeUnsupported("/tmp/Archive ".into()).into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["code"], "WORKSPACE_PATH_CONTAINS_WHITESPACE_RUNTIME_UNSUPPORTED");
        assert_eq!(json["details"]["field"], "workspace");
        assert_eq!(json["details"]["workspace_path"], "/tmp/Archive ");
        assert_eq!(json["details"]["offending_segments"], serde_json::json!(["Archive "]));
        assert_eq!(json["details"]["operation"], "runtime");
    }

    #[test]
    fn test_workspace_path_has_whitespace_segment() {
        assert!(workspace_path_has_whitespace_segment(Path::new("/tmp/my project")));
        assert!(workspace_path_has_whitespace_segment(Path::new("/tmp/project ")));
        assert!(!workspace_path_has_whitespace_segment(Path::new("/tmp/my-project")));
    }

    #[test]
    fn test_workspace_path_whitespace_segments() {
        assert_eq!(
            workspace_path_whitespace_segments(Path::new("/tmp/my project/Archive ")),
            vec!["my project".to_owned(), "Archive ".to_owned()]
        );
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
        let err = AppError::NotFound("x".into());
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
