/**
 * @license
 * Copyright 2025 AionUi (aionui.com)
 * SPDX-License-Identifier: Apache-2.0
 */
// OpenCode permission-policy adapter — the pilot for issue #4018.
//
// OpenCode stores its permission policy in `~/.config/opencode/opencode.json`
// under the `permission` field. This adapter reuses the JSONC read/write
// helpers from the MCP adapter (`adapters/opencode.rs`).
//
// Reference (opencode.ai/docs/permissions):
//   - `"permission": { "*": "ask" }`            -> ask for everything
//   - `"permission": { "*": "allow", "<tool>": "ask" }` -> auto for most, ask for listed
//   - `"permission": "allow"`                   -> auto-approve everything not denied
//
// Normalized level -> OpenCode schema:
//   Ask       -> `{ "*": "ask" }`
//   AutoEdit  -> `{ "*": "allow", "bash": "ask", "webfetch": "ask" }`
//   FullAuto  -> `"allow"`
use std::path::PathBuf;

use async_trait::async_trait;

use crate::adapters::opencode::{config_dir, config_file_path, parse_jsonc};
use crate::error::McpError;
use crate::permission::{PermissionLevel, PermissionPolicyAdapter};

/// Adapter for managing OpenCode's permission policy via `opencode.json`.
pub struct OpenCodePermissionAdapter;

const PERMS: [&str; 9] = [
    "bash",
    "read",
    "edit",
    "glob",
    "grep",
    "webfetch",
    "task",
    "todowrite",
    "websearch",
];

/// Build the `permission` JSON value for a normalized level (None = drop key).
fn permission_value_for(level: PermissionLevel) -> Option<serde_json::Value> {
    match level {
        PermissionLevel::Ask => Some(serde_json::json!({ "*": "ask" })),
        PermissionLevel::AutoEdit => {
            let mut object = serde_json::Map::new();
            for tool in PERMS {
                object.insert(tool.to_string(), serde_json::json!("ask"));
            }
            object.insert("*".to_string(), serde_json::json!("allow"));
            Some(serde_json::Value::Object(object))
        }
        PermissionLevel::FullAuto => Some(serde_json::json!("allow")),
    }
}

/// Interpret an opencode `permission` value back into a normalized level.
fn level_from_permission(value: &serde_json::Value) -> Option<PermissionLevel> {
    match value {
        serde_json::Value::String(s) if s.eq_ignore_ascii_case("allow") => Some(PermissionLevel::FullAuto),
        serde_json::Value::String(s) if s.eq_ignore_ascii_case("ask") => Some(PermissionLevel::Ask),
        serde_json::Value::String(_) => None,
        serde_json::Value::Object(map) => {
            let allow_all = map.get("*").and_then(|v| v.as_str()).is_some_and(|s| s == "allow");
            if !allow_all {
                // `{ "*": "ask" }` or any ask-centric map.
                return Some(PermissionLevel::Ask);
            }
            // allow-all plus ask-for-shell => auto-edit; allow-all alone => full-auto.
            let has_shell_ask = ["bash", "webfetch", "task"]
                .iter()
                .any(|t| map.get(*t).and_then(|v| v.as_str()).is_some_and(|s| s == "ask"));
            if has_shell_ask {
                Some(PermissionLevel::AutoEdit)
            } else {
                Some(PermissionLevel::FullAuto)
            }
        }
        _ => None,
    }
}

fn read_root() -> Result<serde_json::Value, McpError> {
    let path = config_file_path().ok_or_else(|| McpError::AgentNotInstalled("opencode".to_string()))?;
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| McpError::AgentOperationFailed(format!("failed to read {}: {e}", path.display())))?;
    parse_jsonc(&content)
}

/// Atomically persist the config (temp file + rename) since it may hold secrets.
fn persist_root(root: serde_json::Value) -> Result<(), McpError> {
    let path = config_file_path().ok_or_else(|| McpError::AgentNotInstalled("opencode".to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| McpError::AgentOperationFailed(format!("failed to create dir: {e}")))?;
    }
    let serialized = serde_json::to_string_pretty(&root)
        .map_err(|e| McpError::AgentOperationFailed(format!("failed to serialize: {e}")))?;
    let tmp =
        tempfile_like(&path).map_err(|e| McpError::AgentOperationFailed(format!("failed to create temp file: {e}")))?;
    std::fs::write(&tmp, serialized)
        .map_err(|e| McpError::AgentOperationFailed(format!("failed to write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| McpError::AgentOperationFailed(format!("failed to rename over {}: {e}", path.display())))?;
    Ok(())
}

/// Build a sibling temp path for atomic rename.
fn tempfile_like(path: &std::path::Path) -> std::io::Result<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("opencode.json");
    let tmp = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    Ok(tmp)
}

#[async_trait]
impl PermissionPolicyAdapter for OpenCodePermissionAdapter {
    fn agent(&self) -> &'static str {
        "opencode"
    }

    async fn installed(&self) -> Result<bool, McpError> {
        Ok(config_dir().is_some_and(|d| d.exists()))
    }

    fn config_path(&self) -> Option<String> {
        config_file_path().map(|p| p.display().to_string())
    }

    async fn read_current(&self) -> Result<Option<PermissionLevel>, McpError> {
        let root = read_root()?;
        let Some(permission) = root.get("permission") else {
            return Ok(None);
        };
        Ok(level_from_permission(permission))
    }

    async fn apply(&self, level: PermissionLevel) -> Result<(), McpError> {
        let mut root = read_root()?;
        let object = root
            .as_object_mut()
            .ok_or_else(|| McpError::AgentOperationFailed("config root is not an object".to_string()))?;
        object.insert("permission".to_string(), permission_value_for(level).unwrap());
        persist_root(root)
    }

    async fn clear(&self) -> Result<(), McpError> {
        let mut root = read_root()?;
        let object = root
            .as_object_mut()
            .ok_or_else(|| McpError::AgentOperationFailed("config root is not an object".to_string()))?;
        object.remove("permission");
        persist_root(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_roundtrip_str() {
        assert_eq!(PermissionLevel::from_name("ask"), Some(PermissionLevel::Ask));
        assert_eq!(PermissionLevel::from_name("full_auto"), Some(PermissionLevel::FullAuto));
        assert_eq!(PermissionLevel::from_name("AutoEdit"), Some(PermissionLevel::AutoEdit));
        assert_eq!(PermissionLevel::from_name("nope"), None);
    }

    #[test]
    fn permission_value_maps() {
        let ask = permission_value_for(PermissionLevel::Ask).unwrap();
        assert_eq!(ask["*"], "ask");
        let auto_edit = permission_value_for(PermissionLevel::AutoEdit).unwrap();
        assert_eq!(auto_edit["*"], "allow");
        assert_eq!(auto_edit["bash"], "ask");
        let full = permission_value_for(PermissionLevel::FullAuto).unwrap();
        assert_eq!(full, "allow");
    }

    #[test]
    fn level_from_permission_parses() {
        assert_eq!(
            level_from_permission(&serde_json::json!("allow")),
            Some(PermissionLevel::FullAuto)
        );
        assert_eq!(
            level_from_permission(&serde_json::json!({ "*": "ask" })),
            Some(PermissionLevel::Ask)
        );
        assert_eq!(
            level_from_permission(&serde_json::json!({ "*": "allow", "bash": "ask" })),
            Some(PermissionLevel::AutoEdit)
        );
        assert_eq!(
            level_from_permission(&serde_json::json!({ "*": "allow" })),
            Some(PermissionLevel::FullAuto)
        );
        assert_eq!(
            level_from_permission(&serde_json::json!({"bash": "ask"})),
            Some(PermissionLevel::Ask)
        );
        assert_eq!(level_from_permission(&serde_json::json!(42)), None);
    }
}
