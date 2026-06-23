use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::AgentMetadataRow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentBindingResolution {
    pub agent_id: String,
    pub agent_source: String,
    pub agent_type: String,
    pub runtime_backend: String,
}

pub fn runtime_backend_for_agent(row: &AgentMetadataRow) -> String {
    row.backend
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(row.agent_type.as_str())
        .to_owned()
}

pub fn binding_resolution_for_agent(row: &AgentMetadataRow) -> AgentBindingResolution {
    AgentBindingResolution {
        agent_id: row.id.clone(),
        agent_source: row.agent_source.clone(),
        agent_type: row.agent_type.clone(),
        runtime_backend: runtime_backend_for_agent(row),
    }
}

pub fn resolve_agent_binding_from_rows(rows: &[AgentMetadataRow], value: &str) -> Option<AgentBindingResolution> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    rows.iter()
        .filter(|row| row.id == value)
        .min_by_key(|row| agent_match_rank(row))
        .or_else(|| {
            rows.iter()
                .filter(|row| row.backend.as_deref() == Some(value))
                .min_by_key(|row| agent_match_rank(row))
        })
        .or_else(|| {
            rows.iter()
                .filter(|row| row.agent_type == value)
                .min_by_key(|row| agent_match_rank(row))
        })
        .map(binding_resolution_for_agent)
}

pub async fn resolve_agent_binding(pool: &SqlitePool, value: &str) -> Result<Option<AgentBindingResolution>, DbError> {
    let rows = sqlx::query_as::<_, AgentMetadataRow>("SELECT * FROM agent_metadata")
        .fetch_all(pool)
        .await?;
    Ok(resolve_agent_binding_from_rows(&rows, value))
}

fn agent_match_rank(row: &AgentMetadataRow) -> (i32, i64, &str) {
    let source_rank = match row.agent_source.as_str() {
        "builtin" => 0,
        "internal" => 1,
        _ => 2,
    };
    (source_rank, row.sort_order, row.name.as_str())
}
