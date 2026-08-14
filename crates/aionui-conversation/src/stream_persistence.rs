use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use aionui_ai_agent::protocol::events::{
    ErrorEventData, TipType, TipsEventData,
    tool_call::{AcpToolCallStatus, ToolCallStatus},
};
use aionui_api_types::{ConversationRuntimeSummary, WebSocketMessage};
use aionui_common::{ErrorChain, normalize_keys_to_snake_case, now_ms};
use aionui_db::models::MessageRow;
use aionui_db::{ConversationRowUpdate, DbError, IConversationRepository, MessageRowUpdate};
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::{debug, error, warn};

use crate::runtime_completion::RuntimeCompletionPublisher;
use crate::runtime_persistence::{RuntimePersistenceCoordinator, RuntimeWriteKind};
use crate::service::ConversationService;

fn is_not_found(err: &DbError) -> bool {
    matches!(err, DbError::NotFound(_))
}

const DEFAULT_RETAINED_OUTPUT_PREVIEW_BYTES: usize = 64 * 1024;
static SPILL_STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub(crate) struct OutputRetentionPolicy {
    root: PathBuf,
    preview_bytes: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct RetainedOutput {
    pub reference: String,
    pub sha256: String,
    pub size: u64,
    pub preview: String,
}

impl OutputRetentionPolicy {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            preview_bytes: DEFAULT_RETAINED_OUTPUT_PREVIEW_BYTES,
        }
    }

    #[cfg(test)]
    fn with_preview_bytes(root: PathBuf, preview_bytes: usize) -> Self {
        Self { root, preview_bytes }
    }

    pub async fn retain(
        &self,
        user_id: &str,
        conversation_id: &str,
        output: &str,
    ) -> Result<Option<RetainedOutput>, std::io::Error> {
        if output.len() <= self.preview_bytes {
            return Ok(None);
        }
        let user_scope = stable_scope(user_id);
        let conversation_scope = stable_scope(conversation_id);
        let sha256 = hex::encode(Sha256::digest(output.as_bytes()));
        let directory = self.root.join(&user_scope).join(&conversation_scope);
        let target = directory.join(format!("{sha256}.txt"));
        ensure_contained(&self.root, &target)?;
        tokio::fs::create_dir_all(&directory).await?;
        if !target.is_file() {
            let staging = directory.join(format!(
                ".{sha256}-{}-{}.tmp",
                std::process::id(),
                SPILL_STAGING_COUNTER.fetch_add(1, Ordering::Relaxed),
            ));
            ensure_contained(&self.root, &staging)?;
            tokio::fs::write(&staging, output.as_bytes()).await?;
            match tokio::fs::rename(&staging, &target).await {
                Ok(()) => {}
                Err(error) if target.is_file() => {
                    let _ = tokio::fs::remove_file(staging).await;
                    drop(error);
                }
                Err(error) => return Err(error),
            }
        }
        let preview_end = output.floor_char_boundary(self.preview_bytes);
        Ok(Some(RetainedOutput {
            reference: format!("v1_{user_scope}_{conversation_scope}_{sha256}"),
            sha256,
            size: output.len() as u64,
            preview: output[..preview_end].to_owned(),
        }))
    }

    pub async fn read(
        &self,
        user_id: &str,
        conversation_id: &str,
        reference: &str,
    ) -> Result<(String, String), std::io::Error> {
        let parts: Vec<&str> = reference.split('_').collect();
        if parts.len() != 4
            || parts[0] != "v1"
            || parts[1] != stable_scope(user_id)
            || parts[2] != stable_scope(conversation_id)
            || parts[3].len() != 64
            || !parts[3].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "invalid retained output reference",
            ));
        }
        let target = self
            .root
            .join(parts[1])
            .join(parts[2])
            .join(format!("{}.txt", parts[3]));
        ensure_contained(&self.root, &target)?;
        let bytes = tokio::fs::read(target).await?;
        let actual_sha256 = hex::encode(Sha256::digest(&bytes));
        if actual_sha256 != parts[3] {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "retained output checksum mismatch",
            ));
        }
        let content =
            String::from_utf8(bytes).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        Ok((actual_sha256, content))
    }
}

fn stable_scope(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

pub(crate) fn canonical_event_id(seed: &str, payload: &serde_json::Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    hasher.update([0]);
    hasher.update(serde_json::to_vec(payload).unwrap_or_default());
    hex::encode(hasher.finalize())
}

fn ensure_contained(root: &Path, target: &Path) -> Result<(), std::io::Error> {
    if target == root || !target.starts_with(root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "retained output path escapes storage root",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub(crate) struct CanonicalJournalEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub conversation_id: String,
    pub sequence: u64,
    pub timestamp: i64,
    pub kind: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct CanonicalReplayProjection {
    pub schema_version: u32,
    pub conversation_id: String,
    pub event_count: u64,
    pub last_sequence: u64,
    pub last_event_id: Option<String>,
    pub kind_counts: BTreeMap<String, u64>,
    pub journal_sha256: String,
}

impl CanonicalReplayProjection {
    fn from_empty(conversation_id: &str, events: &[CanonicalJournalEvent]) -> Result<Self, std::io::Error> {
        let mut kind_counts = BTreeMap::new();
        let mut digest = Sha256::new();
        for (index, event) in events.iter().enumerate() {
            if event.conversation_id != conversation_id || event.sequence != index as u64 + 1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "canonical replay event does not match its projection scope",
                ));
            }
            *kind_counts.entry(event.kind.clone()).or_insert(0) += 1;
            digest.update(
                serde_json::to_vec(event)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?,
            );
            digest.update([b'\n']);
        }
        Ok(Self {
            schema_version: 1,
            conversation_id: conversation_id.to_owned(),
            event_count: events.len() as u64,
            last_sequence: events.last().map_or(0, |event| event.sequence),
            last_event_id: events.last().map(|event| event.event_id.clone()),
            kind_counts,
            journal_sha256: hex::encode(digest.finalize()),
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CanonicalEventJournal {
    root: PathBuf,
}

static JOURNAL_APPEND_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

impl CanonicalEventJournal {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub async fn append(
        &self,
        user_id: &str,
        conversation_id: &str,
        event_id: String,
        kind: String,
        payload: serde_json::Value,
    ) -> Result<CanonicalJournalEvent, std::io::Error> {
        let _guard = JOURNAL_APPEND_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let path = self.path(user_id, conversation_id)?;
        self.repair_incomplete_tail(&path).await?;
        let events = self.replay_unlocked(&path).await?;
        if let Some(existing) = events.iter().find(|event| event.event_id == event_id) {
            return Ok(existing.clone());
        }
        let event = CanonicalJournalEvent {
            schema_version: 1,
            event_id,
            conversation_id: conversation_id.to_owned(),
            sequence: events.last().map_or(1, |event| event.sequence.saturating_add(1)),
            timestamp: now_ms(),
            kind,
            payload,
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        let mut encoded =
            serde_json::to_vec(&event).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        encoded.push(b'\n');
        file.write_all(&encoded).await?;
        file.flush().await?;
        file.sync_data().await?;
        Ok(event)
    }

    pub async fn replay(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<CanonicalJournalEvent>, std::io::Error> {
        let path = self.path(user_id, conversation_id)?;
        self.replay_unlocked(&path).await
    }

    pub async fn replay_projection(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<CanonicalReplayProjection, std::io::Error> {
        let events = self.replay(user_id, conversation_id).await?;
        CanonicalReplayProjection::from_empty(conversation_id, &events)
    }

    #[cfg(test)]
    pub async fn replay_and_compare(
        &self,
        user_id: &str,
        conversation_id: &str,
        expected: &CanonicalReplayProjection,
    ) -> Result<CanonicalReplayProjection, std::io::Error> {
        let actual = self.replay_projection(user_id, conversation_id).await?;
        if &actual != expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "canonical replay projection mismatch: expected count/digest {}/{}, actual {}/{}",
                    expected.event_count, expected.journal_sha256, actual.event_count, actual.journal_sha256
                ),
            ));
        }
        Ok(actual)
    }

    fn path(&self, user_id: &str, conversation_id: &str) -> Result<PathBuf, std::io::Error> {
        let path = self
            .root
            .join(stable_scope(user_id))
            .join(format!("{}.ndjson", stable_scope(conversation_id)));
        ensure_contained(&self.root, &path)?;
        Ok(path)
    }

    async fn replay_unlocked(&self, path: &Path) -> Result<Vec<CanonicalJournalEvent>, std::io::Error> {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let committed_len = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let raw = std::str::from_utf8(&bytes[..committed_len])
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let mut events = Vec::new();
        for (index, line) in raw.lines().filter(|line| !line.trim().is_empty()).enumerate() {
            let event: CanonicalJournalEvent = serde_json::from_str(line)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            if event.schema_version != 1 || event.sequence != index as u64 + 1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "canonical event journal sequence is invalid",
                ));
            }
            if events
                .iter()
                .any(|seen: &CanonicalJournalEvent| seen.event_id == event.event_id)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "canonical event journal contains a duplicate event id",
                ));
            }
            events.push(event);
        }
        Ok(events)
    }

    async fn repair_incomplete_tail(&self, path: &Path) -> Result<(), std::io::Error> {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if bytes.is_empty() || bytes.last() == Some(&b'\n') {
            return Ok(());
        }
        let committed_len = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let file = tokio::fs::OpenOptions::new().write(true).open(path).await?;
        file.set_len(committed_len as u64).await?;
        file.sync_data().await?;
        warn!(
            discarded_bytes = bytes.len() - committed_len,
            "Recovered incomplete canonical journal tail"
        );
        Ok(())
    }
}

fn is_foreign_key_constraint(err: &DbError) -> bool {
    err.to_string().contains("FOREIGN KEY constraint failed")
}

fn is_deleted_during_stream_persistence(err: &DbError) -> bool {
    is_not_found(err) || is_foreign_key_constraint(err)
}

fn log_persist_error(err: &DbError, message: &'static str) {
    if is_deleted_during_stream_persistence(err) {
        debug!(error = %ErrorChain(err), "{message}; conversation was likely deleted during stream finalization");
    } else {
        error!(error = %ErrorChain(err), "{message}");
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TextSegmentState {
    pub id: String,
    pub buffer: String,
    pub created_at: i64,
    pub record_created: bool,
    pub flush_counter: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct PersistedTextSegment {
    pub id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ThinkingSegmentState {
    pub id: String,
    pub buffer: String,
    pub started_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalTextOverride {
    pub msg_id: String,
    pub text: String,
    pub hidden: bool,
}

#[derive(Clone)]
pub(crate) struct StreamPersistenceAdapter {
    user_id: String,
    conversation_id: String,
    msg_id: String,
    repo: Arc<dyn IConversationRepository>,
    persistence: Option<RuntimePersistenceCoordinator>,
    /// The backend's own id for the in-flight turn (codex `Turn.id`), stamped
    /// onto every message row this adapter persists — the lookup key for
    /// `thread/fork`'s `lastTurnId`. Set by the relay on the internal
    /// `BackendTurnBound` frame; `None` for backends without one (claude/ACP).
    /// Shared across clones (the relay and its helpers clone the adapter).
    backend_turn_id: Arc<std::sync::Mutex<Option<String>>>,
}

impl StreamPersistenceAdapter {
    pub fn new(
        user_id: String,
        conversation_id: String,
        msg_id: String,
        repo: Arc<dyn IConversationRepository>,
        persistence: Option<RuntimePersistenceCoordinator>,
    ) -> Self {
        Self {
            user_id,
            conversation_id,
            msg_id,
            repo,
            persistence,
            backend_turn_id: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Record the backend's turn id for the in-flight turn (relay-only).
    pub(crate) fn set_backend_turn_id(&self, backend_turn_id: String) {
        *self.backend_turn_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(backend_turn_id);
    }

    /// The stamp every persisted message row of the current turn carries.
    /// Also read by the relay so live `message.stream` frames carry the anchor
    /// (without it, mid-history fork entries only appear after a reload).
    pub(crate) fn current_backend_turn_id(&self) -> Option<String> {
        self.backend_turn_id.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn with_persistence(mut self, persistence: RuntimePersistenceCoordinator) -> Self {
        self.persistence = Some(persistence);
        self
    }

    #[tracing::instrument(skip_all, fields(conversation_id = %self.conversation_id))]
    pub async fn complete_conversation(
        &self,
        broadcaster: &Arc<dyn EventBroadcaster>,
        turn_id: &str,
        runtime: Option<ConversationRuntimeSummary>,
    ) {
        if let Some(persistence) = &self.persistence {
            RuntimeCompletionPublisher::new(
                self.user_id.clone(),
                self.repo.clone(),
                broadcaster.clone(),
                persistence.clone(),
            )
            .publish(&self.conversation_id, turn_id, runtime)
            .await;
            return;
        }

        let update = ConversationRowUpdate {
            status: Some("finished".to_owned()),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        if let Err(e) = self.repo.update(&self.user_id, &self.conversation_id, &update).await {
            log_persist_error(&e, "Failed to update conversation status");
        }

        let payload = json!({
            "user_id": self.user_id,
            "conversation_id": self.conversation_id,
            "session_id": self.conversation_id,
            "turn_id": turn_id,
            "status": "finished",
            "canSendMessage": true,
            "runtime": runtime,
        });
        broadcaster.broadcast(WebSocketMessage::new("turn.completed", payload));

        debug!(conversation_id = %self.conversation_id, turn_id, status = "finished", "Turn completed");
    }

    fn allows_write(&self, kind: RuntimeWriteKind) -> bool {
        self.persistence
            .as_ref()
            .is_none_or(|persistence| persistence.allows(&self.conversation_id, kind))
    }

    #[tracing::instrument(skip_all)]
    pub async fn flush_text_segment(&self, segment: &mut TextSegmentState) {
        if !self.allows_write(RuntimeWriteKind::AssistantTextFlush) {
            return;
        }
        if segment.buffer.is_empty() {
            return;
        }

        let content = json!({ "content": segment.buffer }).to_string();

        if segment.record_created {
            let update = MessageRowUpdate {
                content: Some(content),
                status: Some(Some("work".into())),
                hidden: None,
            };
            if let Err(e) = self
                .repo
                .update_message(&self.user_id, &self.conversation_id, &segment.id, &update)
                .await
            {
                log_persist_error(&e, "Failed to update streaming text segment");
            }
        } else {
            let row = MessageRow {
                id: segment.id.clone(),
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(segment.id.clone()),
                r#type: "text".into(),
                content,
                position: Some("left".into()),
                status: Some("work".into()),
                hidden: false,
                created_at: segment.created_at,
                backend_turn_id: self.current_backend_turn_id(),
            };
            if let Err(e) = self.repo.insert_message(&self.user_id, &row).await {
                log_persist_error(&e, "Failed to create streaming text segment");
            }
            segment.record_created = true;
        }
    }

    #[tracing::instrument(skip_all)]
    pub async fn finalize_text_segment(&self, segment: TextSegmentState, status: &str) -> Option<PersistedTextSegment> {
        if !self.allows_write(RuntimeWriteKind::AssistantTextFinalize) {
            return None;
        }
        if segment.buffer.is_empty() {
            return None;
        }

        let content = json!({ "content": segment.buffer }).to_string();
        if segment.record_created {
            let update = MessageRowUpdate {
                content: Some(content),
                status: Some(Some(status.to_owned())),
                hidden: Some(false),
            };
            if let Err(e) = self
                .repo
                .update_message(&self.user_id, &self.conversation_id, &segment.id, &update)
                .await
            {
                log_persist_error(&e, "Failed to finalize text segment");
                return None;
            }
        } else {
            let row = MessageRow {
                id: segment.id.clone(),
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(segment.id.clone()),
                r#type: "text".into(),
                content,
                position: Some("left".into()),
                status: Some(status.to_owned()),
                hidden: false,
                created_at: segment.created_at,
                backend_turn_id: self.current_backend_turn_id(),
            };
            if let Err(e) = self.repo.insert_message(&self.user_id, &row).await {
                log_persist_error(&e, "Failed to create finalized text segment");
                return None;
            }
        }

        Some(PersistedTextSegment { id: segment.id })
    }

    #[tracing::instrument(skip_all)]
    pub async fn persist_final_text(
        &self,
        text_segments: &[PersistedTextSegment],
        status: &str,
        final_text: &str,
        hidden: bool,
        rewrite_segments: bool,
    ) -> Vec<FinalTextOverride> {
        if !self.allows_write(RuntimeWriteKind::TerminalFinalize) {
            return Vec::new();
        }

        let mut overrides = Vec::new();
        if let Some(primary_segment) = text_segments.first() {
            if rewrite_segments {
                let content = json!({ "content": final_text }).to_string();
                let update = MessageRowUpdate {
                    content: Some(content),
                    status: Some(Some(status.to_owned())),
                    hidden: Some(hidden),
                };
                if let Err(e) = self
                    .repo
                    .update_message(&self.user_id, &self.conversation_id, &primary_segment.id, &update)
                    .await
                {
                    log_persist_error(&e, "Failed to rewrite finalized text segment");
                }
                overrides.push(FinalTextOverride {
                    msg_id: primary_segment.id.clone(),
                    text: final_text.to_owned(),
                    hidden,
                });

                for segment in text_segments.iter().skip(1) {
                    let hide_update = MessageRowUpdate {
                        content: None,
                        status: Some(Some(status.to_owned())),
                        hidden: Some(true),
                    };
                    if let Err(e) = self
                        .repo
                        .update_message(&self.user_id, &self.conversation_id, &segment.id, &hide_update)
                        .await
                    {
                        log_persist_error(&e, "Failed to hide superseded text segment");
                    }
                    overrides.push(FinalTextOverride {
                        msg_id: segment.id.clone(),
                        text: String::new(),
                        hidden: true,
                    });
                }
            } else {
                for segment in text_segments {
                    let status_update = MessageRowUpdate {
                        content: None,
                        status: Some(Some(status.to_owned())),
                        hidden: Some(false),
                    };
                    if let Err(e) = self
                        .repo
                        .update_message(&self.user_id, &self.conversation_id, &segment.id, &status_update)
                        .await
                    {
                        log_persist_error(&e, "Failed to finalize text segment status");
                    }
                }
            }
        } else if !hidden {
            let row = MessageRow {
                id: self.msg_id.clone(),
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(self.msg_id.clone()),
                r#type: "text".into(),
                content: json!({ "content": final_text }).to_string(),
                position: Some("left".into()),
                status: Some(status.to_owned()),
                hidden: false,
                created_at: now_ms(),
                backend_turn_id: self.current_backend_turn_id(),
            };
            if let Err(e) = self.repo.insert_message(&self.user_id, &row).await {
                log_persist_error(&e, "Failed to create final fallback message");
            }
        }

        overrides
    }

    #[tracing::instrument(skip_all)]
    pub async fn persist_error_tip(&self, data: &ErrorEventData) {
        if !self.allows_write(RuntimeWriteKind::TerminalFinalize) {
            return;
        }

        let content = json!({ "content": &data.message, "type": "error", "error": &data }).to_string();
        let row = MessageRow {
            id: ConversationService::mint_msg_id(),
            conversation_id: self.conversation_id.clone(),
            msg_id: None,
            r#type: "tips".into(),
            content,
            position: Some("left".into()),
            status: Some("error".into()),
            hidden: false,
            created_at: now_ms(),
            backend_turn_id: self.current_backend_turn_id(),
        };
        if let Err(e) = self.repo.insert_message(&self.user_id, &row).await {
            log_persist_error(&e, "Failed to store error message");
        }
    }

    #[tracing::instrument(skip_all)]
    pub async fn persist_tip(&self, data: &TipsEventData) {
        if !self.allows_write(RuntimeWriteKind::TerminalFinalize) {
            return;
        }

        let status = match data.tip_type {
            TipType::Error => "error",
            TipType::Success | TipType::Warning | TipType::Info => "finish",
        };
        // `supersedes_key` has to survive persistence too: on reload the history
        // is folded with the same merge the live stream uses, so without the key
        // a stalled turn's retry attempts come back as N stacked cards even
        // though the user only ever saw one counting up.
        let content = json!({
            "content": &data.content,
            "type": &data.tip_type,
            "code": &data.code,
            "params": &data.params,
            "supersedes_key": &data.supersedes_key,
        })
        .to_string();
        let row = MessageRow {
            id: ConversationService::mint_msg_id(),
            conversation_id: self.conversation_id.clone(),
            msg_id: None,
            r#type: "tips".into(),
            content,
            position: Some("left".into()),
            status: Some(status.into()),
            hidden: false,
            created_at: now_ms(),
            backend_turn_id: self.current_backend_turn_id(),
        };
        if let Err(e) = self.repo.insert_message(&self.user_id, &row).await {
            log_persist_error(&e, "Failed to store tip message");
        }
    }

    #[tracing::instrument(skip_all)]
    pub async fn persist_thinking_segment(&self, segment: ThinkingSegmentState, duration_ms: u64) {
        // An empty segment should no longer be reachable: StreamRelay drops a
        // thinking chunk that has no text before it can open one (see the POLICY
        // note there). This stays as the second line of defense — persisting a
        // contentless row is what put a column of blank "thinking done · 0s" cards
        // into the reloaded view, so the storage layer refuses it too rather than
        // trusting every future caller to have filtered upstream.
        if segment.buffer.is_empty() {
            return;
        }
        if !self.allows_write(RuntimeWriteKind::AssistantThinkingFinalize) {
            return;
        }
        let content = json!({
            "content": segment.buffer,
            "status": "done",
            "duration_ms": duration_ms,
        })
        .to_string();
        let row = MessageRow {
            id: segment.id.clone(),
            conversation_id: self.conversation_id.clone(),
            msg_id: Some(segment.id),
            r#type: "thinking".into(),
            content,
            position: Some("left".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: segment.started_at,
            backend_turn_id: self.current_backend_turn_id(),
        };
        if let Err(e) = self.repo.insert_message(&self.user_id, &row).await {
            log_persist_error(&e, "Failed to persist thinking message");
        }
    }

    /// Persist a Gemini-style tool_call event.
    #[tracing::instrument(skip_all)]
    pub async fn persist_tool_call(&self, data: &aionui_ai_agent::protocol::events::tool_call::ToolCallEventData) {
        if !self.allows_write(RuntimeWriteKind::ToolCallPersist) {
            return;
        }
        if data.call_id.trim().is_empty() {
            warn!(
                tool = %data.name,
                status = ?data.status,
                "Skipping tool_call persistence because call_id is empty"
            );
            return;
        }

        let status = match data.status {
            ToolCallStatus::Running => "work",
            ToolCallStatus::Completed => "finish",
            ToolCallStatus::Error => "error",
            // A cancelled call is terminal: the row must leave "work" so the
            // frontend spinner (hasRunningToolMessages) stops after interrupt.
            ToolCallStatus::Canceled => "finish",
        };
        let content = serde_json::to_string(data).unwrap_or_default();

        let row = MessageRow {
            id: data.call_id.clone(),
            conversation_id: self.conversation_id.clone(),
            msg_id: Some(data.call_id.clone()),
            r#type: "tool_call".into(),
            content,
            position: Some("left".into()),
            status: Some(status.to_owned()),
            hidden: false,
            created_at: now_ms(),
            backend_turn_id: self.current_backend_turn_id(),
        };
        if let Err(e) = self.repo.upsert_message(&self.user_id, &row).await {
            error!(
                call_id = %data.call_id,
                tool = %data.name,
                status,
                error = %ErrorChain(&e),
                "Failed to upsert tool_call message"
            );
        } else {
            debug!(
                call_id = %data.call_id,
                tool = %data.name,
                status,
                "Upserted tool_call message"
            );
        }
    }

    /// Persist an ACP (Claude CLI) tool call event.
    #[tracing::instrument(skip_all)]
    pub async fn persist_acp_tool_call(
        &self,
        data: &aionui_ai_agent::protocol::events::tool_call::AcpToolCallEventData,
    ) {
        if !self.allows_write(RuntimeWriteKind::AcpToolCallPersist) {
            return;
        }
        let tool_call_id = &data.update.tool_call_id;
        let status = match data.update.status {
            Some(AcpToolCallStatus::Pending) | None => "work",
            Some(AcpToolCallStatus::InProgress) => "work",
            Some(AcpToolCallStatus::Completed) => "finish",
            Some(AcpToolCallStatus::Failed) => "error",
        };

        let mut value = serde_json::to_value(data).unwrap_or_default();
        normalize_keys_to_snake_case(&mut value);
        let content = value.to_string();

        let row = MessageRow {
            id: tool_call_id.clone(),
            conversation_id: self.conversation_id.clone(),
            msg_id: Some(tool_call_id.clone()),
            r#type: "acp_tool_call".into(),
            content,
            position: Some("left".into()),
            status: Some(status.to_owned()),
            hidden: false,
            created_at: now_ms(),
            backend_turn_id: self.current_backend_turn_id(),
        };
        if let Err(e) = self.repo.upsert_message(&self.user_id, &row).await {
            error!(error = %ErrorChain(&e), "Failed to upsert acp_tool_call message");
        }
    }

    /// Apply a STATUS-ONLY settle to an EXISTING tool_call row, if any.
    ///
    /// Returns whether the row existed. Never inserts: a `settle_only` frame is
    /// the pump settling a card it has no memory of (post-resume), and the same
    /// unknown-terminal shape also fires for workflow-internal refs that never
    /// had a row — inserting for those would conjure junk cards.
    #[tracing::instrument(skip_all)]
    pub async fn settle_tool_call_if_present(
        &self,
        data: &aionui_ai_agent::protocol::events::tool_call::ToolCallEventData,
    ) -> bool {
        let existing = self
            .repo
            .get_message_by_msg_id(&self.user_id, &self.conversation_id, &data.call_id, "tool_call")
            .await
            .unwrap_or(None);
        if existing.is_none() {
            debug!(call_id = %data.call_id, "settle-only frame for a row that does not exist; dropped");
            return false;
        }
        self.persist_tool_call(data).await;
        true
    }

    /// Persist a tool_group event (array of tool summaries).
    #[tracing::instrument(skip_all)]
    pub async fn persist_tool_group(&self, entries: &[aionui_ai_agent::protocol::events::tool_call::ToolGroupEntry]) {
        if !self.allows_write(RuntimeWriteKind::ToolGroupPersist) {
            return;
        }
        let all_done = entries.iter().all(|e| e.status.is_terminal());
        let status = if all_done { "finish" } else { "work" };
        let content = serde_json::to_string(entries).unwrap_or_default();

        let group_id = entries
            .first()
            .map(|e| e.call_id.clone())
            .unwrap_or_else(ConversationService::mint_msg_id);

        let existing = self
            .repo
            .get_message_by_msg_id(&self.user_id, &self.conversation_id, &group_id, "tool_group")
            .await
            .unwrap_or(None);

        if existing.is_some() {
            let update = MessageRowUpdate {
                content: Some(content),
                status: Some(Some(status.to_owned())),
                hidden: None,
            };
            if let Err(e) = self
                .repo
                .update_message(&self.user_id, &self.conversation_id, &group_id, &update)
                .await
            {
                error!(error = %ErrorChain(&e), "Failed to update tool_group message");
            }
        } else {
            let row = MessageRow {
                id: group_id.clone(),
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(group_id),
                r#type: "tool_group".into(),
                content,
                position: Some("left".into()),
                status: Some(status.to_owned()),
                hidden: false,
                created_at: now_ms(),
                backend_turn_id: self.current_backend_turn_id(),
            };
            if let Err(e) = self.repo.insert_message(&self.user_id, &row).await {
                error!(error = %ErrorChain(&e), "Failed to persist tool_group message");
            }
        }
    }
}

#[cfg(test)]
mod output_retention_tests {
    use super::*;

    #[tokio::test]
    async fn spills_large_output_and_reads_it_only_in_the_same_scope() {
        let root = tempfile::tempdir().unwrap();
        let policy = OutputRetentionPolicy::with_preview_bytes(root.path().to_path_buf(), 4);
        let retained = policy.retain("user-a", "conv-a", "hello world").await.unwrap().unwrap();
        assert_eq!(retained.preview, "hell");
        assert_eq!(retained.size, 11);
        let (_, full) = policy.read("user-a", "conv-a", &retained.reference).await.unwrap();
        assert_eq!(full, "hello world");
        assert_eq!(
            policy
                .read("user-b", "conv-a", &retained.reference)
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[tokio::test]
    async fn keeps_small_output_inline() {
        let root = tempfile::tempdir().unwrap();
        let policy = OutputRetentionPolicy::with_preview_bytes(root.path().to_path_buf(), 64);
        assert!(policy.retain("user", "conv", "small").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn canonical_journal_is_ordered_and_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let journal = CanonicalEventJournal::new(root.path().to_path_buf());
        let payload = serde_json::json!({"content":"hello"});
        let first = journal
            .append("user", "conv", "event-1".into(), "Text".into(), payload.clone())
            .await
            .unwrap();
        let duplicate = journal
            .append("user", "conv", "event-1".into(), "Text".into(), payload)
            .await
            .unwrap();
        let second = journal
            .append("user", "conv", "event-2".into(), "Finish".into(), serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(first, duplicate);
        assert_eq!(second.sequence, 2);
        assert_eq!(journal.replay("user", "conv").await.unwrap().len(), 2);
        assert!(journal.replay("other-user", "conv").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn canonical_replay_projection_compares_from_an_empty_projection() {
        let root = tempfile::tempdir().unwrap();
        let journal = CanonicalEventJournal::new(root.path().to_path_buf());
        journal
            .append(
                "user",
                "conv",
                "event-1".into(),
                "Text".into(),
                serde_json::json!({"text":"a"}),
            )
            .await
            .unwrap();
        journal
            .append("user", "conv", "event-2".into(), "Finish".into(), serde_json::json!({}))
            .await
            .unwrap();

        let expected = journal.replay_projection("user", "conv").await.unwrap();
        assert_eq!(expected.event_count, 2);
        assert_eq!(expected.kind_counts.get("Text"), Some(&1));
        assert!(journal.replay_and_compare("user", "conv", &expected).await.is_ok());

        let mut wrong = expected;
        wrong.event_count = 3;
        assert_eq!(
            journal
                .replay_and_compare("user", "conv", &wrong)
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn canonical_journal_discards_an_incomplete_crash_tail_before_append() {
        let root = tempfile::tempdir().unwrap();
        let journal = CanonicalEventJournal::new(root.path().to_path_buf());
        journal
            .append("user", "conv", "event-1".into(), "Text".into(), serde_json::json!({}))
            .await
            .unwrap();
        let path = journal.path("user", "conv").unwrap();
        let mut file = tokio::fs::OpenOptions::new().append(true).open(&path).await.unwrap();
        file.write_all(br#"{"schema_version":1"#).await.unwrap();
        file.flush().await.unwrap();

        journal
            .append("user", "conv", "event-2".into(), "Finish".into(), serde_json::json!({}))
            .await
            .unwrap();
        let events = journal.replay("user", "conv").await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].sequence, 2);
    }
}
