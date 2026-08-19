//! Agent-session operations on ConversationService.
//!
//! These forward to the active AgentInstance (via `self.task(id)`) for
//! config-options/usage/slash-commands/side-question queries, plus workspace
//! browsing that needs the conversations.extra.workspace field.
//!
//! Kept in a separate file from service.rs to avoid pushing that file
//! over 2000 lines.

use std::path::Component;

use aionui_ai_agent::{AcpError, AgentError};
use aionui_api_types::{
    CanonicalReplayProjectionResponse, ConfigOptionConfirmation, GetConfigOptionsResponse, HostPolicyResponse,
    InputChangedEvent, JournalTranscriptResponse, RETIRED_DEEPSEEK_HARNESS_BACKEND, RetainedOutputResponse,
    SetConfigOptionRequest, SetConfigOptionResponse, SetHostPolicyRequest, SideQuestionRequest, SideQuestionResponse,
    SlashCommandItem, SubmitConversationInputRequest, ToolEnforcementLevel, WorkspaceBrowseQuery, WorkspaceEntry,
};
use aionui_api_types::{
    ConversationCapabilities, ConversationInputMode, ConversationInputReceipt, ConversationInputResponse,
    ConversationInputStatus, SendMessageRequest, WebSocketMessage,
};
use aionui_common::{AgentKillReason, ErrorChain, now_ms};
use aionui_db::models::ConversationInputRow;
use aionui_db::{ConversationInputInsert, ConversationInputUpdate};
use sha2::{Digest, Sha256};

use tracing::warn;

use crate::ConversationError;
use crate::journal_transcript::{RequestedVisibility, derive_transcript};
use crate::service::{
    AssistantRuntimePreferenceUpdate, ConversationService, reject_deprecated_runtime_row, team_id_from_extra,
};

const MAX_DIR_DEPTH: usize = 10;

#[derive(Clone, Copy)]
struct InputStatusChange<'a> {
    status: ConversationInputStatus,
    turn_id: Option<&'a str>,
    msg_id: Option<&'a str>,
    error_code: Option<&'a str>,
}

fn input_mode_name(mode: ConversationInputMode) -> &'static str {
    match mode {
        ConversationInputMode::Followup => "followup",
        ConversationInputMode::Steer => "steer",
        ConversationInputMode::Inject => "inject",
    }
}

fn input_status_name(status: ConversationInputStatus) -> &'static str {
    match status {
        ConversationInputStatus::Held => "held",
        ConversationInputStatus::Dispatching => "dispatching",
        ConversationInputStatus::Accepted => "accepted",
        ConversationInputStatus::Applied => "applied",
        ConversationInputStatus::Canceled => "canceled",
        ConversationInputStatus::Failed => "failed",
    }
}

pub(crate) fn input_row_response(row: ConversationInputRow) -> Result<ConversationInputResponse, ConversationError> {
    let mode = match row.mode.as_str() {
        "followup" => ConversationInputMode::Followup,
        "steer" => ConversationInputMode::Steer,
        "inject" => ConversationInputMode::Inject,
        value => {
            return Err(ConversationError::internal(format!(
                "Invalid conversation input mode '{value}'"
            )));
        }
    };
    let status = match row.status.as_str() {
        "held" => ConversationInputStatus::Held,
        "dispatching" => ConversationInputStatus::Dispatching,
        "accepted" => ConversationInputStatus::Accepted,
        "applied" => ConversationInputStatus::Applied,
        "canceled" => ConversationInputStatus::Canceled,
        "failed" => ConversationInputStatus::Failed,
        value => {
            return Err(ConversationError::internal(format!(
                "Invalid conversation input status '{value}'"
            )));
        }
    };
    Ok(ConversationInputResponse {
        input_id: row.id,
        conversation_id: row.conversation_id,
        mode,
        status,
        content: row.content,
        files: serde_json::from_str(&row.files)
            .map_err(|error| ConversationError::internal(format!("Invalid input files projection: {error}")))?,
        inject_skills: serde_json::from_str(&row.inject_skills)
            .map_err(|error| ConversationError::internal(format!("Invalid input skills projection: {error}")))?,
        hidden: row.hidden,
        client_key: row.client_key,
        turn_id: row.turn_id,
        msg_id: row.msg_id,
        error_code: row.error_code,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

impl ConversationService {
    pub async fn replay_event_projection(
        &self,
        user_id: &str,
        conversation_id: &str,
        expected_sha256: Option<&str>,
    ) -> Result<CanonicalReplayProjectionResponse, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        if let Some(expected) = expected_sha256
            && (expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(ConversationError::BadRequest {
                reason: "expected_sha256 must be a 64-character hexadecimal digest".to_owned(),
            });
        }

        let projection = self
            .canonical_event_journal()
            .replay_projection(user_id, conversation_id)
            .await
            .map_err(|error| ConversationError::internal(format!("Failed to replay canonical events: {error}")))?;
        if expected_sha256.is_some_and(|expected| !projection.journal_sha256.eq_ignore_ascii_case(expected)) {
            return Err(ConversationError::Busy {
                reason: "canonical event projection does not match the expected digest".to_owned(),
            });
        }

        Ok(CanonicalReplayProjectionResponse {
            schema_version: projection.schema_version,
            conversation_id: projection.conversation_id,
            event_count: projection.event_count,
            last_sequence: projection.last_sequence,
            last_event_id: projection.last_event_id,
            kind_counts: projection.kind_counts,
            journal_sha256: projection.journal_sha256,
        })
    }

    pub async fn derive_event_transcript(
        &self,
        user_id: &str,
        conversation_id: &str,
        visibility: Option<&str>,
    ) -> Result<JournalTranscriptResponse, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        let requested =
            RequestedVisibility::parse(visibility).map_err(|reason| ConversationError::BadRequest { reason })?;
        let journal = self.canonical_event_journal();
        let events = journal
            .replay(user_id, conversation_id)
            .await
            .map_err(|error| ConversationError::internal(format!("Failed to replay canonical events: {error}")))?;
        let projection = journal
            .replay_projection(user_id, conversation_id)
            .await
            .map_err(|error| ConversationError::internal(format!("Failed to project canonical events: {error}")))?;
        let model_surface_reconstructible = match crate::model_visible::check_model_surface_reconstructible(&events) {
            Ok(()) => true,
            Err(violation) => {
                warn!(
                    conversation_id,
                    error = %violation,
                    "Model-visible invariant violated while deriving transcript"
                );
                false
            }
        };
        let transcript = derive_transcript(conversation_id, &events, requested);
        Ok(JournalTranscriptResponse {
            schema_version: transcript.schema_version,
            conversation_id: transcript.conversation_id,
            visibility: transcript.visibility.to_owned(),
            items: transcript
                .items
                .into_iter()
                .map(|item| aionui_api_types::JournalTranscriptItem {
                    sequence: item.sequence,
                    event_id: item.event_id,
                    journal_kind: item.journal_kind,
                    transcript_kind: item.transcript_kind.to_owned(),
                    visibility: item.visibility.to_owned(),
                    summary: item.summary,
                    content: item.content,
                    compacted: item.compacted,
                    source_sequences: item.source_sequences,
                })
                .collect(),
            model_visible_count: transcript.model_visible_count,
            model_visible_sha256: transcript.model_visible_sha256,
            journal_sha256: projection.journal_sha256,
            compaction_lock: transcript.compaction_lock.as_str().to_owned(),
            tokens: aionui_api_types::JournalTranscriptTokens {
                log_revision: transcript.tokens.log_revision,
                surface_tokens: transcript.tokens.surface_tokens,
                nodes: transcript
                    .tokens
                    .nodes
                    .into_iter()
                    .map(|node| aionui_api_types::JournalTranscriptTokenNode {
                        sequence: node.sequence,
                        tokens: node.tokens,
                    })
                    .collect(),
            },
            tool_pairing_balanced: transcript.tool_pairing_balanced,
            model_surface_reconstructible,
            approval_policy: transcript.approval_policy.to_owned(),
            compaction_keep_n: transcript.compaction_keep_n as u32,
        })
    }

    pub async fn set_host_policy(
        &self,
        user_id: &str,
        conversation_id: &str,
        req: SetHostPolicyRequest,
    ) -> Result<HostPolicyResponse, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        if req.approval.is_none() && req.compaction_keep_n.is_none() {
            return Err(ConversationError::BadRequest {
                reason: "approval or compaction_keep_n is required".to_owned(),
            });
        }

        let journal = self.canonical_event_journal();
        let events = journal
            .replay(user_id, conversation_id)
            .await
            .map_err(|error| ConversationError::internal(format!("Failed to replay canonical events: {error}")))?;
        let current_approval = crate::approval_audit::fold_approval_policy(&events);
        let current_keep_n = crate::journal_compaction::fold_compaction_keep_n(&events);

        let next_approval = match req.approval.as_deref() {
            Some(value) => aionui_ai_agent::shared_kernel::ApprovalPolicy::parse(value).ok_or_else(|| {
                ConversationError::BadRequest {
                    reason: format!("unsupported approval policy '{value}'"),
                }
            })?,
            None => current_approval,
        };
        let next_keep_n = match req.compaction_keep_n {
            Some(value) => crate::journal_compaction::parse_keep_n(u64::from(value)).ok_or_else(|| {
                ConversationError::BadRequest {
                    reason: format!("compaction_keep_n must be between 1 and 20, got {value}"),
                }
            })?,
            None => current_keep_n,
        };

        if next_approval != current_approval {
            crate::approval_audit::append_approval_policy(&journal, user_id, conversation_id, next_approval)
                .await
                .map_err(|error| ConversationError::internal(format!("Failed to journal approval policy: {error}")))?;
        }
        if next_keep_n != current_keep_n {
            crate::journal_compaction::append_compaction_policy(&journal, user_id, conversation_id, next_keep_n)
                .await
                .map_err(|error| {
                    ConversationError::internal(format!("Failed to journal compaction policy: {error}"))
                })?;
        }

        self.update_extra(
            user_id,
            conversation_id,
            serde_json::json!({
                "host_policy": {
                    "approval": next_approval.as_str(),
                    "compaction_keep_n": next_keep_n as u64
                }
            }),
        )
        .await?;

        Ok(HostPolicyResponse {
            approval: next_approval.as_str().to_owned(),
            compaction_keep_n: next_keep_n as u32,
        })
    }

    pub async fn read_retained_output(
        &self,
        user_id: &str,
        conversation_id: &str,
        reference: &str,
    ) -> Result<RetainedOutputResponse, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        let (sha256, content) = self
            .output_retention_policy()
            .read(user_id, conversation_id, reference)
            .await
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => ConversationError::NotFound {
                    id: reference.to_owned(),
                },
                std::io::ErrorKind::PermissionDenied => ConversationError::BadRequest {
                    reason: "invalid retained output reference".to_owned(),
                },
                _ => ConversationError::internal(format!("Failed to read retained output: {error}")),
            })?;
        Ok(RetainedOutputResponse {
            reference: reference.to_owned(),
            sha256,
            size: content.len() as u64,
            content,
        })
    }

    // ── Config Options ──────────────────────────────────────────────

    pub async fn get_config_options(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<GetConfigOptionsResponse, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        self.task(conversation_id)?
            .get_config_options()
            .await
            .map_err(ConversationError::from)
    }

    pub async fn set_config_option(
        &self,
        user_id: &str,
        conversation_id: &str,
        option_id: &str,
        req: SetConfigOptionRequest,
    ) -> Result<SetConfigOptionResponse, ConversationError> {
        if option_id.trim().is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "option_id must not be empty".into(),
            });
        }
        if req.value.trim().is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "value must not be empty".into(),
            });
        }
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        let agent = self.task(conversation_id)?;
        if agent.backend() == Some(RETIRED_DEEPSEEK_HARNESS_BACKEND) {
            return Err(ConversationError::RuntimeRetired {
                backend: RETIRED_DEEPSEEK_HARNESS_BACKEND.to_owned(),
            });
        }
        let response = {
            match agent.set_config_option(option_id, &req.value).await {
                Ok(response) => response,
                Err(err @ AgentError::Acp(AcpError::NotConnected)) => {
                    warn!(
                        conversation_id,
                        option_id,
                        reason = ?AgentKillReason::AgentErrorRecovery,
                        error = %ErrorChain(&err),
                        "ACP config option failed because protocol is disconnected; evicting task"
                    );
                    self.task_manager()
                        .kill_and_wait(conversation_id, Some(AgentKillReason::AgentErrorRecovery))
                        .await;
                    return Err(ConversationError::from(err));
                }
                Err(err) => return Err(ConversationError::from(err)),
            }
        };

        // Mirror runtime model/mode/thought-level switches into the persisted assistant
        // snapshot + preference so the next conversation seeded from this
        // assistant in `auto` mode reflects the latest pick.
        //
        // `PendingNextTurn` counts: it means the agent WILL apply the value from the next
        // turn, so it is the user's settled choice, merely not in force yet. codex reports
        // every mode switch that way (its schema documents `permissions` as "for
        // subsequent turns"), so excluding it would strip preference memory from every
        // codex conversation. `CommandAck` still does NOT count — it means nothing could
        // be established either way. Persistence failures are logged but do not roll back
        // the user-facing config switch.
        if matches!(
            response.confirmation,
            ConfigOptionConfirmation::Observed | ConfigOptionConfirmation::PendingNextTurn
        ) {
            let category = response
                .config_options
                .as_ref()
                .and_then(|options| options.iter().find(|option| option.id == option_id))
                .and_then(|option| option.category.as_deref())
                .unwrap_or(option_id);
            let updates = match category {
                "model" => Some(AssistantRuntimePreferenceUpdate {
                    model: Some(req.value.as_str()),
                    permission: None,
                    thought_level: None,
                }),
                "mode" => Some(AssistantRuntimePreferenceUpdate {
                    model: None,
                    permission: Some(req.value.as_str()),
                    thought_level: None,
                }),
                "thought_level" | "reasoning_effort" => Some(AssistantRuntimePreferenceUpdate {
                    model: None,
                    permission: None,
                    thought_level: Some(req.value.as_str()),
                }),
                _ => None,
            };
            if let Some(updates) = updates {
                if let Err(err) = self
                    .persist_runtime_assistant_snapshot(user_id, conversation_id, updates)
                    .await
                {
                    warn!(
                        conversation_id,
                        option_id,
                        error = %ErrorChain(&err),
                        "Failed to persist runtime assistant snapshot after set_config_option",
                    );
                }
                if let Err(err) = self
                    .persist_runtime_assistant_preferences(user_id, conversation_id, updates)
                    .await
                {
                    warn!(
                        conversation_id,
                        option_id,
                        error = %ErrorChain(&err),
                        "Failed to persist runtime assistant preferences after set_config_option",
                    );
                }
            }
        }

        Ok(response)
    }

    // ── Usage / Slash commands ──────────────────────────────────────

    pub async fn list_usage_events(
        &self,
        user_id: &str,
        since: Option<i64>,
        limit: Option<i64>,
    ) -> Result<aionui_api_types::UsageListResponse, ConversationError> {
        let Some(repo) = self.usage_event_repo() else {
            return Ok(aionui_api_types::UsageListResponse { events: Vec::new() });
        };
        crate::usage_ledger::list_usage_events(repo.as_ref(), user_id, since, limit).await
    }

    pub async fn clear_usage_events(&self, user_id: &str) -> Result<u64, ConversationError> {
        let Some(repo) = self.usage_event_repo() else {
            return Ok(0);
        };
        crate::usage_ledger::clear_usage_events(repo.as_ref(), user_id).await
    }

    pub async fn get_usage(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<serde_json::Value>, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        // A reaped task must NOT mean "no usage". The indicator's whole point is
        // to survive switching away and back, and the snapshot it needs is
        // already durable in `acp_session.session_config.runtime.context_usage`
        // — `SessionAgentTask::get_usage` reads it from there too. Requiring a
        // live task here made the figure vanish exactly when the user returned
        // to an idle conversation.
        if let Ok(task) = self.task(conversation_id) {
            return task.get_usage().await.map_err(ConversationError::from);
        }
        let state = self
            .acp_session_repo()
            .load_runtime_state_for_user(user_id, conversation_id)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to load usage state: {e}")))?;
        Ok(state
            .and_then(|s| s.context_usage_json)
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok()))
    }

    pub async fn get_slash_commands(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<SlashCommandItem>, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        self.task(conversation_id)?
            .get_slash_commands()
            .await
            .map_err(ConversationError::from)
    }

    // ── Side question ───────────────────────────────────────────────

    pub async fn handle_side_question(
        &self,
        user_id: &str,
        conversation_id: &str,
        req: SideQuestionRequest,
    ) -> Result<SideQuestionResponse, ConversationError> {
        self.ensure_owned_conversation(user_id, conversation_id).await?;
        // `AgentInstance::handle_side_question` already validates that the
        // question is non-empty; no need to duplicate the check here.
        self.task(conversation_id)?
            .handle_side_question(req)
            .await
            .map_err(ConversationError::from)
    }

    pub async fn conversation_capabilities(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<ConversationCapabilities, ConversationError> {
        let row = self
            .conversation_repo()
            .get(user_id, conversation_id)
            .await?
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;
        Ok(ConversationCapabilities {
            followup: true,
            steer: false,
            inject: row.r#type == "aionrs",
            tool_enforcement: if row.r#type == "aionrs" {
                ToolEnforcementLevel::Native
            } else {
                ToolEnforcementLevel::ObserveOnly
            },
        })
    }

    pub async fn submit_input(
        &self,
        user_id: &str,
        conversation_id: &str,
        req: SubmitConversationInputRequest,
    ) -> Result<ConversationInputReceipt, ConversationError> {
        if req.content.trim().is_empty() {
            return Err(ConversationError::bad_request("Input content must not be empty"));
        }
        if req.client_key.trim().is_empty() || req.client_key.len() > 256 {
            return Err(ConversationError::bad_request(
                "client_key must contain between 1 and 256 bytes",
            ));
        }
        let row = self
            .conversation_repo()
            .get(user_id, conversation_id)
            .await?
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;
        if team_id_from_extra(&row.extra).is_some() {
            return Err(ConversationError::Forbidden {
                reason: "Team-owned conversations must be sent through Team API".into(),
            });
        }
        reject_deprecated_runtime_row(&row)?;
        let capabilities = self.conversation_capabilities(user_id, conversation_id).await?;
        let supported = match req.mode {
            ConversationInputMode::Followup => capabilities.followup,
            ConversationInputMode::Steer => capabilities.steer,
            ConversationInputMode::Inject => capabilities.inject,
        };
        if !supported {
            return Err(ConversationError::CapabilityUnsupported {
                capability: input_mode_name(req.mode).to_owned(),
            });
        }

        let input_id = {
            let digest = Sha256::digest(format!("{user_id}\0{conversation_id}\0{}", req.client_key.trim()).as_bytes());
            format!("input_{}", &hex::encode(digest)[..24])
        };
        let created_at = now_ms();
        let files = serde_json::to_string(&req.files)
            .map_err(|error| ConversationError::internal(format!("Failed to encode input files: {error}")))?;
        let inject_skills = serde_json::to_string(&req.inject_skills)
            .map_err(|error| ConversationError::internal(format!("Failed to encode input skills: {error}")))?;

        let lock = self.input_queue_lock(conversation_id);
        {
            let _guard = lock.lock().await;
            if let Some(existing) = self
                .conversation_repo()
                .get_conversation_input(user_id, conversation_id, &input_id)
                .await?
            {
                if existing.client_key != req.client_key.trim()
                    || existing.content != req.content
                    || existing.mode != input_mode_name(req.mode)
                    || existing.files != files
                    || existing.inject_skills != inject_skills
                    || existing.hidden != req.hidden
                {
                    return Err(ConversationError::bad_request(
                        "client_key was already used for different conversation input",
                    ));
                }
                if existing.status == "failed" || existing.status == "canceled" {
                    self.persist_input_status(
                        user_id,
                        conversation_id,
                        &input_id,
                        InputStatusChange {
                            status: ConversationInputStatus::Held,
                            turn_id: None,
                            msg_id: None,
                            error_code: None,
                        },
                    )
                    .await?;
                }
            } else {
                let payload = serde_json::json!({
                    "type": "conversation_input",
                    "visibility": "host",
                    "data": {
                        "input_id": input_id,
                        "mode": input_mode_name(req.mode),
                        "status": "held",
                        "content": req.content,
                        "files": req.files,
                        "inject_skills": req.inject_skills,
                        "hidden": req.hidden,
                        "client_key": req.client_key,
                    }
                });
                let event_id = crate::stream_persistence::canonical_event_id(
                    &format!("conversation_input:{input_id}:held"),
                    &payload,
                );
                self.canonical_event_journal()
                    .append(user_id, conversation_id, event_id, "InputHeld".into(), payload)
                    .await
                    .map_err(|error| {
                        ConversationError::internal(format!("Failed to journal conversation input: {error}"))
                    })?;
                let row = self
                    .conversation_repo()
                    .insert_conversation_input(&ConversationInputInsert {
                        id: &input_id,
                        user_id,
                        conversation_id,
                        mode: input_mode_name(req.mode),
                        status: "held",
                        content: &req.content,
                        files: &files,
                        inject_skills: &inject_skills,
                        hidden: req.hidden,
                        client_key: req.client_key.trim(),
                        created_at,
                    })
                    .await?;
                self.broadcast_input_changed(user_id, input_row_response(row)?);
            }
        }

        self.dispatch_next_held_input(user_id, conversation_id).await;
        let input = self
            .conversation_repo()
            .get_conversation_input(user_id, conversation_id, &input_id)
            .await?
            .ok_or_else(|| ConversationError::internal("Input projection disappeared after submission"))?;
        Ok(ConversationInputReceipt {
            input: input_row_response(input)?,
            runtime: self.runtime_summary_for(conversation_id).await,
            capabilities,
        })
    }

    pub async fn list_inputs(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<ConversationInputResponse>, ConversationError> {
        self.conversation_repo()
            .list_conversation_inputs(user_id, conversation_id)
            .await?
            .into_iter()
            .map(input_row_response)
            .collect()
    }

    pub async fn cancel_input(
        &self,
        user_id: &str,
        conversation_id: &str,
        input_id: &str,
    ) -> Result<ConversationInputResponse, ConversationError> {
        let lock = self.input_queue_lock(conversation_id);
        let _guard = lock.lock().await;
        let current = self
            .conversation_repo()
            .get_conversation_input(user_id, conversation_id, input_id)
            .await?
            .ok_or_else(|| ConversationError::NotFoundReason {
                reason: format!("Conversation input '{input_id}' not found"),
            })?;
        let current = input_row_response(current)?;
        if current.status.is_terminal() {
            return Err(ConversationError::Busy {
                reason: "conversation input is already terminal".into(),
            });
        }
        let changed = self
            .persist_input_status(
                user_id,
                conversation_id,
                input_id,
                InputStatusChange {
                    status: ConversationInputStatus::Canceled,
                    turn_id: None,
                    msg_id: None,
                    error_code: None,
                },
            )
            .await?;
        Ok(changed)
    }

    pub(crate) async fn dispatch_next_held_input(&self, user_id: &str, conversation_id: &str) {
        let lock = self.input_queue_lock(conversation_id);
        let _guard = lock.lock().await;
        let runtime = self.runtime_summary_for(conversation_id).await;
        let rows = match self
            .conversation_repo()
            .list_conversation_inputs(user_id, conversation_id)
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                warn!(conversation_id, error = %error, "Failed to read durable input queue");
                return;
            }
        };
        let Some(next) = rows.into_iter().find(|row| row.status == "held") else {
            return;
        };
        let can_dispatch = match next.mode.as_str() {
            "followup" => runtime.can_send_message,
            "inject" | "steer" => matches!(
                runtime.state,
                aionui_api_types::ConversationRuntimeStateKind::Running
                    | aionui_api_types::ConversationRuntimeStateKind::WaitingConfirmation
            ),
            _ => true,
        };
        if !can_dispatch && next.mode == "followup" {
            return;
        }
        if let Err(error) = self
            .append_input_status_event(
                user_id,
                conversation_id,
                &next.id,
                InputStatusChange {
                    status: ConversationInputStatus::Dispatching,
                    turn_id: None,
                    msg_id: None,
                    error_code: None,
                },
            )
            .await
        {
            warn!(conversation_id, input_id = next.id, error = %error, "Failed to journal input dispatch");
            return;
        }
        let claimed = match self
            .conversation_repo()
            .claim_next_conversation_input(user_id, conversation_id, now_ms())
            .await
        {
            Ok(Some(row)) if row.id == next.id => row,
            Ok(_) => return,
            Err(error) => {
                warn!(conversation_id, input_id = next.id, error = %error, "Failed to claim queued input");
                return;
            }
        };
        let claimed_response = match input_row_response(claimed.clone()) {
            Ok(response) => response,
            Err(error) => {
                warn!(conversation_id, input_id = next.id, error = %error, "Invalid queued input projection");
                return;
            }
        };
        self.broadcast_input_changed(user_id, claimed_response.clone());
        if claimed_response.mode == ConversationInputMode::Inject {
            let active_turn_id = runtime.turn_id.as_deref();
            let result = self.task(conversation_id).and_then(|task| {
                task.inject(next.id.clone(), claimed_response.content.clone())
                    .map_err(ConversationError::from)
            });
            match result {
                Ok(()) => {
                    if let Err(error) = self
                        .persist_input_status(
                            user_id,
                            conversation_id,
                            &next.id,
                            InputStatusChange {
                                status: ConversationInputStatus::Accepted,
                                turn_id: active_turn_id,
                                msg_id: None,
                                error_code: None,
                            },
                        )
                        .await
                    {
                        warn!(conversation_id, input_id = next.id, error = %error, "Failed to project accepted injection");
                    }
                }
                Err(error) => {
                    let error_code = if matches!(&error, ConversationError::Busy { reason } if reason == "too_late") {
                        "too_late"
                    } else {
                        error.error_code()
                    };
                    if let Err(persist_error) = self
                        .persist_input_status(
                            user_id,
                            conversation_id,
                            &next.id,
                            InputStatusChange {
                                status: ConversationInputStatus::Failed,
                                turn_id: active_turn_id,
                                msg_id: None,
                                error_code: Some(error_code),
                            },
                        )
                        .await
                    {
                        warn!(conversation_id, input_id = next.id, error = %persist_error, "Failed to project rejected injection");
                    }
                }
            }
            return;
        }
        if claimed_response.mode == ConversationInputMode::Steer {
            if let Err(error) = self
                .persist_input_status(
                    user_id,
                    conversation_id,
                    &next.id,
                    InputStatusChange {
                        status: ConversationInputStatus::Failed,
                        turn_id: runtime.turn_id.as_deref(),
                        msg_id: None,
                        error_code: Some("capability_unsupported"),
                    },
                )
                .await
            {
                warn!(conversation_id, input_id = next.id, error = %error, "Failed to reject unsupported steer");
            }
            return;
        }
        let request = SendMessageRequest {
            content: claimed_response.content,
            files: claimed_response.files,
            inject_skills: claimed_response.inject_skills,
            hidden: claimed_response.hidden,
        };
        match self
            .send_message(user_id, conversation_id, request, self.task_manager())
            .await
        {
            Ok(response) => {
                if let Err(error) = self
                    .persist_input_status(
                        user_id,
                        conversation_id,
                        &next.id,
                        InputStatusChange {
                            status: ConversationInputStatus::Accepted,
                            turn_id: Some(&response.turn_id),
                            msg_id: Some(&response.msg_id),
                            error_code: None,
                        },
                    )
                    .await
                {
                    warn!(conversation_id, input_id = next.id, error = %error, "Failed to project accepted input");
                    return;
                }
                if let Err(error) = self
                    .persist_input_status(
                        user_id,
                        conversation_id,
                        &next.id,
                        InputStatusChange {
                            status: ConversationInputStatus::Applied,
                            turn_id: Some(&response.turn_id),
                            msg_id: Some(&response.msg_id),
                            error_code: None,
                        },
                    )
                    .await
                {
                    warn!(conversation_id, input_id = next.id, error = %error, "Failed to project applied input");
                }
            }
            Err(ConversationError::Busy { .. }) => {
                if let Err(error) = self
                    .persist_input_status(
                        user_id,
                        conversation_id,
                        &next.id,
                        InputStatusChange {
                            status: ConversationInputStatus::Held,
                            turn_id: None,
                            msg_id: None,
                            error_code: None,
                        },
                    )
                    .await
                {
                    warn!(conversation_id, input_id = next.id, error = %error, "Failed to requeue raced input");
                }
            }
            Err(error) => {
                let error_code = error.error_code();
                if let Err(persist_error) = self
                    .persist_input_status(
                        user_id,
                        conversation_id,
                        &next.id,
                        InputStatusChange {
                            status: ConversationInputStatus::Failed,
                            turn_id: None,
                            msg_id: None,
                            error_code: Some(error_code),
                        },
                    )
                    .await
                {
                    warn!(conversation_id, input_id = next.id, error = %persist_error, "Failed to project failed input");
                }
            }
        }
    }

    async fn persist_input_status(
        &self,
        user_id: &str,
        conversation_id: &str,
        input_id: &str,
        change: InputStatusChange<'_>,
    ) -> Result<ConversationInputResponse, ConversationError> {
        self.append_input_status_event(user_id, conversation_id, input_id, change)
            .await?;
        let row = self
            .conversation_repo()
            .update_conversation_input(
                user_id,
                conversation_id,
                input_id,
                &ConversationInputUpdate {
                    status: Some(input_status_name(change.status)),
                    turn_id: change.turn_id,
                    msg_id: change.msg_id,
                    error_code: change.error_code,
                    updated_at: now_ms(),
                },
            )
            .await?
            .ok_or_else(|| ConversationError::NotFoundReason {
                reason: format!("Conversation input '{input_id}' not found"),
            })?;
        let response = input_row_response(row)?;
        self.broadcast_input_changed(user_id, response.clone());
        Ok(response)
    }

    pub(crate) async fn recover_input_status(
        &self,
        user_id: &str,
        conversation_id: &str,
        input_id: &str,
        status: ConversationInputStatus,
        error_code: Option<&str>,
    ) -> Result<ConversationInputResponse, ConversationError> {
        self.persist_input_status(
            user_id,
            conversation_id,
            input_id,
            InputStatusChange {
                status,
                turn_id: None,
                msg_id: None,
                error_code,
            },
        )
        .await
    }

    async fn append_input_status_event(
        &self,
        user_id: &str,
        conversation_id: &str,
        input_id: &str,
        change: InputStatusChange<'_>,
    ) -> Result<(), ConversationError> {
        append_input_status_event(
            &self.canonical_event_journal(),
            user_id,
            conversation_id,
            input_id,
            change.status,
            change.turn_id,
            change.msg_id,
            change.error_code,
        )
        .await
    }

    fn broadcast_input_changed(&self, user_id: &str, input: ConversationInputResponse) {
        self.broadcaster().broadcast(WebSocketMessage::new(
            "conversation.inputChanged",
            serde_json::json!(InputChangedEvent {
                user_id: user_id.to_owned(),
                input,
            }),
        ));
    }

    async fn ensure_owned_conversation(&self, user_id: &str, conversation_id: &str) -> Result<(), ConversationError> {
        let exists = self
            .conversation_repo()
            .get(user_id, conversation_id)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to load conversation: {e}")))?
            .is_some();
        if exists {
            Ok(())
        } else {
            Err(ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })
        }
    }

    // ── Workspace browsing ──────────────────────────────────────────

    /// Enumerate entries under `query.path` inside the conversation's
    /// workspace root. Enforces workspace isolation (no traversal outside
    /// the root, with an allowance for symlinked sub-directories) and a
    /// depth cap of [`MAX_DIR_DEPTH`].
    pub async fn browse_workspace(
        &self,
        user_id: &str,
        conversation_id: &str,
        query: WorkspaceBrowseQuery,
    ) -> Result<Vec<WorkspaceEntry>, ConversationError> {
        if query.path.trim().is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "path must not be empty".into(),
            });
        }

        let row = self
            .conversation_repo()
            .get(user_id, conversation_id)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to load conversation: {e}")))?
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;

        let extra: serde_json::Value = serde_json::from_str(&row.extra)
            .map_err(|e| ConversationError::internal(format!("Invalid extra JSON: {e}")))?;
        let workspace = extra
            .get("workspace")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        if workspace.is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "Conversation has no workspace assigned".into(),
            });
        }

        let relative_path = query.path.trim_start_matches('/');
        let relative_path_obj = std::path::Path::new(relative_path);
        if relative_path_obj
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(ConversationError::BadRequest {
                reason: "Path traversal outside workspace is not allowed".into(),
            });
        }

        // Resolve the browsed path relative to the workspace root
        let base = std::path::Path::new(&workspace);
        let browse_path = if relative_path.is_empty() {
            base.to_path_buf()
        } else {
            base.join(relative_path_obj)
        };

        // Security: reject direct traversal outside the workspace root, but allow
        // symlinked directories mounted inside the workspace (e.g. native skill
        // dirs that point at the builtin skills corpus under data-dir).
        let canonical_base = base
            .canonicalize()
            .map_err(|e| ConversationError::internal(format!("Failed to resolve workspace path: {e}")))?;
        let canonical_browse = browse_path
            .canonicalize()
            .map_err(|_| ConversationError::not_found_reason("Directory not found"))?;
        if !browse_path.starts_with(base) && !canonical_browse.starts_with(&canonical_base) {
            return Err(ConversationError::BadRequest {
                reason: "Path traversal outside workspace is not allowed".into(),
            });
        }

        // Check depth limit
        let depth = relative_path_obj.components().count();
        if depth > MAX_DIR_DEPTH {
            return Err(ConversationError::BadRequest {
                reason: format!("Directory depth exceeds maximum of {MAX_DIR_DEPTH}"),
            });
        }

        let mut entries = Vec::new();
        let mut dir_reader = tokio::fs::read_dir(&canonical_browse)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to read directory: {e}")))?;

        while let Ok(Some(entry)) = dir_reader.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();

            // Apply search filter if provided
            if let Some(ref search) = query.search
                && !search.is_empty()
                && !name.to_lowercase().contains(&search.to_lowercase())
            {
                continue;
            }

            let entry_path = entry.path();
            let metadata = tokio::fs::metadata(&entry_path)
                .await
                .map_err(|e| ConversationError::internal(format!("Failed to read entry metadata: {e}")))?;

            let entry_type = if metadata.is_dir() { "directory" } else { "file" };

            entries.push(WorkspaceEntry {
                name,
                entry_type: entry_type.into(),
            });
        }

        // Sort: directories first, then alphabetically
        entries.sort_by(|a, b| {
            let type_cmp = a.entry_type.cmp(&b.entry_type);
            if type_cmp == std::cmp::Ordering::Equal {
                a.name.to_lowercase().cmp(&b.name.to_lowercase())
            } else {
                type_cmp
            }
        });

        Ok(entries)
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn append_input_status_event(
    journal: &crate::stream_persistence::CanonicalEventJournal,
    user_id: &str,
    conversation_id: &str,
    input_id: &str,
    status: ConversationInputStatus,
    turn_id: Option<&str>,
    msg_id: Option<&str>,
    error_code: Option<&str>,
) -> Result<(), ConversationError> {
    let payload = serde_json::json!({
        "type": "conversation_input_status",
        "visibility": "host",
        "data": {
            "input_id": input_id,
            "status": input_status_name(status),
            "turn_id": turn_id,
            "msg_id": msg_id,
            "error_code": error_code,
        }
    });
    let event_id = crate::stream_persistence::canonical_event_id(
        &format!("conversation_input:{input_id}:{}", input_status_name(status)),
        &payload,
    );
    journal
        .append(
            user_id,
            conversation_id,
            event_id,
            match status {
                ConversationInputStatus::Held => "InputHeld",
                ConversationInputStatus::Dispatching => "InputDispatching",
                ConversationInputStatus::Accepted => "InputAccepted",
                ConversationInputStatus::Applied => "InputApplied",
                ConversationInputStatus::Canceled => "InputCanceled",
                ConversationInputStatus::Failed => "InputFailed",
            }
            .into(),
            payload,
        )
        .await
        .map_err(|error| ConversationError::internal(format!("Failed to journal input status: {error}")))?;
    Ok(())
}
