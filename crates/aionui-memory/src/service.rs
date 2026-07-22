//! Memory domain business operations.

use std::sync::Arc;

use aionui_api_types::{
    CompleteMemoryJobRequest, MemoryCandidateMutation, MemoryEntryKind, MemoryJobFailureCode, MemoryJobResponse,
    MemorySummary, MemoryUpdateInput, NormalizedMemoryJobFailure,
};
use aionui_common::{generate_prefixed_id, now_ms};
use aionui_db::models::{MemoryJobRow, MessageRow};
use aionui_db::{
    ClaimMemoryJobRow, CommitMemoryEntryRow, CommitMemoryEntryTransition, CommitMemorySourceRow,
    CommitMemoryUpdateResult, CommitMemoryUpdateRow, EnqueueMemoryTurnRow, IConversationRepository, IMemoryRepository,
    MemoryCandidateQueryRow, ReleaseMemoryLeaseRow, RenewMemoryLeaseRow, SplitMemoryJobRow, TransitionMemoryJobRow,
    UpdateConversationMemoryLifecycleRow, UpdateMemoryLifecycleRow,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::{
    AppOperationsReadinessPort, EvidenceBuildRequest, MemoryError, MemoryTurnOutcome,
    evidence::EvidenceBuilder,
    jobs::{ClaimedMemoryJob, eligible_completed_turn, job_response},
    sanitizer::{
        MAX_EXISTING_ENTRIES, MAX_MUTATION_COUNT, MAX_STRING_LENGTH, MAX_SUMMARY_BYTES, MAX_SUMMARY_ITEMS,
        OPERATION_VERSION, SANITIZER_VERSION, sanitize_text, strip_user_context_sentences,
    },
};

const RETRY_DELAYS_MS: [i64; 5] = [30_000, 120_000, 600_000, 3_600_000, 21_600_000];
/// Maximum worker lease duration accepted by Memory routes.
pub const MAX_LEASE_DURATION_MS: u64 = 15 * 60 * 1_000;

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
        let turn_ids = vec![turn_id.to_owned()];
        let turn_hash = evidence_input_hash(None, &turn_ids, &messages)?;
        let enqueued = jobs
            .memory
            .enqueue_completed_turn(EnqueueMemoryTurnRow {
                id: generate_prefixed_id("memory-job"),
                user_id: user_id.into(),
                conversation_id: conversation_id.into(),
                from_turn_id: previous.as_ref().map(|memory| memory.through_turn_id.clone()),
                through_turn_id: turn_id.into(),
                operation_version: OPERATION_VERSION.into(),
                turn_hash,
                expected_global_epoch: policy.global_epoch,
                expected_conversation_epoch: policy.conversation_epoch,
                required_consent_version: super::jobs::MEMORY_DISCLOSURE_VERSION,
                expected_revision: previous.as_ref().map_or(0, |memory| memory.revision),
                now: now_ms(),
            })
            .await
            .map_err(map_db_error)?;
        Ok(enqueued.is_some())
    }

    async fn bound_claimed_job(
        &self,
        user_id: &str,
        lease_token: &str,
        row: &mut MemoryJobRow,
    ) -> Result<(), MemoryError> {
        let jobs = self.job_dependencies()?;
        let queued = jobs
            .memory
            .list_job_turns(user_id, &row.id, (crate::sanitizer::MAX_EVIDENCE_TURNS + 1) as u32)
            .await
            .map_err(map_db_error)?;
        let turn_ids = queued.iter().map(|turn| turn.turn_id.clone()).collect::<Vec<_>>();
        if turn_ids.is_empty() {
            return Err(MemoryError::InvalidInput);
        }
        let conversation = jobs
            .conversations
            .get(&row.conversation_id)
            .await
            .map_err(map_db_error)?
            .filter(|conversation| conversation.user_id == user_id)
            .ok_or(MemoryError::NotFound)?;
        let all_messages = self
            .load_exact_messages(user_id, &row.conversation_id, &turn_ids)
            .await?;

        let mut bounded_count = turn_ids.len().min(crate::sanitizer::MAX_EVIDENCE_TURNS);
        while bounded_count > 1 {
            let bounded_turn_ids = turn_ids[..bounded_count].to_vec();
            let bounded_messages = messages_for_turns(&all_messages, &bounded_turn_ids);
            if self
                .build_evidence(EvidenceBuildRequest {
                    conversation: conversation.clone(),
                    messages: bounded_messages,
                    previous_summary: None,
                    summary_cursor: row.from_turn_id.clone(),
                    claimed_turn_ids: bounded_turn_ids,
                    existing_entries: Vec::new(),
                })
                .is_ok()
            {
                break;
            }
            bounded_count -= 1;
        }

        if i64::try_from(bounded_count).map_err(|_| MemoryError::Internal)? < row.turn_count {
            let split = jobs
                .memory
                .split_claimed_job(SplitMemoryJobRow {
                    user_id: user_id.into(),
                    job_id: row.id.clone(),
                    lease_token: lease_token.into(),
                    prefix_count: bounded_count.try_into().map_err(|_| MemoryError::Internal)?,
                    pending_job_id: generate_prefixed_id("memory-job"),
                    now: now_ms(),
                })
                .await
                .map_err(map_db_error)?;
            if !split {
                return Err(MemoryError::LeaseLost);
            }
            *row = jobs
                .memory
                .get_job(user_id, &row.id)
                .await
                .map_err(map_db_error)?
                .ok_or(MemoryError::LeaseLost)?;
        }
        Ok(())
    }

    async fn load_exact_messages(
        &self,
        user_id: &str,
        conversation_id: &str,
        turn_ids: &[String],
    ) -> Result<Vec<MessageRow>, MemoryError> {
        let jobs = self.job_dependencies()?;
        let mut messages = Vec::new();
        for turn_id in turn_ids {
            let mut turn_messages = jobs
                .conversations
                .list_messages_by_turn(user_id, conversation_id, turn_id)
                .await
                .map_err(map_db_error)?;
            if turn_messages.is_empty() {
                return Err(MemoryError::InvalidInput);
            }
            messages.append(&mut turn_messages);
        }
        Ok(messages)
    }

    pub async fn claim_job(
        &self,
        user_id: &str,
        worker_id: &str,
        lease_ms: u64,
    ) -> Result<Option<ClaimedMemoryJob>, MemoryError> {
        valid_worker_id(worker_id)?;
        let lease_duration_ms = valid_lease_ms(lease_ms)?;
        let jobs = self.job_dependencies()?;
        if !jobs.readiness.is_usable().await? {
            jobs.memory.block_jobs(user_id, now_ms()).await.map_err(map_db_error)?;
            return Ok(None);
        }
        let now = now_ms();
        now.checked_add(lease_duration_ms).ok_or(MemoryError::InvalidInput)?;
        jobs.memory.unblock_jobs(user_id, now).await.map_err(map_db_error)?;
        let lease_token = generate_prefixed_id("memory-lease");
        let Some(mut row) = jobs
            .memory
            .claim_next_job(ClaimMemoryJobRow {
                user_id: user_id.into(),
                worker_id: worker_id.into(),
                lease_token: lease_token.clone(),
                now,
                lease_duration_ms,
            })
            .await
            .map_err(map_db_error)?
        else {
            return Ok(None);
        };
        self.bound_claimed_job(user_id, &lease_token, &mut row).await?;
        if !jobs
            .memory
            .validate_lease(user_id, &row.id, &lease_token, now_ms())
            .await
            .map_err(map_db_error)?
        {
            return Err(MemoryError::LeaseLost);
        }
        Ok(Some(ClaimedMemoryJob {
            job: job_response(row)?,
            lease_token,
        }))
    }

    pub async fn renew_job_lease(
        &self,
        user_id: &str,
        job_id: &str,
        worker_id: &str,
        lease_token: &str,
        lease_ms: u64,
    ) -> Result<i64, MemoryError> {
        valid_worker_id(worker_id)?;
        valid_lease_token(lease_token)?;
        let lease_duration_ms = valid_lease_ms(lease_ms)?;
        let jobs = self.job_dependencies()?;
        let now = now_ms();
        let lease_expires_at = now.checked_add(lease_duration_ms).ok_or(MemoryError::InvalidInput)?;
        let renewed = jobs
            .memory
            .renew_lease(RenewMemoryLeaseRow {
                user_id: user_id.into(),
                job_id: job_id.into(),
                worker_id: worker_id.into(),
                lease_token: lease_token.into(),
                now,
                lease_duration_ms,
            })
            .await
            .map_err(map_db_error)?;
        renewed.then_some(lease_expires_at).ok_or(MemoryError::LeaseLost)
    }

    pub async fn release_job(
        &self,
        user_id: &str,
        job_id: &str,
        worker_id: &str,
        lease_token: &str,
    ) -> Result<bool, MemoryError> {
        let jobs = self.job_dependencies()?;
        valid_worker_id(worker_id)?;
        valid_lease_token(lease_token)?;
        let released = jobs
            .memory
            .release_lease(ReleaseMemoryLeaseRow {
                user_id: user_id.into(),
                job_id: job_id.into(),
                worker_id: worker_id.into(),
                lease_token: lease_token.into(),
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
        lease_token: &str,
        failure: NormalizedMemoryJobFailure,
    ) -> Result<MemoryJobResponse, MemoryError> {
        let jobs = self.job_dependencies()?;
        valid_worker_id(worker_id)?;
        valid_lease_token(lease_token)?;
        let current = jobs
            .memory
            .get_job(user_id, job_id)
            .await
            .map_err(map_db_error)?
            .ok_or(MemoryError::NotFound)?;
        let now = now_ms();
        let (state, next_attempt_at, increment_attempt, increment_invalid_output) =
            failure_transition(&failure.code, current.attempt_count, current.invalid_output_count, now);
        let row = jobs
            .memory
            .transition_running_job(TransitionMemoryJobRow {
                user_id: user_id.into(),
                job_id: job_id.into(),
                worker_id: worker_id.into(),
                lease_token: lease_token.into(),
                state: state.into(),
                next_attempt_at,
                error_code: Some(failure_code(&failure.code).into()),
                increment_attempt,
                increment_invalid_output,
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
        lease_token: &str,
    ) -> Result<MemoryUpdateInput, MemoryError> {
        let jobs = self.job_dependencies()?;
        valid_lease_token(lease_token)?;
        let job = jobs
            .memory
            .get_job(user_id, job_id)
            .await
            .map_err(map_db_error)?
            .ok_or(MemoryError::NotFound)?;
        if !jobs
            .memory
            .validate_lease(user_id, job_id, lease_token, now_ms())
            .await
            .map_err(map_db_error)?
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
        let claimed_turn_ids = jobs
            .memory
            .list_job_turns(user_id, job_id, (crate::sanitizer::MAX_EVIDENCE_TURNS + 1) as u32)
            .await
            .map_err(map_db_error)?
            .into_iter()
            .map(|turn| turn.turn_id)
            .collect::<Vec<_>>();
        if i64::try_from(claimed_turn_ids.len()).map_err(|_| MemoryError::Internal)? != job.turn_count {
            return Err(MemoryError::InvalidInput);
        }
        let messages = self
            .load_exact_messages(user_id, &job.conversation_id, &claimed_turn_ids)
            .await?;
        let previous = jobs
            .memory
            .get_conversation_memory(user_id, &job.conversation_id)
            .await
            .map_err(map_db_error)?;
        let previous_summary = previous
            .as_ref()
            .map(|row| serde_json::from_str::<MemorySummary>(&row.summary_json).map_err(|_| MemoryError::Internal))
            .transpose()?;
        if claimed_turn_ids.last() != Some(&job.through_turn_id) || claimed_turn_ids.is_empty() {
            return Err(MemoryError::InvalidInput);
        }
        let unscoped = self.build_evidence(EvidenceBuildRequest {
            conversation: conversation.clone(),
            messages: messages.clone(),
            previous_summary: previous_summary.clone(),
            summary_cursor: job.from_turn_id.clone(),
            claimed_turn_ids: claimed_turn_ids.clone(),
            existing_entries: Vec::new(),
        })?;
        let existing_entries = jobs
            .memory
            .retrieval_candidates(MemoryCandidateQueryRow {
                user_id: user_id.into(),
                project_id: unscoped.conversation.project_id,
                workspace_key: unscoped.conversation.workspace_key,
                limit: MAX_EXISTING_ENTRIES as u32,
            })
            .await
            .map_err(map_db_error)?;
        let input = self.build_evidence(EvidenceBuildRequest {
            conversation,
            messages,
            previous_summary,
            summary_cursor: job.from_turn_id,
            claimed_turn_ids,
            existing_entries,
        })?;
        if !jobs
            .memory
            .validate_lease(user_id, job_id, lease_token, now_ms())
            .await
            .map_err(map_db_error)?
        {
            return Err(MemoryError::LeaseLost);
        }
        Ok(input)
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
        worker_id: &str,
        request: CompleteMemoryJobRequest,
    ) -> Result<(), MemoryError> {
        let jobs = self.job_dependencies()?;
        valid_worker_id(worker_id)?;
        valid_lease_token(&request.lease_token)?;
        let job = jobs
            .memory
            .get_job(user_id, job_id)
            .await
            .map_err(map_db_error)?
            .ok_or(MemoryError::NotFound)?;
        if request.expected_revision != u64::try_from(job.expected_revision).map_err(|_| MemoryError::Internal)? {
            return Err(MemoryError::StaleRevision);
        }
        if job.lease_owner.as_deref() != Some(worker_id) {
            return Err(MemoryError::LeaseLost);
        }
        let evidence = self.load_job_evidence(user_id, job_id, &request.lease_token).await?;
        if request.output.mutations.len() > MAX_MUTATION_COUNT {
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
            let content = strip_user_context_sentences(&sanitize_text(&content));
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
            let fingerprint = structured_hash(&(
                user_id,
                evidence.conversation.project_id.as_deref(),
                evidence.conversation.workspace_key.as_deref(),
                kind,
                stable_key.as_str(),
            ))?;
            entries.push(CommitMemoryEntryRow {
                id: generate_prefixed_id("memory-entry"),
                project_id: evidence.conversation.project_id.clone(),
                workspace_key: evidence.conversation.workspace_key.clone(),
                kind: kind.into(),
                stable_key,
                fingerprint,
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
                lease_owner: worker_id.into(),
                lease_token: request.lease_token,
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

    /// Changes the global capture default and cancels queued/running work when capture is disabled.
    pub async fn set_global_capture_enabled(&self, user_id: &str, enabled: bool) -> Result<(), MemoryError> {
        let jobs = self.job_dependencies()?;
        jobs.memory
            .update_memory_lifecycle(UpdateMemoryLifecycleRow {
                user_id: user_id.into(),
                enabled: None,
                default_capture: Some(enabled),
                now: now_ms(),
            })
            .await
            .map_err(map_db_error)?;
        Ok(())
    }

    /// Enables or disables Memory globally and cancels queued/running work when disabled.
    pub async fn set_memory_enabled(&self, user_id: &str, enabled: bool) -> Result<(), MemoryError> {
        let jobs = self.job_dependencies()?;
        jobs.memory
            .update_memory_lifecycle(UpdateMemoryLifecycleRow {
                user_id: user_id.into(),
                enabled: Some(enabled),
                default_capture: None,
                now: now_ms(),
            })
            .await
            .map_err(map_db_error)?;
        Ok(())
    }

    /// Changes capture for one conversation and cancels its work when capture is disabled.
    pub async fn set_conversation_capture_enabled(
        &self,
        user_id: &str,
        conversation_id: &str,
        enabled: bool,
    ) -> Result<(), MemoryError> {
        let jobs = self.job_dependencies()?;
        jobs.memory
            .update_conversation_memory_lifecycle(UpdateConversationMemoryLifecycleRow {
                user_id: user_id.into(),
                conversation_id: conversation_id.into(),
                capture_enabled: enabled,
                now: now_ms(),
            })
            .await
            .map_err(map_db_error)?;
        Ok(())
    }

    /// Forgets one conversation's durable Memory state and establishes a reset boundary.
    pub async fn forget_conversation(&self, user_id: &str, conversation_id: &str) -> Result<(), MemoryError> {
        self.job_dependencies()?
            .memory
            .delete_conversation_memory(user_id, conversation_id, now_ms())
            .await
            .map_err(map_db_error)
    }

    /// Clears all Memory state for the user and establishes a global reset boundary.
    pub async fn clear_all_memory(&self, user_id: &str) -> Result<(), MemoryError> {
        self.job_dependencies()?
            .memory
            .clear_memory(user_id, now_ms())
            .await
            .map_err(map_db_error)
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
    (lease_ms > 0 && lease_ms <= MAX_LEASE_DURATION_MS as i64)
        .then_some(lease_ms)
        .ok_or(MemoryError::InvalidInput)
}

fn valid_worker_id(worker_id: &str) -> Result<(), MemoryError> {
    (!worker_id.trim().is_empty() && worker_id.len() <= 200)
        .then_some(())
        .ok_or(MemoryError::InvalidInput)
}

fn valid_lease_token(lease_token: &str) -> Result<(), MemoryError> {
    (!lease_token.trim().is_empty() && lease_token.len() <= 200)
        .then_some(())
        .ok_or(MemoryError::InvalidInput)
}

fn messages_for_turns(messages: &[MessageRow], turn_ids: &[String]) -> Vec<MessageRow> {
    let selected = turn_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    messages
        .iter()
        .filter(|message| {
            message
                .turn_id
                .as_deref()
                .is_some_and(|turn_id| selected.contains(turn_id))
        })
        .cloned()
        .collect()
}

#[derive(Serialize)]
struct EvidenceHashInput<'a> {
    operation_version: &'static str,
    sanitizer_version: &'static str,
    summary_cursor: Option<&'a str>,
    turn_ids: &'a [String],
    messages: Vec<CanonicalMessageHashInput<'a>>,
}

#[derive(Serialize)]
struct CanonicalMessageHashInput<'a> {
    id: &'a str,
    message_type: &'a str,
    status: Option<&'a str>,
    hidden: bool,
    position: Option<&'a str>,
    content: &'a str,
}

fn evidence_input_hash(
    summary_cursor: Option<&str>,
    turn_ids: &[String],
    messages: &[MessageRow],
) -> Result<String, MemoryError> {
    let material = EvidenceHashInput {
        operation_version: OPERATION_VERSION,
        sanitizer_version: SANITIZER_VERSION,
        summary_cursor,
        turn_ids,
        messages: messages
            .iter()
            .map(|message| CanonicalMessageHashInput {
                id: &message.id,
                message_type: &message.r#type,
                status: message.status.as_deref(),
                hidden: message.hidden,
                position: message.position.as_deref(),
                content: &message.content,
            })
            .collect(),
    };
    structured_hash(&material)
}

fn structured_hash(value: &impl Serialize) -> Result<String, MemoryError> {
    let material = serde_json::to_vec(value).map_err(|_| MemoryError::Internal)?;
    Ok(Sha256::digest(material)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
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

fn sanitize_output_summary(summary: MemorySummary) -> Result<MemorySummary, MemoryError> {
    let goal = strip_user_context_sentences(&sanitize_text(&summary.goal));
    let sanitize_values = |values: Vec<String>| -> Result<Vec<String>, MemoryError> {
        values
            .into_iter()
            .map(|value| {
                let value = strip_user_context_sentences(&sanitize_text(&value));
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

fn failure_transition(
    code: &MemoryJobFailureCode,
    attempt_count: i64,
    invalid_output_count: i64,
    now: i64,
) -> (&'static str, Option<i64>, bool, bool) {
    match code {
        MemoryJobFailureCode::NotConfigured
        | MemoryJobFailureCode::ModelUnavailable
        | MemoryJobFailureCode::ProviderAuthFailed => ("blocked", None, true, false),
        MemoryJobFailureCode::InvalidInput => ("failed", None, true, false),
        MemoryJobFailureCode::Canceled | MemoryJobFailureCode::QueueFull => ("pending", None, false, false),
        MemoryJobFailureCode::InvalidOutput if invalid_output_count >= 1 => ("failed", None, true, true),
        _ if attempt_count >= RETRY_DELAYS_MS.len() as i64 => ("failed", None, true, false),
        _ => (
            "retry_wait",
            Some(now + RETRY_DELAYS_MS[attempt_count as usize]),
            true,
            matches!(code, MemoryJobFailureCode::InvalidOutput),
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

    use super::{MemoryService, RETRY_DELAYS_MS, failure_transition, sanitize_output_summary};
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

    #[test]
    fn retry_accounting_counts_failures_not_claims_and_invalid_output_has_its_own_limit() {
        let now = 1_000;
        for (attempt_count, delay) in RETRY_DELAYS_MS.into_iter().enumerate() {
            assert_eq!(
                failure_transition(&MemoryJobFailureCode::Timeout, attempt_count as i64, 0, now),
                ("retry_wait", Some(now + delay), true, false),
            );
        }
        assert_eq!(
            failure_transition(&MemoryJobFailureCode::Timeout, 5, 0, now),
            ("failed", None, true, false),
        );
        assert_eq!(
            failure_transition(&MemoryJobFailureCode::QueueFull, 4, 0, now),
            ("pending", None, false, false),
        );
        assert_eq!(
            failure_transition(&MemoryJobFailureCode::InvalidOutput, 0, 0, now),
            ("retry_wait", Some(now + RETRY_DELAYS_MS[0]), true, true),
        );
        assert_eq!(
            failure_transition(&MemoryJobFailureCode::InvalidOutput, 1, 1, now),
            ("failed", None, true, true),
        );
    }

    #[test]
    fn output_summary_removes_user_context_sentences() {
        let summary = sanitize_output_summary(MemorySummary {
            goal: "My name is Ada. Ship the release.".into(),
            current_state: vec!["I prefer concise responses. Tests pass.".into()],
            decisions: Vec::new(),
            artifacts: Vec::new(),
            issues: Vec::new(),
            next_steps: Vec::new(),
            work_constraints: Vec::new(),
        })
        .unwrap();
        assert_eq!(summary.goal.trim(), "Ship the release.");
        assert_eq!(summary.current_state[0].trim(), "Tests pass.");
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
            .load_job_evidence(USER_ID, &claimed.id, &claimed.lease_token)
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
    async fn oversized_backlog_is_split_into_deterministic_exact_turn_batches() {
        let fixture = fixture(true).await;
        for index in 0..35 {
            let turn_id = format!("turn-{index:02}");
            fixture.persist_turn(&turn_id, 10 + index * 10).await;
            fixture
                .service
                .on_turn_completed(USER_ID, "conversation-1", &turn_id, MemoryTurnOutcome::Completed)
                .await;
        }

        let first = fixture
            .service
            .claim_job(USER_ID, "worker-1", 30_000)
            .await
            .unwrap()
            .unwrap();
        let first_evidence = fixture
            .service
            .load_job_evidence(USER_ID, &first.id, &first.lease_token)
            .await
            .unwrap();
        assert_eq!(first_evidence.source_turns.len(), 32);
        fixture
            .service
            .complete_job(USER_ID, &first.id, "worker-1", empty_completion(&first))
            .await
            .unwrap();

        let second = fixture
            .service
            .claim_job(USER_ID, "worker-2", 30_000)
            .await
            .unwrap()
            .unwrap();
        let second_evidence = fixture
            .service
            .load_job_evidence(USER_ID, &second.id, &second.lease_token)
            .await
            .unwrap();
        assert_eq!(second_evidence.source_turns.len(), 3);
        assert_eq!(second_evidence.source_turns[0].turn_id, "turn-32");
        assert_eq!(second.from_turn_id.as_deref(), Some("turn-31"));
        assert_eq!(second.expected_revision, 1);
    }

    #[tokio::test]
    async fn message_and_byte_limits_split_only_at_exact_turn_boundaries() {
        let message_fixture = fixture(true).await;
        for (turn_id, created_at) in [("turn-1", 10), ("turn-2", 1_000)] {
            message_fixture.persist_dense_turn(turn_id, created_at, 65, 8).await;
            message_fixture
                .service
                .on_turn_completed(USER_ID, "conversation-1", turn_id, MemoryTurnOutcome::Completed)
                .await;
        }
        let message_job = message_fixture
            .service
            .claim_job(USER_ID, "worker-messages", 30_000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            message_fixture
                .service
                .load_job_evidence(USER_ID, &message_job.id, &message_job.lease_token)
                .await
                .unwrap()
                .source_turns
                .len(),
            1,
        );
        assert!(
            message_fixture
                .service
                .claim_job(USER_ID, "other-worker", 30_000)
                .await
                .unwrap()
                .is_none(),
            "one running job permits only one pending successor",
        );

        let byte_fixture = fixture(true).await;
        for (turn_id, created_at) in [("turn-1", 10), ("turn-2", 1_000)] {
            byte_fixture.persist_dense_turn(turn_id, created_at, 8, 6 * 1024).await;
            byte_fixture
                .service
                .on_turn_completed(USER_ID, "conversation-1", turn_id, MemoryTurnOutcome::Completed)
                .await;
        }
        let byte_job = byte_fixture
            .service
            .claim_job(USER_ID, "worker-bytes", 30_000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            byte_fixture
                .service
                .load_job_evidence(USER_ID, &byte_job.id, &byte_job.lease_token)
                .await
                .unwrap()
                .source_turns
                .len(),
            1,
        );
    }

    #[tokio::test]
    async fn queue_admission_and_release_do_not_consume_failure_attempts() {
        let fixture = fixture(true).await;
        fixture.persist_turn("turn-1", 10).await;
        fixture
            .service
            .on_turn_completed(USER_ID, "conversation-1", "turn-1", MemoryTurnOutcome::Completed)
            .await;
        let first = fixture
            .service
            .claim_job(USER_ID, "worker-1", 30_000)
            .await
            .unwrap()
            .unwrap();
        let queued = fixture
            .service
            .record_job_failure(
                USER_ID,
                &first.id,
                "worker-1",
                &first.lease_token,
                NormalizedMemoryJobFailure {
                    code: MemoryJobFailureCode::QueueFull,
                    message: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(queued.attempt_count, 0);
        let second = fixture
            .service
            .claim_job(USER_ID, "worker-2", 30_000)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first.lease_token, second.lease_token);
        let failed = fixture
            .service
            .record_job_failure(
                USER_ID,
                &second.id,
                "worker-2",
                &second.lease_token,
                NormalizedMemoryJobFailure {
                    code: MemoryJobFailureCode::InvalidInput,
                    message: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(failed.attempt_count, 1);
        assert_eq!(failed.state, MemoryJobState::Failed);
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
                .renew_job_lease(USER_ID, &running.id, "other", &running.lease_token, 30_000)
                .await,
            Err(MemoryError::LeaseLost),
        );
        assert!(
            fixture
                .service
                .renew_job_lease(USER_ID, &running.id, "worker-1", &running.lease_token, 30_000)
                .await
                .unwrap()
                > 0
        );
        assert_eq!(
            fixture
                .service
                .release_job(USER_ID, &running.id, "other", &running.lease_token)
                .await,
            Err(MemoryError::LeaseLost),
        );

        fixture
            .service
            .record_job_failure(
                USER_ID,
                &running.id,
                "worker-1",
                &running.lease_token,
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
            .load_job_evidence(USER_ID, &job.id, &job.lease_token)
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
                    lease_token: job.lease_token.clone(),
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
                &job.lease_token,
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
    async fn unusable_readiness_moves_pending_work_to_blocked() {
        let fixture = fixture(true).await;
        fixture.persist_turn("turn-1", 10).await;
        fixture
            .service
            .on_turn_completed(USER_ID, "conversation-1", "turn-1", MemoryTurnOutcome::Completed)
            .await;
        let known = fixture
            .service
            .claim_job(USER_ID, "worker-1", 30_000)
            .await
            .unwrap()
            .unwrap();
        fixture
            .service
            .release_job(USER_ID, &known.id, "worker-1", &known.lease_token)
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
        assert_eq!(
            fixture.memory.get_job(USER_ID, &known.id).await.unwrap().unwrap().state,
            "blocked",
        );
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
            .claim_job(USER_ID, "worker-1", 50)
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;

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
        fixture
            .service
            .release_job(USER_ID, &job.id, "worker-1", &job.lease_token)
            .await
            .unwrap();
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

        fixture
            .service
            .set_global_capture_enabled(USER_ID, false)
            .await
            .unwrap();
        fixture.persist_turn("turn-global-capture-disabled", 30).await;
        fixture
            .service
            .on_turn_completed(
                USER_ID,
                "conversation-1",
                "turn-global-capture-disabled",
                MemoryTurnOutcome::Completed,
            )
            .await;
        assert!(
            fixture
                .service
                .claim_job(USER_ID, "worker-global", 30_000)
                .await
                .unwrap()
                .is_none()
        );
        fixture.service.set_global_capture_enabled(USER_ID, true).await.unwrap();

        fixture.service.set_memory_enabled(USER_ID, false).await.unwrap();
        fixture.persist_turn("turn-memory-disabled", 35).await;
        fixture
            .service
            .on_turn_completed(
                USER_ID,
                "conversation-1",
                "turn-memory-disabled",
                MemoryTurnOutcome::Completed,
            )
            .await;
        assert!(
            fixture
                .service
                .claim_job(USER_ID, "worker-memory-disabled", 30_000)
                .await
                .unwrap()
                .is_none()
        );
        fixture.service.set_memory_enabled(USER_ID, true).await.unwrap();

        fixture
            .service
            .set_conversation_capture_enabled(USER_ID, "conversation-1", false)
            .await
            .unwrap();
        fixture.persist_turn("turn-disabled", 40).await;
        fixture
            .service
            .on_turn_completed(USER_ID, "conversation-1", "turn-disabled", MemoryTurnOutcome::Completed)
            .await;
        assert!(
            fixture
                .service
                .claim_job(USER_ID, "worker-disabled", 30_000)
                .await
                .unwrap()
                .is_none()
        );

        fixture
            .service
            .set_conversation_capture_enabled(USER_ID, "conversation-1", true)
            .await
            .unwrap();
        fixture
            .service
            .forget_conversation(USER_ID, "conversation-1")
            .await
            .unwrap();
        fixture
            .service
            .on_turn_completed(USER_ID, "conversation-1", "turn-disabled", MemoryTurnOutcome::Completed)
            .await;
        assert!(
            fixture
                .service
                .claim_job(USER_ID, "worker-forgotten", 30_000)
                .await
                .unwrap()
                .is_none()
        );

        fixture.service.clear_all_memory(USER_ID).await.unwrap();
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

        async fn persist_dense_turn(&self, turn_id: &str, created_at: i64, message_count: usize, content_bytes: usize) {
            for index in 0..message_count {
                let position = if index % 2 == 0 { "right" } else { "left" };
                let content = format!(
                    "{}{}",
                    if position == "right" { "Work " } else { "Done " },
                    "x".repeat(content_bytes)
                );
                self.conversations
                    .insert_message(&message(
                        &format!("{turn_id}-{index}"),
                        turn_id,
                        position,
                        &content,
                        created_at + index as i64,
                    ))
                    .await
                    .unwrap();
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

    fn empty_completion(job: &crate::ClaimedMemoryJob) -> CompleteMemoryJobRequest {
        CompleteMemoryJobRequest {
            lease_token: job.lease_token.clone(),
            expected_revision: job.expected_revision,
            output: MemoryUpdateOutput {
                summary: MemorySummary {
                    goal: "Process durable work".into(),
                    current_state: Vec::new(),
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
