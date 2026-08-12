use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{GrantShareParams, ResourceAccess, ResourceShareRow, SharePermission, ShareResourceType};
use crate::repository::IResourceShareRepository;

/// SQLite-backed implementation of [`IResourceShareRepository`].
#[derive(Clone, Debug)]
pub struct SqliteResourceShareRepository {
    pool: SqlitePool,
}

impl SqliteResourceShareRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IResourceShareRepository for SqliteResourceShareRepository {
    async fn grant(&self, params: GrantShareParams<'_>) -> Result<ResourceShareRow, DbError> {
        if params.owner_user_id == params.grantee_user_id {
            return Err(DbError::Conflict("Cannot share a resource with its owner".to_owned()));
        }

        let id = aionui_common::generate_prefixed_id("share");
        let now = aionui_common::now_ms();
        let resource_type = params.resource_type.as_str();
        let permission = params.permission.as_str();

        // Upsert: if the grantee already has a share, refresh permission/created_by.
        sqlx::query(
            "INSERT INTO resource_shares \
                (id, resource_type, resource_id, owner_user_id, grantee_user_id, permission, created_at, created_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(resource_type, resource_id, grantee_user_id) DO UPDATE SET \
                permission = excluded.permission, \
                created_by = excluded.created_by, \
                owner_user_id = excluded.owner_user_id",
        )
        .bind(&id)
        .bind(resource_type)
        .bind(params.resource_id)
        .bind(params.owner_user_id)
        .bind(params.grantee_user_id)
        .bind(permission)
        .bind(now)
        .bind(params.created_by)
        .execute(&self.pool)
        .await
        .map_err(map_write_error)?;

        // ON CONFLICT keeps the original id; re-select the canonical row.
        self.find_by_resource_grantee(params.resource_type, params.resource_id, params.grantee_user_id)
            .await?
            .ok_or_else(|| DbError::NotFound("Share row missing after grant".to_owned()))
    }

    async fn revoke(&self, share_id: &str) -> Result<(), DbError> {
        let result = sqlx::query("DELETE FROM resource_shares WHERE id = ?")
            .bind(share_id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Share '{share_id}' not found")));
        }
        Ok(())
    }

    async fn list_for_resource(
        &self,
        resource_type: ShareResourceType,
        resource_id: &str,
    ) -> Result<Vec<ResourceShareRow>, DbError> {
        let rows = sqlx::query_as::<_, ResourceShareRow>(
            "SELECT * FROM resource_shares \
             WHERE resource_type = ? AND resource_id = ? \
             ORDER BY created_at ASC, id ASC",
        )
        .bind(resource_type.as_str())
        .bind(resource_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_granted_by(&self, owner_user_id: &str) -> Result<Vec<ResourceShareRow>, DbError> {
        let rows = sqlx::query_as::<_, ResourceShareRow>(
            "SELECT * FROM resource_shares \
             WHERE owner_user_id = ? \
             ORDER BY created_at DESC, id DESC",
        )
        .bind(owner_user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_received_by(&self, grantee_user_id: &str) -> Result<Vec<ResourceShareRow>, DbError> {
        let rows = sqlx::query_as::<_, ResourceShareRow>(
            "SELECT * FROM resource_shares \
             WHERE grantee_user_id = ? \
             ORDER BY created_at DESC, id DESC",
        )
        .bind(grantee_user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_permission(
        &self,
        resource_type: ShareResourceType,
        resource_id: &str,
        grantee_user_id: &str,
    ) -> Result<Option<SharePermission>, DbError> {
        let permission: Option<String> = sqlx::query_scalar(
            "SELECT permission FROM resource_shares \
             WHERE resource_type = ? AND resource_id = ? AND grantee_user_id = ?",
        )
        .bind(resource_type.as_str())
        .bind(resource_id)
        .bind(grantee_user_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(permission.and_then(|p| SharePermission::parse(&p)))
    }

    async fn resolve_access(
        &self,
        resource_type: ShareResourceType,
        resource_id: &str,
        user_id: &str,
    ) -> Result<ResourceAccess, DbError> {
        let Some(owner_id) = self.resource_owner(resource_type, resource_id).await? else {
            return Ok(ResourceAccess::None);
        };

        if owner_id == user_id {
            return Ok(ResourceAccess::Owner);
        }

        match self.get_permission(resource_type, resource_id, user_id).await? {
            Some(SharePermission::Edit) => Ok(ResourceAccess::Edit),
            Some(SharePermission::View) => Ok(ResourceAccess::View),
            None => Ok(ResourceAccess::None),
        }
    }

    async fn resource_owner(
        &self,
        resource_type: ShareResourceType,
        resource_id: &str,
    ) -> Result<Option<String>, DbError> {
        let sql = match resource_type {
            ShareResourceType::Conversation => "SELECT user_id FROM conversations WHERE id = ?",
            ShareResourceType::Project => "SELECT user_id FROM projects WHERE project_id = ?",
            ShareResourceType::Provider => "SELECT user_id FROM providers WHERE id = ?",
        };
        let owner = sqlx::query_scalar::<_, String>(sql)
            .bind(resource_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(owner)
    }

    async fn find_by_id(&self, share_id: &str) -> Result<Option<ResourceShareRow>, DbError> {
        let row = sqlx::query_as::<_, ResourceShareRow>("SELECT * FROM resource_shares WHERE id = ?")
            .bind(share_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }
}

impl SqliteResourceShareRepository {
    async fn find_by_resource_grantee(
        &self,
        resource_type: ShareResourceType,
        resource_id: &str,
        grantee_user_id: &str,
    ) -> Result<Option<ResourceShareRow>, DbError> {
        let row = sqlx::query_as::<_, ResourceShareRow>(
            "SELECT * FROM resource_shares \
             WHERE resource_type = ? AND resource_id = ? AND grantee_user_id = ?",
        )
        .bind(resource_type.as_str())
        .bind(resource_id)
        .bind(grantee_user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }
}

fn map_write_error(error: sqlx::Error) -> DbError {
    match &error {
        sqlx::Error::Database(db_err) if is_unique_violation(db_err.as_ref()) => {
            DbError::Conflict("Share already exists".to_owned())
        }
        sqlx::Error::Database(db_err) if is_foreign_key_violation(db_err.as_ref()) => {
            DbError::NotFound("Referenced user or resource is missing".to_owned())
        }
        sqlx::Error::Database(db_err) if is_check_violation(db_err.as_ref()) => {
            DbError::Conflict(db_err.message().to_owned())
        }
        _ => DbError::Query(error),
    }
}

fn is_unique_violation(err: &dyn sqlx::error::DatabaseError) -> bool {
    err.code().is_some_and(|c| c == "2067" || c == "1555")
}

fn is_foreign_key_violation(err: &dyn sqlx::error::DatabaseError) -> bool {
    err.code().is_some_and(|c| c == "787")
}

fn is_check_violation(err: &dyn sqlx::error::DatabaseError) -> bool {
    err.code().is_some_and(|c| c == "275")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IUserRepository, SqliteUserRepository, init_database_memory};

    async fn setup() -> (SqliteResourceShareRepository, String, String) {
        let db = init_database_memory().await.unwrap();
        let share_repo = SqliteResourceShareRepository::new(db.pool().clone());
        let user_repo = SqliteUserRepository::new(db.pool().clone());
        let owner = user_repo.create_user("owner", "hash").await.unwrap();
        let grantee = user_repo.create_user("grantee", "hash").await.unwrap();

        // Seed a conversation owned by owner so resolve_access can find it.
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, extra, status, source, pinned, created_at, updated_at) \
             VALUES ('conv_1', ?, 'Test', 'gemini', '{}', 'pending', 'aionui', 0, 1, 1)",
        )
        .bind(&owner.id)
        .execute(db.pool())
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO providers \
                (id, user_id, platform, name, base_url, api_key_encrypted, models, enabled, \
                 capabilities, model_settings, is_full_url, created_at, updated_at) \
             VALUES ('prov_1', ?, 'openai', 'OpenAI', 'https://api.openai.com', 'enc', '[]', 1, '[]', '{}', 0, 1, 1)",
        )
        .bind(&owner.id)
        .execute(db.pool())
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) \
             VALUES ('proj_1', ?, 'Project', 'standard', 1, 1)",
        )
        .bind(&owner.id)
        .execute(db.pool())
        .await
        .unwrap();

        (share_repo, owner.id, grantee.id)
    }

    #[tokio::test]
    async fn grant_and_resolve_access() {
        let (repo, owner_id, grantee_id) = setup().await;

        assert_eq!(
            repo.resolve_access(ShareResourceType::Conversation, "conv_1", &owner_id)
                .await
                .unwrap(),
            ResourceAccess::Owner
        );
        assert_eq!(
            repo.resolve_access(ShareResourceType::Conversation, "conv_1", &grantee_id)
                .await
                .unwrap(),
            ResourceAccess::None
        );

        let share = repo
            .grant(GrantShareParams {
                resource_type: ShareResourceType::Conversation,
                resource_id: "conv_1",
                owner_user_id: &owner_id,
                grantee_user_id: &grantee_id,
                permission: SharePermission::View,
                created_by: &owner_id,
            })
            .await
            .unwrap();

        assert_eq!(share.permission, "view");
        assert_eq!(
            repo.resolve_access(ShareResourceType::Conversation, "conv_1", &grantee_id)
                .await
                .unwrap(),
            ResourceAccess::View
        );
        assert!(
            !repo
                .resolve_access(ShareResourceType::Conversation, "conv_1", &grantee_id)
                .await
                .unwrap()
                .allows_edit()
        );
    }

    #[tokio::test]
    async fn grant_upserts_permission() {
        let (repo, owner_id, grantee_id) = setup().await;

        repo.grant(GrantShareParams {
            resource_type: ShareResourceType::Provider,
            resource_id: "prov_1",
            owner_user_id: &owner_id,
            grantee_user_id: &grantee_id,
            permission: SharePermission::View,
            created_by: &owner_id,
        })
        .await
        .unwrap();

        let upgraded = repo
            .grant(GrantShareParams {
                resource_type: ShareResourceType::Provider,
                resource_id: "prov_1",
                owner_user_id: &owner_id,
                grantee_user_id: &grantee_id,
                permission: SharePermission::Edit,
                created_by: &owner_id,
            })
            .await
            .unwrap();

        assert_eq!(upgraded.permission, "edit");
        assert_eq!(
            repo.list_for_resource(ShareResourceType::Provider, "prov_1")
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            repo.resolve_access(ShareResourceType::Provider, "prov_1", &grantee_id)
                .await
                .unwrap(),
            ResourceAccess::Edit
        );
    }

    #[tokio::test]
    async fn revoke_removes_access() {
        let (repo, owner_id, grantee_id) = setup().await;
        let share = repo
            .grant(GrantShareParams {
                resource_type: ShareResourceType::Project,
                resource_id: "proj_1",
                owner_user_id: &owner_id,
                grantee_user_id: &grantee_id,
                permission: SharePermission::Edit,
                created_by: &owner_id,
            })
            .await
            .unwrap();

        repo.revoke(&share.id).await.unwrap();
        assert_eq!(
            repo.resolve_access(ShareResourceType::Project, "proj_1", &grantee_id)
                .await
                .unwrap(),
            ResourceAccess::None
        );
        assert!(matches!(
            repo.revoke(&share.id).await.unwrap_err(),
            DbError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn cannot_share_with_self() {
        let (repo, owner_id, _) = setup().await;
        let err = repo
            .grant(GrantShareParams {
                resource_type: ShareResourceType::Conversation,
                resource_id: "conv_1",
                owner_user_id: &owner_id,
                grantee_user_id: &owner_id,
                permission: SharePermission::View,
                created_by: &owner_id,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn list_granted_and_received() {
        let (repo, owner_id, grantee_id) = setup().await;
        repo.grant(GrantShareParams {
            resource_type: ShareResourceType::Conversation,
            resource_id: "conv_1",
            owner_user_id: &owner_id,
            grantee_user_id: &grantee_id,
            permission: SharePermission::View,
            created_by: &owner_id,
        })
        .await
        .unwrap();

        assert_eq!(repo.list_granted_by(&owner_id).await.unwrap().len(), 1);
        assert!(repo.list_granted_by(&grantee_id).await.unwrap().is_empty());
        assert_eq!(repo.list_received_by(&grantee_id).await.unwrap().len(), 1);
        assert!(repo.list_received_by(&owner_id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_resource_resolves_none() {
        let (repo, owner_id, _) = setup().await;
        assert_eq!(
            repo.resolve_access(ShareResourceType::Conversation, "missing", &owner_id)
                .await
                .unwrap(),
            ResourceAccess::None
        );
    }
}
