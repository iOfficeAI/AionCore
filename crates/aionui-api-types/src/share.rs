use serde::{Deserialize, Serialize};

/// Permission granted by an explicit resource share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SharePermission {
    View,
    Edit,
}

/// Resource types that support multi-user sharing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareResourceType {
    Conversation,
    Project,
    Provider,
}

/// A single resource share row exposed to clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceShare {
    pub id: String,
    pub resource_type: ShareResourceType,
    pub resource_id: String,
    pub owner_user_id: String,
    pub grantee_user_id: String,
    pub grantee_username: Option<String>,
    pub permission: SharePermission,
    pub created_at: i64,
    pub created_by: String,
}

/// Request body for `POST /api/shares`.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateShareRequest {
    pub resource_type: ShareResourceType,
    pub resource_id: String,
    pub grantee_username: String,
    pub permission: SharePermission,
}

/// Query for listing shares on a resource.
#[derive(Debug, Clone, Deserialize)]
pub struct ListSharesQuery {
    pub resource_type: ShareResourceType,
    pub resource_id: String,
}

/// Response for share list endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShareListResponse {
    pub items: Vec<ResourceShare>,
}

/// Minimal directory entry for the share picker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectoryUser {
    pub id: String,
    pub username: String,
}

/// Response for `GET /api/users/directory`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserDirectoryResponse {
    pub items: Vec<DirectoryUser>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn create_share_request_snake_case() {
        let raw = json!({
            "resource_type": "conversation",
            "resource_id": "conv_1",
            "grantee_username": "alice",
            "permission": "edit"
        });
        let req: CreateShareRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.resource_type, ShareResourceType::Conversation);
        assert_eq!(req.permission, SharePermission::Edit);
        assert_eq!(req.grantee_username, "alice");
    }

    #[test]
    fn resource_share_serialization() {
        let share = ResourceShare {
            id: "share_1".into(),
            resource_type: ShareResourceType::Provider,
            resource_id: "prov_1".into(),
            owner_user_id: "u1".into(),
            grantee_user_id: "u2".into(),
            grantee_username: Some("bob".into()),
            permission: SharePermission::View,
            created_at: 100,
            created_by: "u1".into(),
        };
        let json = serde_json::to_value(&share).unwrap();
        assert_eq!(json["resource_type"], "provider");
        assert_eq!(json["permission"], "view");
        assert_eq!(json["grantee_username"], "bob");
    }
}
