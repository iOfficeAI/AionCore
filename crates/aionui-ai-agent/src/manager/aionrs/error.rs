use aion_agent::error::AgentError as AionrsAgentError;
use aion_providers::ProviderError;
use aionui_api_types::{
    AgentErrorCode, AgentErrorOwnership, AgentErrorResolution, AgentErrorResolutionKind, AgentErrorResolutionTarget,
};

use crate::protocol::send_error::AgentSendError;

pub(super) fn aionrs_engine_error_to_send_error(error: &AionrsAgentError) -> AgentSendError {
    let detail = format!("Aionrs agent error: {error}");
    match error {
        AionrsAgentError::Provider(provider_error) => aionrs_provider_error_to_send_error(provider_error, detail),
        AionrsAgentError::ToolCallMalformed { .. } => provider_send_error(
            "The model provider repeatedly returned malformed tool calls",
            AgentErrorCode::UserLlmProviderInvalidRequest,
            detail,
            false,
            AgentErrorResolutionKind::ChangeModel,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ),
        AionrsAgentError::ToolCallFailures { .. } => tool_call_failure_send_error(detail),
        AionrsAgentError::ContextTooLong { .. } => provider_send_error(
            "The request is too large for the configured model context window",
            AgentErrorCode::UserLlmProviderContextTooLarge,
            detail,
            false,
            AgentErrorResolutionKind::ReduceContext,
            None,
        ),
        AionrsAgentError::ApiError(_) => unknown_upstream_send_error(detail),
        AionrsAgentError::UserAborted => unknown_upstream_send_error(detail),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AionrsRuntimeErrorSummary {
    pub(super) kind: &'static str,
    pub(super) provider_error_class: Option<&'static str>,
    pub(super) http_status: Option<u16>,
    pub(super) failure_count: Option<usize>,
    pub(super) failure_limit: Option<usize>,
}

impl AionrsRuntimeErrorSummary {
    fn new(kind: &'static str, provider_error_class: Option<&'static str>) -> Self {
        Self {
            kind,
            provider_error_class,
            http_status: None,
            failure_count: None,
            failure_limit: None,
        }
    }
}

pub(super) fn aionrs_runtime_error_summary(error: &AionrsAgentError) -> AionrsRuntimeErrorSummary {
    match error {
        AionrsAgentError::Provider(ProviderError::Api { status, .. }) => AionrsRuntimeErrorSummary {
            http_status: Some(*status),
            ..AionrsRuntimeErrorSummary::new("provider", Some("http_status"))
        },
        AionrsAgentError::Provider(ProviderError::Connection(_) | ProviderError::Http(_)) => {
            AionrsRuntimeErrorSummary::new("provider", Some("network"))
        }
        AionrsAgentError::Provider(ProviderError::RateLimited { .. }) => AionrsRuntimeErrorSummary {
            http_status: Some(429),
            ..AionrsRuntimeErrorSummary::new("provider", Some("rate_limited"))
        },
        AionrsAgentError::Provider(ProviderError::PromptTooLong(_)) => {
            AionrsRuntimeErrorSummary::new("provider", Some("context_too_large"))
        }
        AionrsAgentError::Provider(ProviderError::Parse(_)) => {
            AionrsRuntimeErrorSummary::new("provider", Some("parse"))
        }
        AionrsAgentError::ToolCallFailures { count, limit } => AionrsRuntimeErrorSummary {
            kind: "tool_call_failures",
            provider_error_class: None,
            http_status: None,
            failure_count: Some(*count),
            failure_limit: Some(*limit),
        },
        AionrsAgentError::ToolCallMalformed { count, limit } => AionrsRuntimeErrorSummary {
            kind: "tool_call_malformed",
            provider_error_class: None,
            http_status: None,
            failure_count: Some(*count),
            failure_limit: Some(*limit),
        },
        AionrsAgentError::ContextTooLong { .. } => {
            AionrsRuntimeErrorSummary::new("context_too_large", Some("context_too_large"))
        }
        AionrsAgentError::ApiError(_) => AionrsRuntimeErrorSummary::new("api_error", None),
        AionrsAgentError::UserAborted => AionrsRuntimeErrorSummary::new("user_aborted", None),
    }
}

fn aionrs_provider_error_to_send_error(error: &ProviderError, detail: String) -> AgentSendError {
    match error {
        ProviderError::Api { status, .. } => aionrs_provider_status_to_send_error(*status, detail),
        ProviderError::RateLimited { body, .. } => provider_send_error(
            "The model provider rate limited the request",
            AgentErrorCode::UserLlmProviderRateLimited,
            append_provider_body(detail, body.as_deref()),
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ),
        ProviderError::PromptTooLong(_) => provider_send_error(
            "The request is too large for the configured model context window",
            AgentErrorCode::UserLlmProviderContextTooLarge,
            detail,
            false,
            AgentErrorResolutionKind::ReduceContext,
            None,
        ),
        ProviderError::Connection(_) | ProviderError::Http(_) => provider_send_error(
            "The model provider could not be reached",
            AgentErrorCode::UserLlmProviderNetworkError,
            detail,
            true,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ),
        ProviderError::Parse(_) => provider_send_error(
            "The model provider returned a server error",
            AgentErrorCode::UserLlmProviderGatewayError,
            detail,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ),
    }
}

fn aionrs_provider_status_to_send_error(status: u16, detail: String) -> AgentSendError {
    match status {
        400 => provider_send_error(
            "The model provider rejected the request",
            AgentErrorCode::UserLlmProviderInvalidRequest,
            detail,
            false,
            AgentErrorResolutionKind::SendFeedback,
            Some(AgentErrorResolutionTarget::Feedback),
        ),
        401 => provider_send_error(
            "The model provider rejected the request",
            AgentErrorCode::UserLlmProviderAuthFailed,
            detail,
            false,
            AgentErrorResolutionKind::CheckProviderCredentials,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ),
        402 => provider_send_error(
            "The model provider account requires billing attention",
            AgentErrorCode::UserLlmProviderBillingRequired,
            detail,
            false,
            AgentErrorResolutionKind::CheckProviderBilling,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ),
        403 => provider_send_error(
            "The model provider denied access to the request",
            AgentErrorCode::UserLlmProviderPermissionDenied,
            detail,
            false,
            AgentErrorResolutionKind::CheckProviderCredentials,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ),
        404 => provider_send_error(
            "The model provider endpoint was not found",
            AgentErrorCode::UserLlmProviderEndpointNotFound,
            detail,
            false,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ),
        408 | 504 => provider_send_error(
            "The model provider did not respond in time",
            AgentErrorCode::UserLlmProviderTimeout,
            detail,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ),
        429 => provider_send_error(
            "The model provider rate limited the request",
            AgentErrorCode::UserLlmProviderRateLimited,
            detail,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ),
        500..=599 => provider_send_error(
            "The model provider returned a server error",
            AgentErrorCode::UserLlmProviderGatewayError,
            detail,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ),
        _ => provider_send_error(
            "The model provider returned an error",
            AgentErrorCode::UserLlmProviderGatewayError,
            detail,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ),
    }
}

fn provider_send_error(
    message: &'static str,
    code: AgentErrorCode,
    detail: String,
    retryable: bool,
    resolution_kind: AgentErrorResolutionKind,
    resolution_target: Option<AgentErrorResolutionTarget>,
) -> AgentSendError {
    AgentSendError::new(
        message,
        code,
        AgentErrorOwnership::UserLlmProvider,
        Some(detail),
        retryable,
        false,
        Some(AgentErrorResolution::new(resolution_kind, resolution_target)),
    )
}

fn unknown_upstream_send_error(detail: String) -> AgentSendError {
    AgentSendError::new(
        "The upstream Agent failed while handling the request",
        AgentErrorCode::UnknownUpstreamError,
        AgentErrorOwnership::UnknownUpstream,
        Some(detail),
        true,
        true,
        Some(AgentErrorResolution::new(
            AgentErrorResolutionKind::SendFeedback,
            Some(AgentErrorResolutionTarget::Feedback),
        )),
    )
}

/// Append the raw upstream response body (if any) to the detail string so
/// the UI's technical-details drawer surfaces provider-specific hints such
/// as `insufficient_quota`, `payment_required`, or per-endpoint rate-limit
/// notes. The body is passed through the existing `sanitize_error_detail`
/// pipeline downstream (redaction + truncation), so no extra scrubbing is
/// needed here.
fn append_provider_body(detail: String, body: Option<&str>) -> String {
    match body.map(str::trim).filter(|b| !b.is_empty()) {
        Some(body) => format!("{detail}\nProvider response: {body}"),
        None => detail,
    }
}

fn tool_call_failure_send_error(detail: String) -> AgentSendError {
    AgentSendError::new(
        "The upstream Agent repeatedly failed while executing tool calls",
        AgentErrorCode::UnknownUpstreamError,
        AgentErrorOwnership::UnknownUpstream,
        Some(detail),
        true,
        true,
        Some(AgentErrorResolution::new(AgentErrorResolutionKind::Retry, None)),
    )
}

#[cfg(test)]
#[path = "error_test.rs"]
mod error_test;
