use crate::error::AgentError;
use crate::protocol::error::AcpError;
use crate::protocol::send_error::AgentSendError;

#[derive(Debug)]
pub(super) enum AcpSendFailure {
    Agent(AgentError),
    Acp(AcpError),
}

impl AcpSendFailure {
    #[allow(dead_code)]
    pub(super) fn to_agent_send_error(&self) -> AgentSendError {
        match self {
            AcpSendFailure::Agent(err) => AgentSendError::from_agent_error_ref(err),
            AcpSendFailure::Acp(err) => AgentSendError::from_acp_error_ref(err),
        }
    }

    pub(super) fn to_agent_send_error_for_backend(&self, backend: Option<&str>) -> AgentSendError {
        match self {
            AcpSendFailure::Agent(err) => AgentSendError::from_agent_error_ref_for_backend(err, backend),
            AcpSendFailure::Acp(err) => AgentSendError::from_acp_error_ref_for_backend(err, backend),
        }
    }

    pub(super) fn into_agent_error(self) -> AgentError {
        match self {
            AcpSendFailure::Agent(err) => err,
            AcpSendFailure::Acp(err) => AgentError::Acp(err),
        }
    }
}

impl From<AgentError> for AcpSendFailure {
    fn from(err: AgentError) -> Self {
        AcpSendFailure::Agent(err)
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
            AcpSendFailure::Agent(err) => std::fmt::Display::fmt(err, f),
            AcpSendFailure::Acp(err) => f.write_str(&acp_error_public_message(err)),
        }
    }
}

impl std::error::Error for AcpSendFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AcpSendFailure::Agent(err) => Some(err),
            AcpSendFailure::Acp(err) => Some(err),
        }
    }
}

pub(super) fn is_acp_session_not_found(err: &AcpError) -> bool {
    matches!(err, AcpError::SessionNotFound { .. })
}

fn acp_error_public_message(err: &AcpError) -> String {
    match err {
        AcpError::AgentInternal { code, .. } => format!("Agent internal error (code {code})"),
        _ => err.to_string(),
    }
}
