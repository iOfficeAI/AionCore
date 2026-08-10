use crate::error::DbError;
use crate::models::{GrantShareParams, ResourceAccess, ResourceShareRow, SharePermission, ShareResourceType};

/// Resource share data access abstraction.
///
/// Explicit grants only: resources stay private unless the owner creates a share.
#[async_trait::async_trait]
pub trait IResourceShareRepository: Send + Sync {
    /// Grant a share (or upsert permission when the grantee already has one).
    async fn grant(&self, params: GrantShareParams<'_>) -> Result<ResourceShareRow, DbError>;

    /// Revoke a share by id. Returns `NotFound` when missing.
    async fn revoke(&self, share_id: &str) -> Result<(), DbError>;

    /// List shares for a specific resource (owner view).
    async fn list_for_resource(
        &self,
        resource_type: ShareResourceType,
        resource_id: &str,
    ) -> Result<Vec<ResourceShareRow>, DbError>;

    /// Shares granted by this owner (across all resource types).
    async fn list_granted_by(&self, owner_user_id: &str) -> Result<Vec<ResourceShareRow>, DbError>;

    /// Shares received by this grantee (across all resource types).
    async fn list_received_by(&self, grantee_user_id: &str) -> Result<Vec<ResourceShareRow>, DbError>;

    /// Permission granted to `grantee_user_id` for the resource, if any.
    ///
    /// Does not treat ownership as a permission row — use [`resolve_access`].
    async fn get_permission(
        &self,
        resource_type: ShareResourceType,
        resource_id: &str,
        grantee_user_id: &str,
    ) -> Result<Option<SharePermission>, DbError>;

    /// Resolve effective access for `user_id`: owner | view | edit | none.
    ///
    /// Returns `None` both when the resource is missing and when the user has
    /// no grant (callers should not distinguish these to avoid existence leaks).
    async fn resolve_access(
        &self,
        resource_type: ShareResourceType,
        resource_id: &str,
        user_id: &str,
    ) -> Result<ResourceAccess, DbError>;

    /// Owner user id for a resource, if the resource exists.
    async fn resource_owner(
        &self,
        resource_type: ShareResourceType,
        resource_id: &str,
    ) -> Result<Option<String>, DbError>;

    /// Load a share by primary key.
    async fn find_by_id(&self, share_id: &str) -> Result<Option<ResourceShareRow>, DbError>;
}
