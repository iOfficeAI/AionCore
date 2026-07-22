use std::collections::{BTreeMap, BTreeSet};

use aionui_api_types::{
    ExistingMemoryEntryInput, MemoryEntryKind, MemorySourceMessageInput, MemorySourceMessageRole,
    MemorySourceTurnInput, MemorySummary, MemoryUpdateConversationInput, MemoryUpdateInput,
};
use aionui_db::models::{ConversationRow, MemoryEntryRow, MessageRow};
use serde_json::Value;

use crate::{
    MemoryError,
    sanitizer::{
        MAX_EXISTING_ENTRIES, MAX_STRING_LENGTH, MAX_SUMMARY_BYTES, MAX_SUMMARY_ITEMS, sanitize_text,
        strip_user_context_sentences,
    },
};

pub use crate::sanitizer::{MAX_EVIDENCE_BYTES, MAX_EVIDENCE_MESSAGES, MAX_EVIDENCE_TURNS};

/// Canonical database rows and trusted job bounds used to build an App Operations payload.
#[derive(Debug, Clone)]
pub struct EvidenceBuildRequest {
    pub conversation: ConversationRow,
    pub messages: Vec<MessageRow>,
    pub previous_summary: Option<MemorySummary>,
    /// Canonical summary cursor before the exact queued turns.
    pub summary_cursor: Option<String>,
    /// Ordered canonical turn IDs claimed by the durable job.
    pub claimed_turn_ids: Vec<String>,
    /// Current active entries preselected by the domain for reconciliation.
    pub existing_entries: Vec<MemoryEntryRow>,
}

/// Builds size-bounded, sanitized evidence without accepting renderer-supplied transcripts.
#[derive(Debug, Clone, Default)]
pub(crate) struct EvidenceBuilder;

impl EvidenceBuilder {
    /// Reconstructs a task input from canonical rows and trusted conversation metadata.
    pub(crate) fn build(&self, request: EvidenceBuildRequest) -> Result<MemoryUpdateInput, MemoryError> {
        if !valid_identifier(&request.conversation.id) || !valid_identifier(&request.conversation.user_id) {
            return Err(MemoryError::InvalidInput);
        }

        let turn_ids = selected_turn_ids(&request.claimed_turn_ids)?;
        let scope = scope_from_conversation(&request.conversation)?;
        let source_turns = source_turns_from_rows(&request.conversation, &request.messages, &turn_ids)?;
        let existing_entries = existing_entries_from_rows(request.existing_entries, &request.conversation, &scope)?;
        let previous_summary = request.previous_summary.map(sanitize_summary).transpose()?.flatten();

        Ok(MemoryUpdateInput {
            conversation: MemoryUpdateConversationInput {
                id: request.conversation.id,
                project_id: scope.project_id,
                workspace_key: scope.workspace_key,
            },
            previous_summary,
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

fn selected_turn_ids(claimed_turn_ids: &[String]) -> Result<Vec<String>, MemoryError> {
    if claimed_turn_ids.iter().any(|turn_id| !valid_identifier(turn_id))
        || claimed_turn_ids.iter().collect::<BTreeSet<_>>().len() != claimed_turn_ids.len()
    {
        return Err(MemoryError::InvalidInput);
    }

    if claimed_turn_ids.len() > MAX_EVIDENCE_TURNS {
        return Err(MemoryError::InvalidInput);
    }
    Ok(claimed_turn_ids.to_vec())
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
    conversation: &ConversationRow,
    messages: &[MessageRow],
    turn_ids: &[String],
) -> Result<Vec<MemorySourceTurnInput>, MemoryError> {
    let mut grouped = BTreeMap::<String, Vec<(i64, String, MemorySourceMessageInput)>>::new();
    let selected_turn_ids = turn_ids.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let mut message_count = 0_usize;
    let mut evidence_bytes = 0_usize;

    for message in messages {
        if message.conversation_id != conversation.id {
            return Err(MemoryError::InvalidInput);
        }
        let Some(turn_id) = message.turn_id.as_deref() else {
            continue;
        };
        if !selected_turn_ids.contains(turn_id) || should_exclude_message(message) {
            continue;
        }

        let Some(content) = visible_text_content(message)? else {
            continue;
        };
        let content = strip_user_context_sentences(&sanitize_text(&content));
        if content.trim().is_empty() {
            continue;
        }
        if !valid_string(&content) || !valid_identifier(&message.id) {
            return Err(MemoryError::InvalidInput);
        }

        message_count += 1;
        evidence_bytes += content.len();
        if message_count > MAX_EVIDENCE_MESSAGES || evidence_bytes > MAX_EVIDENCE_BYTES {
            return Err(MemoryError::InvalidInput);
        }

        let role = message_role(message).ok_or(MemoryError::InvalidInput)?;
        grouped.entry(turn_id.to_owned()).or_default().push((
            message.created_at,
            message.id.clone(),
            MemorySourceMessageInput {
                message_id: message.id.clone(),
                role,
                content,
            },
        ));
    }

    Ok(turn_ids
        .iter()
        .filter_map(|turn_id| {
            grouped.remove(turn_id).map(|mut messages| {
                messages.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
                MemorySourceTurnInput {
                    turn_id: turn_id.clone(),
                    messages: messages.into_iter().map(|(_, _, message)| message).collect(),
                }
            })
        })
        .collect())
}

fn should_exclude_message(message: &MessageRow) -> bool {
    if message.hidden {
        return true;
    }
    let message_type = message.r#type.trim().to_ascii_lowercase();
    !matches!(message_type.as_str(), "text" | "artifact" | "tool_result_summary")
        || message.status.as_deref() != Some("finish")
}

fn visible_text_content(message: &MessageRow) -> Result<Option<String>, MemoryError> {
    let value: Value = serde_json::from_str(&message.content).map_err(|_| MemoryError::InvalidInput)?;
    Ok(value
        .get("content")
        .or_else(|| value.get("summary"))
        .and_then(Value::as_str)
        .map(str::to_owned))
}

fn message_role(message: &MessageRow) -> Option<MemorySourceMessageRole> {
    match message.position.as_deref() {
        Some("right") => Some(MemorySourceMessageRole::User),
        Some("left") => Some(MemorySourceMessageRole::Assistant),
        _ => None,
    }
}

fn existing_entries_from_rows(
    rows: Vec<MemoryEntryRow>,
    conversation: &ConversationRow,
    scope: &ConversationScope,
) -> Result<Vec<ExistingMemoryEntryInput>, MemoryError> {
    let mut entries = Vec::new();
    for row in rows {
        if row.user_id != conversation.user_id || !entry_scope_is_compatible(&row, scope) {
            return Err(MemoryError::InvalidInput);
        }
        if row.state != "active" {
            continue;
        }
        let Some(content) = row.content else {
            continue;
        };
        let content = strip_user_context_sentences(&sanitize_text(&content));
        if content.trim().is_empty() {
            continue;
        }
        if !valid_identifier(&row.id) || !valid_string(&row.stable_key) || !valid_string(&content) {
            return Err(MemoryError::InvalidInput);
        }
        entries.push(ExistingMemoryEntryInput {
            id: row.id,
            kind: entry_kind(&row.kind)?,
            stable_key: row.stable_key,
            content,
            pinned: row.pinned,
            user_edited: row.user_edited,
        });
    }
    if entries.len() > MAX_EXISTING_ENTRIES {
        return Err(MemoryError::InvalidInput);
    }
    Ok(entries)
}

fn entry_scope_is_compatible(row: &MemoryEntryRow, scope: &ConversationScope) -> bool {
    row.project_id
        .as_deref()
        .is_none_or(|project_id| scope.project_id.as_deref() == Some(project_id))
        && row
            .workspace_key
            .as_deref()
            .is_none_or(|workspace_key| scope.workspace_key.as_deref() == Some(workspace_key))
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

fn sanitize_summary(summary: MemorySummary) -> Result<Option<MemorySummary>, MemoryError> {
    let goal = sanitized_summary_value(summary.goal)?.unwrap_or_default();
    let current_state = sanitize_summary_values(summary.current_state)?;
    let decisions = sanitize_summary_values(summary.decisions)?;
    let artifacts = sanitize_summary_values(summary.artifacts)?;
    let issues = sanitize_summary_values(summary.issues)?;
    let next_steps = sanitize_summary_values(summary.next_steps)?;
    let work_constraints = sanitize_summary_values(summary.work_constraints)?;
    let summary_bytes = goal.len()
        + current_state.iter().map(String::len).sum::<usize>()
        + decisions.iter().map(String::len).sum::<usize>()
        + artifacts.iter().map(String::len).sum::<usize>()
        + issues.iter().map(String::len).sum::<usize>()
        + next_steps.iter().map(String::len).sum::<usize>()
        + work_constraints.iter().map(String::len).sum::<usize>();
    let summary_items = usize::from(!goal.is_empty())
        + current_state.len()
        + decisions.len()
        + artifacts.len()
        + issues.len()
        + next_steps.len()
        + work_constraints.len();
    if summary_items > MAX_SUMMARY_ITEMS || summary_bytes > MAX_SUMMARY_BYTES {
        return Err(MemoryError::InvalidInput);
    }
    if !goal.is_empty()
        || !current_state.is_empty()
        || !decisions.is_empty()
        || !artifacts.is_empty()
        || !issues.is_empty()
        || !next_steps.is_empty()
        || !work_constraints.is_empty()
    {
        Ok(Some(MemorySummary {
            goal,
            current_state,
            decisions,
            artifacts,
            issues,
            next_steps,
            work_constraints,
        }))
    } else {
        Ok(None)
    }
}

fn sanitize_summary_values(values: Vec<String>) -> Result<Vec<String>, MemoryError> {
    values
        .into_iter()
        .map(sanitized_summary_value)
        .filter_map(Result::transpose)
        .collect()
}

fn sanitized_summary_value(value: String) -> Result<Option<String>, MemoryError> {
    let value = strip_user_context_sentences(&sanitize_text(&value));
    if value.trim().is_empty() {
        Ok(None)
    } else if valid_string(&value) {
        Ok(Some(value))
    } else {
        Err(MemoryError::InvalidInput)
    }
}

fn valid_string(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= MAX_STRING_LENGTH
}

fn valid_identifier(value: &str) -> bool {
    valid_string(value)
}

#[cfg(test)]
mod tests {
    use aionui_api_types::MemorySummary;
    use aionui_db::models::{ConversationRow, MemoryEntryRow, MessageRow};
    use serde_json::json;

    use super::{EvidenceBuildRequest, EvidenceBuilder, MAX_EVIDENCE_BYTES, MAX_EVIDENCE_MESSAGES, MAX_EVIDENCE_TURNS};

    #[test]
    fn reconstructs_only_safe_canonical_evidence_from_exact_queued_turns() {
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
            claimed_turn_ids: vec!["turn-1".into(), "turn-2".into()],
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
            builder.build(too_many_turns).unwrap_err(),
            crate::MemoryError::InvalidInput
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
        assert_eq!(
            builder.build(too_many_messages).unwrap_err(),
            crate::MemoryError::InvalidInput
        );

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
        assert_eq!(
            builder.build(too_many_bytes).unwrap_err(),
            crate::MemoryError::InvalidInput
        );
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

    #[test]
    fn rejects_mixed_canonical_rows_and_scope_incompatible_entries() {
        let builder = EvidenceBuilder::default();
        let request = EvidenceBuildRequest {
            conversation: conversation(json!({ "project_id": "project-a", "workspace": "/work/a" })),
            messages: vec![MessageRow {
                conversation_id: "other-conversation".into(),
                ..text_message("foreign-message", "turn-1", "right", "foreign evidence")
            }],
            previous_summary: None,
            summary_cursor: None,
            claimed_turn_ids: vec!["turn-1".into()],
            existing_entries: Vec::new(),
        };
        assert_eq!(builder.build(request).unwrap_err(), crate::MemoryError::InvalidInput);

        let mut foreign_entry = active_entry("foreign-entry");
        foreign_entry.user_id = "other-user".into();
        let request = EvidenceBuildRequest {
            conversation: conversation(json!({ "project_id": "project-a", "workspace": "/work/a" })),
            messages: Vec::new(),
            previous_summary: None,
            summary_cursor: None,
            claimed_turn_ids: Vec::new(),
            existing_entries: vec![foreign_entry],
        };
        assert_eq!(builder.build(request).unwrap_err(), crate::MemoryError::InvalidInput);

        let mut foreign_scope = active_entry("foreign-scope");
        foreign_scope.project_id = Some("project-b".into());
        let request = EvidenceBuildRequest {
            conversation: conversation(json!({ "project_id": "project-a", "workspace": "/work/a" })),
            messages: Vec::new(),
            previous_summary: None,
            summary_cursor: None,
            claimed_turn_ids: Vec::new(),
            existing_entries: vec![foreign_scope],
        };
        assert_eq!(builder.build(request).unwrap_err(), crate::MemoryError::InvalidInput);
    }

    #[test]
    fn removes_user_context_sentences_but_keeps_work_local_preferences_and_http_outcomes() {
        let output = EvidenceBuilder::default()
            .build(EvidenceBuildRequest {
                conversation: conversation(json!({})),
                messages: vec![text_message(
                    "mixed-context",
                    "turn-1",
                    "right",
                    "My name is Ada. Call me Ada; I prefer concise responses. Prefer option B for deployment. Always respond with HTTP 503.",
                )],
                previous_summary: None,
                summary_cursor: None,
                claimed_turn_ids: vec!["turn-1".into()],
                existing_entries: Vec::new(),
            })
            .unwrap();

        let evidence = &output.source_turns[0].messages[0].content;
        for excluded in ["My name is Ada", "Call me Ada", "I prefer concise responses"] {
            assert!(!evidence.contains(excluded));
        }
        assert!(evidence.contains("Prefer option B for deployment"));
        assert!(evidence.contains("Always respond with HTTP 503"));
    }

    #[test]
    fn rejects_duplicate_claims_and_treats_the_summary_cursor_as_prior_state() {
        let builder = EvidenceBuilder::default();
        let duplicate_cursor = EvidenceBuildRequest {
            conversation: conversation(json!({})),
            messages: Vec::new(),
            previous_summary: None,
            summary_cursor: Some("turn-0".into()),
            claimed_turn_ids: vec!["turn-0".into(), "turn-0".into(), "turn-1".into()],
            existing_entries: Vec::new(),
        };
        assert_eq!(
            builder.build(duplicate_cursor).unwrap_err(),
            crate::MemoryError::InvalidInput
        );

        let output = builder
            .build(EvidenceBuildRequest {
                conversation: conversation(json!({})),
                messages: vec![text_message("selected", "selected", "right", "safe")],
                previous_summary: None,
                summary_cursor: Some("cursor".into()),
                claimed_turn_ids: vec!["selected".into()],
                existing_entries: Vec::new(),
            })
            .unwrap();
        assert_eq!(output.source_turns.len(), 1);

        let excluded_messages = (0..=MAX_EVIDENCE_MESSAGES)
            .map(|index| raw_message(&format!("tool-{index}"), "turn-1", "tool_call", "raw payload"))
            .collect::<Vec<_>>();
        let output = builder
            .build(EvidenceBuildRequest {
                conversation: conversation(json!({})),
                messages: excluded_messages,
                previous_summary: None,
                summary_cursor: None,
                claimed_turn_ids: vec!["turn-1".into()],
                existing_entries: Vec::new(),
            })
            .unwrap();
        assert!(output.source_turns.is_empty());
    }

    #[test]
    fn orders_final_messages_and_rejects_non_final_or_cumulative_oversize_evidence() {
        let builder = EvidenceBuilder::default();
        let ordered = builder
            .build(EvidenceBuildRequest {
                conversation: conversation(json!({})),
                messages: vec![
                    message_at("later", "turn-1", "right", "later", 2, "finish"),
                    message_at("first", "turn-1", "left", "first", 1, "finish"),
                    message_at("pending", "turn-1", "left", "partial", 3, "pending"),
                    message_at("work", "turn-1", "left", "stream", 4, "work"),
                    message_at("error", "turn-1", "left", "provider log", 5, "error"),
                ],
                previous_summary: None,
                summary_cursor: None,
                claimed_turn_ids: vec!["turn-1".into()],
                existing_entries: Vec::new(),
            })
            .unwrap();
        let messages = &ordered.source_turns[0].messages;
        assert_eq!(
            messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            ["first", "later"]
        );

        let cumulative = (0..9)
            .map(|index| {
                message_at(
                    &format!("large-{index}"),
                    "turn-1",
                    "right",
                    &"x".repeat(8_000),
                    index,
                    "finish",
                )
            })
            .collect();
        let result = builder.build(EvidenceBuildRequest {
            conversation: conversation(json!({})),
            messages: cumulative,
            previous_summary: None,
            summary_cursor: None,
            claimed_turn_ids: vec!["turn-1".into()],
            existing_entries: Vec::new(),
        });
        assert_eq!(result.unwrap_err(), crate::MemoryError::InvalidInput);
    }

    #[test]
    fn rejects_aggregate_summary_or_identifier_overflow_but_ignores_inactive_entries_for_limits() {
        let builder = EvidenceBuilder::default();
        let summary_overflow = builder.build(EvidenceBuildRequest {
            conversation: conversation(json!({})),
            messages: Vec::new(),
            previous_summary: Some(MemorySummary {
                goal: "goal".into(),
                current_state: (0..9).map(|_| "x".repeat(8_000)).collect(),
                decisions: Vec::new(),
                artifacts: Vec::new(),
                issues: Vec::new(),
                next_steps: Vec::new(),
                work_constraints: Vec::new(),
            }),
            summary_cursor: None,
            claimed_turn_ids: Vec::new(),
            existing_entries: Vec::new(),
        });
        assert_eq!(summary_overflow.unwrap_err(), crate::MemoryError::InvalidInput);

        let oversized_id = builder.build(EvidenceBuildRequest {
            conversation: conversation(json!({})),
            messages: vec![text_message(&"m".repeat(8_193), "turn-1", "right", "safe")],
            previous_summary: None,
            summary_cursor: None,
            claimed_turn_ids: vec!["turn-1".into()],
            existing_entries: Vec::new(),
        });
        assert_eq!(oversized_id.unwrap_err(), crate::MemoryError::InvalidInput);

        let inactive_entries = (0..=64)
            .map(|index| {
                let mut entry = active_entry(&format!("inactive-{index}"));
                entry.state = "superseded".into();
                entry
            })
            .collect();
        let output = builder
            .build(EvidenceBuildRequest {
                conversation: conversation(json!({})),
                messages: Vec::new(),
                previous_summary: None,
                summary_cursor: None,
                claimed_turn_ids: Vec::new(),
                existing_entries: inactive_entries,
            })
            .unwrap();
        assert!(output.existing_entries.is_empty());
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

    fn message_at(id: &str, turn_id: &str, position: &str, content: &str, created_at: i64, status: &str) -> MessageRow {
        let mut message = text_message(id, turn_id, position, content);
        message.created_at = created_at;
        message.status = Some(status.into());
        message
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
