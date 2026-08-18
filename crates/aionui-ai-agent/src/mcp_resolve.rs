//! Neutral MCP server resolution (Wave 0c E) — the SINGLE source of truth for
//! turning a conversation's configured MCP servers into the SDK-free
//! [`SessionMcpServer`] shape the clean-slate session stack carries in
//! `SessionConfig.init.mcp_servers`.
//!
//! The legacy per-backend resolvers (`factory::acp::load_user_mcp_servers` →
//! `Vec<agent_client_protocol::McpServer>`, `factory::aionrs::load_user_mcp_servers`
//! → `HashMap<String, McpServerConfig>`) emit SDK/engine-specific types. This
//! module emits the NEUTRAL `aionui_api_types::SessionMcpServer` so the app
//! boundary (`aionui-app`) can convert it once into the crate-local
//! `aionui_session::McpServerSpec`, and each backend serializes that into its own
//! wire shape. Same row-walking + selection + stdio-launch-resolution logic as
//! the legacy ACP path, but vendor-neutral.

use std::sync::Arc;

use aionui_api_types::{SessionMcpServer, SessionMcpTransport, TEAM_MCP_SERVER_NAME};
use aionui_db::models::McpServerRow;
use aionui_db::{IMcpServerRepository, IOAuthTokenRepository};
use aionui_realtime::EventBroadcaster;
use aionui_runtime::ensure_runtime_command;
use tracing::{info, warn};

/// Resolve a conversation's user-configured MCP servers into neutral
/// [`SessionMcpServer`]s. `selected_ids = Some` → that frozen snapshot defines the
/// session (injected regardless of the row's global `enabled` flag); `None` → all
/// enabled rows. Builtin rows are excluded (guide/team MCP are folded separately
/// by the caller). Stdio launch commands are RESOLVED here (e.g. `npx` → the
/// bundled-node absolute path) so the spec that reaches `open_session` is final —
/// the Wave 0c contract that `McpTransport::Stdio.command` is pre-resolved.
///
/// Best-effort: a repo error, a capability-unsupported transport, or a malformed
/// `transport_config` row is warn-logged and skipped, never fatal. `broadcaster`
/// is accepted for parity with the legacy reporter path (runtime-resolution status
/// reporting) and reserved for that use.
pub async fn resolve_session_mcp_servers(
    repo: &dyn IMcpServerRepository,
    user_id: &str,
    selected_ids: Option<&[String]>,
    conversation_id: &str,
    _broadcaster: Arc<dyn EventBroadcaster>,
    oauth_token_repo: Option<&dyn IOAuthTokenRepository>,
) -> Vec<SessionMcpServer> {
    let rows_result = match selected_ids {
        Some(ids) => repo.list_by_ids_any(user_id, ids).await,
        None => repo.list(user_id).await,
    };
    let rows = match rows_result {
        Ok(r) => r,
        Err(err) => {
            warn!(conversation_id, error = %err, "mcp_resolve: list() failed; skipping injection");
            return Vec::new();
        }
    };

    let mut servers = Vec::with_capacity(rows.len());
    for row in rows {
        let selected = selected_ids
            .map(|ids| ids.iter().any(|id| id == &row.id))
            .unwrap_or(row.enabled);
        // `aionui-team` is a reserved wire-level name: the team coordination MCP
        // must win, so a user row that collides with it is skipped (never
        // injected), regardless of selection state.
        if !selected || row.builtin || row.name == TEAM_MCP_SERVER_NAME {
            continue;
        }
        match row_to_session_mcp_server(&row, user_id, oauth_token_repo).await {
            Ok(server) => servers.push(server),
            Err(err) => {
                warn!(
                    conversation_id,
                    server_id = %row.id,
                    server_name = %row.name,
                    error = %err,
                    "mcp_resolve: failed to convert row; skipping"
                );
            }
        }
    }

    if !servers.is_empty() {
        info!(
            conversation_id,
            count = servers.len(),
            "mcp_resolve: resolved user MCP servers"
        );
    }
    servers
}

/// Parse one `McpServerRow` into a neutral `SessionMcpServer`, resolving the stdio
/// launch command. Mirrors `factory::acp::row_to_sdk_mcp_server` but emits the
/// neutral type. Returns an error string when `transport_config` is malformed.
///
/// This is the SINGLE shared row→neutral conversion: `aionui-conversation` (team
/// snapshot refresh) and the session stack reuse it so the stdio command is
/// always normalized exactly once, the same way the direct claude/codex path
/// consumes it.
pub async fn row_to_session_mcp_server(
    row: &McpServerRow,
    user_id: &str,
    oauth_token_repo: Option<&dyn IOAuthTokenRepository>,
) -> Result<SessionMcpServer, String> {
    let value: serde_json::Value =
        serde_json::from_str(&row.transport_config).map_err(|e| format!("invalid transport_config JSON: {e}"))?;

    let transport = match row.transport_type.as_str() {
        "stdio" => {
            let command = value
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "stdio: missing command".to_owned())?;
            let args: Vec<String> = value
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let mut env: std::collections::HashMap<String, String> = value
                .get("env")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                        .collect()
                })
                .unwrap_or_default();

            // Resolve the launch command (npx/bun → bundled path) + fold in the
            // runtime-provided args prefix + env, exactly like the legacy
            // `ensure_stdio_launch`. The resolved form is what the agent spawns.
            let resolved = ensure_runtime_command(command).await.map_err(|e| e.to_string())?;
            let mut final_args: Vec<String> = resolved
                .args_prefix
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            final_args.extend(args);
            for (k, v) in resolved.env {
                env.insert(k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned());
            }
            SessionMcpTransport::Stdio {
                command: resolved.program.to_string_lossy().into_owned(),
                args: final_args,
                env,
            }
        }
        "http" | "streamable_http" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "http: missing url".to_owned())?
                .to_owned();
            let mut headers = parse_headers(value.get("headers"));
            inject_oauth_bearer_header(&mut headers, user_id, &url, oauth_token_repo).await;
            SessionMcpTransport::StreamableHttp { url, headers }
        }
        "sse" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "sse: missing url".to_owned())?
                .to_owned();
            let mut headers = parse_headers(value.get("headers"));
            inject_oauth_bearer_header(&mut headers, user_id, &url, oauth_token_repo).await;
            SessionMcpTransport::Sse { url, headers }
        }
        other => return Err(format!("unknown transport type: {other}")),
    };

    Ok(SessionMcpServer {
        id: row.id.clone(),
        name: row.name.clone(),
        transport,
    })
}

/// Attach a stored OAuth access token as `Authorization: Bearer <token>` for
/// an HTTP/SSE MCP server, if one exists for this user and URL.
///
/// A server-configured `Authorization` header (case-insensitive) always
/// wins — a user who set one explicitly presumably knows what they're
/// doing, and OAuth login is opt-in on top of that, not a silent override.
/// Best-effort: an expired token or a repo error just means the server
/// won't have a token attached and the tool call fails with a normal
/// auth-required error downstream, exactly as if OAuth had never run.
async fn inject_oauth_bearer_header(
    headers: &mut std::collections::HashMap<String, String>,
    user_id: &str,
    server_url: &str,
    oauth_token_repo: Option<&dyn IOAuthTokenRepository>,
) {
    let Some(repo) = oauth_token_repo else { return };
    if headers.keys().any(|k| k.eq_ignore_ascii_case("authorization")) {
        return;
    }

    let token = match repo.get_by_url(user_id, server_url).await {
        Ok(Some(row)) => row,
        Ok(None) => return,
        Err(err) => {
            warn!(server_url, error = %err, "mcp_resolve: OAuth token lookup failed; continuing without it");
            return;
        }
    };

    if let Some(expires_at) = token.expires_at
        && aionui_common::now_ms() >= expires_at
    {
        // Expired and unrefreshed here (refresh happens lazily via the
        // check-status/get-token API paths, not this session-build path) —
        // an expired token would just fail auth anyway, so omit it rather
        // than send a token known to be rejected.
        return;
    }

    headers.insert("Authorization".to_string(), format!("Bearer {}", token.access_token));
}

/// Parse a JSON headers object into a `HashMap` (string values only).
fn parse_headers(value: Option<&serde_json::Value>) -> std::collections::HashMap<String, String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::models::McpServerRow;

    const TEST_USER_ID: &str = "user-1";

    struct MockRepo {
        rows: Vec<McpServerRow>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl IMcpServerRepository for MockRepo {
        async fn list(&self, user_id: &str) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            if self.fail {
                return Err(aionui_db::DbError::Init("boom".into()));
            }
            Ok(self.rows.iter().filter(|row| row.user_id == user_id).cloned().collect())
        }
        async fn find_by_id(&self, user_id: &str, id: &str) -> Result<Option<McpServerRow>, aionui_db::DbError> {
            Ok(self
                .rows
                .iter()
                .find(|row| row.user_id == user_id && row.id == id)
                .cloned())
        }
        async fn find_by_name(&self, user_id: &str, name: &str) -> Result<Option<McpServerRow>, aionui_db::DbError> {
            Ok(self
                .rows
                .iter()
                .find(|row| row.user_id == user_id && row.name == name)
                .cloned())
        }
        async fn create(
            &self,
            _params: aionui_db::CreateMcpServerParams<'_>,
        ) -> Result<McpServerRow, aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn update(
            &self,
            _user_id: &str,
            _id: &str,
            _params: aionui_db::UpdateMcpServerParams<'_>,
        ) -> Result<McpServerRow, aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn delete(&self, _user_id: &str, _id: &str) -> Result<(), aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn batch_upsert(
            &self,
            _user_id: &str,
            _servers: &[aionui_db::CreateMcpServerParams<'_>],
        ) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn update_status(
            &self,
            _user_id: &str,
            _id: &str,
            _status: &str,
            _last_connected: Option<aionui_common::TimestampMs>,
        ) -> Result<(), aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn update_tools(
            &self,
            _user_id: &str,
            _id: &str,
            _tools: Option<&str>,
        ) -> Result<(), aionui_db::DbError> {
            unimplemented!("not needed")
        }
    }

    fn make_row(name: &str, enabled: bool) -> McpServerRow {
        McpServerRow {
            id: format!("mcp_{name}"),
            user_id: TEST_USER_ID.to_owned(),
            name: name.to_owned(),
            description: None,
            enabled,
            transport_type: "http".into(),
            transport_config: r#"{"url":"http://127.0.0.1:9999/mcp"}"#.into(),
            tools: None,
            last_test_status: "disconnected".into(),
            last_connected: None,
            original_json: None,
            builtin: false,
            deleted_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn test_broadcaster() -> Arc<dyn EventBroadcaster> {
        Arc::new(aionui_realtime::BroadcastEventBus::new(16))
    }

    #[tokio::test]
    async fn resolve_skips_reserved_team_mcp_name() {
        let repo = MockRepo {
            rows: vec![make_row("docs", true), make_row(TEAM_MCP_SERVER_NAME, true)],
            fail: false,
        };
        let servers = resolve_session_mcp_servers(&repo, TEST_USER_ID, None, "conv-1", test_broadcaster(), None).await;
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "docs");
    }

    #[tokio::test]
    async fn resolve_skips_disabled_and_builtin_rows() {
        let repo = MockRepo {
            rows: vec![
                make_row("docs", true),
                make_row("off", false),
                McpServerRow {
                    id: "mcp_builtin".into(),
                    user_id: TEST_USER_ID.to_owned(),
                    name: "chrome-devtools".into(),
                    description: None,
                    enabled: true,
                    transport_type: "stdio".into(),
                    transport_config: r#"{"command":"/bin/true","args":[],"env":{}}"#.into(),
                    tools: None,
                    last_test_status: "disconnected".into(),
                    last_connected: None,
                    original_json: None,
                    builtin: true,
                    deleted_at: None,
                    created_at: 0,
                    updated_at: 0,
                },
            ],
            fail: false,
        };
        let servers = resolve_session_mcp_servers(&repo, TEST_USER_ID, None, "conv-1", test_broadcaster(), None).await;
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "docs");
    }

    // -- inject_oauth_bearer_header ------------------------------------------
    //
    // Regression coverage: OAuth login alone never made a stored token reach
    // an actual MCP tool call — nothing attached it as an Authorization
    // header when building the session's HTTP/SSE transport. Login worked,
    // the token was stored, and every tool call still failed as
    // unauthenticated.

    struct MockOAuthRepo {
        row: Option<aionui_db::models::OAuthTokenRow>,
    }

    #[async_trait::async_trait]
    impl IOAuthTokenRepository for MockOAuthRepo {
        async fn get_by_url(
            &self,
            _user_id: &str,
            _server_url: &str,
        ) -> Result<Option<aionui_db::models::OAuthTokenRow>, aionui_db::DbError> {
            Ok(self.row.clone())
        }
        async fn upsert(
            &self,
            _params: aionui_db::UpsertOAuthTokenParams<'_>,
        ) -> Result<aionui_db::models::OAuthTokenRow, aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn delete(&self, _user_id: &str, _server_url: &str) -> Result<(), aionui_db::DbError> {
            unimplemented!("not needed")
        }
        async fn list_authenticated_urls(&self, _user_id: &str) -> Result<Vec<String>, aionui_db::DbError> {
            unimplemented!("not needed")
        }
    }

    fn token_row(
        access_token: &str,
        expires_at: Option<aionui_common::TimestampMs>,
    ) -> aionui_db::models::OAuthTokenRow {
        aionui_db::models::OAuthTokenRow {
            user_id: TEST_USER_ID.to_owned(),
            server_url: "http://127.0.0.1:9999/mcp".to_owned(),
            access_token: access_token.to_owned(),
            refresh_token: None,
            token_type: "bearer".to_owned(),
            expires_at,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[tokio::test]
    async fn attaches_bearer_header_when_valid_token_exists() {
        let oauth_repo = MockOAuthRepo {
            row: Some(token_row("tok-abc", None)),
        };
        let row = make_row("higgsfield", true);

        let server = row_to_session_mcp_server(&row, TEST_USER_ID, Some(&oauth_repo))
            .await
            .unwrap();

        match server.transport {
            SessionMcpTransport::StreamableHttp { headers, .. } => {
                assert_eq!(headers.get("Authorization"), Some(&"Bearer tok-abc".to_string()));
            }
            _ => panic!("expected StreamableHttp transport"),
        }
    }

    #[tokio::test]
    async fn omits_header_when_token_is_expired() {
        let oauth_repo = MockOAuthRepo {
            row: Some(token_row("tok-abc", Some(1))), // 1ms since epoch: always expired
        };
        let row = make_row("higgsfield", true);

        let server = row_to_session_mcp_server(&row, TEST_USER_ID, Some(&oauth_repo))
            .await
            .unwrap();

        match server.transport {
            SessionMcpTransport::StreamableHttp { headers, .. } => {
                assert!(!headers.contains_key("Authorization"));
            }
            _ => panic!("expected StreamableHttp transport"),
        }
    }

    #[tokio::test]
    async fn omits_header_when_no_oauth_repo_given() {
        let row = make_row("higgsfield", true);

        let server = row_to_session_mcp_server(&row, TEST_USER_ID, None).await.unwrap();

        match server.transport {
            SessionMcpTransport::StreamableHttp { headers, .. } => {
                assert!(!headers.contains_key("Authorization"));
            }
            _ => panic!("expected StreamableHttp transport"),
        }
    }

    #[tokio::test]
    async fn omits_header_when_no_token_stored() {
        let oauth_repo = MockOAuthRepo { row: None };
        let row = make_row("higgsfield", true);

        let server = row_to_session_mcp_server(&row, TEST_USER_ID, Some(&oauth_repo))
            .await
            .unwrap();

        match server.transport {
            SessionMcpTransport::StreamableHttp { headers, .. } => {
                assert!(!headers.contains_key("Authorization"));
            }
            _ => panic!("expected StreamableHttp transport"),
        }
    }

    #[tokio::test]
    async fn a_server_configured_authorization_header_wins_over_oauth() {
        let oauth_repo = MockOAuthRepo {
            row: Some(token_row("tok-from-oauth", None)),
        };
        let mut row = make_row("higgsfield", true);
        row.transport_config =
            r#"{"url":"http://127.0.0.1:9999/mcp","headers":{"Authorization":"Bearer static-key"}}"#.to_owned();

        let server = row_to_session_mcp_server(&row, TEST_USER_ID, Some(&oauth_repo))
            .await
            .unwrap();

        match server.transport {
            SessionMcpTransport::StreamableHttp { headers, .. } => {
                assert_eq!(headers.get("Authorization"), Some(&"Bearer static-key".to_string()));
            }
            _ => panic!("expected StreamableHttp transport"),
        }
    }
}
