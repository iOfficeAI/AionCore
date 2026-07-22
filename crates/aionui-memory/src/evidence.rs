use std::collections::{BTreeMap, BTreeSet};

use aionui_api_types::{
    ExistingMemoryEntryInput, MemoryEntryKind, MemorySourceMessageInput, MemorySourceMessageRole,
    MemorySourceTurnInput, MemorySummary, MemoryUpdateConversationInput, MemoryUpdateInput,
};
use aionui_db::models::{ConversationRow, MemoryEntryRow, MessageRow};
use serde_json::Value;

use crate::{
    MemoryError,
    sanitizer::{MAX_EXISTING_ENTRIES, MAX_STRING_LENGTH, is_user_context_content, sanitize_text},
};

pub use crate::sanitizer::{MAX_EVIDENCE_BYTES, MAX_EVIDENCE_MESSAGES, MAX_EVIDENCE_TURNS};

/// Canonical database rows and trusted job bounds used to build an App Operations payload.
#[derive(Debug, Clone)]
pub struct EvidenceBuildRequest {
    pub conversation: ConversationRow,
    pub messages: Vec<MessageRow>,
    pub previous_summary: Option<MemorySummary>,
    /// Canonical summary cursor. Only turn IDs after this cursor are included.
    pub summary_cursor: Option<String>,
    /// Ordered canonical turn IDs claimed by the durable job.
    pub claimed_turn_ids: Vec<String>,
    /// Current active entries preselected by the domain for reconciliation.
    pub existing_entries: Vec<MemoryEntryRow>,
}

/// Builds size-bounded, sanitized evidence without accepting renderer-supplied transcripts.
#[derive(Debug, Clone, Default)]
pub struct EvidenceBuilder;

impl EvidenceBuilder {
    /// Reconstructs a task input from canonical rows and trusted conversation metadata.
    pub fn build(&self, request: EvidenceBuildRequest) -> Result<MemoryUpdateInput, MemoryError> {
        if request.claimed_turn_ids.len() > MAX_EVIDENCE_TURNS || request.existing_entries.len() > MAX_EXISTING_ENTRIES
        {
            return Err(MemoryError::InvalidInput);
        }

        let turn_ids = selected_turn_ids(&request.claimed_turn_ids, request.summary_cursor.as_deref())?;
        let scope = scope_from_conversation(&request.conversation)?;
        let source_turns = source_turns_from_rows(&request.messages, &turn_ids)?;
        let existing_entries = existing_entries_from_rows(request.existing_entries)?;

        Ok(MemoryUpdateInput {
            conversation: MemoryUpdateConversationInput {
                id: request.conversation.id,
                project_id: scope.project_id,
                workspace_key: scope.workspace_key,
            },
            previous_summary: request.previous_summary.and_then(sanitize_summary),
            existing_entries,
            source_turns,
        })
    }
}

#[derive(Default)]
struct ConversationScope {
    project_id: Option<String>,
    workspace_key: Option<String>,
}

fn selected_turn_ids(claimed_turn_ids: &[String], summary_cursor: Option<&str>) -> Result<Vec<String>, MemoryError> {
    if claimed_turn_ids.iter().any(|turn_id| !valid_string(turn_id)) {
        return Err(MemoryError::InvalidInput);
    }

    let start = match summary_cursor {
        Some(cursor) => claimed_turn_ids
            .iter()
            .position(|turn_id| turn_id == cursor)
            .map(|index| index + 1)
            .ok_or(MemoryError::InvalidInput)?,
        None => 0,
    };

    let selected = claimed_turn_ids[start..].to_vec();
    let unique = selected.iter().collect::<BTreeSet<_>>();
    if unique.len() != selected.len() {
        return Err(MemoryError::InvalidInput);
    }
    Ok(selected)
}

fn scope_from_conversation(conversation: &ConversationRow) -> Result<ConversationScope, MemoryError> {
    let extra: Value = serde_json::from_str(&conversation.extra).map_err(|_| MemoryError::InvalidInput)?;
    let object = extra.as_object().ok_or(MemoryError::InvalidInput)?;

    let project_id = optional_metadata_string(object.get("project_id"))?;
    let workspace_key = optional_metadata_string(object.get("workspace"))?
        .map(normalize_workspace_key)
        .transpose()?;

    Ok(ConversationScope {
        project_id,
        workspace_key,
    })
}

fn optional_metadata_string(value: Option<&Value>) -> Result<Option<String>, MemoryError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if valid_string(value) => Ok(Some(value.trim().to_owned())),
        Some(_) => Err(MemoryError::InvalidInput),
    }
}

fn normalize_workspace_key(workspace: String) -> Result<String, MemoryError> {
    let mut components = Vec::new();
    let absolute = workspace.starts_with('/') || workspace.starts_with('\\');
    let normalized_separators = workspace.replace('\\', "/");
    for component in normalized_separators.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(MemoryError::InvalidInput);
                }
            }
            component => components.push(component),
        }
    }
    if components.is_empty() {
        return Err(MemoryError::InvalidInput);
    }
    let normalized = components.join("/");
    let normalized = if absolute { format!("/{normalized}") } else { normalized };
    if valid_string(&normalized) {
        Ok(normalized)
    } else {
        Err(MemoryError::InvalidInput)
    }
}

fn source_turns_from_rows(
    messages: &[MessageRow],
    turn_ids: &[String],
) -> Result<Vec<MemorySourceTurnInput>, MemoryError> {
    let mut grouped = BTreeMap::<String, Vec<MemorySourceMessageInput>>::new();
    let selected_turn_ids = turn_ids.iter().collect::<BTreeSet<_>>();
    let mut message_count = 0_usize;
    let mut evidence_bytes = 0_usize;

    for message in messages {
        let Some(turn_id) = message.turn_id.as_deref() else {
            continue;
        };
        if !selected_turn_ids.contains(&turn_id.to_owned()) || should_exclude_message(message) {
            continue;
        }

        let Some(content) = visible_text_content(message)? else {
            continue;
        };
        let content = sanitize_text(&content);
        if content.trim().is_empty() || is_user_context_content(&content) {
            continue;
        }
        if !valid_string(&content) {
            return Err(MemoryError::InvalidInput);
        }

        message_count += 1;
        evidence_bytes += content.len();
        if message_count > MAX_EVIDENCE_MESSAGES || evidence_bytes > MAX_EVIDENCE_BYTES {
            return Err(MemoryError::InvalidInput);
        }

        let role = message_role(message).ok_or(MemoryError::InvalidInput)?;
        grouped
            .entry(turn_id.to_owned())
            .or_default()
            .push(MemorySourceMessageInput {
                message_id: message.id.clone(),
                role,
                content,
            });
    }

    Ok(turn_ids
        .iter()
        .filter_map(|turn_id| {
            grouped.remove(turn_id).map(|messages| MemorySourceTurnInput {
                turn_id: turn_id.clone(),
                messages,
            })
        })
        .collect())
}

fn should_exclude_message(message: &MessageRow) -> bool {
    if message.hidden {
        return true;
    }
    let message_type = message.r#type.trim().to_ascii_lowercase();
    message_type != "text"
        || message_type.contains("tool")
        || message_type.contains("permission")
        || message_type.contains("file")
}

fn visible_text_content(message: &MessageRow) -> Result<Option<String>, MemoryError> {
    let value: Value = serde_json::from_str(&message.content).map_err(|_| MemoryError::InvalidInput)?;
    Ok(value.get("content").and_then(Value::as_str).map(str::to_owned))
}

fn message_role(message: &MessageRow) -> Option<MemorySourceMessageRole> {
    match message.position.as_deref() {
        Some("right") => Some(MemorySourceMessageRole::User),
        Some("left") => Some(MemorySourceMessageRole::Assistant),
        _ => None,
    }
}

fn existing_entries_from_rows(rows: Vec<MemoryEntryRow>) -> Result<Vec<ExistingMemoryEntryInput>, MemoryError> {
    rows.into_iter()
        .filter(|row| row.state == "active")
        .filter_map(|mut row| row.content.take().map(|content| (row, content)))
        .filter_map(|(row, content)| {
            let content = sanitize_text(&content);
            (!content.trim().is_empty() && !is_user_context_content(&content)).then_some((row, content))
        })
        .map(|(row, content)| {
            if !valid_string(&row.id) || !valid_string(&row.stable_key) || !valid_string(&content) {
                return Err(MemoryError::InvalidInput);
            }
            Ok(ExistingMemoryEntryInput {
                id: row.id,
                kind: entry_kind(&row.kind)?,
                stable_key: row.stable_key,
                content,
                pinned: row.pinned,
                user_edited: row.user_edited,
            })
        })
        .collect()
}

fn entry_kind(kind: &str) -> Result<MemoryEntryKind, MemoryError> {
    match kind {
        "decision" => Ok(MemoryEntryKind::Decision),
        "outcome" => Ok(MemoryEntryKind::Outcome),
        "artifact" => Ok(MemoryEntryKind::Artifact),
        "issue" => Ok(MemoryEntryKind::Issue),
        "next_step" => Ok(MemoryEntryKind::NextStep),
        "work_constraint" => Ok(MemoryEntryKind::WorkConstraint),
        _ => Err(MemoryError::InvalidInput),
    }
}

fn sanitize_summary(summary: MemorySummary) -> Option<MemorySummary> {
    let goal = sanitized_summary_value(summary.goal);
    let current_state = sanitize_summary_values(summary.current_state);
    let decisions = sanitize_summary_values(summary.decisions);
    let artifacts = sanitize_summary_values(summary.artifacts);
    let issues = sanitize_summary_values(summary.issues);
    let next_steps = sanitize_summary_values(summary.next_steps);
    let work_constraints = sanitize_summary_values(summary.work_constraints);
    (!goal.is_empty()
        || !current_state.is_empty()
        || !decisions.is_empty()
        || !artifacts.is_empty()
        || !issues.is_empty()
        || !next_steps.is_empty()
        || !work_constraints.is_empty())
    .then_some(MemorySummary {
        goal,
        current_state,
        decisions,
        artifacts,
        issues,
        next_steps,
        work_constraints,
    })
}

fn sanitize_summary_values(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(sanitized_summary_value)
        .filter(|value| !value.is_empty())
        .collect()
}

fn sanitized_summary_value(value: String) -> String {
    let value = sanitize_text(&value);
    if valid_string(&value) && !is_user_context_content(&value) {
        value
    } else {
        String::new()
    }
}

fn valid_string(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= MAX_STRING_LENGTH
}

#[cfg(test)]
mod tests {
    use aionui_api_types::MemorySummary;
    use aionui_db::models::{ConversationRow, MemoryEntryRow, MessageRow};
    use serde_json::json;

    use super::{EvidenceBuildRequest, EvidenceBuilder, MAX_EVIDENCE_BYTES, MAX_EVIDENCE_MESSAGES, MAX_EVIDENCE_TURNS};

    #[test]
    fn reconstructs_only_safe_canonical_evidence_after_the_summary_cursor() {
        let request = EvidenceBuildRequest {
            conversation: conversation(json!({
                "project_id": "project-alpha",
                "workspace": "/work/alpha/",
            })),
            messages: vec![
                text_message("before", "turn-0", "right", "old transcript"),
                text_message("user", "turn-1", "right", "Ship the report password=do-not-store"),
                text_message("assistant", "turn-1", "left", "Created /work/alpha/report.md"),
                hidden_message("hidden", "turn-1", "hidden evidence"),
                raw_message("permission", "turn-1", "permission_prompt", "permission payload"),
                raw_message("tool", "turn-1", "tool_call", "raw tool input and output"),
                raw_message("file", "turn-1", "file", "data:application/octet-stream;base64,AAAA"),
                text_message(
                    "profile",
                    "turn-2",
                    "right",
                    "My name is Ada and I prefer concise responses.",
                ),
            ],
            previous_summary: Some(summary()),
            summary_cursor: Some("turn-0".into()),
            claimed_turn_ids: vec!["turn-0".into(), "turn-1".into(), "turn-2".into()],
            existing_entries: vec![active_entry("active"), superseded_entry("superseded")],
        };

        let output = EvidenceBuilder::default().build(request).unwrap();

        assert_eq!(output.conversation.id, "conversation-1");
        assert_eq!(output.conversation.project_id.as_deref(), Some("project-alpha"));
        assert_eq!(output.conversation.workspace_key.as_deref(), Some("/work/alpha"));
        assert_eq!(output.previous_summary, Some(summary()));
        assert_eq!(output.existing_entries.len(), 1);
        assert_eq!(output.existing_entries[0].id, "active");
        assert_eq!(output.source_turns.len(), 1);
        assert_eq!(output.source_turns[0].turn_id, "turn-1");
        assert_eq!(output.source_turns[0].messages.len(), 2);

        let evidence = output.source_turns[0]
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for excluded in [
            "old transcript",
            "do-not-store",
            "hidden evidence",
            "permission payload",
            "raw tool input and output",
            "application/octet-stream",
            "My name is Ada",
        ] {
            assert!(!evidence.contains(excluded));
        }
        assert!(evidence.contains("[REDACTED]"));
        assert!(evidence.contains("Created /work/alpha/report.md"));
    }

    #[test]
    fn rejects_excess_evidence_limits_deterministically() {
        let builder = EvidenceBuilder::default();

        let too_many_turns = EvidenceBuildRequest {
            conversation: conversation(json!({})),
            messages: Vec::new(),
            previous_summary: None,
            summary_cursor: None,
            claimed_turn_ids: (0..=MAX_EVIDENCE_TURNS).map(|index| format!("turn-{index}")).collect(),
            existing_entries: Vec::new(),
        };
        assert_eq!(
            builder.build(too_many_turns.clone()).unwrap_err(),
            builder.build(too_many_turns).unwrap_err()
        );

        let too_many_messages = EvidenceBuildRequest {
            conversation: conversation(json!({})),
            messages: (0..=MAX_EVIDENCE_MESSAGES)
                .map(|index| text_message(&format!("message-{index}"), "turn-1", "right", "safe"))
                .collect(),
            previous_summary: None,
            summary_cursor: None,
            claimed_turn_ids: vec!["turn-1".into()],
            existing_entries: Vec::new(),
        };
        assert!(builder.build(too_many_messages).is_err());

        let too_many_bytes = EvidenceBuildRequest {
            conversation: conversation(json!({})),
            messages: vec![text_message(
                "large",
                "turn-1",
                "right",
                &"x".repeat(MAX_EVIDENCE_BYTES + 1),
            )],
            previous_summary: None,
            summary_cursor: None,
            claimed_turn_ids: vec!["turn-1".into()],
            existing_entries: Vec::new(),
        };
        assert!(builder.build(too_many_bytes).is_err());
    }

    #[test]
    fn retains_the_canonical_claimed_turn_order() {
        let output = EvidenceBuilder::default()
            .build(EvidenceBuildRequest {
                conversation: conversation(json!({})),
                messages: vec![
                    text_message("b", "turn-b", "right", "second claimed turn"),
                    text_message("a", "turn-a", "left", "third claimed turn"),
                ],
                previous_summary: None,
                summary_cursor: Some("turn-cursor".into()),
                claimed_turn_ids: vec!["turn-cursor".into(), "turn-b".into(), "turn-a".into()],
                existing_entries: Vec::new(),
            })
            .unwrap();

        assert_eq!(
            output
                .source_turns
                .iter()
                .map(|turn| turn.turn_id.as_str())
                .collect::<Vec<_>>(),
            ["turn-b", "turn-a"]
        );
    }

    fn conversation(extra: serde_json::Value) -> ConversationRow {
        ConversationRow {
            id: "conversation-1".into(),
            user_id: "user-1".into(),
            name: "Conversation".into(),
            r#type: "acp".into(),
            extra: extra.to_string(),
            model: None,
            status: Some("finished".into()),
            source: Some("aionui".into()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 1,
            updated_at: 2,
        }
    }

    fn text_message(id: &str, turn_id: &str, position: &str, content: &str) -> MessageRow {
        raw_message(id, turn_id, "text", &json!({ "content": content }).to_string()).with_position(position)
    }

    fn hidden_message(id: &str, turn_id: &str, content: &str) -> MessageRow {
        let mut message = text_message(id, turn_id, "right", content);
        message.hidden = true;
        message
    }

    fn raw_message(id: &str, turn_id: &str, message_type: &str, content: &str) -> MessageRow {
        MessageRow {
            id: id.into(),
            conversation_id: "conversation-1".into(),
            turn_id: Some(turn_id.into()),
            msg_id: Some(id.into()),
            r#type: message_type.into(),
            content: content.into(),
            position: None,
            status: Some("finish".into()),
            hidden: false,
            created_at: 1,
        }
    }

    trait WithPosition {
        fn with_position(self, position: &str) -> Self;
    }

    impl WithPosition for MessageRow {
        fn with_position(mut self, position: &str) -> Self {
            self.position = Some(position.into());
            self
        }
    }

    fn active_entry(id: &str) -> MemoryEntryRow {
        MemoryEntryRow {
            id: id.into(),
            user_id: "user-1".into(),
            project_id: None,
            workspace_key: None,
            kind: "decision".into(),
            stable_key: "report".into(),
            fingerprint: "fingerprint".into(),
            content: Some("Keep the report format.".into()),
            state: "active".into(),
            pinned: false,
            user_edited: false,
            supersedes_id: None,
            conflict_group_id: None,
            schema_version: 1,
            deleted_at: None,
            created_at: 1,
            updated_at: 1,
            sources: Vec::new(),
        }
    }

    fn superseded_entry(id: &str) -> MemoryEntryRow {
        let mut entry = active_entry(id);
        entry.state = "superseded".into();
        entry
    }

    fn summary() -> MemorySummary {
        MemorySummary {
            goal: "Ship report".into(),
            current_state: vec!["Drafted".into()],
            decisions: Vec::new(),
            artifacts: Vec::new(),
            issues: Vec::new(),
            next_steps: Vec::new(),
            work_constraints: Vec::new(),
        }
    }
}
