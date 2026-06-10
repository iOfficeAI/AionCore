use aionui_ai_agent::{AcpError, AgentError, AgentSendError};
use aionui_api_types::{AgentErrorCode, AgentErrorOwnership};

fn assert_error<E: std::error::Error + Send + Sync + 'static>() {}

#[test]
fn acp_error_is_public_error_contract() {
    assert_error::<AcpError>();
}

#[test]
fn agent_send_error_classifies_owned_unauthorized_error() {
    let err = AgentSendError::from_agent_error_ref(&AgentError::unauthorized("Agent requires authentication"));

    assert_eq!(err.code(), Some(AgentErrorCode::UserAgentAuthRequired));
    assert_eq!(err.ownership(), Some(AgentErrorOwnership::UserAgent));
}

#[test]
fn agent_send_error_classifies_owned_workspace_runtime_unavailable_error() {
    let err = AgentSendError::from_agent_error_ref(&AgentError::workspace_path_runtime_unavailable(
        "/tmp/Project With Space",
    ));
    let stream = err.stream_error();

    assert_eq!(stream.code, Some(AgentErrorCode::WorkspacePathRuntimeUnavailable));
    assert_eq!(stream.workspace_path.as_deref(), Some("/tmp/Project With Space"));
}
