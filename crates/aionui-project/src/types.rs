use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProjectInput {
    pub name: String,
    pub local_path: String,
    pub repository_url: Option<String>,
    pub default_branch: Option<String>,
    #[serde(default = "default_project_type")]
    pub project_type: String,
}

fn default_project_type() -> String {
    "unknown".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateProjectInput {
    pub name: Option<String>,
    pub local_path: Option<String>,
    pub repository_url: Option<Option<String>>,
    pub default_branch: Option<Option<String>>,
    pub project_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectCommandProfileInput {
    pub install_command: Option<String>,
    pub format_command: Option<String>,
    pub lint_command: Option<String>,
    pub typecheck_command: Option<String>,
    pub unit_test_command: Option<String>,
    pub integration_test_command: Option<String>,
    pub e2e_command: Option<String>,
    pub build_command: Option<String>,
    pub security_scan_command: Option<String>,
    #[serde(default = "default_command_timeout")]
    pub command_timeout_seconds: i64,
}

impl Default for ProjectCommandProfileInput {
    fn default() -> Self {
        Self {
            install_command: None,
            format_command: None,
            lint_command: None,
            typecheck_command: None,
            unit_test_command: None,
            integration_test_command: None,
            e2e_command: None,
            build_command: None,
            security_scan_command: None,
            command_timeout_seconds: default_command_timeout(),
        }
    }
}

fn default_command_timeout() -> i64 {
    900
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRuntimeProfileInput {
    #[serde(default = "default_environment_kind")]
    pub environment_kind: String,
    pub language: Option<String>,
    pub package_manager: Option<String>,
    pub runtime_version: Option<String>,
    #[serde(default)]
    pub env_keys: Vec<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

fn default_environment_kind() -> String {
    "local".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapabilitySnapshot {
    pub id: String,
    pub enabled: bool,
    pub installed: bool,
    pub status: String,
    pub last_check_status: Option<String>,
    pub last_check_at: Option<TimestampMs>,
    pub last_success_at: Option<TimestampMs>,
    pub agent_capabilities: Option<serde_json::Value>,
    pub available_models: Option<serde_json::Value>,
    pub available_modes: Option<serde_json::Value>,
    pub available_commands: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightCheck {
    pub code: String,
    pub level: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPreflightResult {
    pub agent_id: String,
    pub level: String,
    pub summary: String,
    pub snapshot: Option<AgentCapabilitySnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectPreflightResult {
    pub project_id: String,
    pub overall_status: String,
    pub checks: Vec<PreflightCheck>,
    pub agents: Vec<AgentPreflightResult>,
    pub checked_at: TimestampMs,
}
