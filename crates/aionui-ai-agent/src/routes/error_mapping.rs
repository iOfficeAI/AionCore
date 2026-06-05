#![allow(clippy::disallowed_types)]

use aionui_common::ApiError;

use crate::error::AgentError;
use crate::protocol::error::AcpError;

pub(crate) fn agent_error_to_api_error(err: AgentError) -> ApiError {
    match err {
        AgentError::BadRequest(message) => ApiError::BadRequest(message),
        AgentError::Unauthorized(message) => ApiError::Unauthorized(message),
        AgentError::Forbidden(message) => ApiError::Forbidden(message),
        AgentError::NotFound(message) => ApiError::NotFound(message),
        AgentError::Conflict(message) => ApiError::Conflict(message),
        AgentError::BadGateway(message) => ApiError::BadGateway(message),
        AgentError::Timeout(message) => ApiError::Timeout(message),
        AgentError::RateLimited => ApiError::RateLimited,
        AgentError::ConversationArchived(message) => ApiError::ConversationArchived(message),
        AgentError::WorkspacePathRuntimeUnavailable(path) => ApiError::WorkspacePathRuntimeUnavailable(path),
        AgentError::Internal(message) => ApiError::Internal(message),
        AgentError::Acp(err) => acp_error_to_api_error(err),
    }
}

pub(crate) fn acp_error_to_api_error(err: AcpError) -> ApiError {
    match &err {
        AcpError::SpawnFailed { .. } | AcpError::StartupCrash { .. } | AcpError::Disconnected { .. } => {
            ApiError::BadGateway(err.to_string())
        }
        AcpError::AuthRequired => ApiError::Unauthorized("Agent requires authentication".into()),
        AcpError::SessionNotFound { .. } => ApiError::NotFound(err.to_string()),
        AcpError::MethodNotFound { .. } => ApiError::BadRequest(err.to_string()),
        AcpError::InvalidParams { .. } => ApiError::BadRequest(err.to_string()),
        AcpError::AgentInternal { .. } => ApiError::BadGateway(acp_error_public_message(&err)),
        AcpError::NotConnected => ApiError::Internal("ACP protocol not connected".into()),
        AcpError::InitTimeout { .. } => ApiError::BadGateway(err.to_string()),
    }
}

fn acp_error_public_message(err: &AcpError) -> String {
    match err {
        AcpError::AgentInternal { code, .. } => format!("Agent internal error (code {code})"),
        _ => err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn acp_error_to_api_error_status_codes() {
        let cases = vec![
            (AcpError::SpawnFailed { message: "x".into() }, StatusCode::BAD_GATEWAY),
            (AcpError::AuthRequired, StatusCode::UNAUTHORIZED),
            (
                AcpError::SessionNotFound { session_id: "s".into() },
                StatusCode::NOT_FOUND,
            ),
            (AcpError::MethodNotFound { method: "m".into() }, StatusCode::BAD_REQUEST),
            (AcpError::InvalidParams { message: "p".into() }, StatusCode::BAD_REQUEST),
            (
                AcpError::AgentInternal {
                    message: "e".into(),
                    code: -1,
                    data: None,
                },
                StatusCode::BAD_GATEWAY,
            ),
            (AcpError::NotConnected, StatusCode::INTERNAL_SERVER_ERROR),
            (AcpError::InitTimeout { timeout_secs: 30 }, StatusCode::BAD_GATEWAY),
        ];

        for (acp_err, expected_status) in cases {
            let api_err = acp_error_to_api_error(acp_err);
            assert_eq!(api_err.status_code(), expected_status, "Mismatch for {api_err:?}");
        }
    }

    #[test]
    fn acp_error_to_api_error_omits_stderr_and_structured_data() {
        let startup = acp_error_to_api_error(AcpError::StartupCrash {
            exit_code: Some(1),
            signal: None,
            stderr: "Authorization: Bearer sk-secret".into(),
        });
        assert!(!startup.to_string().contains("sk-secret"));
        assert!(!startup.to_string().contains("Authorization"));

        let internal = acp_error_to_api_error(AcpError::AgentInternal {
            message: "Internal error".into(),
            code: -32603,
            data: Some(serde_json::json!({
                "error": "Failed to connect MCP servers",
                "api_key": "sk-secret"
            })),
        });
        let rendered = internal.to_string();
        assert!(rendered.contains("Agent internal error (code -32603)"));
        assert!(!rendered.contains("Failed to connect MCP servers"));
        assert!(!rendered.contains("sk-secret"));
        assert!(!rendered.contains("api_key"));
    }
}
