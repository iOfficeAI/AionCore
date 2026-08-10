use crate::error::DbError;
use crate::models::{AdminAuditRecord, AuditActor, SiteRole, User, UserStatus};

#[derive(Debug, thiserror::Error)]
pub enum AdminUserRepositoryError {
    #[error("database error: {0}")]
    Database(#[from] DbError),
    #[error("the last active administrator cannot be demoted or disabled")]
    LastActiveAdmin,
    #[error("the operation is only supported for local password users")]
    UnsupportedIdentity,
}

impl From<sqlx::Error> for AdminUserRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(DbError::Query(error))
    }
}

#[async_trait::async_trait]
pub trait IAdminUserRepository: Send + Sync {
    async fn list_managed_users(&self, limit: i64, offset: i64) -> Result<Vec<User>, DbError>;
    async fn count_managed_users(&self) -> Result<i64, DbError>;

    async fn create_managed_user(
        &self,
        username: &str,
        password_hash: &str,
        role: SiteRole,
        actor: &AuditActor,
    ) -> Result<User, AdminUserRepositoryError>;

    async fn update_managed_username(
        &self,
        user_id: &str,
        username: &str,
        actor: &AuditActor,
    ) -> Result<User, AdminUserRepositoryError>;

    async fn update_managed_role(
        &self,
        user_id: &str,
        role: SiteRole,
        actor: &AuditActor,
    ) -> Result<User, AdminUserRepositoryError>;

    async fn update_managed_status(
        &self,
        user_id: &str,
        status: UserStatus,
        actor: &AuditActor,
    ) -> Result<User, AdminUserRepositoryError>;

    async fn reset_managed_password(
        &self,
        user_id: &str,
        password_hash: &str,
        actor: &AuditActor,
    ) -> Result<User, AdminUserRepositoryError>;

    async fn change_own_password(
        &self,
        user_id: &str,
        password_hash: &str,
        actor: &AuditActor,
    ) -> Result<User, AdminUserRepositoryError>;

    async fn revoke_managed_sessions(
        &self,
        user_id: &str,
        actor: &AuditActor,
    ) -> Result<User, AdminUserRepositoryError>;

    /// Atomically creates the first usable administrator, if one does not
    /// already exist. The returned `None` means another process/admin won.
    async fn bootstrap_initial_admin(
        &self,
        username: &str,
        password_hash: &str,
    ) -> Result<Option<User>, AdminUserRepositoryError>;

    /// Returns one extra row when a next page exists.
    async fn list_admin_audit(
        &self,
        cursor: Option<&str>,
        limit_plus_one: i64,
    ) -> Result<Vec<AdminAuditRecord>, DbError>;
}
