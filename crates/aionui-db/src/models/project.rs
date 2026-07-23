use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Project kind, stored as the TEXT `projects.kind` column.
///
/// Column values follow existing crate convention (TEXT + service-layer
/// validation); this enum is the typed boundary used by `IProjectStore`
/// signatures and the `aionui-project` service. It lives in `aionui-db`
/// (the lower layer) so the store trait does not depend upward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectKind {
    /// User-selected workspace resource root; locally a `file:` directory.
    Standard,
    /// System-created temp session directory, auto-generated project.
    Temp,
}

impl ProjectKind {
    /// The canonical TEXT column value.
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectKind::Standard => "standard",
            ProjectKind::Temp => "temp",
        }
    }

    /// Parse a TEXT column value; `None` for unknown values.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "standard" => Some(ProjectKind::Standard),
            "temp" => Some(ProjectKind::Temp),
            _ => None,
        }
    }
}

/// Project Explorer entry role, stored as the TEXT `project_explorer.role` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// The single working root for all Conversations / Teams under the project.
    Workspace,
    /// Additional root for viewing, editing, referencing, searching.
    Attached,
}

impl Role {
    /// The canonical TEXT column value.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Workspace => "workspace",
            Role::Attached => "attached",
        }
    }

    /// Parse a TEXT column value; `None` for unknown values.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "workspace" => Some(Role::Workspace),
            "attached" => Some(Role::Attached),
            _ => None,
        }
    }
}

/// Row mapping for the `projects` table.
///
/// The internal AUTOINCREMENT `id` surrogate is not mapped; only the business
/// columns are selected. `kind` is a TEXT enum-like column (`ProjectKind`).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProjectRow {
    pub project_id: String,
    pub name: String,
    /// One of: "standard", "temp" (see [`ProjectKind`]).
    pub kind: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row mapping for the `folders` table.
///
/// `resource_canonical` is the unique dedupe key; `resource_uri` is the raw
/// input kept for display / audit only. Neither scheme/authority/path is
/// persisted.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct FolderRow {
    pub folder_id: String,
    pub resource_uri: String,
    pub resource_canonical: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row mapping for the `project_explorer` table.
///
/// `role` is a TEXT enum-like column (`Role`). `display_name` is a per-project
/// override; NULL derives from `resource_uri` / provider label at read time.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProjectExplorerRow {
    pub pe_id: String,
    pub project_id: String,
    pub folder_id: String,
    /// One of: "workspace", "attached" (see [`Role`]).
    pub role: String,
    pub display_name: Option<String>,
    pub order_index: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_kind_roundtrips_through_column_value() {
        for kind in [ProjectKind::Standard, ProjectKind::Temp] {
            assert_eq!(ProjectKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ProjectKind::parse("unknown"), None);
    }

    #[test]
    fn role_roundtrips_through_column_value() {
        for role in [Role::Workspace, Role::Attached] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
        assert_eq!(Role::parse("unknown"), None);
    }
}
