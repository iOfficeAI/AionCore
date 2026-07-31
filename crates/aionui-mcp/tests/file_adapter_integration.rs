//! Integration tests for file-based MCP Agent adapters (Opencode, Aionrs, Aionui).
//!
//! These tests exercise the real filesystem read/write logic using temp
//! directories. CLI detection (`is_installed`, `which`) is NOT tested here
//! because it depends on the host environment.
//!
//! For Aionui, we use a mock repository since it reads from the DB.

use std::collections::HashMap;
use std::sync::Arc;

const TEST_USER_ID: &str = "system_default_user";

use aionui_common::McpSource;
use aionui_mcp::{AionuiAdapter, McpAgentAdapter, McpServerTransport};

// ===========================================================================
// Aionui adapter (DB-backed)
// ===========================================================================

mod aionui {
    use super::*;
    use aionui_db::models::McpServerRow;
    use aionui_db::{CreateMcpServerParams, DbError, IMcpServerRepository, UpdateMcpServerParams};

    struct MockRepo {
        servers: tokio::sync::Mutex<Vec<McpServerRow>>,
    }

    impl MockRepo {
        fn new(servers: Vec<McpServerRow>) -> Self {
            Self {
                servers: tokio::sync::Mutex::new(servers),
            }
        }
    }

    #[async_trait::async_trait]
    impl IMcpServerRepository for MockRepo {
        async fn list(&self, user_id: &str) -> Result<Vec<McpServerRow>, DbError> {
            Ok(self
                .servers
                .lock()
                .await
                .iter()
                .filter(|server| server.user_id == user_id)
                .cloned()
                .collect())
        }

        async fn find_by_id(&self, user_id: &str, id: &str) -> Result<Option<McpServerRow>, DbError> {
            Ok(self
                .servers
                .lock()
                .await
                .iter()
                .find(|s| s.user_id == user_id && s.id == id)
                .cloned())
        }

        async fn find_by_name(&self, user_id: &str, name: &str) -> Result<Option<McpServerRow>, DbError> {
            Ok(self
                .servers
                .lock()
                .await
                .iter()
                .find(|s| s.user_id == user_id && s.name == name)
                .cloned())
        }

        async fn create(&self, _p: CreateMcpServerParams<'_>) -> Result<McpServerRow, DbError> {
            unimplemented!()
        }

        async fn update(
            &self,
            _user_id: &str,
            _id: &str,
            _p: UpdateMcpServerParams<'_>,
        ) -> Result<McpServerRow, DbError> {
            unimplemented!()
        }

        async fn delete(&self, _user_id: &str, _id: &str) -> Result<(), DbError> {
            unimplemented!()
        }

        async fn batch_upsert(
            &self,
            _user_id: &str,
            _s: &[CreateMcpServerParams<'_>],
        ) -> Result<Vec<McpServerRow>, DbError> {
            unimplemented!()
        }

        async fn update_status(
            &self,
            _user_id: &str,
            _id: &str,
            _s: &str,
            _lc: Option<aionui_common::TimestampMs>,
        ) -> Result<(), DbError> {
            unimplemented!()
        }

        async fn update_tools(&self, _user_id: &str, _id: &str, _t: Option<&str>) -> Result<(), DbError> {
            unimplemented!()
        }
    }

    fn make_row(name: &str, t_type: &str, t_config: &str) -> McpServerRow {
        McpServerRow {
            user_id: TEST_USER_ID.to_string(),
            id: format!("mcp_{name}"),
            name: name.to_owned(),
            description: None,
            enabled: true,
            transport_type: t_type.into(),
            transport_config: t_config.into(),
            tools: None,
            last_test_status: "disconnected".into(),
            last_connected: None,
            original_json: None,
            builtin: false,
            deleted_at: None,
            created_at: 1000,
            updated_at: 1000,
        }
    }

    #[tokio::test]
    async fn source_is_aionui() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter = AionuiAdapter::new(repo);
        assert_eq!(adapter.source(), McpSource::Aionui);
    }

    #[tokio::test]
    async fn always_installed() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter = AionuiAdapter::new(repo);
        assert!(adapter.is_installed().await.unwrap());
    }

    #[tokio::test]
    async fn detect_returns_all_db_servers() {
        let rows = vec![
            make_row("stdio-srv", "stdio", r#"{"command":"npx","args":[]}"#),
            make_row("http-srv", "http", r#"{"url":"https://example.com/mcp","headers":{}}"#),
            make_row("sse-srv", "sse", r#"{"url":"https://example.com/sse","headers":{}}"#),
        ];
        let repo = Arc::new(MockRepo::new(rows));
        let adapter = AionuiAdapter::new(repo);

        let servers = adapter.detect_existing(TEST_USER_ID).await.unwrap();
        assert_eq!(servers.len(), 3);
        assert_eq!(servers[0].name, "stdio-srv");
        assert_eq!(servers[1].name, "http-srv");
        assert_eq!(servers[2].name, "sse-srv");

        assert!(matches!(servers[0].transport, McpServerTransport::Stdio { .. }));
        assert!(matches!(servers[1].transport, McpServerTransport::Http { .. }));
        assert!(matches!(servers[2].transport, McpServerTransport::Sse { .. }));
    }

    #[tokio::test]
    async fn detect_empty_db_returns_empty() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter = AionuiAdapter::new(repo);
        let servers = adapter.detect_existing(TEST_USER_ID).await.unwrap();
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn install_is_noop() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter = AionuiAdapter::new(repo.clone());

        let transport = McpServerTransport::Stdio {
            command: "npx".into(),
            args: vec![],
            env: HashMap::new(),
        };
        adapter.install_server("test", &transport).await.unwrap();

        // DB should still be empty since install is a no-op
        let servers = adapter.detect_existing(TEST_USER_ID).await.unwrap();
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn remove_is_noop() {
        let rows = vec![make_row("srv", "stdio", r#"{"command":"npx","args":[]}"#)];
        let repo = Arc::new(MockRepo::new(rows));
        let adapter = AionuiAdapter::new(repo);

        adapter.remove_server("srv").await.unwrap();

        // Server should still be in DB since remove is a no-op
        let servers = adapter.detect_existing(TEST_USER_ID).await.unwrap();
        assert_eq!(servers.len(), 1);
    }

    #[tokio::test]
    async fn trait_object_safety() {
        let repo = Arc::new(MockRepo::new(vec![]));
        let adapter: Arc<dyn McpAgentAdapter> = Arc::new(AionuiAdapter::new(repo));
        assert_eq!(adapter.source(), McpSource::Aionui);
        assert!(adapter.is_installed().await.unwrap());
    }
}

// ===========================================================================
// Opencode adapter (filesystem-backed)
// ===========================================================================

// Note: Full lifecycle tests for Opencode require controlling the config
// directory path, which the adapter currently derives from `dirs::config_dir()`.
// The unit tests in opencode.rs thoroughly cover parsing and serialization.
// Here we verify that the adapter implements the trait correctly and that
// the public API surface is accessible from outside the crate.

mod opencode {
    use super::*;
    use aionui_mcp::OpencodeAdapter;

    #[test]
    fn source_is_opencode() {
        assert_eq!(OpencodeAdapter.source(), McpSource::OpenCode);
    }

    #[test]
    fn trait_object_safety() {
        let adapter: Box<dyn McpAgentAdapter> = Box::new(OpencodeAdapter);
        assert_eq!(adapter.source(), McpSource::OpenCode);
    }
}

// ===========================================================================
// Aionrs adapter (CLI + TOML-backed)
// ===========================================================================

// Note: Full lifecycle tests for Aionrs require the `aionrs` CLI to be
// installed (for `--config-path`). The unit tests in aionrs.rs thoroughly
// cover TOML parsing, serialization, and roundtrip behavior. Here we
// verify the public API surface.

mod aionrs {
    use super::*;
    use aionui_mcp::AionrsAdapter;

    #[test]
    fn source_is_aionrs() {
        assert_eq!(AionrsAdapter.source(), McpSource::Aionrs);
    }

    #[test]
    fn trait_object_safety() {
        let adapter: Box<dyn McpAgentAdapter> = Box::new(AionrsAdapter);
        assert_eq!(adapter.source(), McpSource::Aionrs);
    }
}
