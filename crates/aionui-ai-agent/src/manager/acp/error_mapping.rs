use crate::protocol::error::AcpError;
use crate::protocol::send_error::AgentSendError;
use aionui_common::AppError;

#[derive(Debug)]
pub(super) enum AcpSendFailure {
    App(AppError),
    Acp(AcpError),
}

impl AcpSendFailure {
    pub(super) fn to_agent_send_error(&self) -> AgentSendError {
        match self {
            AcpSendFailure::App(err) => AgentSendError::from_app_error_ref(err),
            AcpSendFailure::Acp(err) => AgentSendError::from_acp_error_ref(err),
        }
    }

    pub(super) fn into_app_error(self) -> AppError {
        match self {
            AcpSendFailure::App(err) => err,
            AcpSendFailure::Acp(err) => acp_error_to_app_error(err),
        }
    }
}

impl From<AppError> for AcpSendFailure {
    fn from(err: AppError) -> Self {
        AcpSendFailure::App(err)
    }
}

impl From<AcpError> for AcpSendFailure {
    fn from(err: AcpError) -> Self {
        AcpSendFailure::Acp(err)
    }
}

impl std::fmt::Display for AcpSendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpSendFailure::App(err) => std::fmt::Display::fmt(err, f),
            AcpSendFailure::Acp(err) => f.write_str(&acp_error_public_message(err)),
        }
    }
}

impl std::error::Error for AcpSendFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AcpSendFailure::App(err) => Some(err),
            AcpSendFailure::Acp(err) => Some(err),
        }
    }
}

pub(super) fn acp_error_to_app_error(err: AcpError) -> AppError {
    match &err {
        AcpError::SpawnFailed { .. } | AcpError::StartupCrash { .. } | AcpError::Disconnected { .. } => {
            AppError::BadGateway(err.to_string())
        }
        AcpError::AuthRequired => AppError::Unauthorized("Agent requires authentication".into()),
        AcpError::SessionNotFound { .. } => AppError::NotFound(err.to_string()),
        AcpError::MethodNotFound { .. } => AppError::BadRequest(err.to_string()),
        AcpError::InvalidParams { .. } => AppError::BadRequest(err.to_string()),
        AcpError::AgentInternal { .. } => AppError::BadGateway(acp_error_public_message(&err)),
        AcpError::NotConnected => AppError::Internal("ACP protocol not connected".into()),
        AcpError::InitTimeout { .. } => AppError::BadGateway(err.to_string()),
    }
}

pub(super) fn is_acp_session_not_found(err: &AcpError) -> bool {
    matches!(err, AcpError::SessionNotFound { .. })
}

pub(super) fn is_mapped_acp_session_not_found(err: &AppError) -> bool {
    matches!(err, AppError::NotFound(msg) if msg.starts_with("Session not found"))
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
    fn acp_error_to_app_error_status_codes() {
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
            let app_err = acp_error_to_app_error(acp_err);
            assert_eq!(app_err.status_code(), expected_status, "Mismatch for {app_err:?}");
        }
    }

    #[test]
    fn acp_error_to_app_error_omits_stderr_and_structured_data() {
        let startup = acp_error_to_app_error(AcpError::StartupCrash {
            exit_code: Some(1),
            signal: None,
            stderr: "Authorization: Bearer sk-secret".into(),
        });
        assert!(!startup.to_string().contains("sk-secret"));
        assert!(!startup.to_string().contains("Authorization"));

        let internal = acp_error_to_app_error(AcpError::AgentInternal {
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
