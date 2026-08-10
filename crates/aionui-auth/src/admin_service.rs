use std::sync::Arc;

use aionui_api_types::{
    AccountStatus, AdminAuditEntry, AdminAuditListResponse, AdminUser, AdminUserListResponse, AdminUserType,
    TemporaryPasswordResponse, UserRole,
};
use aionui_db::{
    AdminUserRepositoryError, AuditActor, IAdminUserRepository, SiteRole, UserStatus, UserType, models::User,
};

use crate::{AuthError, generate_password, hash_password, validate_username};

const TEMPORARY_PASSWORD_LENGTH: usize = 20;

#[derive(Debug, thiserror::Error)]
pub enum AdminUserServiceError {
    #[error("validation error: {0}")]
    Validation(#[from] AuthError),
    #[error("repository error: {0}")]
    Repository(#[from] AdminUserRepositoryError),
    #[error("database error: {0}")]
    Database(#[from] aionui_db::DbError),
    #[error("password hashing task failed: {0}")]
    HashTask(String),
}

#[derive(Clone)]
pub struct AdminUserService {
    repo: Arc<dyn IAdminUserRepository>,
}

impl AdminUserService {
    pub fn new(repo: Arc<dyn IAdminUserRepository>) -> Self {
        Self { repo }
    }

    pub async fn list_users(&self, limit: u32, offset: u32) -> Result<AdminUserListResponse, AdminUserServiceError> {
        let users = self
            .repo
            .list_managed_users(i64::from(limit), i64::from(offset))
            .await?;
        let total = self.repo.count_managed_users().await?;
        Ok(AdminUserListResponse {
            items: users.into_iter().map(admin_user).collect(),
            total: total.try_into().unwrap_or(0),
        })
    }

    pub async fn create_user(
        &self,
        username: &str,
        role: UserRole,
        actor: &AuditActor,
    ) -> Result<TemporaryPasswordResponse, AdminUserServiceError> {
        let username = validated_username(username)?;
        let temporary_password = generate_password(TEMPORARY_PASSWORD_LENGTH);
        let password_hash = hash_on_blocking_pool(temporary_password.clone()).await?;
        let user = self
            .repo
            .create_managed_user(&username, &password_hash, db_role(role), actor)
            .await?;
        Ok(TemporaryPasswordResponse {
            user: admin_user(user),
            temporary_password,
        })
    }

    pub async fn update_username(
        &self,
        user_id: &str,
        username: &str,
        actor: &AuditActor,
    ) -> Result<AdminUser, AdminUserServiceError> {
        let username = validated_username(username)?;
        Ok(admin_user(
            self.repo.update_managed_username(user_id, &username, actor).await?,
        ))
    }

    pub async fn update_role(
        &self,
        user_id: &str,
        role: UserRole,
        actor: &AuditActor,
    ) -> Result<AdminUser, AdminUserServiceError> {
        Ok(admin_user(
            self.repo.update_managed_role(user_id, db_role(role), actor).await?,
        ))
    }

    pub async fn update_status(
        &self,
        user_id: &str,
        status: AccountStatus,
        actor: &AuditActor,
    ) -> Result<AdminUser, AdminUserServiceError> {
        Ok(admin_user(
            self.repo
                .update_managed_status(user_id, db_status(status), actor)
                .await?,
        ))
    }

    pub async fn reset_password(
        &self,
        user_id: &str,
        actor: &AuditActor,
    ) -> Result<TemporaryPasswordResponse, AdminUserServiceError> {
        let temporary_password = generate_password(TEMPORARY_PASSWORD_LENGTH);
        let password_hash = hash_on_blocking_pool(temporary_password.clone()).await?;
        let user = self.repo.reset_managed_password(user_id, &password_hash, actor).await?;
        Ok(TemporaryPasswordResponse {
            user: admin_user(user),
            temporary_password,
        })
    }

    pub async fn revoke_sessions(&self, user_id: &str, actor: &AuditActor) -> Result<AdminUser, AdminUserServiceError> {
        Ok(admin_user(self.repo.revoke_managed_sessions(user_id, actor).await?))
    }

    pub async fn list_audit(
        &self,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<AdminAuditListResponse, AdminUserServiceError> {
        let mut records = self.repo.list_admin_audit(cursor, i64::from(limit) + 1).await?;
        let next_cursor = if records.len() > limit as usize {
            records.pop();
            records.last().map(|record| record.id.clone())
        } else {
            None
        };
        Ok(AdminAuditListResponse {
            items: records
                .into_iter()
                .map(|record| AdminAuditEntry {
                    id: record.id,
                    occurred_at: record.occurred_at,
                    actor_user_id: record.actor_user_id,
                    actor_username: record.actor_username,
                    action: record.action,
                    target_user_id: record.target_user_id,
                    target_username: record.target_username,
                    details: serde_json::from_str(&record.details).unwrap_or_else(|_| serde_json::json!({})),
                })
                .collect(),
            next_cursor,
        })
    }
}

pub fn audit_actor(user_id: &str, username: &str) -> AuditActor {
    AuditActor {
        user_id: Some(user_id.to_owned()),
        username: Some(username.to_owned()),
    }
}

pub fn admin_user(user: User) -> AdminUser {
    AdminUser {
        id: user.id,
        username: user.username.unwrap_or_else(|| "local_user".to_string()),
        user_type: match user.user_type {
            UserType::Local => AdminUserType::Local,
            UserType::Aionpro => AdminUserType::Aionpro,
        },
        role: match user.site_role {
            SiteRole::Admin => UserRole::Admin,
            SiteRole::Member => UserRole::Member,
        },
        status: match user.status {
            UserStatus::Active => AccountStatus::Active,
            UserStatus::Disabled => AccountStatus::Disabled,
        },
        must_change_password: user.must_change_password,
        created_at: user.created_at,
        updated_at: user.updated_at,
        last_login: user.last_login,
    }
}

fn validated_username(username: &str) -> Result<String, AdminUserServiceError> {
    let username = username.trim().to_owned();
    validate_username(&username)?;
    Ok(username)
}

fn db_role(role: UserRole) -> SiteRole {
    match role {
        UserRole::Admin => SiteRole::Admin,
        UserRole::Member => SiteRole::Member,
    }
}

fn db_status(status: AccountStatus) -> UserStatus {
    match status {
        AccountStatus::Active => UserStatus::Active,
        AccountStatus::Disabled => UserStatus::Disabled,
    }
}

async fn hash_on_blocking_pool(password: String) -> Result<String, AdminUserServiceError> {
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|error| AdminUserServiceError::HashTask(error.to_string()))?
        .map_err(AdminUserServiceError::Validation)
}
