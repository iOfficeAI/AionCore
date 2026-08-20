use std::collections::{BTreeMap, BTreeSet};

use aionui_api_types::ConversationInputStatus;
use aionui_common::{ErrorChain, now_ms};
use aionui_db::models::{ConversationInputRow, MessageRow};
use aionui_db::{MessageRowUpdate, UpsertJournalProjectionCheckpointParams};
use tracing::{info, warn};

use crate::runtime_persistence::RuntimeWriteKind;
use crate::service::ConversationService;
use crate::stream_persistence::CanonicalJournalEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReplayedConversationInput {
    id: String,
    mode: String,
    status: String,
    content: String,
    files: String,
    inject_skills: String,
    hidden: bool,
    client_key: String,
    turn_id: Option<String>,
    msg_id: Option<String>,
    error_code: Option<String>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupRecoveryAction {
    FinishVisibleOutput,
    FinishEmptyPlaceholder,
    /// A tool_call/tool_group row left on `work` by a previous process. At
    /// startup this is dead BY DEFINITION — the CLI process group died with the
    /// old aioncore, so no terminal will ever be reported (a hard kill runs
    /// neither the pump's teardown settle nor leaves anyone to resume). The
    /// row's CONTENT status must be rewritten too: the frontend renders the
    /// embedded status, and `hasRunningToolMessages` would keep the View Steps
    /// spinner alive off a `finish` row whose content still says running.
    SettleToolRow,
}

impl ConversationService {
    pub async fn recover_stale_runtime_state_on_startup(&self) {
        self.recover_conversation_inputs_on_startup().await;
        let rows = match self.conversation_repo().list_stale_runtime_messages().await {
            Ok(rows) => rows,
            Err(error) => {
                warn!(
                    error = %ErrorChain(&error),
                    "startup recovery skipped because stale runtime message query failed"
                );
                return;
            }
        };

        let mut recovered = 0usize;
        for stale in rows {
            let row = stale.message;
            if !self
                .runtime_persistence()
                .allows(&row.conversation_id, RuntimeWriteKind::StartupRecovery)
            {
                continue;
            }

            let action = classify_recovery_action(&row);
            let content = match action {
                StartupRecoveryAction::SettleToolRow => settle_tool_row_content(&row.r#type, &row.content),
                _ => None,
            };
            if matches!(action, StartupRecoveryAction::SettleToolRow) {
                let identifiers = recovered_tool_identifiers(&row);
                let payload = serde_json::json!({
                    "type": "tool_execution_recovery",
                    "visibility": "host",
                    "data": {
                        "execution_id": identifiers.execution_id,
                        "call_id": identifiers.call_id,
                        "turn_id": identifiers.turn_id,
                        "message_id": row.id,
                        "reason": "runtime_epoch_ended",
                        "terminal_state": "canceled",
                    }
                });
                let event_id =
                    crate::stream_persistence::canonical_event_id(&format!("tool_recovery:{}", row.id), &payload);
                if let Err(error) = self
                    .canonical_event_journal()
                    .append(
                        &stale.user_id,
                        &row.conversation_id,
                        event_id,
                        "ToolExecutionRecovered".into(),
                        payload,
                    )
                    .await
                {
                    warn!(
                        conversation_id = %row.conversation_id,
                        message_id = %row.id,
                        error = %ErrorChain(&error),
                        "startup recovery kept stale tool projection because Journal append failed"
                    );
                    continue;
                }
            }
            let update = MessageRowUpdate {
                content,
                status: Some(Some("finish".to_owned())),
                hidden: Some(matches!(action, StartupRecoveryAction::FinishEmptyPlaceholder)),
            };

            match self
                .conversation_repo()
                .update_message(&stale.user_id, &row.conversation_id, &row.id, &update)
                .await
            {
                Ok(()) => {
                    recovered += 1;
                    info!(
                        conversation_id = %row.conversation_id,
                        msg_id = ?row.msg_id,
                        message_type = %row.r#type,
                        recovery_action = ?action,
                        "startup recovery closed stale runtime message"
                    );
                }
                Err(error) => {
                    warn!(
                        conversation_id = %row.conversation_id,
                        msg_id = ?row.msg_id,
                        error = %ErrorChain(&error),
                        "startup recovery failed to close stale runtime message"
                    );
                }
            }
        }

        if recovered > 0 {
            info!(recovered, "startup recovery completed for stale runtime messages");
        }
    }

    async fn recover_conversation_inputs_on_startup(&self) {
        self.rebuild_conversation_input_projection().await;
        let inputs = match self.conversation_repo().list_unfinished_conversation_inputs().await {
            Ok(inputs) => inputs,
            Err(error) => {
                warn!(error = %ErrorChain(&error), "startup recovery skipped durable conversation inputs");
                return;
            }
        };
        let mut scopes = BTreeSet::new();
        for input in inputs {
            scopes.insert((input.user_id.clone(), input.conversation_id.clone()));
            let recovery = match input.status.as_str() {
                "held" => None,
                "accepted" | "dispatching" if input.msg_id.is_some() => {
                    let message_exists = match self
                        .conversation_repo()
                        .get_message(
                            &input.user_id,
                            &input.conversation_id,
                            input.msg_id.as_deref().unwrap_or_default(),
                        )
                        .await
                    {
                        Ok(message) => message.is_some(),
                        Err(error) => {
                            warn!(
                                conversation_id = %input.conversation_id,
                                input_id = %input.id,
                                error = %ErrorChain(&error),
                                "startup recovery could not verify dispatched input"
                            );
                            false
                        }
                    };
                    Some(if message_exists {
                        (ConversationInputStatus::Applied, None)
                    } else {
                        (ConversationInputStatus::Failed, Some("recovery_message_missing"))
                    })
                }
                "accepted" => Some((ConversationInputStatus::Failed, Some("recovery_message_missing"))),
                "dispatching" => Some((ConversationInputStatus::Failed, Some("recovery_dispatch_unconfirmed"))),
                _ => None,
            };
            if let Some((status, error_code)) = recovery
                && let Err(error) = self
                    .recover_input_status(&input.user_id, &input.conversation_id, &input.id, status, error_code)
                    .await
            {
                warn!(
                    conversation_id = %input.conversation_id,
                    input_id = %input.id,
                    error = %error,
                    "startup recovery failed to converge durable input"
                );
            }
        }

        for (user_id, conversation_id) in scopes {
            self.dispatch_next_held_input(&user_id, &conversation_id).await;
        }
    }

    /// Rebuild the query projection before recovering unfinished dispatches.
    /// The canonical Journal is authoritative; the database can be empty or
    /// lag behind after a crash between append and projection update.
    async fn rebuild_conversation_input_projection(&self) {
        const PROJECTOR: &str = "conversation-inputs-v1";
        let journals = match self.canonical_event_journal().discover().await {
            Ok(journals) => journals,
            Err(error) => {
                warn!(error = %ErrorChain(&error), "startup recovery could not discover canonical journals");
                return;
            }
        };
        let mut repaired = 0usize;
        let mut incremental_events = 0usize;
        let mut full_rebuilds = 0usize;
        for journal in journals {
            let conversation_id = journal.conversation_id;
            let user_id = match self.conversation_repo().owner_user_id(&conversation_id).await {
                Ok(Some(user_id)) => user_id,
                Ok(None) => continue,
                Err(error) => {
                    warn!(
                        conversation_id = %conversation_id,
                        error = %ErrorChain(&error),
                        "startup recovery could not resolve journal owner"
                    );
                    continue;
                }
            };
            let checkpoint = self
                .conversation_repo()
                .get_journal_projection_checkpoint(&user_id, &conversation_id, PROJECTOR)
                .await
                .ok()
                .flatten();
            let mut seed = BTreeMap::new();
            let (events, full_rebuild) = if let Some(checkpoint) = checkpoint.filter(|value| value.last_sequence > 0) {
                match self
                    .canonical_event_journal()
                    .replay_after(
                        &user_id,
                        &conversation_id,
                        checkpoint.last_sequence.saturating_sub(1) as u64,
                    )
                    .await
                {
                    Ok(mut events)
                        if events.first().is_some_and(|event| {
                            event.sequence == checkpoint.last_sequence as u64
                                && event.event_id == checkpoint.last_event_id
                        }) =>
                    {
                        events.remove(0);
                        match self
                            .conversation_repo()
                            .list_conversation_inputs(&user_id, &conversation_id)
                            .await
                        {
                            Ok(rows) => {
                                seed.extend(rows.into_iter().map(|row| {
                                    let id = row.id.clone();
                                    (id, ReplayedConversationInput::from(row))
                                }));
                                incremental_events += events.len();
                                (events, false)
                            }
                            Err(error) => {
                                warn!(conversation_id = %conversation_id, error = %ErrorChain(&error), "could not load input projection seed");
                                match self.canonical_event_journal().replay(&user_id, &conversation_id).await {
                                    Ok(events) => (events, true),
                                    Err(error) => {
                                        warn!(conversation_id = %conversation_id, error = %ErrorChain(&error), "could not rebuild journal after projection seed failure");
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                    _ => match self.canonical_event_journal().replay(&user_id, &conversation_id).await {
                        Ok(events) => (events, true),
                        Err(error) => {
                            warn!(conversation_id = %conversation_id, error = %ErrorChain(&error), "could not rebuild invalid journal checkpoint");
                            continue;
                        }
                    },
                }
            } else {
                match self.canonical_event_journal().replay(&user_id, &conversation_id).await {
                    Ok(events) => (events, true),
                    Err(error) => {
                        warn!(conversation_id = %conversation_id, error = %ErrorChain(&error), "could not perform initial journal projection");
                        continue;
                    }
                }
            };
            if full_rebuild {
                full_rebuilds += 1;
            }
            let Some(last_event) = events.last() else {
                continue;
            };
            let inputs: Vec<_> = fold_conversation_inputs_with_seed(seed, &events)
                .into_iter()
                .map(|input| input.into_row(&user_id, &conversation_id))
                .collect();
            let projected = inputs.len();
            if let Err(error) = self
                .conversation_repo()
                .apply_conversation_input_projection(
                    &inputs,
                    &UpsertJournalProjectionCheckpointParams {
                        user_id: &user_id,
                        conversation_id: &conversation_id,
                        projector: PROJECTOR,
                        last_sequence: last_event.sequence as i64,
                        last_event_id: &last_event.event_id,
                        updated_at: now_ms(),
                    },
                )
                .await
            {
                warn!(conversation_id = %conversation_id, error = %ErrorChain(&error), "could not atomically apply journal projection and checkpoint");
            } else {
                repaired += projected;
            }
        }
        info!(
            repaired,
            incremental_events, full_rebuilds, "startup journal projection recovery completed"
        );
    }
}

#[derive(Debug, Default)]
struct RecoveredToolIdentifiers {
    execution_id: Option<String>,
    call_id: Option<String>,
    turn_id: Option<String>,
}

fn recovered_tool_identifiers(row: &MessageRow) -> RecoveredToolIdentifiers {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&row.content) else {
        return RecoveredToolIdentifiers {
            call_id: row.msg_id.clone(),
            ..Default::default()
        };
    };
    let first = value.as_array().and_then(|entries| entries.first()).unwrap_or(&value);
    let string = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| first.get(*key).and_then(serde_json::Value::as_str))
            .map(str::to_owned)
    };
    RecoveredToolIdentifiers {
        execution_id: string(&["execution_id", "executionId"]),
        call_id: string(&["tool_call_id", "toolCallId", "id"]).or_else(|| row.msg_id.clone()),
        turn_id: string(&["turn_id", "turnId"]),
    }
}

fn fold_conversation_inputs(events: &[CanonicalJournalEvent]) -> Vec<ReplayedConversationInput> {
    fold_conversation_inputs_with_seed(BTreeMap::new(), events)
}

impl From<aionui_db::models::ConversationInputRow> for ReplayedConversationInput {
    fn from(row: aionui_db::models::ConversationInputRow) -> Self {
        Self {
            id: row.id,
            mode: row.mode,
            status: row.status,
            content: row.content,
            files: row.files,
            inject_skills: row.inject_skills,
            hidden: row.hidden,
            client_key: row.client_key,
            turn_id: row.turn_id,
            msg_id: row.msg_id,
            error_code: row.error_code,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

impl ReplayedConversationInput {
    fn into_row(self, user_id: &str, conversation_id: &str) -> ConversationInputRow {
        ConversationInputRow {
            id: self.id,
            user_id: user_id.to_owned(),
            conversation_id: conversation_id.to_owned(),
            mode: self.mode,
            status: self.status,
            content: self.content,
            files: self.files,
            inject_skills: self.inject_skills,
            hidden: self.hidden,
            client_key: self.client_key,
            turn_id: self.turn_id,
            msg_id: self.msg_id,
            error_code: self.error_code,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

fn fold_conversation_inputs_with_seed(
    mut inputs: BTreeMap<String, ReplayedConversationInput>,
    events: &[CanonicalJournalEvent],
) -> Vec<ReplayedConversationInput> {
    for event in events {
        let data = &event.payload["data"];
        if event.kind == "InputHeld" {
            let Some(id) = data["input_id"].as_str() else {
                continue;
            };
            let Some(mode) = data["mode"].as_str() else {
                continue;
            };
            let Some(content) = data["content"].as_str() else {
                continue;
            };
            let Some(client_key) = data["client_key"].as_str() else {
                continue;
            };
            inputs
                .entry(id.to_owned())
                .or_insert_with(|| ReplayedConversationInput {
                    id: id.to_owned(),
                    mode: mode.to_owned(),
                    status: "held".to_owned(),
                    content: content.to_owned(),
                    files: serde_json::to_string(&data["files"]).unwrap_or_else(|_| "[]".to_owned()),
                    inject_skills: serde_json::to_string(&data["inject_skills"]).unwrap_or_else(|_| "[]".to_owned()),
                    hidden: data["hidden"].as_bool().unwrap_or(false),
                    client_key: client_key.to_owned(),
                    turn_id: None,
                    msg_id: None,
                    error_code: None,
                    created_at: event.timestamp,
                    updated_at: event.timestamp,
                });
            continue;
        }

        if event.kind == "UserPrompt" {
            let Some(msg_id) = data["msg_id"].as_str() else {
                continue;
            };
            if let Some(input) = inputs.get_mut(msg_id) {
                input.status = "applied".to_owned();
                input.msg_id = Some(msg_id.to_owned());
                input.updated_at = event.timestamp;
            }
            continue;
        }

        let status = match event.kind.as_str() {
            "InputDispatching" => "dispatching",
            "InputAccepted" => "accepted",
            "InputApplied" => "applied",
            "InputCanceled" => "canceled",
            "InputFailed" => "failed",
            _ => continue,
        };
        let Some(id) = data["input_id"].as_str() else {
            continue;
        };
        let Some(input) = inputs.get_mut(id) else {
            continue;
        };
        input.status = status.to_owned();
        input.turn_id = data["turn_id"].as_str().map(str::to_owned).or(input.turn_id.take());
        input.msg_id = data["msg_id"].as_str().map(str::to_owned).or(input.msg_id.take());
        input.error_code = data["error_code"].as_str().map(str::to_owned);
        input.updated_at = event.timestamp;
    }
    inputs.into_values().collect()
}

fn classify_recovery_action(row: &MessageRow) -> StartupRecoveryAction {
    if matches!(row.r#type.as_str(), "tool_call" | "tool_group") {
        return StartupRecoveryAction::SettleToolRow;
    }
    if message_has_visible_content(row) {
        StartupRecoveryAction::FinishVisibleOutput
    } else {
        StartupRecoveryAction::FinishEmptyPlaceholder
    }
}

/// Rewrite a stale tool row's embedded status to its terminal form. Returns
/// `None` when the content needs no change (already terminal, or unparsable —
/// the row-level `finish` still applies either way).
///
/// The two channels speak different status vocabularies (see `ToolGroupStatus`):
/// `tool_call` content is snake_case, `tool_group` entries are PascalCase.
fn settle_tool_row_content(row_type: &str, content: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(content).ok()?;
    match row_type {
        "tool_call" => {
            let stale = value
                .get("status")
                .and_then(|s| s.as_str())
                .is_none_or(|s| !matches!(s, "completed" | "error" | "canceled"));
            if !stale {
                return None;
            }
            value["status"] = serde_json::Value::String("canceled".into());
            Some(value.to_string())
        }
        "tool_group" => {
            let entries = value.as_array_mut()?;
            let mut changed = false;
            for entry in entries {
                let stale = entry
                    .get("status")
                    .and_then(|s| s.as_str())
                    .is_none_or(|s| !matches!(s, "Success" | "Error" | "Canceled"));
                if stale {
                    entry["status"] = serde_json::Value::String("Canceled".into());
                    changed = true;
                }
            }
            changed.then(|| value.to_string())
        }
        _ => None,
    }
}

fn message_has_visible_content(row: &MessageRow) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&row.content) else {
        return !row.content.trim().is_empty();
    };

    value
        .get("content")
        .and_then(|content| content.as_str())
        .is_some_and(|content| !content.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use aionui_db::models::MessageRow;

    use super::*;

    fn journal_event(sequence: u64, timestamp: i64, kind: &str, data: serde_json::Value) -> CanonicalJournalEvent {
        CanonicalJournalEvent {
            schema_version: 1,
            runtime_epoch: "test-runtime".into(),
            event_id: format!("event-{sequence}"),
            conversation_id: "conv-1".into(),
            sequence,
            timestamp,
            kind: kind.into(),
            payload: serde_json::json!({ "data": data }),
        }
    }

    #[test]
    fn recovered_tool_identifiers_excludes_tool_payload() {
        let row = MessageRow {
            id: "message-1".into(),
            conversation_id: "conv-1".into(),
            msg_id: Some("call-fallback".into()),
            r#type: "tool_call".into(),
            content: serde_json::json!({
                "execution_id": "exec-1",
                "tool_call_id": "call-1",
                "turn_id": "turn-1",
                "input": { "secret": "must-not-be-journaled" },
                "output": "must-not-be-journaled",
            })
            .to_string(),
            position: None,
            status: Some("work".into()),
            hidden: false,
            created_at: 1,
            backend_turn_id: None,
        };

        let identifiers = recovered_tool_identifiers(&row);
        assert_eq!(identifiers.execution_id.as_deref(), Some("exec-1"));
        assert_eq!(identifiers.call_id.as_deref(), Some("call-1"));
        assert_eq!(identifiers.turn_id.as_deref(), Some("turn-1"));
    }

    #[test]
    fn journal_fold_rebuilds_input_and_applies_lifecycle_edges() {
        let events = vec![
            journal_event(
                1,
                10,
                "InputHeld",
                serde_json::json!({
                    "input_id": "input-1",
                    "mode": "followup",
                    "content": "hello",
                    "files": [{"name": "note.txt"}],
                    "inject_skills": ["review"],
                    "hidden": true,
                    "client_key": "client-1"
                }),
            ),
            journal_event(
                2,
                20,
                "InputDispatching",
                serde_json::json!({"input_id": "input-1", "status": "dispatching", "turn_id": "turn-1"}),
            ),
            journal_event(
                3,
                30,
                "InputApplied",
                serde_json::json!({"input_id": "input-1", "status": "applied", "msg_id": "msg-1"}),
            ),
        ];

        let inputs = fold_conversation_inputs(&events);
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].status, "applied");
        assert_eq!(inputs[0].turn_id.as_deref(), Some("turn-1"));
        assert_eq!(inputs[0].msg_id.as_deref(), Some("msg-1"));
        assert_eq!(inputs[0].created_at, 10);
        assert_eq!(inputs[0].updated_at, 30);
        assert!(inputs[0].hidden);
        assert!(inputs[0].files.contains("note.txt"));
    }

    #[test]
    fn journal_fold_recovers_legacy_user_prompt_and_ignores_orphan_status() {
        let events = vec![
            journal_event(
                1,
                10,
                "InputFailed",
                serde_json::json!({"input_id": "missing", "status": "failed"}),
            ),
            journal_event(
                2,
                20,
                "InputHeld",
                serde_json::json!({
                    "input_id": "input-1",
                    "mode": "inject",
                    "content": "context",
                    "files": [],
                    "inject_skills": [],
                    "client_key": "client-1"
                }),
            ),
            journal_event(
                3,
                30,
                "UserPrompt",
                serde_json::json!({"msg_id": "input-1", "content": "context"}),
            ),
        ];

        let inputs = fold_conversation_inputs(&events);
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].status, "applied");
        assert_eq!(inputs[0].msg_id.as_deref(), Some("input-1"));
    }

    #[test]
    fn visible_text_finishes_as_visible_output() {
        let row = MessageRow {
            id: "msg-1".into(),
            conversation_id: "conv-1".into(),
            msg_id: Some("msg-1".into()),
            r#type: "text".into(),
            content: serde_json::json!({ "content": "hello" }).to_string(),
            position: Some("left".into()),
            status: Some("work".into()),
            hidden: false,
            created_at: 1,
            backend_turn_id: None,
        };

        assert_eq!(
            classify_recovery_action(&row),
            StartupRecoveryAction::FinishVisibleOutput
        );
    }

    #[test]
    fn stale_tool_rows_classify_as_settle() {
        for ty in ["tool_call", "tool_group"] {
            let row = MessageRow {
                id: "m".into(),
                conversation_id: "c".into(),
                msg_id: Some("m".into()),
                r#type: ty.into(),
                content: "{}".into(),
                position: Some("left".into()),
                status: Some("work".into()),
                hidden: false,
                created_at: 1,
                backend_turn_id: None,
            };
            assert_eq!(classify_recovery_action(&row), StartupRecoveryAction::SettleToolRow);
        }
    }

    /// The hard-kill residue (audit 2026-08-04): rows whose CONTENT still says
    /// running must be rewritten — the frontend renders the embedded status, so
    /// a row-level `finish` alone leaves the View Steps spinner alive.
    #[test]
    fn settle_rewrites_running_content_and_leaves_terminal_content_alone() {
        // tool_call: snake_case vocabulary.
        let card = serde_json::json!({"call_id": "t1", "name": "Bash", "status": "running"}).to_string();
        let out = settle_tool_row_content("tool_call", &card).expect("running → rewritten");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "canceled");
        assert_eq!(v["name"], "Bash", "only the status changes");

        let done = serde_json::json!({"call_id": "t1", "status": "completed"}).to_string();
        assert!(
            settle_tool_row_content("tool_call", &done).is_none(),
            "terminal content is left untouched"
        );

        // tool_group: PascalCase vocabulary, per entry.
        let group = serde_json::json!([
            {"call_id": "a", "name": "run:A", "status": "Executing"},
            {"call_id": "b", "name": "run:B", "status": "Success"}
        ])
        .to_string();
        let out = settle_tool_row_content("tool_group", &group).expect("one executing entry");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v[0]["status"], "Canceled");
        assert_eq!(v[1]["status"], "Success", "terminal entries keep their outcome");

        let all_done = serde_json::json!([{"call_id": "a", "status": "Success"}]).to_string();
        assert!(settle_tool_row_content("tool_group", &all_done).is_none());
    }

    #[test]
    fn empty_text_finishes_as_hidden_placeholder() {
        let row = MessageRow {
            id: "msg-1".into(),
            conversation_id: "conv-1".into(),
            msg_id: Some("msg-1".into()),
            r#type: "text".into(),
            content: serde_json::json!({ "content": "" }).to_string(),
            position: Some("left".into()),
            status: Some("work".into()),
            hidden: false,
            created_at: 1,
            backend_turn_id: None,
        };

        assert_eq!(
            classify_recovery_action(&row),
            StartupRecoveryAction::FinishEmptyPlaceholder
        );
    }
}
