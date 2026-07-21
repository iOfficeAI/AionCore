use aionui_api_types::TeamRunTargetRole;
use aionui_common::TimestampMs;

use crate::work_source::WorkSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkPriority {
    Foreground,
    Control,
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeConstraint {
    Ready,
    Starting {
        operation_id: u64,
    },
    Failed {
        operation_id: u64,
        classification: &'static str,
    },
    Removing {
        operation_id: u64,
    },
    SessionStopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotPhase {
    Idle,
    Queued,
    Starting,
    Running,
    Paused,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkIntentState {
    Queued,
    Starting {
        batch_id: String,
        operation_id: u64,
    },
    Running {
        batch_id: String,
        operation_id: u64,
        turn_id: String,
    },
    Completed,
    Failed {
        classification: &'static str,
    },
    Cancelled {
        classification: &'static str,
    },
}

impl WorkIntentState {
    pub(super) fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed { .. } | Self::Cancelled { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkIntent {
    pub(crate) intent_id: String,
    pub(crate) session_generation: String,
    pub(crate) slot_id: String,
    pub(crate) role: TeamRunTargetRole,
    pub(crate) source: WorkSource,
    pub(crate) priority: WorkPriority,
    pub(crate) mailbox_message_id: Option<String>,
    pub(crate) team_run_id: Option<String>,
    pub(crate) created_at_ms: TimestampMs,
    pub(crate) state: WorkIntentState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CausalBinding {
    UserVisible,
    InheritRunningBatch { caller_slot_id: String },
    ActiveRunOrBackground,
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnqueueRequest {
    pub(crate) slot_id: String,
    pub(crate) role: TeamRunTargetRole,
    pub(crate) source: WorkSource,
    pub(crate) binding: CausalBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnqueueLease {
    pub(crate) lease_id: String,
    pub(crate) session_generation: String,
    pub(crate) slot_id: String,
    pub(crate) role: TeamRunTargetRole,
    pub(crate) source: WorkSource,
    pub(crate) team_run_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkBatch {
    pub(crate) batch_id: String,
    pub(crate) session_generation: String,
    pub(crate) slot_id: String,
    pub(crate) intent_ids: Vec<String>,
    pub(crate) mailbox_message_ids: Vec<String>,
    pub(crate) highest_priority: WorkPriority,
    pub(crate) team_run_ids: Vec<String>,
    pub(crate) operation_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReconcileDecision {
    Claim(WorkBatch),
    WaitingForCompletion,
    Blocked(RuntimeConstraint),
    SettleSignals(Vec<String>),
    Quiescent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommitResult {
    Committed,
    StaleOwner,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StartCommitResult {
    Accepted,
    CancelImmediately,
    StaleOwner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnqueueDisposition {
    Accepted,
    Queued,
    BlockedRuntimeStarting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnqueueCommit {
    pub(crate) intent_id: String,
    pub(crate) team_run_id: Option<String>,
    pub(crate) disposition: EnqueueDisposition,
    pub(crate) slot: SlotWorkSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlotWorkSnapshot {
    pub(crate) slot_id: String,
    pub(crate) role: TeamRunTargetRole,
    pub(crate) state: SlotPhase,
    pub(crate) queued_foreground_count: usize,
    pub(crate) queued_background_count: usize,
    pub(crate) active_batch: Option<WorkBatch>,
    pub(crate) active_turn_id: Option<String>,
    pub(crate) active_turn_started_at_ms: Option<TimestampMs>,
    pub(crate) runtime_constraint: RuntimeConstraint,
    pub(crate) team_run_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunWorkSummary {
    pub(crate) team_run_id: String,
    pub(crate) queued_intent_count: usize,
    pub(crate) starting_batch_count: usize,
    pub(crate) running_batch_count: usize,
    pub(crate) active_enqueue_lease_count: usize,
    pub(crate) paused_intent_count: usize,
    pub(crate) failed_intent_count: usize,
    pub(crate) slots: Vec<SlotWorkSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoordinatorSnapshot {
    pub(crate) session_generation: String,
    pub(crate) slots: Vec<SlotWorkSnapshot>,
    pub(crate) active_run_summary: Option<RunWorkSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReconcileProjection {
    pub(crate) created_recovery_intent_ids: Vec<String>,
    pub(crate) retained_intent_ids: Vec<String>,
    pub(crate) cleared_stale_intent_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BatchCancelTarget {
    pub(crate) batch: WorkBatch,
    pub(crate) turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PauseWorkResult {
    pub(crate) cancel_target: Option<BatchCancelTarget>,
    pub(crate) slot: SlotWorkSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoveWorkResult {
    pub(crate) cancel_target: Option<BatchCancelTarget>,
    pub(crate) terminal_message_ids: Vec<String>,
    pub(crate) affected_run_summaries: Vec<RunWorkSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CancelRunWorkResult {
    pub(crate) cancel_targets: Vec<BatchCancelTarget>,
    pub(crate) terminal_message_ids: Vec<String>,
    pub(crate) summary: RunWorkSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeConstraintUpdate {
    pub(crate) slot: SlotWorkSnapshot,
    pub(crate) terminal_message_ids: Vec<String>,
    pub(crate) affected_run_summaries: Vec<RunWorkSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunBinding {
    pub(crate) team_run_id: Option<String>,
    pub(crate) created_new_run: bool,
    pub(crate) user_intervention: bool,
}

pub(crate) trait RunCausalityPort: Send + Sync {
    fn bind_enqueue(&self, request: &EnqueueRequest) -> RunBinding;
    fn abort_binding(&self, binding: &RunBinding);
    fn apply_work_summary(&self, summary: RunWorkSummary);
}
