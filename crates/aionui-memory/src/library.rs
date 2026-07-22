use aionui_api_types::{
    MemoryChangeSetResponse, MemoryEntryKind, MemoryEntryResponse, MemoryEntrySourceResponse, MemoryEntryState,
    MemorySettings,
};
use aionui_db::models::{MemoryChangeSetRow, MemoryEntryRow, MemorySettingsRow};

use crate::MemoryError;

pub(crate) fn settings_response(row: MemorySettingsRow) -> Result<MemorySettings, MemoryError> {
    Ok(MemorySettings {
        enabled: row.enabled,
        default_capture: row.default_capture,
        default_recall: row.default_recall,
        consent_version: optional_u64(row.consent_version)?,
        consented_at: row.consented_at,
        reset_at: row.reset_at,
    })
}

pub(crate) fn entry_response(row: MemoryEntryRow) -> Result<MemoryEntryResponse, MemoryError> {
    let content = row.content.ok_or(MemoryError::NotFound)?;
    Ok(MemoryEntryResponse {
        id: row.id,
        user_id: row.user_id,
        project_id: row.project_id,
        workspace_key: row.workspace_key,
        kind: entry_kind(&row.kind)?,
        stable_key: row.stable_key,
        fingerprint: row.fingerprint,
        content,
        state: entry_state(&row.state)?,
        pinned: row.pinned,
        user_edited: row.user_edited,
        sources: row
            .sources
            .into_iter()
            .map(|source| {
                Ok(MemoryEntrySourceResponse {
                    memory_entry_id: source.memory_entry_id,
                    conversation_id: source.conversation_id,
                    turn_id: source.turn_id,
                    message_ids: serde_json::from_str(&source.message_ids_json).map_err(|_| MemoryError::Internal)?,
                    first_observed_at: source.first_observed_at,
                    last_observed_at: source.last_observed_at,
                })
            })
            .collect::<Result<_, MemoryError>>()?,
        supersedes_id: row.supersedes_id,
        conflict_group_id: row.conflict_group_id,
        schema_version: row.schema_version.try_into().map_err(|_| MemoryError::Internal)?,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

pub(crate) fn change_set_response(row: MemoryChangeSetRow) -> Result<MemoryChangeSetResponse, MemoryError> {
    Ok(MemoryChangeSetResponse {
        id: row.id,
        user_id: row.user_id,
        conversation_id: row.conversation_id,
        through_turn_id: row.through_turn_id,
        job_id: row.job_id,
        added_ids: parse_ids(&row.added_ids_json)?,
        refined_ids: parse_ids(&row.refined_ids_json)?,
        superseded_ids: parse_ids(&row.superseded_ids_json)?,
        conflict_ids: parse_ids(&row.conflict_ids_json)?,
        created_at: row.created_at,
    })
}

pub(crate) fn kind_name(kind: &MemoryEntryKind) -> &'static str {
    match kind {
        MemoryEntryKind::Decision => "decision",
        MemoryEntryKind::Outcome => "outcome",
        MemoryEntryKind::Artifact => "artifact",
        MemoryEntryKind::Issue => "issue",
        MemoryEntryKind::NextStep => "next_step",
        MemoryEntryKind::WorkConstraint => "work_constraint",
    }
}

pub(crate) fn state_name(state: &MemoryEntryState) -> &'static str {
    match state {
        MemoryEntryState::Active => "active",
        MemoryEntryState::Superseded => "superseded",
        MemoryEntryState::Conflict => "conflict",
        MemoryEntryState::Deleted => "deleted",
    }
}

fn entry_kind(value: &str) -> Result<MemoryEntryKind, MemoryError> {
    match value {
        "decision" => Ok(MemoryEntryKind::Decision),
        "outcome" => Ok(MemoryEntryKind::Outcome),
        "artifact" => Ok(MemoryEntryKind::Artifact),
        "issue" => Ok(MemoryEntryKind::Issue),
        "next_step" => Ok(MemoryEntryKind::NextStep),
        "work_constraint" => Ok(MemoryEntryKind::WorkConstraint),
        _ => Err(MemoryError::Internal),
    }
}

fn entry_state(value: &str) -> Result<MemoryEntryState, MemoryError> {
    match value {
        "active" => Ok(MemoryEntryState::Active),
        "superseded" => Ok(MemoryEntryState::Superseded),
        "conflict" => Ok(MemoryEntryState::Conflict),
        "deleted" => Ok(MemoryEntryState::Deleted),
        _ => Err(MemoryError::Internal),
    }
}

fn optional_u64(value: Option<i64>) -> Result<Option<u64>, MemoryError> {
    value
        .map(|value| value.try_into().map_err(|_| MemoryError::Internal))
        .transpose()
}

fn parse_ids(value: &str) -> Result<Vec<String>, MemoryError> {
    serde_json::from_str(value).map_err(|_| MemoryError::Internal)
}

#[cfg(test)]
mod tests {
    use aionui_db::models::MemoryEntryRow;

    use super::entry_response;
    use crate::MemoryError;

    #[test]
    fn content_free_tombstones_never_cross_the_public_library_contract() {
        let result = entry_response(MemoryEntryRow {
            id: "tombstone-1".into(),
            user_id: "user-1".into(),
            project_id: None,
            workspace_key: None,
            kind: "decision".into(),
            stable_key: "decision".into(),
            fingerprint: "fingerprint".into(),
            content: None,
            state: "deleted".into(),
            pinned: false,
            user_edited: false,
            revision: 1,
            supersedes_id: None,
            conflict_group_id: None,
            schema_version: 1,
            deleted_at: Some(10),
            created_at: 1,
            updated_at: 10,
            sources: Vec::new(),
        });

        assert_eq!(result.unwrap_err(), MemoryError::NotFound);
    }
}
