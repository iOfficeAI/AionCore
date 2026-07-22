use aionui_api_types::{
    AgentErrorCode, AgentErrorOwnership, AgentErrorResolution, AgentErrorResolutionKind, AgentErrorResolutionTarget,
    AgentStreamErrorData,
};

use super::error::AcpError;
use crate::error::AgentError;

const MAX_DETAIL_CHARS: usize = 1000;
const OPENCLAW_BACKEND: &str = "openclaw";
const OPENCLAW_GATEWAY_MESSAGE: &str = "OpenClaw Gateway is not reachable";
const OPENCLAW_GATEWAY_DETAIL: &str = "OpenClaw Gateway is not running or cannot be reached at 127.0.0.1:18789.\n\nStart OpenClaw Gateway and try again. You can run:\nopenclaw gateway status\nopenclaw gateway start";
const AWS_SSO_EXPIRED_DETAIL: &str =
    "Token is expired. To refresh this SSO session run 'aws sso login' with the corresponding profile.";

#[derive(Debug, Clone)]
pub struct AgentSendError {
    stream_error: AgentStreamErrorData,
}

#[derive(Debug, Clone, Copy)]
struct ClassifiedError {
    message: &'static str,
    code: AgentErrorCode,
    ownership: AgentErrorOwnership,
    retryable: bool,
    feedback_recommended: bool,
    resolution_kind: Option<AgentErrorResolutionKind>,
    resolution_target: Option<AgentErrorResolutionTarget>,
}

impl ClassifiedError {
    fn into_send_error(self, detail: String) -> AgentSendError {
        AgentSendError::new(
            self.message,
            self.code,
            self.ownership,
            Some(detail),
            self.retryable,
            self.feedback_recommended,
            self.resolution_kind
                .and_then(|kind| resolution(kind, self.resolution_target)),
        )
    }
}

impl AgentSendError {
    pub fn new(
        message: impl Into<String>,
        code: AgentErrorCode,
        ownership: AgentErrorOwnership,
        detail: Option<String>,
        retryable: bool,
        feedback_recommended: bool,
        resolution: Option<AgentErrorResolution>,
    ) -> Self {
        Self {
            stream_error: AgentStreamErrorData::classified(
                message,
                code,
                ownership,
                detail.map(|d| sanitize_error_detail(&d)),
                retryable,
                feedback_recommended,
                resolution,
            ),
        }
    }

    pub fn from_agent_error(err: AgentError) -> Self {
        Self::from_agent_error_ref(&err)
    }

    pub fn from_agent_error_ref(err: &AgentError) -> Self {
        let detail = err.public_message();
        match err {
            AgentError::WorkspacePathRuntimeUnavailable(path) => Self {
                stream_error: AgentStreamErrorData {
                    message: "Current Agent failed to run in this workspace path".into(),
                    code: Some(AgentErrorCode::WorkspacePathRuntimeUnavailable),
                    ownership: Some(AgentErrorOwnership::Aionui),
                    detail: Some(sanitize_error_detail(&detail)),
                    workspace_path: Some(path.clone()),
                    retryable: Some(false),
                    feedback_recommended: Some(false),
                    resolution: Some(AgentErrorResolution::new(
                        AgentErrorResolutionKind::StartNewSession,
                        Some(AgentErrorResolutionTarget::NewConversation),
                    )),
                },
            },
            AgentError::Internal(_) => Self::new(
                "AionUI failed while sending the message",
                AgentErrorCode::AionuiInternalError,
                AgentErrorOwnership::Aionui,
                Some(detail),
                true,
                true,
                resolution(
                    AgentErrorResolutionKind::SendFeedback,
                    Some(AgentErrorResolutionTarget::Feedback),
                ),
            ),
            AgentError::Forbidden(_) => Self::new(
                "AionUI blocked the request before it reached the Agent",
                AgentErrorCode::AionuiPermissionError,
                AgentErrorOwnership::Aionui,
                Some(detail),
                false,
                true,
                resolution(
                    AgentErrorResolutionKind::SendFeedback,
                    Some(AgentErrorResolutionTarget::Feedback),
                ),
            ),
            AgentError::Unauthorized(_) => Self::new(
                "The selected Agent requires authentication",
                AgentErrorCode::UserAgentAuthRequired,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                resolution(
                    AgentErrorResolutionKind::CheckAgentLogin,
                    Some(AgentErrorResolutionTarget::AgentSettings),
                ),
            ),
            AgentError::NotFound(msg) if msg.starts_with("Session not found") => Self::new(
                "The Agent session was not found",
                AgentErrorCode::UserAgentSessionNotFound,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                true,
                false,
                None,
            ),
            AgentError::BadRequest(msg) if msg.contains("Method not supported") => Self::new(
                "The selected Agent does not support this operation",
                AgentErrorCode::UserAgentUnsupportedMethod,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                None,
            ),
            AgentError::BadRequest(msg) if msg.contains("Invalid parameters") => Self::new(
                "The selected Agent rejected the request parameters",
                AgentErrorCode::UserAgentInvalidParams,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                None,
            ),
            AgentError::Timeout(_) => Self::new(
                "The model provider did not respond in time",
                AgentErrorCode::UserLlmProviderTimeout,
                AgentErrorOwnership::UserLlmProvider,
                Some(detail),
                true,
                false,
                resolution(AgentErrorResolutionKind::Retry, None),
            ),
            AgentError::RateLimited => Self::new(
                "The model provider rate limited the request",
                AgentErrorCode::UserLlmProviderRateLimited,
                AgentErrorOwnership::UserLlmProvider,
                Some(detail),
                true,
                false,
                resolution(AgentErrorResolutionKind::Retry, None),
            ),
            AgentError::BadGateway(_) => classify_upstream_detail(&detail),
            AgentError::Acp(err) => Self::from_acp_error_ref(err),
            _ => Self::new(
                "The upstream Agent failed while handling the request",
                AgentErrorCode::UnknownUpstreamError,
                AgentErrorOwnership::UnknownUpstream,
                Some(detail),
                true,
                true,
                resolution(
                    AgentErrorResolutionKind::SendFeedback,
                    Some(AgentErrorResolutionTarget::Feedback),
                ),
            ),
        }
    }

    pub fn from_agent_error_ref_for_backend(err: &AgentError, backend: Option<&str>) -> Self {
        match err {
            AgentError::Acp(acp_err)
                if is_openclaw_backend(backend) && openclaw_gateway_unreachable_from_acp_error(acp_err) =>
            {
                openclaw_gateway_unreachable_send_error()
            }
            AgentError::BadGateway(detail)
                if is_openclaw_backend(backend) && contains_openclaw_gateway_unreachable_signature(detail) =>
            {
                openclaw_gateway_unreachable_send_error()
            }
            _ => Self::from_agent_error_ref(err),
        }
    }

    pub fn stream_error(&self) -> &AgentStreamErrorData {
        &self.stream_error
    }

    pub fn into_stream_error(self) -> AgentStreamErrorData {
        self.stream_error
    }

    pub fn from_stream_error_data(stream_error: AgentStreamErrorData) -> Self {
        Self { stream_error }
    }

    pub fn code(&self) -> Option<AgentErrorCode> {
        self.stream_error.code
    }

    pub fn ownership(&self) -> Option<AgentErrorOwnership> {
        self.stream_error.ownership
    }

    pub(crate) fn from_acp_error_ref(err: &AcpError) -> Self {
        classify_acp_error(err)
    }

    pub(crate) fn from_acp_error_ref_for_backend(err: &AcpError, backend: Option<&str>) -> Self {
        if is_openclaw_backend(backend) && openclaw_gateway_unreachable_from_acp_error(err) {
            return openclaw_gateway_unreachable_send_error();
        }
        Self::from_acp_error_ref(err)
    }

    pub fn is_openclaw_gateway_unreachable(&self) -> bool {
        self.code() == Some(AgentErrorCode::UserAgentOpenClawGatewayUnreachable)
    }

    fn from_acp_non_internal(err: &AcpError, detail: String) -> Self {
        match err {
            AcpError::SpawnFailed { .. } => Self::new(
                "The selected Agent executable could not be started",
                AgentErrorCode::UserAgentNotInstalled,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                resolution(
                    AgentErrorResolutionKind::CheckAgentInstallation,
                    Some(AgentErrorResolutionTarget::AgentSettings),
                ),
            ),
            AcpError::StartupCrash { .. } | AcpError::InitTimeout { .. } => Self::new(
                "The selected Agent failed to start",
                AgentErrorCode::UserAgentStartupFailed,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                true,
                false,
                resolution(
                    AgentErrorResolutionKind::CheckAgentInstallation,
                    Some(AgentErrorResolutionTarget::AgentSettings),
                ),
            ),
            AcpError::Disconnected { .. } => Self::new(
                "The selected Agent disconnected",
                AgentErrorCode::UserAgentDisconnected,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                true,
                false,
                resolution(
                    AgentErrorResolutionKind::ReconnectAgent,
                    Some(AgentErrorResolutionTarget::AgentSettings),
                ),
            ),
            AcpError::AuthRequired => Self::new(
                "The selected Agent requires authentication",
                AgentErrorCode::UserAgentAuthRequired,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                resolution(
                    AgentErrorResolutionKind::CheckAgentLogin,
                    Some(AgentErrorResolutionTarget::AgentSettings),
                ),
            ),
            AcpError::ProtocolParseError { .. } => Self::new(
                "The selected Agent reported a protocol parse error",
                AgentErrorCode::UserAgentProtocolParseError,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                None,
            ),
            AcpError::InvalidRequest { .. } => Self::new(
                "The selected Agent reported an invalid protocol request",
                AgentErrorCode::UserAgentInvalidRequest,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                None,
            ),
            AcpError::SessionNotFound { .. } => Self::new(
                "The Agent session was not found",
                AgentErrorCode::UserAgentSessionNotFound,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                true,
                false,
                None,
            ),
            AcpError::ResourceNotFound { .. } => Self::new(
                "The selected Agent could not find a required resource",
                AgentErrorCode::UserAgentResourceNotFound,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                None,
            ),
            AcpError::MethodNotFound { .. } => Self::new(
                "The selected Agent does not support this operation",
                AgentErrorCode::UserAgentUnsupportedMethod,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                None,
            ),
            AcpError::InvalidParams { .. } => Self::new(
                "The selected Agent rejected the request parameters",
                AgentErrorCode::UserAgentInvalidParams,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                None,
            ),
            AcpError::NotConnected => Self::new(
                "AionUI lost its Agent protocol connection",
                AgentErrorCode::UserAgentDisconnected,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                true,
                false,
                resolution(
                    AgentErrorResolutionKind::ReconnectAgent,
                    Some(AgentErrorResolutionTarget::AgentSettings),
                ),
            ),
            AcpError::OtherProtocolError { .. } => Self::new(
                "The selected Agent returned a non-standard protocol error",
                AgentErrorCode::UserAgentProtocolError,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                false,
                false,
                None,
            ),
            AcpError::RequestTimeout { .. } => Self::new(
                "The selected Agent did not respond to the request in time",
                AgentErrorCode::UserAgentDisconnected,
                AgentErrorOwnership::UserAgent,
                Some(detail),
                true,  // retryable — the user can immediately retry the config change
                false, // feedback_recommended
                None,
            ),
            AcpError::AgentInternal { .. } => unknown_upstream_error(detail),
        }
    }
}

impl std::fmt::Display for AgentSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.stream_error.message)
    }
}

impl std::error::Error for AgentSendError {}

impl From<AcpError> for AgentSendError {
    fn from(err: AcpError) -> Self {
        Self::from_acp_error_ref(&err)
    }
}

impl From<AgentError> for AgentSendError {
    fn from(err: AgentError) -> Self {
        Self::from_agent_error(err)
    }
}

fn classify_acp_error(err: &AcpError) -> AgentSendError {
    match err {
        AcpError::AgentInternal { message, code, data } => {
            let default_detail = acp_agent_internal_public_detail(*code);
            if *code != -32603 {
                return unknown_upstream_error(default_detail);
            }

            let classified = acp_agent_internal_texts(message, data.as_ref())
                .into_iter()
                .find_map(|text| {
                    let lower = text.to_ascii_lowercase();
                    classify_agent_lifecycle(&lower)
                        .or_else(|| classify_provider_text(&lower))
                        .or_else(|| classify_aionui_state(&lower))
                        .map(|classification| (classification, text))
                });

            match classified {
                Some((classification, text)) => {
                    let detail = acp_agent_internal_detail_for_classification(*code, classification, text);
                    classification.into_send_error(detail)
                }
                None => unknown_upstream_error(default_detail),
            }
        }
        _ => AgentSendError::from_acp_non_internal(err, err.to_string()),
    }
}

fn acp_agent_internal_public_detail(code: i32) -> String {
    format!("Agent internal error (code {code})")
}

fn acp_agent_internal_detail_for_classification(code: i32, classification: ClassifiedError, text: &str) -> String {
    if classification.code == AgentErrorCode::UserLlmProviderAwsSsoExpired
        && contains_sso_expired_auth_signature(&text.to_ascii_lowercase())
    {
        return AWS_SSO_EXPIRED_DETAIL.to_owned();
    }

    let sanitized = sanitize_error_detail(text);
    if sanitized.is_empty() {
        acp_agent_internal_public_detail(code)
    } else {
        format!("{}: {}", acp_agent_internal_public_detail(code), sanitized)
    }
}

fn acp_agent_internal_texts<'a>(message: &'a str, data: Option<&'a serde_json::Value>) -> Vec<&'a str> {
    let mut texts = Vec::new();
    if let Some(data) = data {
        collect_acp_data_texts(data, &mut texts);
    }

    let trimmed = message.trim();
    if !is_uninformative_acp_message(trimmed) {
        texts.push(trimmed);
    }

    texts
}

fn collect_acp_data_texts<'a>(value: &'a serde_json::Value, texts: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::String(text) => texts.push(text.as_str()),
        serde_json::Value::Object(map) => {
            for key in ["error", "details", "message"] {
                if let Some(value) = map.get(key) {
                    collect_acp_data_texts(value, texts);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_acp_data_texts(value, texts);
            }
        }
        _ => {}
    }
}

fn is_uninformative_acp_message(message: &str) -> bool {
    message.is_empty()
        || [
            "Parse error",
            "Invalid request",
            "Method not found",
            "Invalid params",
            "Internal error",
        ]
        .iter()
        .any(|default| default.eq_ignore_ascii_case(message))
}

fn classify_upstream_detail(detail: &str) -> AgentSendError {
    let lower = detail.to_ascii_lowercase();
    let classified = classify_agent_lifecycle(&lower)
        .or_else(|| classify_provider_text(&lower))
        .or_else(|| classify_aionui_state(&lower))
        .unwrap_or_else(unknown_upstream_classification);

    classified.into_send_error(detail.to_owned())
}

fn unknown_upstream_error(detail: String) -> AgentSendError {
    unknown_upstream_classification().into_send_error(detail)
}

fn unknown_upstream_classification() -> ClassifiedError {
    ClassifiedError {
        message: "The upstream Agent failed while handling the request",
        code: AgentErrorCode::UnknownUpstreamError,
        ownership: AgentErrorOwnership::UnknownUpstream,
        retryable: true,
        feedback_recommended: false,
        resolution_kind: None,
        resolution_target: None,
    }
}

fn classify_agent_lifecycle(lower: &str) -> Option<ClassifiedError> {
    if lower.contains("agent process exited before initialize handshake completed") {
        return Some(agent_error(
            "The selected Agent exited before it finished starting",
            AgentErrorCode::UserAgentHandshakeFailed,
            true,
            AgentErrorResolutionKind::CheckAgentInstallation,
        ));
    }
    if lower.contains("process exited with code") {
        return Some(agent_error(
            "The selected Agent process exited unexpectedly",
            AgentErrorCode::UserAgentDisconnected,
            true,
            AgentErrorResolutionKind::ReconnectAgent,
        ));
    }
    if lower.contains("initialize handshake timed out") {
        return Some(agent_error(
            "The selected Agent did not finish starting in time",
            AgentErrorCode::UserAgentHandshakeTimeout,
            true,
            AgentErrorResolutionKind::ReconnectAgent,
        ));
    }
    if lower.contains("cli found but acp initialization failed") || lower.contains("找到 cli 但 acp 初始化失败")
    {
        return Some(agent_error(
            "The selected Agent CLI was found but could not initialize ACP",
            AgentErrorCode::UserAgentAcpInitFailed,
            false,
            AgentErrorResolutionKind::CheckAgentInstallation,
        ));
    }
    if lower.contains("protocol mismatch") || lower.contains("max reconnect attempts") {
        return Some(ClassifiedError {
            message: "The selected Agent protocol is incompatible",
            code: AgentErrorCode::UserAgentProtocolMismatch,
            ownership: AgentErrorOwnership::UserAgent,
            retryable: false,
            feedback_recommended: false,
            resolution_kind: None,
            resolution_target: None,
        });
    }
    if lower.contains("no previous sessions found") {
        return Some(agent_session_error(
            "No previous Agent session was found for this project",
            AgentErrorCode::UserAgentNoPreviousSession,
        ));
    }
    if lower.contains("failed to connect mcp") {
        return Some(agent_error(
            "The selected Agent disconnected from an MCP server",
            AgentErrorCode::UserAgentDisconnected,
            true,
            AgentErrorResolutionKind::ReconnectAgent,
        ));
    }
    if lower.contains("session not found") {
        return Some(agent_session_error(
            "The Agent session was not found",
            AgentErrorCode::UserAgentSessionNotFound,
        ));
    }
    if lower.contains("native binary not found")
        || (lower.contains("managed") && lower.contains("platform binary missing"))
    {
        return Some(agent_error(
            "The selected Agent failed to start",
            AgentErrorCode::UserAgentStartupFailed,
            false,
            AgentErrorResolutionKind::CheckAgentInstallation,
        ));
    }
    if lower.contains("command not found") {
        return Some(agent_error(
            "The selected Agent command was not found",
            AgentErrorCode::UserAgentCommandNotFound,
            false,
            AgentErrorResolutionKind::CheckLocalCommand,
        ));
    }
    if lower.contains("missing environment variable") {
        return Some(agent_error(
            "The selected Agent is missing a required environment variable",
            AgentErrorCode::UserAgentMissingEnv,
            false,
            AgentErrorResolutionKind::CheckAgentInstallation,
        ));
    }

    None
}

fn classify_provider_text(lower: &str) -> Option<ClassifiedError> {
    if contains_sso_expired_auth_signature(lower) {
        return Some(provider_error(
            "The model provider rejected the request",
            AgentErrorCode::UserLlmProviderAwsSsoExpired,
            false,
            AgentErrorResolutionKind::CheckProviderCredentials,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if contains_any(
        lower,
        &[
            "402",
            "insufficient balance",
            "insufficient account balance",
            "credit balance is too low",
            "add credits",
            "update billing",
            "account entitlement is exhausted",
            "purchase credits",
            "plans & billing",
            "plans and billing",
        ],
    ) {
        return Some(provider_error(
            "The model provider account requires billing attention",
            AgentErrorCode::UserLlmProviderBillingRequired,
            false,
            AgentErrorResolutionKind::CheckProviderBilling,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if contains_any(lower, &["403", "forbidden", "permission denied"]) {
        return Some(provider_error(
            "The model provider denied access to the request",
            AgentErrorCode::UserLlmProviderPermissionDenied,
            false,
            AgentErrorResolutionKind::CheckProviderCredentials,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if contains_any(
        lower,
        &[
            "401",
            "unauthorized",
            "invalid api key",
            "invalid_api_key",
            "invalid x-api-key",
            "invalid authentication credentials",
        ],
    ) {
        return Some(provider_error(
            "The model provider rejected the request",
            AgentErrorCode::UserLlmProviderAuthFailed,
            false,
            AgentErrorResolutionKind::CheckProviderCredentials,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if contains_any(
        lower,
        &[
            "signable request",
            "canonical request",
            "signature",
            "access key",
            "secret key",
            "base url",
            "base_url",
        ],
    ) {
        return Some(provider_error(
            "The model provider configuration is invalid",
            AgentErrorCode::UserLlmProviderConfigError,
            false,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if contains_any(
        lower,
        &[
            "function calling is not enabled",
            "function calling disabled",
            "unsupported model",
        ],
    ) {
        return Some(provider_error(
            "The configured model does not support this request",
            AgentErrorCode::UserLlmProviderUnsupportedModel,
            false,
            AgentErrorResolutionKind::ChangeModel,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if contains_any(
        lower,
        &[
            "context window exceeds",
            "context length",
            "context too large",
            "maximum context",
            "prompt is too long",
        ],
    ) {
        return Some(provider_error(
            "The request is too large for the configured model context window",
            AgentErrorCode::UserLlmProviderContextTooLarge,
            false,
            AgentErrorResolutionKind::ReduceContext,
            None,
        ));
    }
    if contains_any(
        lower,
        &[
            "invalid schema",
            "schema for function",
            "tool schema",
            "function schema",
        ],
    ) {
        return Some(provider_error(
            "The model provider rejected an internal tool schema",
            AgentErrorCode::UserLlmProviderInvalidToolSchema,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ));
    }
    if contains_any(
        lower,
        &[
            "repeatedly returned malformed tool calls",
            "malformed tool call loop",
            "malformed tool calls",
        ],
    ) {
        return Some(provider_error(
            "The model provider repeatedly returned malformed tool calls",
            AgentErrorCode::UserLlmProviderInvalidRequest,
            false,
            AgentErrorResolutionKind::ChangeModel,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if contains_any(
        lower,
        &[
            "model not found",
            "model does not exist",
            "unknown model",
            "invalid model",
            "model_not_found",
            "model identifier is invalid",
        ],
    ) {
        return Some(provider_error(
            "The configured model was not found by the provider",
            AgentErrorCode::UserLlmProviderModelNotFound,
            false,
            AgentErrorResolutionKind::ChangeModel,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if lower.contains("404") || lower.contains("not found") {
        return Some(provider_error(
            "The model provider endpoint was not found",
            AgentErrorCode::UserLlmProviderEndpointNotFound,
            false,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if contains_any(lower, &["429", "rate limit", "rate_limit", "quota"]) {
        return Some(provider_error(
            "The model provider rate limited the request",
            AgentErrorCode::UserLlmProviderRateLimited,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ));
    }
    if contains_any(
        lower,
        &["empty response from llm", "empty response", "response body was empty"],
    ) {
        return Some(provider_error(
            "The model provider returned an empty response",
            AgentErrorCode::UserLlmProviderEmptyResponse,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ));
    }
    if contains_any(lower, &["504", "timeout", "deadline exceeded", "gateway timeout"]) {
        return Some(provider_error(
            "The model provider did not respond in time",
            AgentErrorCode::UserLlmProviderTimeout,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ));
    }
    if contains_any(
        lower,
        &[
            "dns",
            "connection refused",
            "connection reset",
            "tls",
            "certificate",
            "connection error",
            "connect error",
            "error decoding response body",
            "decoding response body",
            "error sending request",
        ],
    ) {
        return Some(provider_error(
            "The model provider could not be reached",
            AgentErrorCode::UserLlmProviderNetworkError,
            true,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
            Some(AgentErrorResolutionTarget::ProviderSettings),
        ));
    }
    if contains_any(
        lower,
        &[
            "invalid request",
            "invalid_request",
            "invalid_request_error",
            "invalid assistant message",
            "content is required",
            "invalid input",
        ],
    ) {
        return Some(provider_error(
            "The model provider rejected the request",
            AgentErrorCode::UserLlmProviderInvalidRequest,
            false,
            AgentErrorResolutionKind::SendFeedback,
            Some(AgentErrorResolutionTarget::Feedback),
        ));
    }
    if contains_any(lower, &["500", "502", "503", "bad gateway", "service unavailable"]) {
        return Some(provider_error(
            "The model provider returned a server error",
            AgentErrorCode::UserLlmProviderGatewayError,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ));
    }
    if lower.contains("provider error") {
        return Some(provider_error(
            "The model provider returned an error",
            AgentErrorCode::UserLlmProviderGatewayError,
            true,
            AgentErrorResolutionKind::Retry,
            None,
        ));
    }

    None
}

fn classify_aionui_state(lower: &str) -> Option<ClassifiedError> {
    if lower.contains("conversation is already processing") {
        return Some(ClassifiedError {
            message: "The current response is still running",
            code: AgentErrorCode::AionuiConversationBusy,
            ownership: AgentErrorOwnership::Aionui,
            retryable: true,
            feedback_recommended: false,
            resolution_kind: Some(AgentErrorResolutionKind::WaitForCurrentResponse),
            resolution_target: Some(AgentErrorResolutionTarget::NewConversation),
        });
    }

    None
}

fn agent_error(
    message: &'static str,
    code: AgentErrorCode,
    retryable: bool,
    resolution_kind: AgentErrorResolutionKind,
) -> ClassifiedError {
    ClassifiedError {
        message,
        code,
        ownership: AgentErrorOwnership::UserAgent,
        retryable,
        feedback_recommended: false,
        resolution_kind: Some(resolution_kind),
        resolution_target: Some(AgentErrorResolutionTarget::AgentSettings),
    }
}

fn agent_session_error(message: &'static str, code: AgentErrorCode) -> ClassifiedError {
    ClassifiedError {
        message,
        code,
        ownership: AgentErrorOwnership::UserAgent,
        retryable: true,
        feedback_recommended: false,
        resolution_kind: None,
        resolution_target: None,
    }
}

fn provider_error(
    message: &'static str,
    code: AgentErrorCode,
    retryable: bool,
    resolution_kind: AgentErrorResolutionKind,
    resolution_target: Option<AgentErrorResolutionTarget>,
) -> ClassifiedError {
    ClassifiedError {
        message,
        code,
        ownership: AgentErrorOwnership::UserLlmProvider,
        retryable,
        feedback_recommended: false,
        resolution_kind: Some(resolution_kind),
        resolution_target,
    }
}

fn resolution(
    kind: AgentErrorResolutionKind,
    target: Option<AgentErrorResolutionTarget>,
) -> Option<AgentErrorResolution> {
    Some(AgentErrorResolution::new(kind, target))
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn contains_sso_expired_auth_signature(lower: &str) -> bool {
    contains_any(lower, &["token is expired", "aws sso login", "sso session"])
}

fn is_openclaw_backend(backend: Option<&str>) -> bool {
    matches!(backend, Some(value) if value.eq_ignore_ascii_case(OPENCLAW_BACKEND))
}

fn contains_openclaw_gateway_unreachable_signature(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    contains_any(
        &lower,
        &[
            "acp bridge failed",
            "connect econnrefused 127.0.0.1:18789",
            "econnrefused 127.0.0.1:18789",
        ],
    )
}

fn openclaw_gateway_unreachable_from_acp_error(err: &AcpError) -> bool {
    match err {
        AcpError::StartupCrash { stderr, .. } => contains_openclaw_gateway_unreachable_signature(stderr),
        AcpError::AgentInternal { message, data, .. } => acp_agent_internal_texts(message, data.as_ref())
            .into_iter()
            .any(contains_openclaw_gateway_unreachable_signature),
        AcpError::InitTimeout { .. } => contains_openclaw_gateway_unreachable_signature(&err.to_string()),
        _ => false,
    }
}

fn openclaw_gateway_unreachable_send_error() -> AgentSendError {
    AgentSendError::new(
        OPENCLAW_GATEWAY_MESSAGE,
        AgentErrorCode::UserAgentOpenClawGatewayUnreachable,
        AgentErrorOwnership::UserAgent,
        Some(OPENCLAW_GATEWAY_DETAIL.to_owned()),
        true,
        false,
        resolution(
            AgentErrorResolutionKind::CheckAgentInstallation,
            Some(AgentErrorResolutionTarget::AgentSettings),
        ),
    )
}

pub(crate) fn sanitize_error_detail(input: &str) -> String {
    let stripped = strip_markup(input);
    let without_query = redact_url_queries(&stripped);
    let redacted = redact_lines(&without_query);
    truncate_chars(redacted.trim(), MAX_DETAIL_CHARS)
}

fn strip_markup(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match (ch, in_tag) {
            ('<', _) => in_tag = true,
            ('>', true) => {
                in_tag = false;
                out.push(' ');
            }
            (c, false) => out.push(c),
            _ => {}
        }
    }
    out
}

fn redact_lines(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        let cleaned = if is_sensitive_header_line(line) {
            "<redacted header>".to_owned()
        } else {
            redact_secret_words(line)
        };
        push_bounded_line(&mut out, &cleaned);
        if out.chars().count() >= MAX_DETAIL_CHARS {
            break;
        }
    }
    out
}

fn push_bounded_line(out: &mut String, line: &str) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(line);
}

fn is_sensitive_header_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("authorization:")
        || lower.contains("x-api-key:")
        || lower.contains("api-key:")
        || lower.contains("api_key:")
}

fn redact_secret_words(line: &str) -> String {
    line.split_whitespace()
        .map(|word| {
            let lower = word.to_ascii_lowercase();
            if lower.starts_with("bearer ")
                || lower.starts_with("sk-")
                || lower.contains("api_key=")
                || lower.contains("apikey=")
                || lower.contains("access_token=")
                || lower.contains("token=")
            {
                "<redacted>"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_url_queries(input: &str) -> String {
    input
        .split_whitespace()
        .map(|word| {
            if (word.starts_with("http://") || word.starts_with("https://")) && word.contains('?') {
                let end_punct = word
                    .chars()
                    .last()
                    .filter(|c| matches!(c, '.' | ',' | ';' | ')' | ']'))
                    .map(|c| c.to_string())
                    .unwrap_or_default();
                let trimmed = word.trim_end_matches(['.', ',', ';', ')', ']']);
                let base = trimmed.split_once('?').map(|(base, _)| base).unwrap_or(trimmed);
                format!("{base}?<redacted>{end_punct}")
            } else {
                word.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_chars(value: &str, max: usize) -> String {
    let mut out = String::new();
    for ch in value.chars().take(max) {
        out.push(ch);
    }
    if value.chars().count() > max {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::{AgentErrorResolutionKind, AgentErrorResolutionTarget};
    use serde_json::json;

    fn assert_classification(
        detail: &str,
        code: AgentErrorCode,
        ownership: AgentErrorOwnership,
        resolution: AgentErrorResolutionKind,
    ) {
        let err = AgentSendError::from_agent_error(AgentError::bad_gateway(detail));
        assert_eq!(err.code(), Some(code));
        assert_eq!(err.ownership(), Some(ownership));
        assert_eq!(err.stream_error().resolution.map(|value| value.kind), Some(resolution));
    }

    fn assert_classification_without_resolution(detail: &str, code: AgentErrorCode, ownership: AgentErrorOwnership) {
        let err = AgentSendError::from_agent_error(AgentError::bad_gateway(detail));
        assert_eq!(err.code(), Some(code));
        assert_eq!(err.ownership(), Some(ownership));
        assert!(err.stream_error().resolution.is_none());
        assert_eq!(err.stream_error().feedback_recommended, Some(false));
    }

    fn assert_acp_classification(
        err: AcpError,
        code: AgentErrorCode,
        ownership: AgentErrorOwnership,
        resolution: AgentErrorResolutionKind,
    ) -> AgentSendError {
        let err = AgentSendError::from(err);
        assert_eq!(err.code(), Some(code));
        assert_eq!(err.ownership(), Some(ownership));
        assert_eq!(err.stream_error().resolution.map(|value| value.kind), Some(resolution));
        err
    }

    fn assert_resolution_target(detail: &str, target: AgentErrorResolutionTarget) {
        let err = AgentSendError::from_agent_error(AgentError::bad_gateway(detail));
        assert_eq!(
            err.stream_error().resolution.and_then(|value| value.target),
            Some(target)
        );
    }

    #[test]
    fn classifies_openclaw_gateway_unreachable_from_startup_stderr_with_backend_context() {
        let err = AgentError::from(AcpError::StartupCrash {
            exit_code: Some(1),
            signal: None,
            stderr: "ACP bridge failed: connect ECONNREFUSED 127.0.0.1:18789".into(),
        });

        let send_error = AgentSendError::from_agent_error_ref_for_backend(&err, Some("openclaw"));

        assert_eq!(
            send_error.code(),
            Some(AgentErrorCode::UserAgentOpenClawGatewayUnreachable)
        );
        assert_eq!(send_error.ownership(), Some(AgentErrorOwnership::UserAgent));
        assert_eq!(send_error.stream_error().retryable, Some(true));
        assert_eq!(send_error.stream_error().feedback_recommended, Some(false));
        assert_eq!(
            send_error.stream_error().resolution.map(|value| value.kind),
            Some(AgentErrorResolutionKind::CheckAgentInstallation)
        );
        assert!(
            send_error
                .stream_error()
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("openclaw gateway start")
        );
    }

    #[test]
    fn keeps_openclaw_generic_startup_crash_when_gateway_signature_is_absent() {
        let err = AgentError::from(AcpError::StartupCrash {
            exit_code: Some(1),
            signal: None,
            stderr: "ordinary OpenClaw startup failure without gateway signature".into(),
        });

        let send_error = AgentSendError::from_agent_error_ref_for_backend(&err, Some("openclaw"));

        assert_ne!(
            send_error.code(),
            Some(AgentErrorCode::UserAgentOpenClawGatewayUnreachable)
        );
        assert_eq!(send_error.code(), Some(AgentErrorCode::UserAgentStartupFailed));
    }

    #[test]
    fn does_not_classify_openclaw_gateway_signature_for_other_backends() {
        let err = AgentError::from(AcpError::StartupCrash {
            exit_code: Some(1),
            signal: None,
            stderr: "ACP bridge failed: connect ECONNREFUSED 127.0.0.1:18789".into(),
        });

        let send_error = AgentSendError::from_agent_error_ref_for_backend(&err, Some("codex"));

        assert_ne!(
            send_error.code(),
            Some(AgentErrorCode::UserAgentOpenClawGatewayUnreachable)
        );
        assert_eq!(send_error.code(), Some(AgentErrorCode::UserAgentStartupFailed));
    }

    #[test]
    fn does_not_classify_plain_init_timeout_as_openclaw_gateway_unreachable() {
        let err = AgentError::from(AcpError::InitTimeout { timeout_secs: 30 });

        let send_error = AgentSendError::from_agent_error_ref_for_backend(&err, Some("openclaw"));

        assert_ne!(
            send_error.code(),
            Some(AgentErrorCode::UserAgentOpenClawGatewayUnreachable)
        );
        assert_eq!(send_error.code(), Some(AgentErrorCode::UserAgentStartupFailed));
    }

    #[test]
    fn classifies_openclaw_gateway_unreachable_from_bad_gateway_detail_only_with_backend_context() {
        let detail = "Bad gateway: ACP bridge failed: connect ECONNREFUSED 127.0.0.1:18789";
        let err = AgentError::bad_gateway(detail);

        let openclaw = AgentSendError::from_agent_error_ref_for_backend(&err, Some("openclaw"));
        let codex = AgentSendError::from_agent_error_ref_for_backend(&err, Some("codex"));

        assert_eq!(
            openclaw.code(),
            Some(AgentErrorCode::UserAgentOpenClawGatewayUnreachable)
        );
        assert_ne!(codex.code(), Some(AgentErrorCode::UserAgentOpenClawGatewayUnreachable));
    }

    #[test]
    fn classifies_provider_auth_failure() {
        let err = AgentSendError::from_agent_error(AgentError::bad_gateway("provider returned 401 invalid api key"));

        assert_eq!(err.code(), Some(AgentErrorCode::UserLlmProviderAuthFailed));
        assert_eq!(err.ownership(), Some(AgentErrorOwnership::UserLlmProvider));
        assert_eq!(err.stream_error().feedback_recommended, Some(false));
    }

    #[test]
    fn classifies_unknown_upstream_when_heuristics_do_not_match() {
        let err = AgentSendError::from_agent_error(AgentError::bad_gateway("agent exploded"));

        assert_eq!(err.code(), Some(AgentErrorCode::UnknownUpstreamError));
        assert_eq!(err.ownership(), Some(AgentErrorOwnership::UnknownUpstream));
        assert_eq!(err.stream_error().feedback_recommended, Some(false));
        assert!(err.stream_error().resolution.is_none());
    }

    #[test]
    fn classifies_acp_protocol_errors_without_unknown_upstream_fallback() {
        let cases = [
            (
                AcpError::ProtocolParseError {
                    message: "Parse error".into(),
                },
                AgentErrorCode::UserAgentProtocolParseError,
            ),
            (
                AcpError::InvalidRequest {
                    message: "Invalid request".into(),
                },
                AgentErrorCode::UserAgentInvalidRequest,
            ),
            (
                AcpError::ResourceNotFound {
                    resource: Some("file:///missing.txt".into()),
                    message: "Resource not found".into(),
                },
                AgentErrorCode::UserAgentResourceNotFound,
            ),
            (
                AcpError::OtherProtocolError {
                    code: -32099,
                    message: "custom protocol error".into(),
                    data: None,
                },
                AgentErrorCode::UserAgentProtocolError,
            ),
        ];

        for (err, code) in cases {
            let send_error = AgentSendError::from(err);
            assert_eq!(send_error.code(), Some(code));
            assert_eq!(send_error.ownership(), Some(AgentErrorOwnership::UserAgent));
            assert_ne!(send_error.code(), Some(AgentErrorCode::UnknownUpstreamError));
            assert!(send_error.stream_error().resolution.is_none());
            assert_eq!(send_error.stream_error().feedback_recommended, Some(false));
        }
    }

    #[test]
    fn preserves_runtime_workspace_validation_as_structured_aionui_error() {
        let err =
            AgentSendError::from_agent_error(AgentError::workspace_path_runtime_unavailable("/Users/test/Archive "));

        assert_eq!(err.code(), Some(AgentErrorCode::WorkspacePathRuntimeUnavailable));
        assert_eq!(err.ownership(), Some(AgentErrorOwnership::Aionui));
        assert_eq!(
            err.stream_error().workspace_path.as_deref(),
            Some("/Users/test/Archive ")
        );
        assert_eq!(err.stream_error().retryable, Some(false));
        assert_eq!(err.stream_error().feedback_recommended, Some(false));
        assert_eq!(
            err.stream_error().resolution.map(|value| value.kind),
            Some(AgentErrorResolutionKind::StartNewSession)
        );
    }

    #[test]
    fn classifies_provider_error_without_specific_signal_as_provider_gateway() {
        let err = AgentSendError::from_agent_error(AgentError::bad_gateway("Provider error: upstream failed"));

        assert_eq!(err.code(), Some(AgentErrorCode::UserLlmProviderGatewayError));
        assert_eq!(err.ownership(), Some(AgentErrorOwnership::UserLlmProvider));
        assert_eq!(err.stream_error().feedback_recommended, Some(false));
    }

    #[test]
    fn classifies_provider_config_errors_as_not_retryable() {
        let err = AgentSendError::from_agent_error(AgentError::bad_gateway(
            "Provider error: Connection error: Signable request error: failed to create canonical request",
        ));

        assert_eq!(err.code(), Some(AgentErrorCode::UserLlmProviderConfigError));
        assert_eq!(err.ownership(), Some(AgentErrorOwnership::UserLlmProvider));
        assert_eq!(err.stream_error().retryable, Some(false));
        assert_eq!(err.stream_error().feedback_recommended, Some(false));
    }

    #[test]
    fn sanitizes_secrets_and_query_strings() {
        let detail = sanitize_error_detail(
            "Authorization: Bearer sk-secret\nGET https://example.com/v1?api_key=sk-secret\ninvalid_api_key sk-secret",
        );

        assert!(!detail.contains("sk-secret"));
        assert!(!detail.contains("api_key=sk"));
        assert!(detail.contains("<redacted header>"));
        assert_eq!(
            redact_url_queries("GET https://example.com/v1?api_key=sk-secret"),
            "GET https://example.com/v1?<redacted>"
        );
    }

    #[test]
    fn strip_markup_removes_html_tags_keeping_visible_text() {
        let detail = sanitize_error_detail(
            "Provider error: API error 504: <html>\r\n<head><title>504 Gateway Time-out</title></head>\r\n<body><center><h1>504 Gateway Time-out</h1></center>\r\n<hr><center>openresty</center></body></html>",
        );

        assert!(!detail.contains('<'));
        assert!(!detail.contains('>'));
        assert!(detail.contains("504 Gateway Time-out"));
        assert!(detail.contains("openresty"));
        assert!(detail.starts_with("Provider error: API error 504:"));
    }

    #[test]
    fn strip_markup_is_identity_for_plain_text() {
        let detail = sanitize_error_detail("Provider error: API error 504: error code: 524");

        assert_eq!(detail, "Provider error: API error 504: error code: 524");
    }

    #[test]
    fn redaction_runs_after_strip_markup() {
        let detail = sanitize_error_detail(
            "<html><body>Authorization: Bearer sk-secret\nGET https://example.com/v1?api_key=sk-secret</body></html>",
        );

        assert!(!detail.contains("sk-secret"));
        assert!(!detail.contains("api_key=sk"));
        assert!(detail.contains("<redacted header>"));
        assert!(!detail.contains("<html"));
        assert!(!detail.contains("</body>"));
    }

    #[test]
    fn classifies_provider_504_html_body_as_timeout_with_stripped_detail() {
        let raw = "Aionrs agent error: Provider error: API error 504: <html>\r\n<head><title>504 Gateway Time-out</title></head>\r\n<body>\r\n<center><h1>504 Gateway Time-out</h1></center>\r\n<hr><center>openresty</center>\r\n</body>\r\n</html>";
        let err = AgentSendError::from_agent_error(AgentError::bad_gateway(raw));

        assert_eq!(err.code(), Some(AgentErrorCode::UserLlmProviderTimeout));
        let detail = err.stream_error().detail.clone().expect("detail present");
        assert!(!detail.contains('<'));
        assert!(!detail.contains('>'));
        assert!(detail.chars().count() <= MAX_DETAIL_CHARS);
        assert!(detail.contains("504 Gateway Time-out"));
    }

    #[test]
    fn classifies_agent_lifecycle_before_bad_gateway_wrapper() {
        assert_classification(
            "Bad gateway: Agent process exited before initialize handshake completed (exit code 1)",
            AgentErrorCode::UserAgentHandshakeFailed,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::CheckAgentInstallation,
        );
        assert_classification(
            "Bad gateway: Initialize handshake timed out after 30s",
            AgentErrorCode::UserAgentHandshakeTimeout,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::ReconnectAgent,
        );
    }

    #[test]
    fn classifies_mid_session_cli_exit_as_agent_disconnected() {
        // Mid-session ACP CLI exit (e.g. Claude Code) surfaces as -32603 with
        // `details: "Claude Code process exited with code 1"`. Previously this
        // fell through to UNKNOWN_UPSTREAM_ERROR; it now maps to a retryable
        // agent disconnect.
        assert_classification(
            "Agent internal error (code -32603) ({\"details\":\"Claude Code process exited with code 1\"})",
            AgentErrorCode::UserAgentDisconnected,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::ReconnectAgent,
        );
        let err = AgentSendError::from_agent_error(AgentError::bad_gateway(
            "Agent internal error (code -32603) ({\"details\":\"Claude Code process exited with code 1\"})",
        ));
        assert_eq!(err.stream_error().retryable, Some(true));
        assert_eq!(err.stream_error().feedback_recommended, Some(false));
    }

    #[test]
    fn classifies_acp_internal_mcp_connect_failure_from_structured_data() {
        let err = assert_acp_classification(
            AcpError::AgentInternal {
                message: "Internal error".into(),
                code: -32603,
                data: Some(json!({"error": "Failed to connect MCP servers"})),
            },
            AgentErrorCode::UserAgentDisconnected,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::ReconnectAgent,
        );

        let detail = err.stream_error().detail.as_deref().expect("detail present");
        assert_eq!(
            detail,
            "Agent internal error (code -32603): Failed to connect MCP servers"
        );
    }

    #[test]
    fn classifies_acp_internal_process_exit_from_structured_details() {
        let err = assert_acp_classification(
            AcpError::AgentInternal {
                message: "Internal error".into(),
                code: -32603,
                data: Some(json!({"details": "Claude Code process exited with code 1"})),
            },
            AgentErrorCode::UserAgentDisconnected,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::ReconnectAgent,
        );

        assert_eq!(err.stream_error().retryable, Some(true));
        assert_eq!(err.stream_error().feedback_recommended, Some(false));
    }

    #[test]
    fn not_connected_maps_to_user_agent_disconnected() {
        let disconnected = AgentSendError::from(AcpError::NotConnected);
        assert_eq!(
            disconnected.stream_error.code,
            Some(AgentErrorCode::UserAgentDisconnected)
        );
        assert_eq!(
            disconnected.stream_error.ownership,
            Some(AgentErrorOwnership::UserAgent)
        );
        assert_eq!(
            disconnected
                .stream_error
                .resolution
                .as_ref()
                .map(|resolution| resolution.kind),
            Some(AgentErrorResolutionKind::ReconnectAgent)
        );
    }

    #[test]
    fn request_timeout_maps_to_retryable_user_agent_disconnected() {
        let err = AgentSendError::from(AcpError::RequestTimeout {
            method: "session/setConfigOption".into(),
            timeout_secs: 10,
        });
        assert_eq!(err.code(), Some(AgentErrorCode::UserAgentDisconnected));
        assert_eq!(err.ownership(), Some(AgentErrorOwnership::UserAgent));
        assert_eq!(err.stream_error().retryable, Some(true));
    }

    #[test]
    fn classifies_acp_internal_provider_failure_from_structured_message() {
        assert_acp_classification(
            AcpError::AgentInternal {
                message: "Provider error: API error 401: invalid x-api-key".into(),
                code: -32603,
                data: None,
            },
            AgentErrorCode::UserLlmProviderAuthFailed,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderCredentials,
        );
    }

    #[test]
    fn classifies_acp_internal_bedrock_sso_expired_as_dedicated_provider_error() {
        let err = AgentSendError::from(AcpError::AgentInternal {
                message: "Internal error: API Error: Token is expired. To refresh this SSO session run 'aws sso login' with the corresponding profile.".into(),
                code: -32603,
                data: Some(json!({"errorKind": "unknown"})),
            });

        let payload = serde_json::to_value(err.stream_error()).expect("serialize stream error");
        assert_eq!(payload["code"], "USER_LLM_PROVIDER_AWS_SSO_EXPIRED");
        assert_eq!(err.ownership(), Some(AgentErrorOwnership::UserLlmProvider));
        assert_eq!(
            err.stream_error().resolution.map(|value| value.kind),
            Some(AgentErrorResolutionKind::CheckProviderCredentials)
        );
        let detail = err.stream_error().detail.as_deref().expect("detail present");
        assert!(detail.contains("Token is expired"));
        assert!(detail.contains("aws sso login"));
        assert_eq!(err.stream_error().retryable, Some(false));
        assert_eq!(err.stream_error().feedback_recommended, Some(false));
    }

    #[test]
    fn classifies_acp_internal_nested_provider_error_message() {
        assert_acp_classification(
            AcpError::AgentInternal {
                message: "Internal error".into(),
                code: -32603,
                data: Some(json!({
                    "type": "error",
                    "error": {
                        "message": "Your credit balance is too low to access the Anthropic API"
                    }
                })),
            },
            AgentErrorCode::UserLlmProviderBillingRequired,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderBilling,
        );
    }

    #[test]
    fn classifies_acp_internal_nested_provider_data_before_generic_message() {
        assert_acp_classification(
            AcpError::AgentInternal {
                message: "Provider error".into(),
                code: -32603,
                data: Some(json!({
                    "error": {
                        "message": "invalid x-api-key"
                    }
                })),
            },
            AgentErrorCode::UserLlmProviderAuthFailed,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderCredentials,
        );
    }

    #[test]
    fn acp_internal_public_detail_does_not_include_structured_data() {
        let err = AgentSendError::from(AcpError::AgentInternal {
            message: "Internal error".into(),
            code: -32603,
            data: Some(json!({
                "error": "Failed to connect MCP servers",
                "api_key": "sk-secret"
            })),
        });

        let detail = err.stream_error().detail.as_deref().expect("detail present");
        assert_eq!(
            detail,
            "Agent internal error (code -32603): Failed to connect MCP servers"
        );
        assert!(!detail.contains("sk-secret"));
        assert!(!detail.contains("api_key"));
    }

    #[test]
    fn provider_free_text_still_uses_provider_heuristic_boundary() {
        let err = AgentSendError::from_agent_error(AgentError::bad_gateway(
            "Provider error: API error 401: invalid x-api-key",
        ));

        assert_eq!(err.code(), Some(AgentErrorCode::UserLlmProviderAuthFailed));
        assert_eq!(err.ownership(), Some(AgentErrorOwnership::UserLlmProvider));
        assert_eq!(
            err.stream_error().resolution.map(|value| value.kind),
            Some(AgentErrorResolutionKind::CheckProviderCredentials)
        );
    }

    #[test]
    fn classifies_agent_protocol_and_session_failures() {
        assert_classification_without_resolution(
            "Connection error: protocol mismatch Connection error: Max reconnect attempts (10) reached",
            AgentErrorCode::UserAgentProtocolMismatch,
            AgentErrorOwnership::UserAgent,
        );
        assert_classification_without_resolution(
            "Agent internal error (code -32603) {\"details\":\"Session not found\"}",
            AgentErrorCode::UserAgentSessionNotFound,
            AgentErrorOwnership::UserAgent,
        );
        assert_classification_without_resolution(
            "Bad gateway: Agent internal error (code -32603) {\"details\":\"No previous sessions found for this project\"}",
            AgentErrorCode::UserAgentNoPreviousSession,
            AgentErrorOwnership::UserAgent,
        );
    }

    #[test]
    fn classifies_agent_setup_failures() {
        assert_classification(
            "CLI found but ACP initialization failed.",
            AgentErrorCode::UserAgentAcpInitFailed,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::CheckAgentInstallation,
        );
        assert_classification(
            "找到 CLI 但 ACP 初始化失败",
            AgentErrorCode::UserAgentAcpInitFailed,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::CheckAgentInstallation,
        );
        assert_classification(
            "filesystem: Command not found: npx",
            AgentErrorCode::UserAgentCommandNotFound,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::CheckLocalCommand,
        );
        let native_binary_missing = "Agent internal error (code -32603) ({\"details\":\"Claude native binary not found for win32-x64. Reinstall @anthropic-ai/claude-agent-sdk without --omit=optional, or set CLAUDE_CODE_EXECUTABLE.\"})";
        assert_classification(
            native_binary_missing,
            AgentErrorCode::UserAgentStartupFailed,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::CheckAgentInstallation,
        );
        let err = AgentSendError::from_agent_error(AgentError::bad_gateway(native_binary_missing));
        assert_eq!(err.stream_error().retryable, Some(false));
        assert_eq!(
            err.stream_error().resolution.and_then(|value| value.target),
            Some(AgentErrorResolutionTarget::AgentSettings)
        );

        for managed_binary_missing in [
            "expected managed Claude ACP platform binary missing: C:\\Users\\user\\AppData\\Roaming\\AionUi\\aionui\\runtime\\acp\\claude-agent-acp\\0.58.1\\win32-x64\\node_modules\\@anthropic-ai\\claude-agent-sdk-win32-x64\\claude.exe",
            "expected managed Codex ACP platform binary missing: C:\\Users\\user\\AppData\\Roaming\\AionUi\\aionui\\runtime\\acp\\codex-acp\\1.1.2\\win32-x64\\node_modules\\@openai\\codex-win32-x64\\vendor\\x86_64-pc-windows-msvc\\bin\\codex.exe",
        ] {
            assert_classification(
                managed_binary_missing,
                AgentErrorCode::UserAgentStartupFailed,
                AgentErrorOwnership::UserAgent,
                AgentErrorResolutionKind::CheckAgentInstallation,
            );
            let err = AgentSendError::from_agent_error(AgentError::bad_gateway(managed_binary_missing));
            assert_eq!(err.stream_error().retryable, Some(false));
            assert_eq!(
                err.stream_error().resolution.and_then(|value| value.target),
                Some(AgentErrorResolutionTarget::AgentSettings)
            );
        }
        assert_classification(
            "Agent internal error (code -32603) {\"message\":\"Missing environment variable: 'OMLX API KEY'\"}",
            AgentErrorCode::UserAgentMissingEnv,
            AgentErrorOwnership::UserAgent,
            AgentErrorResolutionKind::CheckAgentInstallation,
        );
    }

    #[test]
    fn classifies_provider_billing_auth_and_rate_limit() {
        assert_classification(
            "Aionrs agent error: Provider error: API error 402: {\"error\":{\"message\":\"Insufficient Balance\"}}",
            AgentErrorCode::UserLlmProviderBillingRequired,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderBilling,
        );
        assert_classification(
            "Aionrs agent error: Provider error: API error 400: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.\"}}",
            AgentErrorCode::UserLlmProviderBillingRequired,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderBilling,
        );
        assert_classification(
            "Aionrs agent error: Provider error: API error 401: invalid x-api-key",
            AgentErrorCode::UserLlmProviderAuthFailed,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderCredentials,
        );
        assert_classification(
            "Aionrs agent error: Provider error: Rate limited, retry after 5000ms",
            AgentErrorCode::UserLlmProviderRateLimited,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::Retry,
        );
    }

    #[test]
    fn classifies_provider_auth_credentials_before_config() {
        assert_classification(
            "API error 401: Invalid authentication credentials",
            AgentErrorCode::UserLlmProviderAuthFailed,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderCredentials,
        );
    }

    #[test]
    fn classifies_provider_permission_denied_separately_from_auth() {
        for detail in [
            "API error 403: forbidden",
            "Provider error: permission denied",
            "API error 403: You do not have permission to access this resource",
        ] {
            assert_classification(
                detail,
                AgentErrorCode::UserLlmProviderPermissionDenied,
                AgentErrorOwnership::UserLlmProvider,
                AgentErrorResolutionKind::CheckProviderCredentials,
            );
        }
    }

    #[test]
    fn classifies_provider_request_model_and_context_errors() {
        assert_classification(
            "API error 400: Function calling is not enabled for this model",
            AgentErrorCode::UserLlmProviderUnsupportedModel,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::ChangeModel,
        );
        assert_classification(
            "Aionrs agent error: provider repeatedly returned malformed tool calls (3/3); stopped to avoid wasting tokens",
            AgentErrorCode::UserLlmProviderInvalidRequest,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::ChangeModel,
        );
        assert_resolution_target(
            "Aionrs agent error: provider repeatedly returned malformed tool calls (3/3); stopped to avoid wasting tokens",
            AgentErrorResolutionTarget::ProviderSettings,
        );
        assert_classification(
            "API error 400: invalid params, context window exceeds limit",
            AgentErrorCode::UserLlmProviderContextTooLarge,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::ReduceContext,
        );
        assert_classification(
            "API error 400: Invalid schema for function 'team_write_plan': None is not of type 'array'",
            AgentErrorCode::UserLlmProviderInvalidToolSchema,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::Retry,
        );
    }

    #[test]
    fn classifies_model_not_found_before_endpoint_404() {
        assert_classification(
            "API error 404: model not found for path /v1/chat/completions",
            AgentErrorCode::UserLlmProviderModelNotFound,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::ChangeModel,
        );
    }

    #[test]
    fn classifies_bedrock_invalid_model_identifier_as_model_not_found() {
        assert_classification(
            "Aionrs agent error: Provider error: API error 400: {\"message\":\"The provided model identifier is invalid.\"}",
            AgentErrorCode::UserLlmProviderModelNotFound,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::ChangeModel,
        );
        assert_resolution_target(
            "Aionrs agent error: Provider error: API error 400: {\"message\":\"The provided model identifier is invalid.\"}",
            AgentErrorResolutionTarget::ProviderSettings,
        );
    }

    #[test]
    fn classifies_generic_provider_invalid_requests() {
        for detail in [
            "API error 400: Invalid request: Invalid input",
            "API error 400: Invalid assistant message: content or tool calls must be set",
            "API error 400: content is required",
            "Provider error: API error 400: {\"type\":\"invalid_request_error\",\"message\":\"bad payload\"}",
        ] {
            assert_classification(
                detail,
                AgentErrorCode::UserLlmProviderInvalidRequest,
                AgentErrorOwnership::UserLlmProvider,
                AgentErrorResolutionKind::SendFeedback,
            );
            let err = AgentSendError::from_agent_error(AgentError::bad_gateway(detail));
            assert_eq!(err.stream_error().retryable, Some(false));
        }
    }

    #[test]
    fn non_retryable_agent_invalid_params_do_not_suggest_retry() {
        let api_err =
            AgentSendError::from_agent_error(AgentError::bad_request("Invalid parameters: malformed request"));
        assert_eq!(api_err.code(), Some(AgentErrorCode::UserAgentInvalidParams));
        assert_eq!(api_err.stream_error().retryable, Some(false));
        assert_eq!(api_err.stream_error().feedback_recommended, Some(false));
        assert!(api_err.stream_error().resolution.is_none());

        let acp_err = AgentSendError::from(AcpError::InvalidParams {
            message: "malformed request".into(),
        });
        assert_eq!(acp_err.code(), Some(AgentErrorCode::UserAgentInvalidParams));
        assert_eq!(acp_err.stream_error().retryable, Some(false));
        assert_eq!(acp_err.stream_error().feedback_recommended, Some(false));
        assert!(acp_err.stream_error().resolution.is_none());
    }

    #[test]
    fn classifies_provider_endpoint_network_timeout_and_empty_response() {
        assert_classification(
            "API error 404: {\"status\":404,\"error\":\"Not Found\",\"path\":\"/v4/v1/chat/completions\"}",
            AgentErrorCode::UserLlmProviderEndpointNotFound,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
        );
        assert_resolution_target(
            "API error 404: {\"status\":404,\"error\":\"Not Found\",\"path\":\"/v4/v1/chat/completions\"}",
            AgentErrorResolutionTarget::ProviderSettings,
        );
        assert_classification(
            "Aionrs agent error: API error: Connection error: error decoding response body",
            AgentErrorCode::UserLlmProviderNetworkError,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
        );
        assert_classification(
            "Aionrs agent error: API error: error sending request for url",
            AgentErrorCode::UserLlmProviderNetworkError,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
        );
        assert_classification(
            "Autocompact failed: Empty response from LLM",
            AgentErrorCode::UserLlmProviderEmptyResponse,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::Retry,
        );
    }

    #[test]
    fn classifies_bare_provider_404_as_endpoint_not_found() {
        let detail = "Aionrs agent error: Provider error: API error 404: {\"detail\":\"Not Found\"}";
        assert_classification(
            detail,
            AgentErrorCode::UserLlmProviderEndpointNotFound,
            AgentErrorOwnership::UserLlmProvider,
            AgentErrorResolutionKind::CheckProviderBaseUrl,
        );
        assert_resolution_target(detail, AgentErrorResolutionTarget::ProviderSettings);
        let err = AgentSendError::from_agent_error(AgentError::bad_gateway(detail));
        assert_eq!(err.stream_error().retryable, Some(false));
    }

    #[test]
    fn classifies_aionui_conversation_busy_after_agent_and_provider_checks() {
        assert_classification(
            "Conflict: Conversation is already processing a message",
            AgentErrorCode::AionuiConversationBusy,
            AgentErrorOwnership::Aionui,
            AgentErrorResolutionKind::WaitForCurrentResponse,
        );
    }
}
