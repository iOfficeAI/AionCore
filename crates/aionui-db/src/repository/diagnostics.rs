use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::DbError;

/// Feedback diagnostics profile requested by the UI.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FeedbackDiagnosticsProfile {
    ConversationSession,
    ModelAuth,
    AgentTeam,
    McpTools,
    GlobalSummary,
}

impl FeedbackDiagnosticsProfile {
    pub fn as_name(&self) -> &'static str {
        match self {
            Self::ConversationSession => "conversation-session",
            Self::ModelAuth => "model-auth",
            Self::AgentTeam => "agent-team",
            Self::McpTools => "mcp-tools",
            Self::GlobalSummary => "global-summary",
        }
    }
}

/// Explicit identifiers known at feedback submission time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackDiagnosticsDbContext {
    pub conversation_id: Option<String>,
    pub provider_id: Option<String>,
    pub agent_id: Option<String>,
    pub team_id: Option<String>,
    pub mcp_server_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackDiagnosticsRequest {
    pub user_id: String,
    pub profiles: Vec<FeedbackDiagnosticsProfile>,
    pub context: FeedbackDiagnosticsDbContext,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedbackDiagnosticsResult {
    pub schema_version: String,
    pub profiles: Vec<FeedbackDiagnosticsProfileResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedbackDiagnosticsProfileResult {
    pub name: String,
    pub mode: String,
    pub data: Value,
    pub warnings: Vec<String>,
}

#[async_trait::async_trait]
pub trait IFeedbackDiagnosticsRepository: Send + Sync {
    async fn collect_feedback_diagnostics(
        &self,
        request: &FeedbackDiagnosticsRequest,
    ) -> Result<FeedbackDiagnosticsResult, DbError>;
}
