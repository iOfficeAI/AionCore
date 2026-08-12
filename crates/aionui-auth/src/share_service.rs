//! Explicit multi-user resource sharing (Phase 2 collaboration).

use std::sync::Arc;

use aionui_api_types::{
    CreateShareRequest, DirectoryUser, ResourceShare, ShareListResponse, SharePermission as ApiSharePermission,
    ShareResourceType as ApiShareResourceType, UserDirectoryResponse,
};
use aionui_db::{
    DbError, GrantShareParams, IResourceShareRepository, IUserRepository, ResourceShareRow, SharePermission,
    ShareResourceType, UserStatus, UserType,
};

#[derive(Debug, thiserror::Error)]
pub enum ShareServiceError {
    #[error("database error: {0}")]
    Database(#[from] DbError),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("bad request: {0}")]
    BadRequest(String),
}

#[derive(Clone)]
pub struct ShareService {
    share_repo: Arc<dyn IResourceShareRepository>,
    user_repo: Arc<dyn IUserRepository>,
}

impl ShareService {
    pub fn new(share_repo: Arc<dyn IResourceShareRepository>, user_repo: Arc<dyn IUserRepository>) -> Self {
        Self { share_repo, user_repo }
    }

    pub async fn grant(
        &self,
        actor_user_id: &str,
        req: CreateShareRequest,
    ) -> Result<ResourceShare, ShareServiceError> {
        let resource_type = to_db_resource_type(req.resource_type);
        let permission = to_db_permission(req.permission);
        let resource_id = req.resource_id.trim();
        if resource_id.is_empty() {
            return Err(ShareServiceError::BadRequest("resource_id must not be empty".into()));
        }
        let grantee_username = req.grantee_username.trim();
        if grantee_username.is_empty() {
            return Err(ShareServiceError::BadRequest(
                "grantee_username must not be empty".into(),
            ));
        }

        let owner_id = self
            .share_repo
            .resource_owner(resource_type, resource_id)
            .await?
            .ok_or_else(|| ShareServiceError::NotFound(format!("Resource '{resource_id}' not found")))?;

        if owner_id != actor_user_id {
            return Err(ShareServiceError::Forbidden(
                "Only the resource owner can grant shares".into(),
            ));
        }

        let grantee = self
            .user_repo
            .find_by_username(grantee_username)
            .await?
            .ok_or_else(|| ShareServiceError::NotFound(format!("User '{grantee_username}' not found")))?;

        if grantee.user_type != UserType::Local || grantee.status != UserStatus::Active {
            return Err(ShareServiceError::NotFound(format!(
                "User '{grantee_username}' not found"
            )));
        }

        if grantee.id == owner_id {
            return Err(ShareServiceError::Conflict(
                "Cannot share a resource with its owner".into(),
            ));
        }

        let row = self
            .share_repo
            .grant(GrantShareParams {
                resource_type,
                resource_id,
                owner_user_id: &owner_id,
                grantee_user_id: &grantee.id,
                permission,
                created_by: actor_user_id,
            })
            .await?;

        Ok(to_api_share(
            row,
            Some(grantee.username.unwrap_or_else(|| grantee_username.to_owned())),
        ))
    }

    pub async fn revoke(&self, actor_user_id: &str, share_id: &str) -> Result<(), ShareServiceError> {
        let share = self
            .share_repo
            .find_by_id(share_id)
            .await?
            .ok_or_else(|| ShareServiceError::NotFound(format!("Share '{share_id}' not found")))?;

        if share.owner_user_id != actor_user_id {
            return Err(ShareServiceError::Forbidden(
                "Only the resource owner can revoke shares".into(),
            ));
        }

        self.share_repo.revoke(share_id).await?;
        Ok(())
    }

    pub async fn list_for_resource(
        &self,
        actor_user_id: &str,
        resource_type: ApiShareResourceType,
        resource_id: &str,
    ) -> Result<ShareListResponse, ShareServiceError> {
        let resource_type = to_db_resource_type(resource_type);
        let owner_id = self
            .share_repo
            .resource_owner(resource_type, resource_id)
            .await?
            .ok_or_else(|| ShareServiceError::NotFound(format!("Resource '{resource_id}' not found")))?;

        if owner_id != actor_user_id {
            return Err(ShareServiceError::Forbidden(
                "Only the resource owner can list shares for a resource".into(),
            ));
        }

        let rows = self.share_repo.list_for_resource(resource_type, resource_id).await?;
        Ok(ShareListResponse {
            items: self.enrich_rows(rows).await?,
        })
    }

    pub async fn list_granted_by(&self, owner_user_id: &str) -> Result<ShareListResponse, ShareServiceError> {
        let rows = self.share_repo.list_granted_by(owner_user_id).await?;
        Ok(ShareListResponse {
            items: self.enrich_rows(rows).await?,
        })
    }

    pub async fn list_received_by(&self, grantee_user_id: &str) -> Result<ShareListResponse, ShareServiceError> {
        let rows = self.share_repo.list_received_by(grantee_user_id).await?;
        Ok(ShareListResponse {
            items: self.enrich_rows(rows).await?,
        })
    }

    /// Active local usernames + ids for the share picker (no secrets).
    pub async fn list_directory(&self, actor_user_id: &str) -> Result<UserDirectoryResponse, ShareServiceError> {
        let users = self.user_repo.list_users().await?;
        let items = users
            .into_iter()
            .filter(|u| {
                u.id != actor_user_id
                    && u.user_type == UserType::Local
                    && u.status == UserStatus::Active
                    && u.username.as_ref().is_some_and(|name| !name.is_empty())
            })
            .map(|u| DirectoryUser {
                id: u.id,
                username: u.username.unwrap_or_default(),
            })
            .collect();
        Ok(UserDirectoryResponse { items })
    }

    async fn enrich_rows(&self, rows: Vec<ResourceShareRow>) -> Result<Vec<ResourceShare>, ShareServiceError> {
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let username = match self.user_repo.find_by_id(&row.grantee_user_id).await? {
                Some(user) => user.username,
                None => None,
            };
            items.push(to_api_share(row, username));
        }
        Ok(items)
    }
}

fn to_db_resource_type(value: ApiShareResourceType) -> ShareResourceType {
    match value {
        ApiShareResourceType::Conversation => ShareResourceType::Conversation,
        ApiShareResourceType::Project => ShareResourceType::Project,
        ApiShareResourceType::Provider => ShareResourceType::Provider,
    }
}

fn to_db_permission(value: ApiSharePermission) -> SharePermission {
    match value {
        ApiSharePermission::View => SharePermission::View,
        ApiSharePermission::Edit => SharePermission::Edit,
    }
}

fn to_api_resource_type(value: ShareResourceType) -> ApiShareResourceType {
    match value {
        ShareResourceType::Conversation => ApiShareResourceType::Conversation,
        ShareResourceType::Project => ApiShareResourceType::Project,
        ShareResourceType::Provider => ApiShareResourceType::Provider,
    }
}

fn to_api_permission(value: SharePermission) -> ApiSharePermission {
    match value {
        SharePermission::View => ApiSharePermission::View,
        SharePermission::Edit => ApiSharePermission::Edit,
    }
}

fn to_api_share(row: ResourceShareRow, grantee_username: Option<String>) -> ResourceShare {
    let resource_type = row
        .resource_type()
        .map(to_api_resource_type)
        .unwrap_or(ApiShareResourceType::Conversation);
    let permission = row
        .permission()
        .map(to_api_permission)
        .unwrap_or(ApiSharePermission::View);
    ResourceShare {
        id: row.id,
        resource_type,
        resource_id: row.resource_id,
        owner_user_id: row.owner_user_id,
        grantee_user_id: row.grantee_user_id,
        grantee_username,
        permission,
        created_at: row.created_at,
        created_by: row.created_by,
    }
}
