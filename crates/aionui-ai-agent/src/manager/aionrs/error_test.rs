use aionui_api_types::{AgentErrorCode, AgentErrorOwnership};

use super::*;

#[test]
fn aionrs_structured_malformed_tool_call_error_is_provider_error() {
    let error = AionrsAgentError::ToolCallMalformed { count: 3, limit: 3 };
    let send_error = aionrs_engine_error_to_send_error(&error);

    assert_eq!(send_error.code(), Some(AgentErrorCode::UserLlmProviderInvalidRequest));
    assert_eq!(send_error.ownership(), Some(AgentErrorOwnership::UserLlmProvider));
    assert_eq!(send_error.stream_error().retryable, Some(false));
}

#[test]
fn aionrs_provider_rate_limited_appends_response_body_to_detail() {
    let error = AionrsAgentError::Provider(ProviderError::RateLimited {
        retry_after_ms: 5000,
        body: Some(r#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}}"#.to_owned()),
    });
    let send_error = aionrs_engine_error_to_send_error(&error);

    assert_eq!(send_error.code(), Some(AgentErrorCode::UserLlmProviderRateLimited));
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

    assert_eq!(send_error.code(), Some(AgentErrorCode::UserLlmProviderNetworkError));
    assert_eq!(send_error.ownership(), Some(AgentErrorOwnership::UserLlmProvider));
    assert_eq!(send_error.stream_error().retryable, Some(true));
}

#[test]
fn provider_error_summary_classifies_network_without_body() {
    let error = AionrsAgentError::Provider(ProviderError::Connection("connect failed".into()));
    let summary = aionrs_runtime_error_summary(&error);

    assert_eq!(summary.kind, "provider");
    assert_eq!(summary.provider_error_class, Some("network"));
    assert_eq!(summary.http_status, None);
}

#[test]
fn tool_call_failure_summary_classifies_loop() {
    let error = AionrsAgentError::ToolCallFailures { count: 3, limit: 3 };
    let summary = aionrs_runtime_error_summary(&error);

    assert_eq!(summary.kind, "tool_call_failures");
    assert_eq!(summary.failure_count, Some(3));
    assert_eq!(summary.failure_limit, Some(3));
}

#[test]
fn aionrs_api_connection_error_is_user_llm_provider_network_error() {
    let error = AionrsAgentError::Provider(ProviderError::Connection("error decoding response body".to_owned()));
    let send_error = aionrs_engine_error_to_send_error(&error);

    assert_eq!(send_error.code(), Some(AgentErrorCode::UserLlmProviderNetworkError));
    assert_eq!(send_error.ownership(), Some(AgentErrorOwnership::UserLlmProvider));
    assert_eq!(send_error.stream_error().retryable, Some(true));
}

#[test]
fn aionrs_provider_status_error_uses_status_instead_of_message_text() {
    let error = AionrsAgentError::Provider(ProviderError::Api {
        status: 401,
        message: "credentials failed".to_owned(),
    });
    let send_error = aionrs_engine_error_to_send_error(&error);

    assert_eq!(send_error.code(), Some(AgentErrorCode::UserLlmProviderAuthFailed));
    assert_eq!(send_error.ownership(), Some(AgentErrorOwnership::UserLlmProvider));
    assert_eq!(send_error.stream_error().retryable, Some(false));
}

#[test]
fn aionrs_context_too_long_is_provider_context_error() {
    let error = AionrsAgentError::ContextTooLong {
        input_tokens: 120_000,
        limit: 100_000,
    };
    let send_error = aionrs_engine_error_to_send_error(&error);

    assert_eq!(send_error.code(), Some(AgentErrorCode::UserLlmProviderContextTooLarge));
    assert_eq!(send_error.ownership(), Some(AgentErrorOwnership::UserLlmProvider));
    assert_eq!(send_error.stream_error().retryable, Some(false));
}

#[test]
fn aionrs_repeated_malformed_tool_call_is_user_llm_provider_error() {
    let error = AionrsAgentError::ToolCallMalformed { count: 3, limit: 3 };
    let send_error = aionrs_engine_error_to_send_error(&error);

    assert_eq!(send_error.code(), Some(AgentErrorCode::UserLlmProviderInvalidRequest));
    assert_eq!(send_error.ownership(), Some(AgentErrorOwnership::UserLlmProvider));
    assert_eq!(send_error.stream_error().retryable, Some(false));
}

#[test]
fn aionrs_tool_call_failures_are_unknown_upstream_error() {
    let error = AionrsAgentError::ToolCallFailures { count: 3, limit: 3 };
    let send_error = aionrs_engine_error_to_send_error(&error);

    assert_eq!(send_error.code(), Some(AgentErrorCode::UnknownUpstreamError));
    assert_eq!(send_error.ownership(), Some(AgentErrorOwnership::UnknownUpstream));
    assert_eq!(send_error.stream_error().retryable, Some(true));
}
