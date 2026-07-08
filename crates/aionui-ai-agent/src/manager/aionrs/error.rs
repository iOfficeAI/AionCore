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
mod tests {
    use super::*;

    #[test]
    fn aionrs_structured_malformed_tool_call_error_is_provider_error() {
        let error = AionrsAgentError::ToolCallMalformed { count: 3, limit: 3 };
        let send_error = aionrs_engine_error_to_send_error(&error);

        assert_eq!(
            send_error.code(),
            Some(aionui_api_types::AgentErrorCode::UserLlmProviderInvalidRequest)
        );
        assert_eq!(
            send_error.ownership(),
            Some(aionui_api_types::AgentErrorOwnership::UserLlmProvider)
        );
        assert_eq!(send_error.stream_error().retryable, Some(false));
    }

    #[test]
    fn aionrs_provider_rate_limited_appends_response_body_to_detail() {
        let error = AionrsAgentError::Provider(ProviderError::RateLimited {
            retry_after_ms: 5000,
            body: Some(
                r#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}}"#.to_owned(),
            ),
        });
        let send_error = aionrs_engine_error_to_send_error(&error);

        assert_eq!(
            send_error.code(),
            Some(aionui_api_types::AgentErrorCode::UserLlmProviderRateLimited)
        );
        let detail = send_error
            .stream_error()
            .detail
            .as_deref()
            .expect("rate-limited errors must carry a detail");
        assert!(
            detail.contains("Provider response: "),
            "detail should include the provider body marker; got: {detail}"
        );
        assert!(
            detail.contains("insufficient_quota"),
            "detail should surface the raw provider signal; got: {detail}"
        );
    }

    #[test]
    fn aionrs_provider_rate_limited_without_body_falls_back_to_bare_detail() {
        let error = AionrsAgentError::Provider(ProviderError::RateLimited {
            retry_after_ms: 5000,
            body: None,
        });
        let send_error = aionrs_engine_error_to_send_error(&error);

        let detail = send_error
            .stream_error()
            .detail
            .as_deref()
            .expect("rate-limited errors must carry a detail");
        assert!(
            !detail.contains("Provider response:"),
            "detail must not add the body marker when body is absent; got: {detail}"
        );
        assert!(
            detail.contains("Rate limited"),
            "detail should still include the base message; got: {detail}"
        );
    }

    #[test]
    fn aionrs_provider_rate_limited_ignores_whitespace_only_body() {
        let error = AionrsAgentError::Provider(ProviderError::RateLimited {
            retry_after_ms: 5000,
            body: Some("   \n\t  ".to_owned()),
        });
        let send_error = aionrs_engine_error_to_send_error(&error);

        let detail = send_error
            .stream_error()
            .detail
            .as_deref()
            .expect("rate-limited errors must carry a detail");
        assert!(
            !detail.contains("Provider response:"),
            "whitespace-only body should be treated as absent; got: {detail}"
        );
    }

    #[test]
    fn aionrs_provider_connection_error_is_user_llm_provider_error() {
        let error = AionrsAgentError::Provider(ProviderError::Connection(
            "Signable request error: failed to create canonical request".to_owned(),
        ));
        let send_error = aionrs_engine_error_to_send_error(&error);

        assert_eq!(
            send_error.code(),
            Some(aionui_api_types::AgentErrorCode::UserLlmProviderNetworkError)
        );
        assert_eq!(
            send_error.ownership(),
            Some(aionui_api_types::AgentErrorOwnership::UserLlmProvider)
        );
        assert_eq!(send_error.stream_error().retryable, Some(true));
    }

    #[test]
    fn aionrs_api_connection_error_is_user_llm_provider_network_error() {
        let error = AionrsAgentError::Provider(ProviderError::Connection("error decoding response body".to_owned()));
        let send_error = aionrs_engine_error_to_send_error(&error);

        assert_eq!(
            send_error.code(),
            Some(aionui_api_types::AgentErrorCode::UserLlmProviderNetworkError)
        );
        assert_eq!(
            send_error.ownership(),
            Some(aionui_api_types::AgentErrorOwnership::UserLlmProvider)
        );
        assert_eq!(send_error.stream_error().retryable, Some(true));
    }

    #[test]
    fn aionrs_provider_status_error_uses_status_instead_of_message_text() {
        let error = AionrsAgentError::Provider(ProviderError::Api {
            status: 401,
            message: "credentials failed".to_owned(),
        });
        let send_error = aionrs_engine_error_to_send_error(&error);

        assert_eq!(
            send_error.code(),
            Some(aionui_api_types::AgentErrorCode::UserLlmProviderAuthFailed)
        );
        assert_eq!(
            send_error.ownership(),
            Some(aionui_api_types::AgentErrorOwnership::UserLlmProvider)
        );
        assert_eq!(send_error.stream_error().retryable, Some(false));
    }

    #[test]
    fn aionrs_context_too_long_is_provider_context_error() {
        let error = AionrsAgentError::ContextTooLong {
            input_tokens: 120_000,
            limit: 100_000,
        };
        let send_error = aionrs_engine_error_to_send_error(&error);

        assert_eq!(
            send_error.code(),
            Some(aionui_api_types::AgentErrorCode::UserLlmProviderContextTooLarge)
        );
        assert_eq!(
            send_error.ownership(),
            Some(aionui_api_types::AgentErrorOwnership::UserLlmProvider)
        );
        assert_eq!(send_error.stream_error().retryable, Some(false));
    }

    #[test]
    fn aionrs_repeated_malformed_tool_call_is_user_llm_provider_error() {
        let error = AionrsAgentError::ToolCallMalformed { count: 3, limit: 3 };
        let send_error = aionrs_engine_error_to_send_error(&error);

        assert_eq!(
            send_error.code(),
            Some(aionui_api_types::AgentErrorCode::UserLlmProviderInvalidRequest)
        );
        assert_eq!(
            send_error.ownership(),
            Some(aionui_api_types::AgentErrorOwnership::UserLlmProvider)
        );
        assert_eq!(send_error.stream_error().retryable, Some(false));
    }

    #[test]
    fn aionrs_tool_call_failures_are_unknown_upstream_error() {
        let error = AionrsAgentError::ToolCallFailures { count: 3, limit: 3 };
        let send_error = aionrs_engine_error_to_send_error(&error);

        assert_eq!(
            send_error.code(),
            Some(aionui_api_types::AgentErrorCode::UnknownUpstreamError)
        );
        assert_eq!(
            send_error.ownership(),
            Some(aionui_api_types::AgentErrorOwnership::UnknownUpstream)
        );
        assert_eq!(send_error.stream_error().retryable, Some(true));
    }
}
