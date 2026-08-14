//! Writes the session's MCP servers into `<workspace>/.agents/mcp_config.json`.
//!
//! agy has no per-run flag for MCP configuration — it only reads files — and
//! the workspace-level file is what gives each conversation its own set of
//! servers (verified: `crates/aionui-app/tests/live_ws_http_parity_e2e.rs`,
//! `live_antigravity_team_mcp_tools_call_and_runtime_env`, against agy 1.1.13).
//! This is also how the team coordination server reaches an Antigravity
//! teammate.
//!
//! agy supports exactly two transports, Stdio and SSE (verified:
//! `~/.gemini/antigravity-cli/builtin/skills/agy-customizations/docs/mcp_servers.md`),
//! so an HTTP server is dropped
//! rather than mistranslated.

use std::path::Path;

use crate::backend::{McpServerSpec, McpTransport};

/// Workspace customization directory agy scans.
const AGENTS_DIR: &str = ".agents";

fn pairs_to_object(pairs: &[(String, String)]) -> serde_json::Map<String, serde_json::Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect()
}

/// Serialize `servers` into the workspace's `mcp_config.json`.
///
/// An empty (or entirely unsupported) server set REMOVES the file: leaving a
/// stale one would hand this session the previous session's servers.
pub(crate) fn write_mcp_config(workspace: &Path, servers: &[McpServerSpec]) -> std::io::Result<()> {
    let path = workspace.join(AGENTS_DIR).join("mcp_config.json");

    let mut map = serde_json::Map::new();
    for server in servers {
        let entry = match &server.transport {
            McpTransport::Stdio { command, args, env } => serde_json::json!({
                "command": command,
                "args": args,
                "env": pairs_to_object(env),
            }),
            McpTransport::Sse { url, headers } => serde_json::json!({
                "serverUrl": url,
                "headers": pairs_to_object(headers),
            }),
            McpTransport::Http { .. } => {
                // Emitting this as `serverUrl` would write a config that looks
                // valid and then never connects — SSE and streamable-HTTP are
                // different protocols.
                tracing::warn!(
                    backend = "antigravity",
                    server = %server.name,
                    transport = "streamable_http",
                    "antigravity: skipping MCP server — agy supports stdio and SSE only"
                );
                continue;
            }
        };
        map.insert(server.name.clone(), entry);
    }

    if map.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        return Ok(());
    }

    std::fs::create_dir_all(workspace.join(AGENTS_DIR))?;
    let body = serde_json::json!({ "mcpServers": map });
    std::fs::write(path, serde_json::to_vec_pretty(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{McpServerSpec, McpTransport};

    fn read_config(dir: &std::path::Path) -> serde_json::Value {
        let raw = std::fs::read_to_string(dir.join(".agents/mcp_config.json")).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn stdio_spec_maps_to_agys_command_shape() {
        assert!(
            crate::backend::backend_capability_descriptor("antigravity")
                .unwrap()
                .mcp
                .stdio
        );
        let dir = tempfile::tempdir().unwrap();
        let servers = vec![McpServerSpec {
            name: "aionui-team".into(),
            transport: McpTransport::Stdio {
                command: "/opt/aionui/backend".into(),
                args: vec!["mcp-team-stdio".into()],
                env: vec![
                    ("TEAM_MCP_PORT".into(), "8931".into()),
                    ("TEAM_MCP_TOKEN".into(), "tok".into()),
                ],
            },
        }];
        write_mcp_config(dir.path(), &servers).unwrap();

        let s = &read_config(dir.path())["mcpServers"]["aionui-team"];
        assert_eq!(s["command"], "/opt/aionui/backend");
        assert_eq!(s["args"][0], "mcp-team-stdio");
        // agy wants env as an OBJECT; we carry it as ordered pairs internally.
        assert_eq!(s["env"]["TEAM_MCP_PORT"], "8931");
        assert_eq!(s["env"]["TEAM_MCP_TOKEN"], "tok");
    }

    #[test]
    fn sse_spec_maps_to_server_url_and_headers() {
        assert!(
            crate::backend::backend_capability_descriptor("antigravity")
                .unwrap()
                .mcp
                .sse
        );
        let dir = tempfile::tempdir().unwrap();
        let servers = vec![McpServerSpec {
            name: "remote".into(),
            transport: McpTransport::Sse {
                url: "https://api.example.com/sse".into(),
                headers: vec![("Authorization".into(), "Bearer t".into())],
            },
        }];
        write_mcp_config(dir.path(), &servers).unwrap();

        let s = &read_config(dir.path())["mcpServers"]["remote"];
        assert_eq!(s["serverUrl"], "https://api.example.com/sse");
        assert_eq!(s["headers"]["Authorization"], "Bearer t");
    }

    #[test]
    fn http_transport_is_skipped_rather_than_passed_off_as_sse() {
        assert!(
            !crate::backend::backend_capability_descriptor("antigravity")
                .unwrap()
                .mcp
                .streamable_http
        );
        // agy supports stdio and SSE only. Writing an HTTP server as
        // `serverUrl` produces a config that looks fine and then fails at
        // runtime with an opaque timeout, because the protocols differ.
        let dir = tempfile::tempdir().unwrap();
        let servers = vec![
            McpServerSpec {
                name: "unsupported".into(),
                transport: McpTransport::Http {
                    url: "https://x/mcp".into(),
                    headers: vec![],
                },
            },
            McpServerSpec {
                name: "kept".into(),
                transport: McpTransport::Stdio {
                    command: "node".into(),
                    args: vec![],
                    env: vec![],
                },
            },
        ];
        write_mcp_config(dir.path(), &servers).unwrap();

        let cfg = read_config(dir.path());
        assert!(cfg["mcpServers"].get("unsupported").is_none());
        assert!(cfg["mcpServers"].get("kept").is_some());
    }

    #[test]
    fn an_empty_server_list_removes_a_stale_config() {
        // Leftover servers would silently give this session the previous
        // session's tools.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".agents")).unwrap();
        std::fs::write(
            dir.path().join(".agents/mcp_config.json"),
            r#"{"mcpServers":{"old":{"command":"x"}}}"#,
        )
        .unwrap();

        write_mcp_config(dir.path(), &[]).unwrap();
        assert!(!dir.path().join(".agents/mcp_config.json").exists());
    }

    #[test]
    fn writing_no_servers_into_a_clean_workspace_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write_mcp_config(dir.path(), &[]).unwrap();
        assert!(!dir.path().join(".agents/mcp_config.json").exists());
    }
}
