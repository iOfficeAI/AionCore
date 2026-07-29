use aionui_db::models::ConversationRow;
use serde_json::{Map, Value};

use crate::{MemoryError, sanitizer::MAX_STRING_LENGTH};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ConversationScope {
    pub project_id: Option<String>,
    pub workspace_key: Option<String>,
}

impl ConversationScope {
    pub(crate) fn from_conversation(row: &ConversationRow) -> Result<Self, MemoryError> {
        let extra: Value = serde_json::from_str(&row.extra).map_err(|_| MemoryError::InvalidInput)?;
        let object = extra.as_object().ok_or(MemoryError::InvalidInput)?;
        let project_id = match row.project_id.as_deref() {
            Some(project_id) => Some(normalized_string(project_id)?),
            None => aliased_string(object, &["project_id", "projectId"])?,
        };
        let workspace_key = aliased_string(object, &["workspace_key", "workspaceKey", "workspace"])?
            .map(normalize_workspace_key)
            .transpose()?;
        Ok(Self {
            project_id,
            workspace_key,
        })
    }
}

fn aliased_string(object: &Map<String, Value>, names: &[&str]) -> Result<Option<String>, MemoryError> {
    let Some((_, value)) = names.iter().find_map(|name| object.get_key_value(*name)) else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::String(value) => normalized_string(value).map(Some),
        _ => Err(MemoryError::InvalidInput),
    }
}

fn normalized_string(value: &str) -> Result<String, MemoryError> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_STRING_LENGTH {
        Err(MemoryError::InvalidInput)
    } else {
        Ok(value.to_owned())
    }
}

fn normalize_workspace_key(workspace: String) -> Result<String, MemoryError> {
    let workspace = workspace.replace('\\', "/");
    let absolute = workspace.starts_with('/');
    let bytes = workspace.as_bytes();
    let has_drive_prefix = bytes.get(1) == Some(&b':');
    let drive_rooted =
        bytes.first().is_some_and(u8::is_ascii_alphabetic) && has_drive_prefix && bytes.get(2) == Some(&b'/');
    if has_drive_prefix && !drive_rooted {
        return Err(MemoryError::InvalidInput);
    }
    let root_components = usize::from(drive_rooted);
    let mut components = Vec::new();
    for component in workspace.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.len() <= root_components {
                    return Err(MemoryError::InvalidInput);
                }
                components.pop();
            }
            component => components.push(component),
        }
    }
    if components.is_empty() {
        return Err(MemoryError::InvalidInput);
    }
    let normalized = components.join("/");
    let normalized = if absolute { format!("/{normalized}") } else { normalized };
    if normalized.len() > MAX_STRING_LENGTH {
        Err(MemoryError::InvalidInput)
    } else {
        Ok(normalized)
    }
}
