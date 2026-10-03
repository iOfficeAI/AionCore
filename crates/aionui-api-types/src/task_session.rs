use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ToolCapability {
    FilesystemRead,
    FilesystemWrite,
    ShellReadonly,
    ShellExecute,
    GitRead,
    GitWrite,
    McpRead,
    McpWrite,
    NetworkInternal,
    NetworkExternal,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecisionKind {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyDecision {
    pub decision: PolicyDecisionKind,
    pub reason: String,
    pub rule_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanningIsolationLevel {
    Guaranteed,
    BestEffort,
    Unsupported,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentIntegrationMode {
    NativeSandbox,
    NativePermissionMode,
    GenericAcp,
    InProcessToolRegistry,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanningIsolationResponse {
    pub level: PlanningIsolationLevel,
    pub integration_mode: AgentIntegrationMode,
    pub automatic_planning_enabled: bool,
    pub reason: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskSessionMode {
    Agent,
    Plan,
    Goal,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskSessionStatus {
    Draft,
    Ready,
    Running,
    WaitingApproval,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl TaskSessionStatus {
    pub fn can_transition_to(self, next: Self) -> bool {
        if self == next {
            return true;
        }
        match self {
            Self::Draft => matches!(next, Self::Ready | Self::Cancelled),
            Self::Ready => matches!(next, Self::Running | Self::Paused | Self::Cancelled),
            Self::Running => matches!(
                next,
                Self::WaitingApproval | Self::Paused | Self::Completed | Self::Failed | Self::Cancelled
            ),
            Self::WaitingApproval => matches!(
                next,
                Self::Ready | Self::Running | Self::Paused | Self::Completed | Self::Failed | Self::Cancelled
            ),
            Self::Paused => matches!(
                next,
                Self::Ready | Self::Running | Self::Completed | Self::Failed | Self::Cancelled
            ),
            Self::Completed | Self::Failed | Self::Cancelled => false,
        }
    }
}

impl std::fmt::Display for TaskSessionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Agent => "agent",
            Self::Plan => "plan",
            Self::Goal => "goal",
        })
    }
}

impl std::fmt::Display for TaskSessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Draft => "draft",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::WaitingApproval => "waiting_approval",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        })
    }
}

impl std::str::FromStr for TaskSessionMode {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "agent" => Ok(Self::Agent),
            "plan" => Ok(Self::Plan),
            "goal" => Ok(Self::Goal),
            _ => Err(()),
        }
    }
}

impl std::str::FromStr for TaskSessionStatus {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "draft" => Ok(Self::Draft),
            "ready" => Ok(Self::Ready),
            "running" => Ok(Self::Running),
            "waiting_approval" => Ok(Self::WaitingApproval),
            "paused" => Ok(Self::Paused),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskSessionResponse {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub mode: TaskSessionMode,
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
    pub status: TaskSessionStatus,
    pub agent_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateTaskSessionRequest {
    pub title: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    pub mode: TaskSessionMode,
    #[serde(default)]
    pub objective: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default = "default_task_session_status")]
    pub status: TaskSessionStatus,
    pub agent_type: String,
    #[serde(default)]
    pub agent_session_id: Option<String>,
}

fn default_task_session_status() -> TaskSessionStatus {
    TaskSessionStatus::Draft
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateTaskSessionRequest {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub mode: Option<TaskSessionMode>,
    #[serde(default)]
    pub objective: Option<String>,
    #[serde(default)]
    pub acceptance_criteria: Option<Vec<String>>,
    #[serde(default)]
    pub status: Option<TaskSessionStatus>,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub agent_session_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListTaskSessionsQuery {
    #[serde(default)]
    pub conversation_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskArtifactKind {
    Plan,
    Goal,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskArtifactStatus {
    Submitted,
    Approved,
    Rejected,
    Superseded,
}

macro_rules! impl_string_enum {
    ($ty:ty, {$($variant:ident => $value:literal),+ $(,)?}) => {
        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(match self { $(Self::$variant => $value),+ })
            }
        }

        impl std::str::FromStr for $ty {
            type Err = ();

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value { $($value => Ok(Self::$variant),)+ _ => Err(()) }
            }
        }
    };
}

impl_string_enum!(TaskArtifactKind, { Plan => "plan", Goal => "goal" });
impl_string_enum!(TaskArtifactStatus, {
    Submitted => "submitted",
    Approved => "approved",
    Rejected => "rejected",
    Superseded => "superseded",
});

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
    Cancelled,
}

impl_string_enum!(TaskApprovalStatus, {
    Pending => "pending",
    Approved => "approved",
    Rejected => "rejected",
    Expired => "expired",
    Cancelled => "cancelled",
});

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskApprovalDecision {
    Approve,
    Reject,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunStatus {
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl_string_enum!(TaskRunStatus, {
    Running => "running",
    Paused => "paused",
    Completed => "completed",
    Failed => "failed",
    Cancelled => "cancelled",
});

impl_string_enum!(AcceptanceCriterionStatus, {
    Pending => "pending",
    Passed => "passed",
    Failed => "failed",
    NeedsVerification => "needs_verification",
});

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceCriterionStatus {
    Pending,
    Passed,
    Failed,
    NeedsVerification,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceEvidenceKind {
    TestResult,
    CommandResult,
    FileDiff,
    Artifact,
    UserConfirmation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceptanceEvidence {
    pub kind: AcceptanceEvidenceKind,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubmitTaskArtifactRequest {
    pub kind: TaskArtifactKind,
    pub content: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskArtifactResponse {
    pub id: String,
    pub task_session_id: String,
    pub kind: TaskArtifactKind,
    pub version: i64,
    pub content: String,
    pub content_hash: String,
    pub status: TaskArtifactStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskApprovalResponse {
    pub id: String,
    pub task_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub approval_type: TaskArtifactKind,
    pub artifact_id: String,
    pub artifact_hash: String,
    pub status: TaskApprovalStatus,
    pub requested_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DecideTaskApprovalRequest {
    pub decision: TaskApprovalDecision,
    pub artifact_id: String,
    pub artifact_hash: String,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExecuteApprovedPlanRequest {
    pub approval_id: String,
    pub artifact_id: String,
    pub artifact_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskRunResponse {
    pub id: String,
    pub task_session_id: String,
    pub conversation_id: String,
    pub plan_artifact_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_artifact_id: Option<String>,
    pub approval_id: String,
    pub status: TaskRunStatus,
    pub started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceptanceCriterionResponse {
    pub id: String,
    pub task_session_id: String,
    pub goal_artifact_id: String,
    pub position: i64,
    pub description: String,
    pub status: AcceptanceCriterionStatus,
    pub evidence: Vec<AcceptanceEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VerifyAcceptanceCriterionRequest {
    pub status: AcceptanceCriterionStatus,
    #[serde(default)]
    pub evidence: Vec<AcceptanceEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubmitTaskArtifactResponse {
    pub artifact: TaskArtifactResponse,
    pub approval: TaskApprovalResponse,
    pub acceptance_criteria: Vec<AcceptanceCriterionResponse>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_statuses_cannot_restart() {
        for status in [
            TaskSessionStatus::Completed,
            TaskSessionStatus::Failed,
            TaskSessionStatus::Cancelled,
        ] {
            assert!(!status.can_transition_to(TaskSessionStatus::Running));
        }
    }

    #[test]
    fn waiting_approval_can_fail_closed_to_paused() {
        assert!(TaskSessionStatus::WaitingApproval.can_transition_to(TaskSessionStatus::Paused));
    }

    #[test]
    fn state_machine_matches_the_declared_transition_table() {
        use TaskSessionStatus::*;
        let statuses = [
            Draft,
            Ready,
            Running,
            WaitingApproval,
            Paused,
            Completed,
            Failed,
            Cancelled,
        ];
        for current in statuses {
            for next in statuses {
                let expected = current == next
                    || match current {
                        Draft => matches!(next, Ready | Cancelled),
                        Ready => matches!(next, Running | Paused | Cancelled),
                        Running => matches!(next, WaitingApproval | Paused | Completed | Failed | Cancelled),
                        WaitingApproval => matches!(next, Ready | Running | Paused | Completed | Failed | Cancelled),
                        Paused => matches!(next, Ready | Running | Completed | Failed | Cancelled),
                        Completed | Failed | Cancelled => false,
                    };
                assert_eq!(
                    current.can_transition_to(next),
                    expected,
                    "unexpected transition {current} -> {next}",
                );
            }
        }
    }

    #[test]
    fn persisted_contract_enums_round_trip() {
        assert_eq!("plan".parse::<TaskArtifactKind>(), Ok(TaskArtifactKind::Plan));
        assert_eq!(
            "approved".parse::<TaskApprovalStatus>(),
            Ok(TaskApprovalStatus::Approved)
        );
        assert_eq!("completed".parse::<TaskRunStatus>(), Ok(TaskRunStatus::Completed));
        assert_eq!(
            "needs_verification".parse::<AcceptanceCriterionStatus>(),
            Ok(AcceptanceCriterionStatus::NeedsVerification),
        );
    }
}
