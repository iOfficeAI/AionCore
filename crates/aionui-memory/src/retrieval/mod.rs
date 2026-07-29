use std::collections::BTreeSet;

use aionui_api_types::{MemoryRetrievalEntrySummary, MemoryRetrievalPreview, MemorySummary};
use aionui_db::memory_summary_selection_id;
use aionui_db::models::{ConversationMemoryRow, MemoryEntryRow, MemoryRetrievalRow, MemorySourceRow};
use sha2::{Digest, Sha256};

use crate::{MemoryError, library};

mod scope;

pub(crate) use scope::ConversationScope;

pub(crate) const RETRIEVAL_POLICY_VERSION: &str = "memory-retrieval-v1";
pub(crate) const RETRIEVAL_TTL_MS: i64 = 10 * 60 * 1_000;
pub(crate) const MAX_RETRIEVAL_CANDIDATES: u32 = 200;
pub(crate) const MAX_SUMMARY_CANDIDATES: u32 = 8;
pub(crate) const MAX_SELECTED_SUMMARIES: usize = 2;

pub(crate) fn summary_entry(row: &ConversationMemoryRow) -> Result<MemoryEntryRow, MemoryError> {
    let summary: MemorySummary = serde_json::from_str(&row.summary_json).map_err(|_| MemoryError::Internal)?;
    let mut parts = vec![format!("Goal: {}", summary.goal.trim())];
    for (label, values) in [
        ("Current state", summary.current_state),
        ("Decisions", summary.decisions),
        ("Artifacts", summary.artifacts),
        ("Issues", summary.issues),
        ("Next steps", summary.next_steps),
        ("Work constraints", summary.work_constraints),
    ] {
        if !values.is_empty() {
            parts.push(format!("{label}: {}", values.join("; ")));
        }
    }
    let content = parts.join(" | ");
    if content.trim().is_empty() || content.len() > 8_000 {
        return Err(MemoryError::Internal);
    }
    let id = memory_summary_selection_id(&row.conversation_id);
    Ok(MemoryEntryRow {
        id: id.clone(),
        user_id: row.user_id.clone(),
        project_id: row.project_id.clone(),
        workspace_key: row.workspace_key.clone(),
        // A living summary is mapped to the existing Outcome kind because it
        // describes the source conversation's achieved/current state. It is
        // never written to memory_entries; its canonical source remains
        // conversation_memories.
        kind: "outcome".into(),
        stable_key: format!("conversation-summary:{}", row.conversation_id),
        fingerprint: id,
        content: Some(content),
        state: "active".into(),
        pinned: false,
        user_edited: false,
        revision: row.revision,
        supersedes_id: None,
        conflict_group_id: None,
        schema_version: row.schema_version,
        deleted_at: None,
        created_at: row.created_at,
        updated_at: row.updated_at,
        sources: vec![MemorySourceRow {
            memory_entry_id: memory_summary_selection_id(&row.conversation_id),
            conversation_id: row.conversation_id.clone(),
            turn_id: row.through_turn_id.clone(),
            message_ids_json: "[]".into(),
            first_observed_at: row.created_at,
            last_observed_at: row.updated_at,
        }],
    })
}

pub(crate) fn prompt_hash(prompt: &str) -> String {
    format!("{:x}", Sha256::digest(prompt.as_bytes()))
}

pub(crate) fn preview_from_rows(
    retrieval: &MemoryRetrievalRow,
    entries: &[MemoryEntryRow],
) -> Result<MemoryRetrievalPreview, MemoryError> {
    Ok(MemoryRetrievalPreview {
        retrieval_id: retrieval.id.clone(),
        conversation_id: retrieval.conversation_id.clone(),
        prompt_hash: retrieval.prompt_hash.clone(),
        entries: entries
            .iter()
            .map(|entry| {
                let mut source_conversation_ids = entry
                    .sources
                    .iter()
                    .map(|source| source.conversation_id.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                source_conversation_ids.truncate(16);
                Ok(MemoryRetrievalEntrySummary {
                    id: entry.id.clone(),
                    kind: library::entry_kind(&entry.kind)?,
                    content: entry.content.clone().ok_or(MemoryError::Internal)?,
                    project_id: entry.project_id.clone(),
                    source_conversation_ids,
                    pinned: entry.pinned,
                })
            })
            .collect::<Result<_, MemoryError>>()?,
        estimated_tokens: retrieval
            .estimated_tokens
            .try_into()
            .map_err(|_| MemoryError::Internal)?,
        expires_at: retrieval.expires_at,
    })
}

#[cfg(test)]
mod tests {
    use aionui_db::models::ConversationRow;

    use super::{ConversationScope, RETRIEVAL_TTL_MS, prompt_hash};

    #[test]
    fn target_uses_canonical_scope_but_never_untrusted_capacity_fields() {
        let row = ConversationRow {
            id: "conv-1".into(),
            user_id: "user-1".into(),
            name: "Conversation".into(),
            r#type: "gemini".into(),
            extra: r#"{"projectId":" project-1 ","workspace":" C:\\work\\.\\draft\\..\\memory\\ ","contextCapacity":999999}"#.into(),
            model: None,
            status: None,
            source: None,
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 1,
            updated_at: 1,
            project_id: None,
            folder_id: None,
        };
        assert_eq!(
            ConversationScope::from_conversation(&row).unwrap(),
            ConversationScope {
                project_id: Some("project-1".into()),
                workspace_key: Some("C:/work/memory".into()),
            }
        );
    }

    #[test]
    fn target_prefers_the_authoritative_bound_project_column() {
        let row = ConversationRow {
            id: "conv-bound".into(),
            user_id: "user-1".into(),
            name: "Conversation".into(),
            r#type: "gemini".into(),
            extra: r#"{"workspace":"/work/memory"}"#.into(),
            model: None,
            status: None,
            source: None,
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 1,
            updated_at: 1,
            project_id: Some(" bound-project ".into()),
            folder_id: Some("folder-1".into()),
        };

        assert_eq!(
            ConversationScope::from_conversation(&row).unwrap(),
            ConversationScope {
                project_id: Some("bound-project".into()),
                workspace_key: Some("/work/memory".into()),
            }
        );
    }

    #[test]
    fn prompt_hash_and_expiry_policy_are_stable() {
        assert_eq!(prompt_hash("hello"), prompt_hash("hello"));
        assert_ne!(prompt_hash("hello"), prompt_hash("hello "));
        assert_eq!(RETRIEVAL_TTL_MS, 600_000);
    }
}
