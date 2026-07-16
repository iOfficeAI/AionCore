// --- File processing ---

pub const AIONUI_TIMESTAMP_SEPARATOR: &str = "_aionui_";
pub const AIONUI_FILES_MARKER: &str = "[[AION_FILES]]";

// --- WebSocket ---

pub const HEARTBEAT_INTERVAL_MS: u64 = 30_000;
pub const HEARTBEAT_TIMEOUT_MS: u64 = 60_000;
pub const WS_CLOSE_NORMAL: u16 = 1000;
pub const WS_CLOSE_POLICY_VIOLATION: u16 = 1008;

// --- Authentication ---

pub const SESSION_EXPIRY: &str = "24h";
pub const COOKIE_NAME: &str = "aionui-session";
pub const COOKIE_MAX_AGE_DAYS: u32 = 30;
pub const CSRF_COOKIE_NAME: &str = "aionui-csrf-token";
pub const CSRF_HEADER_NAME: &str = "x-csrf-token";

// --- Server ---

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const REMOTE_HOST: &str = "0.0.0.0";
pub const DEFAULT_PORT: u16 = 25808;
/// Request body size limit (10 MB).
pub const BODY_LIMIT: usize = 10 * 1024 * 1024;
/// File upload size limit (30 MB).
pub const UPLOAD_MAX_SIZE: usize = 30 * 1024 * 1024;

// --- Team mode ---

/// Runtime backend that supports Team MCP without ACP capability metadata.
pub const AIONRS_RUNTIME_BACKEND: &str = "aionrs";

/// Determine if an agent supports team mode through MCP or CLI fallback.
pub fn is_team_capable(backend: &str, agent_capabilities: Option<&serde_json::Value>) -> bool {
    if backend.trim().is_empty() {
        return false;
    }
    supports_team_mcp(backend, agent_capabilities) || supports_team_cli_fallback(agent_capabilities)
}

/// Determine if an agent supports Team MCP injection.
pub fn supports_team_mcp(backend: &str, agent_capabilities: Option<&serde_json::Value>) -> bool {
    if backend == AIONRS_RUNTIME_BACKEND {
        return true;
    }
    has_enabled_team_mcp_transport(agent_capabilities)
}

/// Determine if an agent is eligible for shell/CLI fallback.
pub fn supports_team_cli_fallback(agent_capabilities: Option<&serde_json::Value>) -> bool {
    let Some(caps) = agent_capabilities else {
        return true;
    };
    !explicit_false(caps, &["shell"])
        && !explicit_false(caps, &["cli"])
        && !explicit_false(caps, &["supports_shell"])
        && !explicit_false(caps, &["supportsShell"])
        && !explicit_false(caps, &["supports_cli"])
        && !explicit_false(caps, &["supportsCli"])
        && !explicit_false(caps, &["execution", "shell"])
        && !explicit_false(caps, &["execution", "cli"])
}

fn explicit_false(value: &serde_json::Value, path: &[&str]) -> bool {
    let mut cursor = value;
    for key in path {
        let Some(next) = cursor.get(*key) else {
            return false;
        };
        cursor = next;
    }
    cursor.as_bool() == Some(false)
}

/// Check whether `agent_capabilities` JSON declares MCP capability metadata.
pub fn has_mcp_capability(agent_capabilities: Option<&serde_json::Value>) -> bool {
    mcp_capability_object(agent_capabilities).is_some()
}

fn has_enabled_team_mcp_transport(agent_capabilities: Option<&serde_json::Value>) -> bool {
    let Some(caps) = mcp_capability_object(agent_capabilities) else {
        return false;
    };
    bool_field(caps, "stdio") || bool_field(caps, "http")
}

fn mcp_capability_object(agent_capabilities: Option<&serde_json::Value>) -> Option<&serde_json::Value> {
    let caps = agent_capabilities?;
    caps.get("mcp_capabilities")
        .or_else(|| caps.get("mcpCapabilities"))
        .or_else(|| caps.get("mcp"))
}

fn bool_field(value: &serde_json::Value, key: &str) -> bool {
    value.get(key).and_then(serde_json::Value::as_bool) == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn has_mcp_capability_requires_mcp_capability_field() {
        assert!(!has_mcp_capability(None));
        assert!(!has_mcp_capability(Some(&json!({}))));

        assert!(has_mcp_capability(Some(&json!({
            "mcp_capabilities": { "http": false, "sse": false }
        }))));
        assert!(has_mcp_capability(Some(&json!({
            "mcp_capabilities": {}
        }))));

        assert!(has_mcp_capability(Some(&json!({
            "mcp_capabilities": { "stdio": true }
        }))));
        assert!(has_mcp_capability(Some(&json!({
            "mcpCapabilities": { "http": true, "sse": false }
        }))));
        assert!(has_mcp_capability(Some(&json!({
            "mcp": { "http": false, "sse": true }
        }))));
    }

    #[test]
    fn team_mcp_builtin_backend_does_not_require_capability_metadata() {
        assert!(supports_team_mcp("aionrs", None));
        assert!(supports_team_mcp("aionrs", Some(&json!({}))));
    }

    #[test]
    fn acp_backends_require_stdio_or_http_capability_for_team_mcp() {
        assert!(supports_team_mcp(
            "claude",
            Some(&json!({ "mcp_capabilities": { "stdio": true } }))
        ));
        assert!(supports_team_mcp(
            "codex",
            Some(&json!({ "mcp_capabilities": { "http": true, "sse": false } }))
        ));

        assert!(!supports_team_mcp(
            "gemini",
            Some(&json!({ "mcp_capabilities": { "http": false, "sse": true } }))
        ));
        assert!(!supports_team_mcp(
            "codebuddy",
            Some(&json!({ "mcp_capabilities": { "http": false, "sse": false } }))
        ));
        assert!(!supports_team_mcp("acp", None));
        assert!(!supports_team_mcp("claude", Some(&json!({ "mcp_capabilities": {} }))));
    }

    #[test]
    fn cli_fallback_is_default_unless_capabilities_disable_shell_or_cli() {
        assert!(supports_team_cli_fallback(None));
        assert!(supports_team_cli_fallback(Some(&json!({}))));
        assert!(!supports_team_cli_fallback(Some(&json!({"shell": false}))));
        assert!(!supports_team_cli_fallback(Some(&json!({"execution": {"cli": false}}))));
    }
}

// --- Image processing ---

pub const SUPPORTED_IMAGE_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".tiff", ".svg"];
/// Remote image download size limit (5 MB).
pub const REMOTE_IMAGE_MAX_SIZE: usize = 5 * 1024 * 1024;
pub const REMOTE_IMAGE_MAX_REDIRECTS: u32 = 5;
