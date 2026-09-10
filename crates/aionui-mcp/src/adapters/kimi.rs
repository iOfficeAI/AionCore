use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aionui_common::McpSource;

use crate::adapter::{DetectedServer, McpAgentAdapter};
use crate::error::McpError;
use crate::types::McpServerTransport;

use super::cli_helpers::is_cli_installed;

const CLI_NAME: &str = "kimi";

/// MCP Agent adapter for Kimi Code CLI.
///
/// Kimi Code CLI stores MCP configuration in `~/.kimi-code/mcp.json`
/// (overridable via `$KIMI_CODE_HOME/mcp.json`). It exposes no `mcp`
/// subcommand, so detection reads the file directly and install/remove
/// rewrite it (same approach as `OpencodeAdapter`).
///
/// # Config Format
///
/// ```json
/// {
///   "mcpServers": {
///     "filesystem": {
///       "command": "npx",
///       "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
///       "env": { "KEY": "VALUE" }
///     },
///     "linear": {
///       "url": "https://mcp.linear.app/mcp"
///     },
///     "legacy-events": {
///       "transport": "sse",
///       "url": "https://mcp.example.com/sse"
///     }
///   }
/// }
/// ```
///
/// Entries with a `command` field are stdio servers; entries with a
/// `url` field and no `transport` are HTTP servers; `transport: "sse"`
/// marks a legacy SSE server. Optional fields include `env`, `cwd`,
/// `headers`, `bearerTokenEnvVar`, `enabled`, and per-server timeouts.
///
/// See <https://moonshotai.github.io/kimi-code/en/customization/mcp>.
pub struct KimiAdapter;

#[async_trait::async_trait]
impl McpAgentAdapter for KimiAdapter {
    fn source(&self) -> McpSource {
        McpSource::Kimi
    }

    async fn is_installed(&self) -> Result<bool, McpError> {
        is_cli_installed(CLI_NAME).await
    }

    async fn detect_existing(&self, _user_id: &str) -> Result<Vec<DetectedServer>, McpError> {
        if !self.is_installed().await? {
            return Err(McpError::AgentNotInstalled(CLI_NAME.into()));
        }

        let config_path = config_file_path()?;
        if !config_path.exists() {
            return Ok(Vec::new());
        }

        let content = tokio::fs::read_to_string(&config_path)
            .await
            .map_err(|e| McpError::AgentOperationFailed(format!("read kimi mcp config: {e}")))?;

        parse_kimi_config(&content)
    }

    async fn install_server(&self, name: &str, transport: &McpServerTransport) -> Result<(), McpError> {
        if !self.is_installed().await? {
            return Err(McpError::AgentNotInstalled(CLI_NAME.into()));
        }

        install_server_at(&config_file_path()?, name, transport).await
    }

    async fn remove_server(&self, name: &str) -> Result<(), McpError> {
        if !self.is_installed().await? {
            return Err(McpError::AgentNotInstalled(CLI_NAME.into()));
        }

        remove_server_at(&config_file_path()?, name).await
    }
}

// ---------------------------------------------------------------------------
// Config file I/O
// ---------------------------------------------------------------------------

/// Get the Kimi Code user-level MCP config path.
///
/// Honors `$KIMI_CODE_HOME` (set via the `KIMI_CODE_HOME` environment
/// variable) and falls back to `~/.kimi-code/mcp.json`.
fn config_file_path() -> Result<PathBuf, McpError> {
    if let Ok(home) = std::env::var("KIMI_CODE_HOME")
        && !home.is_empty()
    {
        return Ok(PathBuf::from(home).join("mcp.json"));
    }

    let home =
        dirs::home_dir().ok_or_else(|| McpError::AgentOperationFailed("cannot determine home directory".into()))?;
    Ok(home.join(".kimi-code").join("mcp.json"))
}

/// Upsert an MCP server entry into the config file at `path`.
///
/// Creates the file (and parent directory) if it does not exist.
/// Replaces an existing entry with the same name.
async fn install_server_at(path: &Path, name: &str, transport: &McpServerTransport) -> Result<(), McpError> {
    let mut root = if path.exists() {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| McpError::AgentOperationFailed(format!("failed to read {}: {e}", path.display())))?;
        parse_kimi_config_root(&content)?
    } else {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| McpError::AgentOperationFailed(format!("failed to create dir: {e}")))?;
        }
        serde_json::json!({})
    };

    let servers = root
        .as_object_mut()
        .ok_or_else(|| McpError::AgentOperationFailed("config root is not an object".into()))?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));

    let servers_obj = servers
        .as_object_mut()
        .ok_or_else(|| McpError::AgentOperationFailed("mcpServers field is not an object".into()))?;

    servers_obj.insert(name.to_owned(), transport_to_json(transport));

    write_json_atomic(path, &root).await
}

/// Remove an MCP server entry from the config file at `path`.
///
/// Idempotent: removing a non-existent server is not an error.
async fn remove_server_at(path: &Path, name: &str) -> Result<(), McpError> {
    if !path.exists() {
        return Ok(());
    }

    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| McpError::AgentOperationFailed(format!("failed to read {}: {e}", path.display())))?;

    let mut root = parse_kimi_config_root(&content)?;

    let removed = root
        .as_object_mut()
        .and_then(|obj| obj.get_mut("mcpServers"))
        .and_then(|servers| servers.as_object_mut())
        .map(|servers_obj| servers_obj.remove(name).is_some())
        .unwrap_or(false);

    if !removed {
        // Idempotent: not found is fine
        return Ok(());
    }

    write_json_atomic(path, &root).await
}

/// Serialize the config root to pretty JSON and write it to `path`
/// via a temporary file + rename to avoid corrupting the user's config
/// on a partial write.
async fn write_json_atomic(path: &Path, root: &serde_json::Value) -> Result<(), McpError> {
    let output = serde_json::to_string_pretty(root)
        .map_err(|e| McpError::AgentOperationFailed(format!("failed to serialize config: {e}")))?;

    let parent = path
        .parent()
        .ok_or_else(|| McpError::AgentOperationFailed("config path has no parent".into()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| McpError::AgentOperationFailed(format!("failed to create dir: {e}")))?;

    let tmp_path = parent.join(format!(".mcp.json.{}.tmp", std::process::id()));

    tokio::fs::write(&tmp_path, output)
        .await
        .map_err(|e| McpError::AgentOperationFailed(format!("failed to write {}: {e}", tmp_path.display())))?;

    tokio::fs::rename(&tmp_path, path)
        .await
        .map_err(|e| McpError::AgentOperationFailed(format!("failed to replace {}: {e}", path.display())))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse the `mcpServers` map from the Kimi `mcp.json` config file.
fn parse_kimi_config(content: &str) -> Result<Vec<DetectedServer>, McpError> {
    let root = parse_kimi_config_root(content)?;
    Ok(parse_mcp_servers(&root))
}

/// Parse the config root JSON object.
fn parse_kimi_config_root(content: &str) -> Result<serde_json::Value, McpError> {
    serde_json::from_str(content).map_err(McpError::from)
}

/// Extract detected servers from the parsed config root.
fn parse_mcp_servers(root: &serde_json::Value) -> Vec<DetectedServer> {
    let servers_obj = match root.get("mcpServers").and_then(|v| v.as_object()) {
        Some(obj) => obj,
        None => return Vec::new(),
    };

    let mut servers = Vec::new();

    for (name, entry) in servers_obj {
        if let Some(transport) = parse_kimi_entry(entry) {
            let enabled = entry.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            servers.push(DetectedServer {
                name: name.clone(),
                transport,
                importable: enabled,
                import_skip_reason: if enabled { None } else { Some("Disabled".into()) },
            });
        }
    }

    servers
}

/// Parse a single Kimi config entry into a transport.
///
/// Entries with a `command` field are stdio; entries with a `url` field
/// are HTTP unless `transport` is `"sse"`.
fn parse_kimi_entry(entry: &serde_json::Value) -> Option<McpServerTransport> {
    let has_command = entry.get("command").and_then(|v| v.as_str()).is_some();
    let has_url = entry.get("url").and_then(|v| v.as_str()).is_some();

    if has_command {
        let command = entry["command"].as_str()?.to_owned();
        let args = entry
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let env = parse_string_map(entry.get("env"));
        Some(McpServerTransport::Stdio { command, args, env })
    } else if has_url {
        let url = entry["url"].as_str()?.to_owned();
        let headers = parse_string_map(entry.get("headers"));
        let transport_type = entry.get("transport").and_then(|v| v.as_str()).unwrap_or("http");
        match transport_type {
            "sse" => Some(McpServerTransport::Sse { url, headers }),
            _ => Some(McpServerTransport::Http { url, headers }),
        }
    } else {
        None
    }
}

/// Parse a JSON object as `HashMap<String, String>`.
fn parse_string_map(value: Option<&serde_json::Value>) -> HashMap<String, String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

/// Convert a `McpServerTransport` to a Kimi config entry.
fn transport_to_json(transport: &McpServerTransport) -> serde_json::Value {
    match transport {
        McpServerTransport::Stdio { command, args, env } => {
            let mut obj = serde_json::json!({
                "command": command,
                "args": args,
            });
            if !env.is_empty() {
                obj["env"] = serde_json::json!(env);
            }
            obj
        }
        McpServerTransport::Sse { url, headers } => {
            let mut obj = serde_json::json!({
                "transport": "sse",
                "url": url,
            });
            if !headers.is_empty() {
                obj["headers"] = serde_json::json!(headers);
            }
            obj
        }
        McpServerTransport::Http { url, headers } => {
            let mut obj = serde_json::json!({
                "url": url,
            });
            if !headers.is_empty() {
                obj["headers"] = serde_json::json!(headers);
            }
            obj
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_is_kimi() {
        assert_eq!(KimiAdapter.source(), McpSource::Kimi);
    }

    #[test]
    fn parse_empty_config() {
        let servers = parse_kimi_config(r#"{ "mcpServers": {} }"#).unwrap();
        assert!(servers.is_empty());
    }

    #[test]
    fn parse_config_without_mcp_servers_field() {
        let servers = parse_kimi_config(r#"{ "other": "stuff" }"#).unwrap();
        assert!(servers.is_empty());
    }

    #[test]
    fn parse_stdio_server_with_env() {
        let content = r#"{
            "mcpServers": {
                "test-mcp": {
                    "command": "npx",
                    "args": ["-y", "@test/server"],
                    "env": { "KEY": "VALUE" }
                }
            }
        }"#;
        let servers = parse_kimi_config(content).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "test-mcp");
        match &servers[0].transport {
            McpServerTransport::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args, &["-y", "@test/server"]);
                assert_eq!(env.get("KEY").unwrap(), "VALUE");
            }
            _ => panic!("expected Stdio"),
        }
        assert!(servers[0].importable);
    }

    #[test]
    fn parse_stdio_server_without_args() {
        let content = r#"{
            "mcpServers": {
                "simple": { "command": "node" }
            }
        }"#;
        let servers = parse_kimi_config(content).unwrap();
        assert_eq!(servers.len(), 1);
        match &servers[0].transport {
            McpServerTransport::Stdio { command, args, env } => {
                assert_eq!(command, "node");
                assert!(args.is_empty());
                assert!(env.is_empty());
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn parse_http_server() {
        let content = r#"{
            "mcpServers": {
                "linear": { "url": "https://mcp.linear.app/mcp" }
            }
        }"#;
        let servers = parse_kimi_config(content).unwrap();
        assert_eq!(servers.len(), 1);
        match &servers[0].transport {
            McpServerTransport::Http { url, headers } => {
                assert_eq!(url, "https://mcp.linear.app/mcp");
                assert!(headers.is_empty());
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn parse_http_server_with_headers() {
        let content = r#"{
            "mcpServers": {
                "remote": {
                    "url": "https://example.com/mcp",
                    "headers": { "Authorization": "Bearer tok" }
                }
            }
        }"#;
        let servers = parse_kimi_config(content).unwrap();
        assert_eq!(servers.len(), 1);
        match &servers[0].transport {
            McpServerTransport::Http { url, headers } => {
                assert_eq!(url, "https://example.com/mcp");
                assert_eq!(headers.get("Authorization").unwrap(), "Bearer tok");
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn parse_sse_server() {
        let content = r#"{
            "mcpServers": {
                "legacy-events": {
                    "transport": "sse",
                    "url": "https://mcp.example.com/sse"
                }
            }
        }"#;
        let servers = parse_kimi_config(content).unwrap();
        assert_eq!(servers.len(), 1);
        match &servers[0].transport {
            McpServerTransport::Sse { url, .. } => {
                assert_eq!(url, "https://mcp.example.com/sse");
            }
            _ => panic!("expected Sse"),
        }
    }

    #[test]
    fn parse_disabled_server_not_importable() {
        let content = r#"{
            "mcpServers": {
                "off": {
                    "command": "npx",
                    "enabled": false
                }
            }
        }"#;
        let servers = parse_kimi_config(content).unwrap();
        assert_eq!(servers.len(), 1);
        assert!(!servers[0].importable);
        assert_eq!(servers[0].import_skip_reason.as_deref(), Some("Disabled"));
    }

    #[test]
    fn parse_multiple_servers() {
        let content = r#"{
            "mcpServers": {
                "stdio-srv": { "command": "node", "args": ["srv.js"] },
                "http-srv": { "url": "https://a.com/mcp" }
            }
        }"#;
        let servers = parse_kimi_config(content).unwrap();
        assert_eq!(servers.len(), 2);
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"stdio-srv"));
        assert!(names.contains(&"http-srv"));
    }

    #[test]
    fn parse_entry_without_command_or_url_skipped() {
        let content = r#"{
            "mcpServers": {
                "bad": { "args": [] }
            }
        }"#;
        let servers = parse_kimi_config(content).unwrap();
        assert!(servers.is_empty());
    }

    #[test]
    fn parse_invalid_json_fails() {
        let result = parse_kimi_config("not json at all");
        assert!(result.is_err());
    }

    // -- transport_to_json ---------------------------------------------------

    #[test]
    fn stdio_to_json_roundtrip() {
        let transport = McpServerTransport::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "@test/srv".into()],
            env: HashMap::from([("K".into(), "V".into())]),
        };
        let json = transport_to_json(&transport);
        let server = parse_kimi_entry(&json).unwrap();
        assert_eq!(server, transport);
    }

    #[test]
    fn stdio_to_json_omits_empty_env() {
        let transport = McpServerTransport::Stdio {
            command: "node".into(),
            args: vec![],
            env: HashMap::new(),
        };
        let json = transport_to_json(&transport);
        assert!(json.get("env").is_none());
    }

    #[test]
    fn http_to_json_roundtrip() {
        let transport = McpServerTransport::Http {
            url: "https://example.com/mcp".into(),
            headers: HashMap::from([("Authorization".into(), "Bearer tok".into())]),
        };
        let json = transport_to_json(&transport);
        let server = parse_kimi_entry(&json).unwrap();
        assert_eq!(server, transport);
    }

    #[test]
    fn sse_to_json_roundtrip() {
        let transport = McpServerTransport::Sse {
            url: "https://example.com/sse".into(),
            headers: HashMap::new(),
        };
        let json = transport_to_json(&transport);
        let server = parse_kimi_entry(&json).unwrap();
        assert_eq!(server, transport);
        assert_eq!(json["transport"], "sse");
    }

    // -- file I/O ------------------------------------------------------------

    #[tokio::test]
    async fn install_creates_new_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");

        let transport = McpServerTransport::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "@test/srv".into()],
            env: HashMap::new(),
        };

        install_server_at(&path, "srv", &transport).await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let servers = parse_kimi_config(&content).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "srv");
        assert_eq!(servers[0].transport, transport);
    }

    #[tokio::test]
    async fn install_appends_to_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        tokio::fs::write(&path, r#"{ "mcpServers": { "existing": { "command": "node" } } }"#)
            .await
            .unwrap();

        let transport = McpServerTransport::Http {
            url: "https://example.com/mcp".into(),
            headers: HashMap::new(),
        };
        install_server_at(&path, "new-srv", &transport).await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let servers = parse_kimi_config(&content).unwrap();
        assert_eq!(servers.len(), 2);
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"existing"));
        assert!(names.contains(&"new-srv"));
    }

    #[tokio::test]
    async fn install_replaces_existing_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        tokio::fs::write(&path, r#"{ "mcpServers": { "srv": { "command": "old" } } }"#)
            .await
            .unwrap();

        let transport = McpServerTransport::Stdio {
            command: "new".into(),
            args: vec![],
            env: HashMap::new(),
        };
        install_server_at(&path, "srv", &transport).await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let servers = parse_kimi_config(&content).unwrap();
        assert_eq!(servers.len(), 1);
        match &servers[0].transport {
            McpServerTransport::Stdio { command, .. } => assert_eq!(command, "new"),
            _ => panic!("expected Stdio"),
        }
    }

    #[tokio::test]
    async fn install_preserves_other_fields_in_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        tokio::fs::write(&path, r#"{ "other": "stuff", "mcpServers": {} }"#)
            .await
            .unwrap();

        let transport = McpServerTransport::Stdio {
            command: "node".into(),
            args: vec![],
            env: HashMap::new(),
        };
        install_server_at(&path, "srv", &transport).await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let root: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(root["other"], "stuff");
        assert!(root["mcpServers"]["srv"]["command"].is_string());
    }

    #[tokio::test]
    async fn remove_existing_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        tokio::fs::write(
            &path,
            r#"{ "mcpServers": { "a": { "command": "node" }, "b": { "command": "node" } } }"#,
        )
        .await
        .unwrap();

        remove_server_at(&path, "a").await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let servers = parse_kimi_config(&content).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "b");
    }

    #[tokio::test]
    async fn remove_nonexistent_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        tokio::fs::write(&path, r#"{ "mcpServers": { "a": { "command": "node" } } }"#)
            .await
            .unwrap();

        remove_server_at(&path, "ghost").await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let servers = parse_kimi_config(&content).unwrap();
        assert_eq!(servers.len(), 1);
    }

    #[tokio::test]
    async fn remove_missing_file_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");

        remove_server_at(&path, "ghost").await.unwrap();
    }

    #[tokio::test]
    async fn install_corrupted_config_fails_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        tokio::fs::write(&path, "this is not json").await.unwrap();

        let transport = McpServerTransport::Stdio {
            command: "node".into(),
            args: vec![],
            env: HashMap::new(),
        };
        let result = install_server_at(&path, "srv", &transport).await;
        assert!(result.is_err());

        // Original content must be untouched.
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "this is not json");
    }

    #[test]
    fn trait_is_object_safe() {
        let adapter: Box<dyn McpAgentAdapter> = Box::new(KimiAdapter);
        assert_eq!(adapter.source(), McpSource::Kimi);
    }
}
