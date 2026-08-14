use std::sync::Arc;

use aionui_common::{CapabilityOrigin, McpTransportCapabilities, ResolvedBackendCapabilities};
use aionui_db::{IAgentMetadataRepository, resolve_agent_binding_from_rows};
use aionui_team::{TeamError, TeamToolCapabilityPort};

pub(crate) struct TeamCapabilityResolver {
    repo: Arc<dyn IAgentMetadataRepository>,
}

impl TeamCapabilityResolver {
    pub(crate) fn new(repo: Arc<dyn IAgentMetadataRepository>) -> Self {
        Self { repo }
    }
}

fn bool_field(value: &serde_json::Value, key: &str) -> bool {
    value.get(key).and_then(serde_json::Value::as_bool) == Some(true)
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

fn resolve_acp_handshake(capabilities: Option<&serde_json::Value>) -> ResolvedBackendCapabilities {
    let Some(capabilities) = capabilities else {
        return ResolvedBackendCapabilities {
            cli_fallback: true,
            ..Default::default()
        };
    };
    let mcp = capabilities
        .get("mcp_capabilities")
        .or_else(|| capabilities.get("mcpCapabilities"))
        .or_else(|| capabilities.get("mcp"));
    let streamable_http = mcp.is_some_and(|mcp| bool_field(mcp, "http"));
    let sse = mcp.is_some_and(|mcp| bool_field(mcp, "sse"));
    let cli_fallback = ![
        &["shell"][..],
        &["cli"][..],
        &["supports_shell"][..],
        &["supportsShell"][..],
        &["supports_cli"][..],
        &["supportsCli"][..],
        &["execution", "shell"][..],
        &["execution", "cli"][..],
    ]
    .iter()
    .any(|path| explicit_false(capabilities, path));
    ResolvedBackendCapabilities {
        // ACP's stdio support is conservatively inferred only when at least one
        // optional MCP transport was positively advertised, preserving the
        // existing live-probed compatibility heuristic.
        mcp: McpTransportCapabilities {
            stdio: streamable_http || sse,
            sse,
            streamable_http,
        },
        cli_fallback,
        origin: CapabilityOrigin::AcpHandshake,
    }
}

#[async_trait::async_trait]
impl TeamToolCapabilityPort for TeamCapabilityResolver {
    async fn resolve(
        &self,
        user_id: &str,
        backend: &str,
        agent_id: Option<&str>,
    ) -> Result<ResolvedBackendCapabilities, TeamError> {
        if let Some(descriptor) = aionui_session::backend_capability_descriptor(backend) {
            return Ok(descriptor.resolved());
        }

        let rows = self.repo.list_all_for_user(user_id).await?;
        let selected_id = agent_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| resolve_agent_binding_from_rows(&rows, backend).map(|binding| binding.agent_id));
        let Some(row) = selected_id.and_then(|id| rows.into_iter().find(|row| row.id == id)) else {
            return Ok(ResolvedBackendCapabilities {
                cli_fallback: true,
                ..Default::default()
            });
        };
        let Some(raw) = row
            .agent_capabilities
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(ResolvedBackendCapabilities {
                cli_fallback: true,
                ..Default::default()
            });
        };
        let capabilities = match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(capabilities) => capabilities,
            Err(error) => {
                tracing::warn!(
                    backend,
                    agent_id = %row.id,
                    error = %error,
                    "ignoring malformed persisted backend capability snapshot"
                );
                return Ok(ResolvedBackendCapabilities {
                    cli_fallback: true,
                    ..Default::default()
                });
            }
        };
        Ok(resolve_acp_handshake(Some(&capabilities)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::{SqliteAgentMetadataRepository, init_database_memory};

    async fn direct_matrix(snapshot: Option<&str>) -> Vec<ResolvedBackendCapabilities> {
        let db = init_database_memory().await.unwrap();
        if let Some(snapshot) = snapshot {
            for id in ["2d23ff1c", "8e1acf31", "a9f3c21e"] {
                sqlx::query("UPDATE agent_metadata SET agent_capabilities = ? WHERE agent_id = ?")
                    .bind(snapshot)
                    .bind(id)
                    .execute(db.pool())
                    .await
                    .unwrap();
            }
        }
        let resolver = TeamCapabilityResolver::new(Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone())));
        let mut resolved = Vec::new();
        for descriptor in aionui_session::backend_capability_descriptors()
            .iter()
            .filter(|descriptor| descriptor.origin == CapabilityOrigin::DirectDescriptor)
        {
            let capabilities = resolver
                .resolve("system_default_user", descriptor.backend_id, None)
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "registered direct backend {} did not resolve its constructed capabilities: {error}",
                        descriptor.backend_id
                    )
                });
            assert_eq!(
                capabilities,
                descriptor.resolved(),
                "registered direct backend {} bypassed its constructed descriptor",
                descriptor.backend_id
            );
            resolved.push(capabilities);
        }
        resolved
    }

    #[tokio::test]
    async fn direct_capabilities_are_identical_for_fresh_and_historical_databases() {
        let fresh = direct_matrix(None).await;
        let historical_true = direct_matrix(Some(r#"{"mcp_capabilities":{"http":true,"sse":true}}"#)).await;
        let historical_false = direct_matrix(Some(r#"{"mcp_capabilities":{}}"#)).await;

        assert_eq!(fresh, historical_true);
        assert_eq!(fresh, historical_false);
        assert!(
            fresh
                .iter()
                .all(|caps| caps.origin == CapabilityOrigin::DirectDescriptor)
        );
    }

    #[test]
    fn acp_resolution_preserves_positive_unknown_and_explicit_no_shell_cases() {
        let positive = serde_json::json!({"mcp_capabilities": {"http": true, "sse": false}});
        let positive = resolve_acp_handshake(Some(&positive));
        assert_eq!(positive.origin, CapabilityOrigin::AcpHandshake);
        assert!(positive.mcp.stdio);

        let unknown = resolve_acp_handshake(None);
        assert_eq!(unknown.origin, CapabilityOrigin::Unknown);
        assert!(unknown.cli_fallback);

        let no_shell = serde_json::json!({"mcp_capabilities": {}, "execution": {"shell": false}});
        let no_shell = resolve_acp_handshake(Some(&no_shell));
        assert_eq!(no_shell.origin, CapabilityOrigin::AcpHandshake);
        assert!(!no_shell.mcp.stdio);
        assert!(!no_shell.cli_fallback);
    }
}
