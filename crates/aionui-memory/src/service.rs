//! Memory domain business operations.

use std::sync::Arc;

use aionui_api_types::{
    CompleteMemoryJobRequest, MemoryCandidateMutation, MemoryEntryKind, MemoryJobFailureCode, MemoryJobResponse,
    MemorySummary, MemoryUpdateInput, NormalizedMemoryJobFailure,
};
use aionui_common::{generate_prefixed_id, now_ms};
use aionui_db::{
    ClaimMemoryJobRow, CommitMemoryEntryRow, CommitMemoryEntryTransition, CommitMemorySourceRow,
    CommitMemoryUpdateResult, CommitMemoryUpdateRow, EnqueueMemoryTurnRow, IConversationRepository, IMemoryRepository,
    ReleaseMemoryLeaseRow, RenewMemoryLeaseRow, TransitionMemoryJobRow,
};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::{
    AppOperationsReadinessPort, EvidenceBuildRequest, MemoryError, MemoryTurnOutcome,
    evidence::EvidenceBuilder,
    jobs::{eligible_completed_turn, job_response},
    sanitizer::{MAX_STRING_LENGTH, MAX_SUMMARY_BYTES, MAX_SUMMARY_ITEMS, sanitize_text},
};

const OPERATION_VERSION: &str = "memory-v1";
const RETRY_DELAYS_MS: [i64; 5] = [30_000, 120_000, 600_000, 3_600_000, 21_600_000];
const MAX_MUTATIONS: usize = 200;

#[derive(Clone)]
struct JobDependencies {
    memory: Arc<dyn IMemoryRepository>,
    conversations: Arc<dyn IConversationRepository>,
    readiness: Arc<dyn AppOperationsReadinessPort>,
}

/// Domain service that owns Memory business-operation entry points.
#[derive(Clone)]
pub struct MemoryService {
    evidence_builder: Arc<EvidenceBuilder>,
    jobs: Option<Arc<JobDependencies>>,
}

impl Default for MemoryService {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryService {
    /// Creates the public Memory business-operation entry point.
    pub fn new() -> Self {
        Self {
            evidence_builder: Arc::new(EvidenceBuilder),
            jobs: None,
        }
    }

    pub fn with_job_dependencies(
        memory: Arc<dyn IMemoryRepository>,
        conversations: Arc<dyn IConversationRepository>,
        readiness: Arc<dyn AppOperationsReadinessPort>,
    ) -> Self {
        Self {
            evidence_builder: Arc::new(EvidenceBuilder),
            jobs: Some(Arc::new(JobDependencies {
                memory,
                conversations,
                readiness,
            })),
        }
    }

    /// Reconstructs validated, sanitized evidence for the registered Memory task.
    pub fn build_evidence(&self, request: EvidenceBuildRequest) -> Result<MemoryUpdateInput, MemoryError> {
        self.evidence_builder.build(request)
    }

    /// Best-effort canonical post-persistence trigger. Logs identifiers and status only.
    pub async fn on_turn_completed(
        &self,
        user_id: &str,
        conversation_id: &str,
        turn_id: &str,
        outcome: MemoryTurnOutcome,
    ) {
        match self
            .enqueue_canonical_turn(user_id, conversation_id, turn_id, outcome)
            .await
        {
            Ok(true) => debug!(
                user_id,
                conversation_id,
                turn_id,
                status = "enqueued",
                "Memory turn evaluated"
            ),
            Ok(false) => debug!(
                user_id,
                conversation_id,
                turn_id,
                status = "ineligible",
                "Memory turn evaluated"
            ),
            Err(error) => {
                warn!(user_id, conversation_id, turn_id, status = "failed", error = %error, "Memory turn evaluation failed")
            }
        }
    }

    async fn enqueue_canonical_turn(
        &self,
        user_id: &str,
        conversation_id: &str,
        turn_id: &str,
        outcome: MemoryTurnOutcome,
    ) -> Result<bool, MemoryError> {
        let jobs = self.job_dependencies()?;
        let conversation = jobs
            .conversations
            .get(conversation_id)
            .await
            .map_err(map_db_error)?
            .filter(|row| row.user_id == user_id)
            .ok_or(MemoryError::NotFound)?;
        let messages = jobs
            .conversations
            .list_messages_by_turn(user_id, conversation_id, turn_id)
            .await
            .map_err(map_db_error)?;
        let policy = jobs
            .memory
            .effective_policy(user_id, conversation_id)
            .await
            .map_err(map_db_error)?;
        if !eligible_completed_turn(&conversation, &policy, &messages, outcome) {
            return Ok(false);
        }
        let previous = jobs
            .memory
            .get_conversation_memory(user_id, conversation_id)
            .await
            .map_err(map_db_error)?;
        let hash_material = messages
            .iter()
            .map(|message| format!("{}:{}:{}", message.id, message.r#type, message.content))
            .collect::<Vec<_>>()
            .join("|");
        let input_hash = hex_hash(&format!("{OPERATION_VERSION}:{turn_id}:{hash_material}"));
        jobs.memory
            .enqueue_completed_turn(EnqueueMemoryTurnRow {
                id: generate_prefixed_id("memory-job"),
                user_id: user_id.into(),
                conversation_id: conversation_id.into(),
                from_turn_id: previous.as_ref().map(|memory| memory.through_turn_id.clone()),
                through_turn_id: turn_id.into(),
                operation_version: OPERATION_VERSION.into(),
                input_hash,
                expected_revision: previous.as_ref().map_or(0, |memory| memory.revision),
                now: now_ms(),
            })
            .await
            .map_err(map_db_error)?;
        Ok(true)
    }

    pub async fn claim_job(
        &self,
        user_id: &str,
        worker_id: &str,
        lease_ms: u64,
    ) -> Result<Option<MemoryJobResponse>, MemoryError> {
        let jobs = self.job_dependencies()?;
        let lease_duration_ms = valid_lease_ms(lease_ms)?;
        if !jobs.readiness.is_usable().await? {
            return Ok(None);
        }
        let now = now_ms();
        jobs.memory.unblock_jobs(user_id, now).await.map_err(map_db_error)?;
        jobs.memory
            .claim_next_job(ClaimMemoryJobRow {
                user_id: user_id.into(),
                worker_id: worker_id.into(),
                now,
                lease_duration_ms,
            })
            .await
            .map_err(map_db_error)?
            .map(job_response)
            .transpose()
    }

    pub async fn renew_job_lease(
        &self,
        user_id: &str,
        job_id: &str,
        worker_id: &str,
        lease_ms: u64,
    ) -> Result<i64, MemoryError> {
        let jobs = self.job_dependencies()?;
        let now = now_ms();
        let lease_duration_ms = valid_lease_ms(lease_ms)?;
        let renewed = jobs
            .memory
            .renew_lease(RenewMemoryLeaseRow {
                user_id: user_id.into(),
                job_id: job_id.into(),
                worker_id: worker_id.into(),
                now,
                lease_duration_ms,
            })
            .await
            .map_err(map_db_error)?;
        renewed.then_some(now + lease_duration_ms).ok_or(MemoryError::LeaseLost)
    }

    pub async fn release_job(&self, user_id: &str, job_id: &str, worker_id: &str) -> Result<bool, MemoryError> {
        let jobs = self.job_dependencies()?;
        let released = jobs
            .memory
            .release_lease(ReleaseMemoryLeaseRow {
                user_id: user_id.into(),
                job_id: job_id.into(),
                worker_id: worker_id.into(),
                now: now_ms(),
            })
            .await
            .map_err(map_db_error)?;
        released.then_some(true).ok_or(MemoryError::LeaseLost)
    }

    pub async fn record_job_failure(
        &self,
        user_id: &str,
        job_id: &str,
        worker_id: &str,
        failure: NormalizedMemoryJobFailure,
    ) -> Result<MemoryJobResponse, MemoryError> {
        let jobs = self.job_dependencies()?;
        let current = jobs
            .memory
            .get_job(user_id, job_id)
            .await
            .map_err(map_db_error)?
            .ok_or(MemoryError::NotFound)?;
        let now = now_ms();
        let (state, next_attempt_at) = failure_transition(&failure.code, current.attempt_count, now);
        let row = jobs
            .memory
            .transition_running_job(TransitionMemoryJobRow {
                user_id: user_id.into(),
                job_id: job_id.into(),
                worker_id: worker_id.into(),
                state: state.into(),
                next_attempt_at,
                error_code: Some(failure_code(&failure.code).into()),
                now,
            })
            .await
            .map_err(map_db_error)?
            .ok_or(MemoryError::LeaseLost)?;
        job_response(row)
    }

    pub async fn load_job_evidence(
        &self,
        user_id: &str,
        job_id: &str,
        lease_owner: &str,
    ) -> Result<MemoryUpdateInput, MemoryError> {
        let jobs = self.job_dependencies()?;
        let job = jobs
            .memory
            .get_job(user_id, job_id)
            .await
            .map_err(map_db_error)?
            .ok_or(MemoryError::NotFound)?;
        if job.state != "running"
            || job.lease_owner.as_deref() != Some(lease_owner)
            || job.lease_expires_at.is_none_or(|expires_at| expires_at <= now_ms())
        {
            return Err(MemoryError::LeaseLost);
        }
        let conversation = jobs
            .conversations
            .get(&job.conversation_id)
            .await
            .map_err(map_db_error)?
            .filter(|row| row.user_id == user_id)
            .ok_or(MemoryError::NotFound)?;
        let messages = jobs
            .conversations
            .list_messages_for_memory_range(
                user_id,
                &job.conversation_id,
                job.from_turn_id.as_deref(),
                &job.through_turn_id,
            )
            .await
            .map_err(map_db_error)?;
        let previous = jobs
            .memory
            .get_conversation_memory(user_id, &job.conversation_id)
            .await
            .map_err(map_db_error)?;
        let previous_summary = previous
            .as_ref()
            .map(|row| serde_json::from_str::<MemorySummary>(&row.summary_json).map_err(|_| MemoryError::Internal))
            .transpose()?;
        let mut claimed_turn_ids = Vec::new();
        if let Some(from_turn_id) = job.from_turn_id.clone() {
            claimed_turn_ids.push(from_turn_id);
        }
        for message in &messages {
            if let Some(turn_id) = &message.turn_id
                && claimed_turn_ids.last() != Some(turn_id)
            {
                claimed_turn_ids.push(turn_id.clone());
            }
        }
        if claimed_turn_ids.last() != Some(&job.through_turn_id) {
            return Err(MemoryError::InvalidInput);
        }
        self.build_evidence(EvidenceBuildRequest {
            conversation,
            messages,
            previous_summary,
            summary_cursor: job.from_turn_id,
            claimed_turn_ids,
            existing_entries: jobs.memory.list_entries(user_id).await.map_err(map_db_error)?,
        })
    }

    pub async fn get_job(&self, user_id: &str, job_id: &str) -> Result<MemoryJobResponse, MemoryError> {
        let row = self
            .job_dependencies()?
            .memory
            .get_job(user_id, job_id)
            .await
            .map_err(map_db_error)?
            .ok_or(MemoryError::NotFound)?;
        job_response(row)
    }

    pub async fn complete_job(
        &self,
        user_id: &str,
        job_id: &str,
        lease_owner: &str,
        request: CompleteMemoryJobRequest,
    ) -> Result<(), MemoryError> {
        let jobs = self.job_dependencies()?;
        let job = jobs
            .memory
            .get_job(user_id, job_id)
            .await
            .map_err(map_db_error)?
            .ok_or(MemoryError::NotFound)?;
        if request.expected_revision != u64::try_from(job.expected_revision).map_err(|_| MemoryError::Internal)? {
            return Err(MemoryError::StaleRevision);
        }
        let evidence = self.load_job_evidence(user_id, job_id, lease_owner).await?;
        if request.output.mutations.len() > MAX_MUTATIONS {
            return Err(MemoryError::InvalidInput);
        }
        if !valid_metadata(&request.task_result_provenance.provider_id)
            || !valid_metadata(&request.task_result_provenance.model_id)
            || !valid_metadata(&request.task_result_provenance.prompt_version)
        {
            return Err(MemoryError::InvalidInput);
        }
        let summary = sanitize_output_summary(request.output.summary)?;
        let summary_json = serde_json::to_string(&summary).map_err(|_| MemoryError::InvalidInput)?;
        if summary_json.len() > MAX_SUMMARY_BYTES {
            return Err(MemoryError::InvalidInput);
        }
        let valid_targets = evidence
            .existing_entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let turns = evidence
            .source_turns
            .iter()
            .map(|turn| (turn.turn_id.as_str(), turn))
            .collect::<std::collections::HashMap<_, _>>();
        let mut entries = Vec::with_capacity(request.output.mutations.len());
        for mutation in request.output.mutations {
            let (kind, stable_key, content, source_turn_ids, transition) = match mutation {
                MemoryCandidateMutation::Create {
                    kind,
                    stable_key,
                    content,
                    source_turn_ids,
                } => (
                    kind,
                    stable_key,
                    content,
                    source_turn_ids,
                    CommitMemoryEntryTransition::Create,
                ),
                MemoryCandidateMutation::Refine {
                    target_entry_id,
                    kind,
                    stable_key,
                    content,
                    source_turn_ids,
                } => {
                    if !valid_targets.contains(target_entry_id.as_str()) {
                        return Err(MemoryError::InvalidInput);
                    }
                    let transition = CommitMemoryEntryTransition::Refine {
                        target_entry_id: target_entry_id.clone(),
                    };
                    (kind, stable_key, content, source_turn_ids, transition)
                }
                MemoryCandidateMutation::Supersede {
                    target_entry_id,
                    kind,
                    stable_key,
                    content,
                    source_turn_ids,
                } => {
                    if !valid_targets.contains(target_entry_id.as_str()) {
                        return Err(MemoryError::InvalidInput);
                    }
                    let transition = CommitMemoryEntryTransition::Supersede {
                        target_entry_id: target_entry_id.clone(),
                    };
                    (kind, stable_key, content, source_turn_ids, transition)
                }
                MemoryCandidateMutation::Conflict {
                    target_entry_id,
                    kind,
                    stable_key,
                    content,
                    source_turn_ids,
                } => {
                    if !valid_targets.contains(target_entry_id.as_str()) {
                        return Err(MemoryError::InvalidInput);
                    }
                    let transition = CommitMemoryEntryTransition::Conflict {
                        target_entry_id: target_entry_id.clone(),
                        conflict_group_id: generate_prefixed_id("memory-conflict"),
                    };
                    (kind, stable_key, content, source_turn_ids, transition)
                }
            };
            let stable_key = stable_key
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            let content = sanitize_text(&content);
            if stable_key.is_empty()
                || stable_key.len() > MAX_STRING_LENGTH
                || content.trim().is_empty()
                || content.len() > MAX_STRING_LENGTH
                || source_turn_ids.is_empty()
            {
                return Err(MemoryError::InvalidInput);
            }
            if source_turn_ids.iter().collect::<std::collections::HashSet<_>>().len() != source_turn_ids.len() {
                return Err(MemoryError::InvalidInput);
            }
            let mut sources = Vec::with_capacity(source_turn_ids.len());
            for turn_id in source_turn_ids {
                let turn = turns.get(turn_id.as_str()).ok_or(MemoryError::InvalidInput)?;
                sources.push(CommitMemorySourceRow {
                    conversation_id: job.conversation_id.clone(),
                    turn_id,
                    message_ids_json: serde_json::to_string(
                        &turn
                            .messages
                            .iter()
                            .map(|message| &message.message_id)
                            .collect::<Vec<_>>(),
                    )
                    .map_err(|_| MemoryError::Internal)?,
                });
            }
            let kind = kind_name(&kind);
            let fingerprint_material = format!(
                "{}|{}|{}|{}|{}",
                user_id,
                evidence.conversation.project_id.as_deref().unwrap_or_default(),
                evidence.conversation.workspace_key.as_deref().unwrap_or_default(),
                kind,
                stable_key,
            );
            entries.push(CommitMemoryEntryRow {
                id: generate_prefixed_id("memory-entry"),
                project_id: evidence.conversation.project_id.clone(),
                workspace_key: evidence.conversation.workspace_key.clone(),
                kind: kind.into(),
                stable_key,
                fingerprint: hex_hash(&fingerprint_material),
                content,
                transition,
                sources,
            });
        }
        match jobs
            .memory
            .commit_update(CommitMemoryUpdateRow {
                user_id: user_id.into(),
                job_id: job_id.into(),
                conversation_id: job.conversation_id,
                expected_revision: job.expected_revision,
                through_turn_id: job.through_turn_id,
                project_id: evidence.conversation.project_id,
                workspace_key: evidence.conversation.workspace_key,
                summary_json,
                schema_version: 1,
                prompt_version: Some(request.task_result_provenance.prompt_version),
                writer_provider_id: Some(request.task_result_provenance.provider_id),
                writer_model_id: Some(request.task_result_provenance.model_id),
                lease_owner: lease_owner.into(),
                expected_attempt_count: job.attempt_count,
                entries,
                change_set_id: generate_prefixed_id("memory-change"),
                now: now_ms(),
            })
            .await
            .map_err(map_db_error)?
        {
            CommitMemoryUpdateResult::Committed { .. } => Ok(()),
            CommitMemoryUpdateResult::StaleRevision { .. } => Err(MemoryError::StaleRevision),
        }
    }

    pub async fn cancel_conversation_jobs(&self, user_id: &str, conversation_id: &str) -> Result<(), MemoryError> {
        self.job_dependencies()?
            .memory
            .cancel_jobs(user_id, Some(conversation_id), now_ms())
            .await
            .map_err(map_db_error)?;
        Ok(())
    }

    pub async fn cancel_all_jobs(&self, user_id: &str) -> Result<(), MemoryError> {
        self.job_dependencies()?
            .memory
            .cancel_jobs(user_id, None, now_ms())
            .await
            .map_err(map_db_error)?;
        Ok(())
    }

    pub async fn recover_expired_jobs(&self) -> Result<u64, MemoryError> {
        self.job_dependencies()?
            .memory
            .recover_expired_jobs(now_ms())
            .await
            .map_err(map_db_error)
    }

    fn job_dependencies(&self) -> Result<&JobDependencies, MemoryError> {
        self.jobs.as_deref().ok_or(MemoryError::Internal)
    }
}

fn valid_lease_ms(lease_ms: u64) -> Result<i64, MemoryError> {
    let lease_ms: i64 = lease_ms.try_into().map_err(|_| MemoryError::InvalidInput)?;
    (lease_ms > 0).then_some(lease_ms).ok_or(MemoryError::InvalidInput)
}

fn kind_name(kind: &MemoryEntryKind) -> &'static str {
    match kind {
        MemoryEntryKind::Decision => "decision",
        MemoryEntryKind::Outcome => "outcome",
        MemoryEntryKind::Artifact => "artifact",
        MemoryEntryKind::Issue => "issue",
        MemoryEntryKind::NextStep => "next_step",
        MemoryEntryKind::WorkConstraint => "work_constraint",
    }
}

fn hex_hash(material: &str) -> String {
    Sha256::digest(material.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sanitize_output_summary(summary: MemorySummary) -> Result<MemorySummary, MemoryError> {
    let goal = sanitize_text(&summary.goal);
    let sanitize_values = |values: Vec<String>| -> Result<Vec<String>, MemoryError> {
        values
            .into_iter()
            .map(|value| {
                let value = sanitize_text(&value);
                (!value.trim().is_empty() && value.len() <= MAX_STRING_LENGTH)
                    .then_some(value)
                    .ok_or(MemoryError::InvalidInput)
            })
            .collect()
    };
    let summary = MemorySummary {
        goal,
        current_state: sanitize_values(summary.current_state)?,
        decisions: sanitize_values(summary.decisions)?,
        artifacts: sanitize_values(summary.artifacts)?,
        issues: sanitize_values(summary.issues)?,
        next_steps: sanitize_values(summary.next_steps)?,
        work_constraints: sanitize_values(summary.work_constraints)?,
    };
    let item_count = usize::from(!summary.goal.is_empty())
        + summary.current_state.len()
        + summary.decisions.len()
        + summary.artifacts.len()
        + summary.issues.len()
        + summary.next_steps.len()
        + summary.work_constraints.len();
    if summary.goal.len() > MAX_STRING_LENGTH || item_count > MAX_SUMMARY_ITEMS {
        return Err(MemoryError::InvalidInput);
    }
    Ok(summary)
}

fn valid_metadata(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= MAX_STRING_LENGTH
}

fn failure_transition(code: &MemoryJobFailureCode, attempt_count: i64, now: i64) -> (&'static str, Option<i64>) {
    match code {
        MemoryJobFailureCode::NotConfigured
        | MemoryJobFailureCode::ModelUnavailable
        | MemoryJobFailureCode::ProviderAuthFailed => ("blocked", None),
        MemoryJobFailureCode::InvalidInput => ("failed", None),
        MemoryJobFailureCode::Canceled => ("pending", None),
        MemoryJobFailureCode::InvalidOutput if attempt_count >= 2 => ("failed", None),
        _ if attempt_count >= RETRY_DELAYS_MS.len() as i64 => ("failed", None),
        _ => (
            "retry_wait",
            Some(now + RETRY_DELAYS_MS[attempt_count.saturating_sub(1) as usize]),
        ),
    }
}

fn failure_code(code: &MemoryJobFailureCode) -> &'static str {
    match code {
        MemoryJobFailureCode::NotConfigured => "not_configured",
        MemoryJobFailureCode::ModelUnavailable => "model_unavailable",
        MemoryJobFailureCode::ProviderAuthFailed => "provider_auth_failed",
        MemoryJobFailureCode::QueueFull => "queue_full",
        MemoryJobFailureCode::Timeout => "timeout",
        MemoryJobFailureCode::RateLimited => "rate_limited",
        MemoryJobFailureCode::ProviderRequestFailed => "provider_request_failed",
        MemoryJobFailureCode::InvalidOutput => "invalid_output",
        MemoryJobFailureCode::InvalidInput => "invalid_input",
        MemoryJobFailureCode::Canceled => "canceled",
    }
}

fn map_db_error(error: aionui_db::DbError) -> MemoryError {
    match error {
        aionui_db::DbError::NotFound(_) => MemoryError::NotFound,
        aionui_db::DbError::Conflict(_) => MemoryError::Conflict,
        _ => MemoryError::Internal,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use aionui_api_types::{
        CompleteMemoryJobRequest, MemoryJobFailureCode, MemoryJobState, MemorySummary, MemoryTaskResultProvenance,
        MemoryUpdateOutput, NormalizedMemoryJobFailure,
    };
    use aionui_db::models::{ConversationRow, MessageRow};
    use aionui_db::{
        IConversationRepository, IMemoryRepository, SqliteConversationRepository, SqliteMemoryRepository,
        UpdateMemorySettingsRow, init_database_memory,
    };

    use super::MemoryService;
    use crate::{AppOperationsReadinessPort, EvidenceBuildRequest, MemoryError, MemoryTurnOutcome};

    const USER_ID: &str = "system_default_user";

    struct MutableReadiness(AtomicBool);

    impl MutableReadiness {
        fn new(usable: bool) -> Self {
            Self(AtomicBool::new(usable))
        }

        fn set(&self, usable: bool) {
            self.0.store(usable, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl AppOperationsReadinessPort for MutableReadiness {
        async fn is_usable(&self) -> Result<bool, MemoryError> {
            Ok(self.0.load(Ordering::SeqCst))
        }
    }

    #[test]
    fn exposes_evidence_building_through_the_public_service() {
        let service = MemoryService::new();
        let output = service
            .build_evidence(EvidenceBuildRequest {
                conversation: ConversationRow {
                    id: "conversation-1".into(),
                    user_id: "user-1".into(),
                    name: "Conversation".into(),
                    r#type: "acp".into(),
                    extra: "{}".into(),
                    model: None,
                    status: Some("finished".into()),
                    source: Some("aionui".into()),
                    channel_chat_id: None,
                    pinned: false,
                    pinned_at: None,
                    created_at: 1,
                    updated_at: 1,
                },
                messages: Vec::new(),
                previous_summary: None,
                summary_cursor: None,
                claimed_turn_ids: Vec::new(),
                existing_entries: Vec::new(),
            })
            .unwrap();

        assert_eq!(output.conversation.id, "conversation-1");
    }

    #[tokio::test]
    async fn canonical_completion_is_idempotent_and_coalesces_pending_turns() {
        let fixture = fixture(true).await;
        fixture.persist_turn("turn-1", 10).await;
        fixture
            .service
            .on_turn_completed(USER_ID, "conversation-1", "turn-1", MemoryTurnOutcome::Completed)
            .await;
        fixture
            .service
            .on_turn_completed(USER_ID, "conversation-1", "turn-1", MemoryTurnOutcome::Completed)
            .await;

        fixture.persist_turn("turn-2", 20).await;
        fixture
            .service
            .on_turn_completed(USER_ID, "conversation-1", "turn-2", MemoryTurnOutcome::Completed)
            .await;

        let claimed = fixture
            .service
            .claim_job(USER_ID, "worker-1", 30_000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.through_turn_id, "turn-2");
        assert_ne!(claimed.input_hash, "");
        assert_eq!(claimed.state, MemoryJobState::Running);
        let evidence = fixture
            .service
            .load_job_evidence(USER_ID, &claimed.id, "worker-1")
            .await
            .unwrap();
        assert_eq!(
            evidence
                .source_turns
                .iter()
                .map(|turn| turn.turn_id.as_str())
                .collect::<Vec<_>>(),
            ["turn-1", "turn-2"],
        );
        assert!(
            fixture
                .service
                .claim_job(USER_ID, "worker-2", 30_000)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn running_work_has_one_next_pending_range_and_lease_operations_are_owner_fenced() {
        let fixture = fixture(true).await;
        fixture.persist_turn("turn-1", 10).await;
        fixture
            .service
            .on_turn_completed(USER_ID, "conversation-1", "turn-1", MemoryTurnOutcome::Completed)
            .await;
        let running = fixture
            .service
            .claim_job(USER_ID, "worker-1", 30_000)
            .await
            .unwrap()
            .unwrap();

        fixture.persist_turn("turn-2", 20).await;
        fixture.persist_turn("turn-3", 30).await;
        for turn_id in ["turn-2", "turn-3"] {
            fixture
                .service
                .on_turn_completed(USER_ID, "conversation-1", turn_id, MemoryTurnOutcome::Completed)
                .await;
        }

        assert_eq!(
            fixture
                .service
                .renew_job_lease(USER_ID, &running.id, "other", 30_000)
                .await,
            Err(MemoryError::LeaseLost),
        );
        assert!(
            fixture
                .service
                .renew_job_lease(USER_ID, &running.id, "worker-1", 30_000)
                .await
                .unwrap()
                > 0
        );
        assert_eq!(
            fixture.service.release_job(USER_ID, &running.id, "other").await,
            Err(MemoryError::LeaseLost),
        );

        fixture
            .service
            .record_job_failure(
                USER_ID,
                &running.id,
                "worker-1",
                NormalizedMemoryJobFailure {
                    code: MemoryJobFailureCode::InvalidInput,
                    message: Some("content must not be persisted".into()),
                },
            )
            .await
            .unwrap();
        let next = fixture
            .service
            .claim_job(USER_ID, "worker-2", 30_000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.through_turn_id, "turn-3");
    }

    #[tokio::test]
    async fn evidence_requires_the_current_unexpired_lease_owner() {
        let fixture = fixture(true).await;
        fixture.persist_turn("turn-1", 10).await;
        fixture
            .service
            .on_turn_completed(USER_ID, "conversation-1", "turn-1", MemoryTurnOutcome::Completed)
            .await;
        let job = fixture
            .service
            .claim_job(USER_ID, "worker-1", 30_000)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            fixture.service.load_job_evidence(USER_ID, &job.id, "other").await,
            Err(MemoryError::LeaseLost),
        );
        let evidence = fixture
            .service
            .load_job_evidence(USER_ID, &job.id, "worker-1")
            .await
            .unwrap();
        assert_eq!(evidence.source_turns.len(), 1);
        assert_eq!(evidence.source_turns[0].turn_id, "turn-1");
    }

    #[tokio::test]
    async fn completion_commits_the_cursor_under_the_current_lease_fence() {
        let fixture = fixture(true).await;
        fixture.persist_turn("turn-1", 10).await;
        fixture
            .service
            .on_turn_completed(USER_ID, "conversation-1", "turn-1", MemoryTurnOutcome::Completed)
            .await;
        let job = fixture
            .service
            .claim_job(USER_ID, "worker-1", 30_000)
            .await
            .unwrap()
            .unwrap();

        fixture
            .service
            .complete_job(
                USER_ID,
                &job.id,
                "worker-1",
                CompleteMemoryJobRequest {
                    expected_revision: job.expected_revision,
                    output: MemoryUpdateOutput {
                        summary: MemorySummary {
                            goal: "Deliver the work".into(),
                            current_state: vec!["Complete".into()],
                            decisions: Vec::new(),
                            artifacts: Vec::new(),
                            issues: Vec::new(),
                            next_steps: Vec::new(),
                            work_constraints: Vec::new(),
                        },
                        mutations: Vec::new(),
                    },
                    task_result_provenance: MemoryTaskResultProvenance {
                        provider_id: "provider-1".into(),
                        model_id: "model-1".into(),
                        prompt_version: "memory-prompt-v1".into(),
                    },
                },
            )
            .await
            .unwrap();

        assert_eq!(
            fixture.memory.get_job(USER_ID, &job.id).await.unwrap().unwrap().state,
            "succeeded"
        );
        assert_eq!(
            fixture
                .memory
                .get_conversation_memory(USER_ID, "conversation-1")
                .await
                .unwrap()
                .unwrap()
                .through_turn_id,
            "turn-1",
        );
    }

    #[tokio::test]
    async fn blocked_work_reenters_claiming_only_after_shared_readiness_is_usable() {
        let fixture = fixture(true).await;
        fixture.persist_turn("turn-1", 10).await;
        fixture
            .service
            .on_turn_completed(USER_ID, "conversation-1", "turn-1", MemoryTurnOutcome::Completed)
            .await;
        let job = fixture
            .service
            .claim_job(USER_ID, "worker-1", 30_000)
            .await
            .unwrap()
            .unwrap();
        fixture
            .service
            .record_job_failure(
                USER_ID,
                &job.id,
                "worker-1",
                NormalizedMemoryJobFailure {
                    code: MemoryJobFailureCode::NotConfigured,
                    message: None,
                },
            )
            .await
            .unwrap();

        fixture.readiness.set(false);
        assert!(
            fixture
                .service
                .claim_job(USER_ID, "worker-2", 30_000)
                .await
                .unwrap()
                .is_none()
        );
        fixture.readiness.set(true);
        let reclaimed = fixture
            .service
            .claim_job(USER_ID, "worker-2", 30_000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reclaimed.id, job.id);
    }

    #[tokio::test]
    async fn startup_recovery_returns_expired_running_leases_to_pending() {
        let fixture = fixture(true).await;
        fixture.persist_turn("turn-1", 10).await;
        fixture
            .service
            .on_turn_completed(USER_ID, "conversation-1", "turn-1", MemoryTurnOutcome::Completed)
            .await;
        let job = fixture
            .service
            .claim_job(USER_ID, "worker-1", 1)
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;

        assert_eq!(fixture.service.recover_expired_jobs().await.unwrap(), 1);
        assert_eq!(
            fixture.memory.get_job(USER_ID, &job.id).await.unwrap().unwrap().state,
            "pending"
        );
    }

    #[tokio::test]
    async fn capture_disable_forget_clear_and_shutdown_preserve_the_required_durable_intent() {
        let fixture = fixture(true).await;
        fixture.persist_turn("turn-1", 10).await;
        fixture
            .service
            .on_turn_completed(USER_ID, "conversation-1", "turn-1", MemoryTurnOutcome::Completed)
            .await;
        let job = fixture
            .service
            .claim_job(USER_ID, "worker-1", 30_000)
            .await
            .unwrap()
            .unwrap();
        fixture.service.release_job(USER_ID, &job.id, "worker-1").await.unwrap();
        assert_eq!(
            fixture
                .service
                .claim_job(USER_ID, "worker-2", 30_000)
                .await
                .unwrap()
                .unwrap()
                .id,
            job.id,
            "shutdown release must return eligible work to pending",
        );

        fixture
            .service
            .cancel_conversation_jobs(USER_ID, "conversation-1")
            .await
            .unwrap();
        assert!(
            fixture
                .service
                .claim_job(USER_ID, "worker-3", 30_000)
                .await
                .unwrap()
                .is_none()
        );

        fixture.persist_turn("turn-2", 20).await;
        fixture
            .service
            .on_turn_completed(USER_ID, "conversation-1", "turn-2", MemoryTurnOutcome::Completed)
            .await;
        fixture.service.cancel_all_jobs(USER_ID).await.unwrap();
        assert!(
            fixture
                .service
                .claim_job(USER_ID, "worker-4", 30_000)
                .await
                .unwrap()
                .is_none()
        );
    }

    struct Fixture {
        service: MemoryService,
        conversations: Arc<SqliteConversationRepository>,
        memory: Arc<SqliteMemoryRepository>,
        readiness: Arc<MutableReadiness>,
        _db: aionui_db::Database,
    }

    impl Fixture {
        async fn persist_turn(&self, turn_id: &str, created_at: i64) {
            for message in [
                message(&format!("{turn_id}-user"), turn_id, "right", "Do the work", created_at),
                message(
                    &format!("{turn_id}-assistant"),
                    turn_id,
                    "left",
                    "Work completed",
                    created_at + 1,
                ),
            ] {
                self.conversations.insert_message(&message).await.unwrap();
            }
        }
    }

    async fn fixture(usable: bool) -> Fixture {
        let db = init_database_memory().await.unwrap();
        let conversations = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let memory = Arc::new(SqliteMemoryRepository::new(db.pool().clone()));
        conversations.create(&conversation()).await.unwrap();
        memory
            .update_settings(UpdateMemorySettingsRow {
                user_id: USER_ID.into(),
                enabled: Some(true),
                default_capture: Some(true),
                default_recall: None,
                consent_version: Some(1),
                now: 1,
            })
            .await
            .unwrap();
        let readiness = Arc::new(MutableReadiness::new(usable));
        let service = MemoryService::with_job_dependencies(memory.clone(), conversations.clone(), readiness.clone());
        Fixture {
            service,
            conversations,
            memory,
            readiness,
            _db: db,
        }
    }

    fn conversation() -> ConversationRow {
        ConversationRow {
            id: "conversation-1".into(),
            user_id: USER_ID.into(),
            name: "Conversation".into(),
            r#type: "gemini".into(),
            extra: "{}".into(),
            model: None,
            status: Some("finished".into()),
            source: Some("aionui".into()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn message(id: &str, turn_id: &str, position: &str, content: &str, created_at: i64) -> MessageRow {
        MessageRow {
            id: id.into(),
            conversation_id: "conversation-1".into(),
            turn_id: Some(turn_id.into()),
            msg_id: Some(id.into()),
            r#type: "text".into(),
            content: serde_json::json!({ "content": content }).to_string(),
            position: Some(position.into()),
            status: Some("finish".into()),
            hidden: false,
            created_at,
        }
    }
}
