use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Resource types that support explicit multi-user sharing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum ShareResourceType {
    Conversation,
    Project,
    Provider,
}

impl ShareResourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Project => "project",
            Self::Provider => "provider",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "conversation" => Some(Self::Conversation),
            "project" => Some(Self::Project),
            "provider" => Some(Self::Provider),
            _ => None,
        }
    }
}

/// Permission granted by a resource share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum SharePermission {
    View,
    Edit,
}

impl SharePermission {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Edit => "edit",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "view" => Some(Self::View),
            "edit" => Some(Self::Edit),
            _ => None,
        }
    }

    /// True when this permission includes write/mutate rights.
    pub fn allows_edit(self) -> bool {
        matches!(self, Self::Edit)
    }
}

/// Resolved access level for a user against a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceAccess {
    Owner,
    View,
    Edit,
    None,
}

impl ResourceAccess {
    pub fn allows_read(self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn allows_edit(self) -> bool {
        matches!(self, Self::Owner | Self::Edit)
    }

    pub fn is_owner(self) -> bool {
        matches!(self, Self::Owner)
    }
}

/// Row mapping for the `resource_shares` table.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ResourceShareRow {
    pub id: String,
    pub resource_type: String,
    pub resource_id: String,
    pub owner_user_id: String,
    pub grantee_user_id: String,
    pub permission: String,
    pub created_at: TimestampMs,
    pub created_by: String,
}

impl ResourceShareRow {
    pub fn resource_type(&self) -> Option<ShareResourceType> {
        ShareResourceType::parse(&self.resource_type)
    }

    pub fn permission(&self) -> Option<SharePermission> {
        SharePermission::parse(&self.permission)
    }
}

/// Parameters for granting a share.
#[derive(Debug, Clone)]
pub struct GrantShareParams<'a> {
    pub resource_type: ShareResourceType,
    pub resource_id: &'a str,
    pub owner_user_id: &'a str,
    pub grantee_user_id: &'a str,
    pub permission: SharePermission,
    pub created_by: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_type_roundtrips() {
        for t in [
            ShareResourceType::Conversation,
            ShareResourceType::Project,
            ShareResourceType::Provider,
        ] {
            assert_eq!(ShareResourceType::parse(t.as_str()), Some(t));
        }
        assert_eq!(ShareResourceType::parse("unknown"), None);
    }

    #[test]
    fn permission_roundtrips_and_edit_flag() {
        assert!(SharePermission::Edit.allows_edit());
        assert!(!SharePermission::View.allows_edit());
        assert_eq!(SharePermission::parse("view"), Some(SharePermission::View));
        assert_eq!(SharePermission::parse("edit"), Some(SharePermission::Edit));
    }

    #[test]
    fn resource_access_flags() {
        assert!(ResourceAccess::Owner.allows_read());
        assert!(ResourceAccess::Owner.allows_edit());
        assert!(ResourceAccess::Owner.is_owner());
        assert!(ResourceAccess::View.allows_read());
        assert!(!ResourceAccess::View.allows_edit());
        assert!(ResourceAccess::Edit.allows_edit());
        assert!(!ResourceAccess::None.allows_read());
    }
}
