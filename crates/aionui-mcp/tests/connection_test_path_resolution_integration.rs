//! Isolated PATH-resolution coverage for stdio MCP connection tests.
//!
//! This file intentionally contains one test because it mutates process PATH
//! to model the startup-enhanced GUI environment.

#![cfg(unix)]

use std::collections::HashMap;
use std::sync::Arc;

use aionui_mcp::{McpConnectionTestService, McpServerTransport};
use aionui_realtime::BroadcastEventBus;

#[tokio::test]
async fn stdio_command_resolves_from_enhanced_process_path() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();

    let fake_server = bin_dir.join("fake-mcp");
    std::fs::write(
        &fake_server,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake-mcp","version":"1.0.0"}}}'
      ;;
    *'"id":2'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}'
      exit 0
      ;;
  esac
done
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&fake_server).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_server, perms).unwrap();

    let original_path = std::env::var_os("PATH");
    unsafe {
        std::env::set_var("PATH", &bin_dir);
    }

    let svc = McpConnectionTestService::new(reqwest::Client::new(), Arc::new(BroadcastEventBus::new(16)));
    let transport = McpServerTransport::Stdio {
        command: "fake-mcp".into(),
        args: vec![],
        env: HashMap::new(),
    };

    let result = svc.test_connection("fake-mcp", &transport).await;

    unsafe {
        if let Some(path) = original_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
    }

    assert!(result.success, "expected fake PATH MCP server to connect: {result:?}");
    assert!(result.tools.unwrap().is_empty());
}
