/**
 * @license
 * Copyright 2025 AionUi (aionui.com)
 * SPDX-License-Identifier: Apache-2.0
 */
// Agent-side permission policy management (write-through to agent config).
//
// AionUi already handles permissions at **runtime** (approval cards). This
// module adds the missing **agent-side policy** control: it reads and writes
// each agent's own permission-policy file so users can get "full auto" /
// "auto-edit" behaviour without hand-editing JSON.
//
// The adapter shape mirrors `McpAgentAdapter`: a trait + one adapter per agent,
// so more agents (Claude Code `~/.claude/settings.json`, Codex, Gemini, ...)
// can follow the OpenCode pilot later. Only OpenCode is wired today.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::McpError;

/// Normalized agent-side permission policy level exposed by the AionUi UI.
///
/// These three levels are intentionally agent-agnostic; each `PermissionPolicyAdapter`
/// maps them to the agent's own permission schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionLevel {
    /// Prompt for approval on potentially destructive actions (ask).
    Ask,
    /// Auto-approve file edits; still ask for shell / network.
    AutoEdit,
    /// Auto-approve everything that is not explicitly denied.
    FullAuto,
}

impl PermissionLevel {
    /// All supported levels in display order.
    pub const ALL: [PermissionLevel; 3] = [
        PermissionLevel::Ask,
        PermissionLevel::AutoEdit,
        PermissionLevel::FullAuto,
    ];

    /// Wire value used in HTTP payloads (serde snake_case).
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionLevel::Ask => "ask",
            PermissionLevel::AutoEdit => "auto_edit",
            PermissionLevel::FullAuto => "full_auto",
        }
    }

    /// Parse a wire/level string, case-insensitive.
    pub fn from_name(s: &str) -> Option<PermissionLevel> {
        match s.to_ascii_lowercase().as_str() {
            "ask" => Some(PermissionLevel::Ask),
            "auto_edit" | "autoedit" => Some(PermissionLevel::AutoEdit),
            "full_auto" | "fullauto" | "yolo" | "auto" => Some(PermissionLevel::FullAuto),
            _ => None,
        }
    }
}

/// Read-model returned to the UI for one agent's permission policy.
#[derive(Debug, Clone, Serialize)]
pub struct PermissionPolicyView {
    /// Agent identifier, e.g. `"opencode"`.
    pub agent: String,
    /// Whether an adapter exists for this agent (true for OpenCode, false otherwise).
    pub supported: bool,
    /// Whether the agent is present on this machine (config file / binary resolved).
    pub installed: bool,
    /// The effective permission level, or `None` when the agent config has no
    /// recognizable policy (agent default behaviour applies).
    pub current_level: Option<PermissionLevel>,
    /// Absolute path of the config file that holds the policy (for display).
    pub config_path: Option<String>,
}

/// Abstraction for reading/writing an agent's permission-policy file.
///
/// Implementations do **not** need to handle concurrency internally.
///
/// # Error handling
///
/// Methods return `McpError` to keep the adapter layer independent of HTTP
/// concerns (mirrors `McpAgentAdapter`).
#[async_trait]
pub trait PermissionPolicyAdapter: Send + Sync {
    /// Agent identifier (e.g. `"opencode"`).
    fn agent(&self) -> &'static str;

    /// Whether the agent + its policy file are available to manage on this machine.
    async fn installed(&self) -> Result<bool, McpError>;

    /// Absolute path of the policy config file, when resolvable.
    fn config_path(&self) -> Option<String>;

    /// Read the current effective permission level from the agent config.
    /// Returns `Ok(None)` when there is no recognizable explicit policy.
    async fn read_current(&self) -> Result<Option<PermissionLevel>, McpError>;

    /// Write-through a permission level to the agent config (creates/updates the policy).
    async fn apply(&self, level: PermissionLevel) -> Result<(), McpError>;

    /// Remove the agent-side policy so the agent falls back to its default behaviour.
    async fn clear(&self) -> Result<(), McpError>;
}

/// Read-only projection of an adapter for listing, with any I/O already done.
pub async fn policy_view(adapter: &dyn PermissionPolicyAdapter) -> PermissionPolicyView {
    PermissionPolicyView {
        agent: adapter.agent().to_string(),
        supported: true,
        installed: adapter.installed().await.unwrap_or(false),
        current_level: adapter.read_current().await.ok().flatten(),
        config_path: adapter.config_path(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_serde_roundtrip_snake_case() {
        for level in PermissionLevel::ALL {
            let wire = serde_json::to_string(&level).unwrap();
            let back: PermissionLevel = serde_json::from_str(&wire).unwrap();
            assert_eq!(back, level);
            assert!(wire.starts_with('"') && wire.ends_with('"'));
            assert!(!wire.contains("PascalCase"));
        }
        // wire values use snake_case
        assert_eq!(
            serde_json::to_string(&PermissionLevel::FullAuto).unwrap(),
            "\"full_auto\""
        );
        assert_eq!(
            serde_json::from_str::<PermissionLevel>("\"auto_edit\"").unwrap(),
            PermissionLevel::AutoEdit
        );
    }

    #[test]
    fn level_names_map_uniquely() {
        let names: Vec<String> = PermissionLevel::ALL.iter().map(|l| l.as_str().to_uppercase()).collect();
        let uniq: std::collections::HashSet<String> = names.iter().cloned().collect();
        assert_eq!(names.len(), uniq.len(), "level wire names must be unique");
    }
}
