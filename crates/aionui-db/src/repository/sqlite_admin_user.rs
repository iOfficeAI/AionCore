use sqlx::{Sqlite, Transaction};

use crate::error::DbError;
use crate::models::{AdminAuditRecord, AuditActor, SiteRole, User, UserStatus, UserType};
use crate::repository::{AdminUserRepositoryError, IAdminUserRepository, SqliteUserRepository};

#[async_trait::async_trait]
impl IAdminUserRepository for SqliteUserRepository {
    async fn list_managed_users(&self, limit: i64, offset: i64) -> Result<Vec<User>, DbError> {
        Ok(sqlx::query_as::<_, User>(
            "SELECT * FROM users WHERE user_type = 'local' ORDER BY created_at ASC, id ASC LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn count_managed_users(&self) -> Result<i64, DbError> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE user_type = 'local'")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    async fn create_managed_user(
        &self,
        username: &str,
        password_hash: &str,
        role: SiteRole,
        actor: &AuditActor,
    ) -> Result<User, AdminUserRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(DbError::from)?;
        let id = aionui_common::generate_prefixed_id("user");
        let now = aionui_common::now_ms();
        sqlx::query(
            "INSERT INTO users \
             (id, user_type, username, password_hash, status, site_role, must_change_password, \
              session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, ?, 'active', ?, 1, 0, ?, ?)",
        )
        .bind(&id)
        .bind(username)
        .bind(password_hash)
        .bind(role.as_str())
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(map_write_error)?;
        append_audit(
            &mut tx,
            actor,
            "user.created",
            Some(&id),
            Some(username),
            serde_json::json!({ "role": role.as_str() }),
        )
        .await?;
        let user = load_user(&mut tx, &id).await?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(user)
    }

    async fn update_managed_username(
        &self,
        user_id: &str,
        username: &str,
        actor: &AuditActor,
    ) -> Result<User, AdminUserRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(DbError::from)?;
        acquire_user_write_lock(&mut tx, user_id).await?;
        let before = load_user(&mut tx, user_id).await?;
        ensure_local_identity(&before)?;
        sqlx::query(
            "UPDATE users SET username = ?, session_generation = session_generation + 1, updated_at = ? \
             WHERE id = ?",
        )
        .bind(username)
        .bind(aionui_common::now_ms())
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(map_write_error)?;
        revoke_sessions(&mut tx, user_id, "username_changed").await?;
        append_audit(
            &mut tx,
            actor,
            "user.username_changed",
            Some(user_id),
            Some(username),
            serde_json::json!({ "from": before.username, "to": username }),
        )
        .await?;
        let user = load_user(&mut tx, user_id).await?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(user)
    }

    async fn update_managed_role(
        &self,
        user_id: &str,
        role: SiteRole,
        actor: &AuditActor,
    ) -> Result<User, AdminUserRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(DbError::from)?;
        acquire_user_write_lock(&mut tx, user_id).await?;
        let before = load_user(&mut tx, user_id).await?;
        ensure_local_identity(&before)?;
        if before.site_role == SiteRole::Admin && role != SiteRole::Admin {
            ensure_another_active_admin(&mut tx, user_id).await?;
        }
        sqlx::query(
            "UPDATE users SET site_role = ?, session_generation = session_generation + 1, updated_at = ? \
             WHERE id = ?",
        )
        .bind(role.as_str())
        .bind(aionui_common::now_ms())
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        revoke_sessions(&mut tx, user_id, "role_changed").await?;
        append_audit(
            &mut tx,
            actor,
            "user.site_role_changed",
            Some(user_id),
            before.username.as_deref(),
            serde_json::json!({ "from": before.site_role.as_str(), "to": role.as_str() }),
        )
        .await?;
        let user = load_user(&mut tx, user_id).await?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(user)
    }

    async fn update_managed_status(
        &self,
        user_id: &str,
        status: UserStatus,
        actor: &AuditActor,
    ) -> Result<User, AdminUserRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(DbError::from)?;
        acquire_user_write_lock(&mut tx, user_id).await?;
        let before = load_user(&mut tx, user_id).await?;
        ensure_local_identity(&before)?;
        if before.site_role == SiteRole::Admin && before.status == UserStatus::Active && status != UserStatus::Active {
            ensure_another_active_admin(&mut tx, user_id).await?;
        }
        sqlx::query(
            "UPDATE users SET status = ?, session_generation = session_generation + 1, updated_at = ? \
             WHERE id = ?",
        )
        .bind(status.as_str())
        .bind(aionui_common::now_ms())
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        revoke_sessions(&mut tx, user_id, "status_changed").await?;
        append_audit(
            &mut tx,
            actor,
            "user.status_changed",
            Some(user_id),
            before.username.as_deref(),
            serde_json::json!({ "from": before.status.as_str(), "to": status.as_str() }),
        )
        .await?;
        let user = load_user(&mut tx, user_id).await?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(user)
    }

    async fn reset_managed_password(
        &self,
        user_id: &str,
        password_hash: &str,
        actor: &AuditActor,
    ) -> Result<User, AdminUserRepositoryError> {
        password_mutation(self, user_id, password_hash, true, actor, "user.password_reset").await
    }

    async fn change_own_password(
        &self,
        user_id: &str,
        password_hash: &str,
        actor: &AuditActor,
    ) -> Result<User, AdminUserRepositoryError> {
        password_mutation(self, user_id, password_hash, false, actor, "user.password_changed").await
    }

    async fn revoke_managed_sessions(
        &self,
        user_id: &str,
        actor: &AuditActor,
    ) -> Result<User, AdminUserRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(DbError::from)?;
        acquire_user_write_lock(&mut tx, user_id).await?;
        let target = load_user(&mut tx, user_id).await?;
        ensure_local_identity(&target)?;
        sqlx::query("UPDATE users SET session_generation = session_generation + 1, updated_at = ? WHERE id = ?")
            .bind(aionui_common::now_ms())
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        revoke_sessions(&mut tx, user_id, "admin_revoked").await?;
        append_audit(
            &mut tx,
            actor,
            "user.sessions_revoked",
            Some(user_id),
            target.username.as_deref(),
            serde_json::json!({}),
        )
        .await?;
        let user = load_user(&mut tx, user_id).await?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(user)
    }

    async fn bootstrap_initial_admin(
        &self,
        username: &str,
        password_hash: &str,
    ) -> Result<Option<User>, AdminUserRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(DbError::from)?;
        // Acquire SQLite's writer lock before evaluating the invariant so two
        // bootstrappers cannot both observe an empty administrator set.
        sqlx::query(
            "UPDATE users SET updated_at = updated_at \
             WHERE id = (SELECT id FROM users ORDER BY created_at ASC, id ASC LIMIT 1)",
        )
        .execute(&mut *tx)
        .await?;
        let usable_admins: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM users WHERE user_type = 'local' AND site_role = 'admin' \
             AND status = 'active' AND password_hash IS NOT NULL AND password_hash != ''",
        )
        .fetch_one(&mut *tx)
        .await?;
        if usable_admins > 0 {
            tx.rollback().await.map_err(DbError::from)?;
            return Ok(None);
        }

        let target_id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM users WHERE user_type = 'local' AND ( \
                 id = 'system_default_user' OR (status = 'active' AND password_hash IS NOT NULL AND password_hash != '') \
             ) ORDER BY CASE WHEN id = 'system_default_user' THEN 0 ELSE 1 END, \
               CASE WHEN username = ? THEN 0 ELSE 1 END, created_at ASC, id ASC LIMIT 1",
        )
        .bind(username)
        .fetch_optional(&mut *tx)
        .await?;
        let target_id = if let Some(target_id) = target_id {
            target_id
        } else {
            let now = aionui_common::now_ms();
            sqlx::query(
                "INSERT INTO users \
                 (id, user_type, username, password_hash, status, site_role, must_change_password, \
                  session_generation, created_at, updated_at) \
                 VALUES ('system_default_user', 'local', ?, ?, 'active', 'admin', 1, 1, ?, ?)",
            )
            .bind(username)
            .bind(password_hash)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(map_write_error)?;
            "system_default_user".to_string()
        };
        sqlx::query(
            "UPDATE users SET username = ?, password_hash = ?, site_role = 'admin', status = 'active', \
             must_change_password = 1, session_generation = session_generation + 1, updated_at = ? \
             WHERE id = ? AND user_type = 'local'",
        )
        .bind(username)
        .bind(password_hash)
        .bind(aionui_common::now_ms())
        .bind(&target_id)
        .execute(&mut *tx)
        .await
        .map_err(map_write_error)?;
        revoke_sessions(&mut tx, &target_id, "bootstrap_credentials_set").await?;
        append_audit(
            &mut tx,
            &AuditActor::system(),
            "bootstrap.credentials_set",
            Some(&target_id),
            Some(username),
            serde_json::json!({ "must_change_password": true }),
        )
        .await?;
        let user = load_user(&mut tx, &target_id).await?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(Some(user))
    }

    async fn list_admin_audit(
        &self,
        cursor: Option<&str>,
        limit_plus_one: i64,
    ) -> Result<Vec<AdminAuditRecord>, DbError> {
        if let Some(cursor) = cursor {
            let boundary: Option<(i64, String)> =
                sqlx::query_as("SELECT occurred_at, id FROM admin_audit_log WHERE id = ?")
                    .bind(cursor)
                    .fetch_optional(&self.pool)
                    .await?;
            let (occurred_at, id) = boundary.ok_or_else(|| DbError::NotFound("audit cursor not found".into()))?;
            Ok(sqlx::query_as::<_, AdminAuditRecord>(
                "SELECT * FROM admin_audit_log \
                 WHERE occurred_at < ? OR (occurred_at = ? AND id < ?) \
                 ORDER BY occurred_at DESC, id DESC LIMIT ?",
            )
            .bind(occurred_at)
            .bind(occurred_at)
            .bind(id)
            .bind(limit_plus_one)
            .fetch_all(&self.pool)
            .await?)
        } else {
            Ok(sqlx::query_as::<_, AdminAuditRecord>(
                "SELECT * FROM admin_audit_log ORDER BY occurred_at DESC, id DESC LIMIT ?",
            )
            .bind(limit_plus_one)
            .fetch_all(&self.pool)
            .await?)
        }
    }
}

async fn password_mutation(
    repo: &SqliteUserRepository,
    user_id: &str,
    password_hash: &str,
    must_change_password: bool,
    actor: &AuditActor,
    action: &str,
) -> Result<User, AdminUserRepositoryError> {
    let mut tx = repo.pool.begin().await.map_err(DbError::from)?;
    acquire_user_write_lock(&mut tx, user_id).await?;
    let target = load_user(&mut tx, user_id).await?;
    if target.user_type != UserType::Local {
        return Err(AdminUserRepositoryError::UnsupportedIdentity);
    }
    sqlx::query(
        "UPDATE users SET password_hash = ?, must_change_password = ?, \
         session_generation = session_generation + 1, updated_at = ? WHERE id = ?",
    )
    .bind(password_hash)
    .bind(must_change_password)
    .bind(aionui_common::now_ms())
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    revoke_sessions(&mut tx, user_id, action).await?;
    append_audit(
        &mut tx,
        actor,
        action,
        Some(user_id),
        target.username.as_deref(),
        serde_json::json!({ "must_change_password": must_change_password }),
    )
    .await?;
    let user = load_user(&mut tx, user_id).await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(user)
}

async fn acquire_user_write_lock(
    tx: &mut Transaction<'_, Sqlite>,
    user_id: &str,
) -> Result<(), AdminUserRepositoryError> {
    let result = sqlx::query("UPDATE users SET updated_at = updated_at WHERE id = ?")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    if result.rows_affected() == 0 {
        return Err(DbError::NotFound(format!("User '{user_id}' not found")).into());
    }
    Ok(())
}

async fn ensure_another_active_admin(
    tx: &mut Transaction<'_, Sqlite>,
    user_id: &str,
) -> Result<(), AdminUserRepositoryError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users WHERE site_role = 'admin' AND status = 'active' \
         AND user_type = 'local' AND password_hash IS NOT NULL AND password_hash != '' AND id != ?",
    )
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?;
    if count == 0 {
        return Err(AdminUserRepositoryError::LastActiveAdmin);
    }
    Ok(())
}

fn ensure_local_identity(user: &User) -> Result<(), AdminUserRepositoryError> {
    if user.user_type != UserType::Local {
        return Err(AdminUserRepositoryError::UnsupportedIdentity);
    }
    Ok(())
}

async fn load_user(tx: &mut Transaction<'_, Sqlite>, user_id: &str) -> Result<User, AdminUserRepositoryError> {
    sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| DbError::NotFound(format!("User '{user_id}' not found")).into())
}

async fn revoke_sessions(
    tx: &mut Transaction<'_, Sqlite>,
    user_id: &str,
    reason: &str,
) -> Result<(), AdminUserRepositoryError> {
    sqlx::query(
        "UPDATE auth_sessions SET revoked_at = ?, revoke_reason = ? \
         WHERE user_id = ? AND revoked_at IS NULL",
    )
    .bind(aionui_common::now_ms())
    .bind(reason)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn append_audit(
    tx: &mut Transaction<'_, Sqlite>,
    actor: &AuditActor,
    action: &str,
    target_user_id: Option<&str>,
    target_username: Option<&str>,
    details: serde_json::Value,
) -> Result<(), AdminUserRepositoryError> {
    sqlx::query(
        "INSERT INTO admin_audit_log \
         (id, occurred_at, actor_user_id, actor_username, action, target_user_id, target_username, details) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(aionui_common::generate_prefixed_id("audit"))
    .bind(aionui_common::now_ms())
    .bind(actor.user_id.as_deref())
    .bind(actor.username.as_deref())
    .bind(action)
    .bind(target_user_id)
    .bind(target_username)
    .bind(details.to_string())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn map_write_error(error: sqlx::Error) -> AdminUserRepositoryError {
    if error
        .as_database_error()
        .is_some_and(|db_error| matches!(db_error.kind(), sqlx::error::ErrorKind::UniqueViolation))
    {
        DbError::Conflict("username already exists".into()).into()
    } else {
        DbError::Query(error).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::IUserRepository;

    async fn setup() -> (SqliteUserRepository, crate::Database) {
        let database = crate::init_database_memory().await.unwrap();
        let repository = SqliteUserRepository::new(database.pool().clone());
        (repository, database)
    }

    #[tokio::test]
    async fn bootstrap_is_atomic_and_marks_temporary_admin() {
        let (repository, _database) = setup().await;
        let created = repository
            .bootstrap_initial_admin("admin", "hash")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(created.site_role, SiteRole::Admin);
        assert_eq!(created.status, UserStatus::Active);
        assert!(created.must_change_password);
        assert!(
            repository
                .bootstrap_initial_admin("other", "hash2")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn bootstrap_recovers_when_the_system_user_row_is_missing() {
        let (repository, database) = setup().await;
        sqlx::query("DELETE FROM users WHERE id = 'system_default_user'")
            .execute(database.pool())
            .await
            .unwrap();

        let created = repository
            .bootstrap_initial_admin("recovered-admin", "hash")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(created.id, "system_default_user");
        assert_eq!(created.username.as_deref(), Some("recovered-admin"));
        assert_eq!(created.site_role, SiteRole::Admin);
        assert!(created.must_change_password);
    }

    #[tokio::test]
    async fn last_usable_admin_cannot_be_demoted_or_disabled() {
        let (repository, _database) = setup().await;
        repository.bootstrap_initial_admin("admin", "hash").await.unwrap();
        let actor = AuditActor::system();
        let demote = repository
            .update_managed_role("system_default_user", SiteRole::Member, &actor)
            .await;
        assert!(matches!(demote, Err(AdminUserRepositoryError::LastActiveAdmin)));
        let disable = repository
            .update_managed_status("system_default_user", UserStatus::Disabled, &actor)
            .await;
        assert!(matches!(disable, Err(AdminUserRepositoryError::LastActiveAdmin)));

        let second = repository
            .create_managed_user("second-admin", "hash", SiteRole::Admin, &actor)
            .await
            .unwrap();
        repository
            .update_managed_role("system_default_user", SiteRole::Member, &actor)
            .await
            .unwrap();
        assert_eq!(second.site_role, SiteRole::Admin);
    }

    #[tokio::test]
    async fn browser_admin_repository_excludes_external_identities() {
        let (repository, _database) = setup().await;
        repository
            .ensure_external_user(UserType::Aionpro, "external", Default::default())
            .await
            .unwrap();
        assert_eq!(repository.count_managed_users().await.unwrap(), 1);
        let external = repository
            .find_by_external_user_id(UserType::Aionpro, "external")
            .await
            .unwrap()
            .unwrap();
        let result = repository
            .update_managed_status(&external.id, UserStatus::Disabled, &AuditActor::system())
            .await;
        assert!(matches!(result, Err(AdminUserRepositoryError::UnsupportedIdentity)));
    }

    #[tokio::test]
    async fn password_reset_revokes_only_target_sessions_and_audit_is_append_only() {
        let (repository, database) = setup().await;
        repository.bootstrap_initial_admin("admin", "hash").await.unwrap();
        let member = repository
            .create_managed_user("member-one", "hash", SiteRole::Member, &AuditActor::system())
            .await
            .unwrap();
        let admin_session = repository
            .create_auth_session("system_default_user", i64::MAX)
            .await
            .unwrap();
        let member_session = repository.create_auth_session(&member.id, i64::MAX).await.unwrap();
        repository
            .reset_managed_password(&member.id, "new-hash", &AuditActor::system())
            .await
            .unwrap();
        assert!(
            repository
                .is_auth_session_active(&admin_session, "system_default_user")
                .await
                .unwrap()
        );
        assert!(
            !repository
                .is_auth_session_active(&member_session, &member.id)
                .await
                .unwrap()
        );

        let audit_id: String = sqlx::query_scalar("SELECT id FROM admin_audit_log LIMIT 1")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert!(
            sqlx::query("DELETE FROM admin_audit_log WHERE id = ?")
                .bind(audit_id)
                .execute(database.pool())
                .await
                .is_err()
        );
    }
}
