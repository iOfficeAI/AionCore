use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{ExternalUserProjection, User, UserStatus, UserType};
use crate::repository::IUserRepository;

/// SQLite-backed implementation of [`IUserRepository`].
#[derive(Clone, Debug)]
pub struct SqliteUserRepository {
    pool: SqlitePool,
}

impl SqliteUserRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IUserRepository for SqliteUserRepository {
    async fn has_users(&self) -> Result<bool, DbError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM users \
             WHERE user_type = 'local' AND password_hash IS NOT NULL AND password_hash != ''",
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0 > 0)
    }

    async fn get_system_user(&self) -> Result<Option<User>, DbError> {
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = 'system_default_user'")
            .fetch_optional(&self.pool)
            .await?;

        Ok(user)
    }

    async fn get_primary_webui_user(&self) -> Result<Option<User>, DbError> {
        // Priority: system default user first
        if let Some(user) = self.get_system_user().await? {
            return Ok(Some(user));
        }

        // Fallback: user named "admin"
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = 'admin'")
            .fetch_optional(&self.pool)
            .await?;

        Ok(user)
    }

    async fn set_system_user_credentials(&self, username: &str, password_hash: &str) -> Result<(), DbError> {
        let now = aionui_common::now_ms();
        let result = sqlx::query(
            "UPDATE users SET username = ?, password_hash = ?, updated_at = ? \
             WHERE id = 'system_default_user' AND user_type = 'local'",
        )
        .bind(username)
        .bind(password_hash)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db_err) if is_unique_violation(db_err.as_ref()) => {
                DbError::Conflict(format!("Username '{username}' already exists"))
            }
            _ => DbError::Query(e),
        })?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound("system_default_user not found".to_string()));
        }

        Ok(())
    }

    async fn create_user(&self, username: &str, password_hash: &str) -> Result<User, DbError> {
        let id = aionui_common::generate_prefixed_id("user");
        let now = aionui_common::now_ms();

        sqlx::query(
            "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, ?, 'active', 0, ?, ?)",
        )
        .bind(&id)
        .bind(username)
        .bind(password_hash)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db_err) if is_unique_violation(db_err.as_ref()) => {
                DbError::Conflict(format!("Username '{username}' already exists"))
            }
            _ => DbError::Query(e),
        })?;

        Ok(User {
            id,
            user_type: UserType::Local,
            external_user_id: None,
            username: Some(username.to_string()),
            email: None,
            password_hash: Some(password_hash.to_string()),
            avatar_path: None,
            jwt_secret: None,
            status: UserStatus::Active,
            session_generation: 0,
            created_at: now,
            updated_at: now,
            last_login: None,
        })
    }

    async fn find_by_username(&self, username: &str) -> Result<Option<User>, DbError> {
        let user = sqlx::query_as::<_, User>(
            "SELECT * FROM users \
             WHERE user_type = 'local' AND password_hash IS NOT NULL AND username = ?",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;

        Ok(user)
    }

    async fn ensure_external_user(
        &self,
        user_type: UserType,
        external_user_id: &str,
        projection: ExternalUserProjection,
    ) -> Result<User, DbError> {
        if user_type == UserType::Local {
            return Err(DbError::Conflict(
                "External identity projection requires a non-local user type".to_string(),
            ));
        }
        if external_user_id.trim().is_empty() {
            return Err(DbError::Conflict("external_user_id must not be empty".to_string()));
        }

        if let Some(existing) = self.find_by_external_user_id(user_type, external_user_id).await? {
            return Ok(existing);
        }

        let id = aionui_common::generate_prefixed_id("user");
        let now = aionui_common::now_ms();

        sqlx::query(
            "INSERT INTO users \
             (id, user_type, external_user_id, username, email, password_hash, avatar_path, status, session_generation, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, NULL, ?, 'active', 0, ?, ?)",
        )
        .bind(&id)
        .bind(user_type.as_str())
        .bind(external_user_id)
        .bind(projection.username.as_deref())
        .bind(projection.email.as_deref())
        .bind(projection.avatar_path.as_deref())
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db_err) if is_unique_violation(db_err.as_ref()) => {
                DbError::Conflict("External user mapping already exists".to_string())
            }
            _ => DbError::Query(e),
        })?;

        self.find_by_id(&id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("User '{id}' not found after insert")))
    }

    async fn find_by_external_user_id(
        &self,
        user_type: UserType,
        external_user_id: &str,
    ) -> Result<Option<User>, DbError> {
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE user_type = ? AND external_user_id = ?")
            .bind(user_type.as_str())
            .bind(external_user_id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(user)
    }

    async fn adopt_system_default_data(&self, owner_id: &str) -> Result<u64, DbError> {
        let mut tx = self.pool.begin().await?;

        // Adoption window: exactly one external user, and it is the caller.
        // A second provisioned account closes the window forever — later
        // accounts must never inherit another machine user's data.
        let (external_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE user_type != 'local'")
            .fetch_one(&mut *tx)
            .await?;
        if external_count != 1 {
            return Ok(0);
        }
        let (is_owner,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE id = ? AND user_type != 'local'")
            .bind(owner_id)
            .fetch_one(&mut *tx)
            .await?;
        if is_owner != 1 {
            return Ok(0);
        }
        // One-shot marker: adoption happens exactly once, on the first
        // external account's first login. Eligibility alone used to be
        // re-evaluated on every provision call, which kept re-sweeping data
        // the local default user accumulated AFTER the first adoption (new
        // AionUi conversations were repeatedly re-owned). A stamped marker
        // closes the window permanently regardless of later data.
        let already_adopted: Option<Option<String>> =
            sqlx::query_scalar("SELECT adopted_by FROM users WHERE id = 'system_default_user'")
                .fetch_optional(&mut *tx)
                .await?;
        if matches!(already_adopted, Some(Some(_))) {
            return Ok(0);
        }

        // Discover ownership tables from the live schema rather than a
        // hand-maintained list, so user-scoped tables added by future
        // migrations are adopted automatically. Convention (root-scope
        // design): the ownership column is `user_id`, or `owner_user_id` on
        // tables that also carry an external platform user id (channel
        // bindings) or reference another root's `user_id` (project explorer).
        // The exhaustiveness of this convention is enforced by the
        // adoption-coverage classification test in aionui-db/tests.
        let mut moved: u64 = 0;
        for owner_column in ["user_id", "owner_user_id"] {
            let tables: Vec<(String,)> = sqlx::query_as(
                "SELECT m.name FROM sqlite_master m \
                 WHERE m.type = 'table' \
                   AND m.name NOT LIKE 'sqlite_%' \
                   AND m.name != 'users' \
                   AND EXISTS (SELECT 1 FROM pragma_table_info(m.name) p WHERE p.name = ?)",
            )
            .bind(owner_column)
            .fetch_all(&mut *tx)
            .await?;

            // Global template rows (`user_id IS NULL`) are shared and stay put;
            // only rows owned by the local default user move. `UPDATE OR IGNORE`
            // skips rows that would collide with the new owner's existing rows on
            // per-user PK/UNIQUE tables (e.g. `system_settings.user_id`).
            for (table,) in &tables {
                let result = sqlx::query(&format!(
                    "UPDATE OR IGNORE \"{table}\" SET \"{owner_column}\" = ? \
                     WHERE \"{owner_column}\" = 'system_default_user'"
                ))
                .bind(owner_id)
                .execute(&mut *tx)
                .await?;
                moved += result.rows_affected();
            }
        }

        // Stamp the one-shot marker in the same transaction: even a zero-row
        // sweep (fresh machine with no local data) consumes the adoption
        // opportunity — from now on local-mode data stays local.
        sqlx::query(
            "UPDATE users SET adopted_by = ?, adopted_at = ? \
             WHERE id = 'system_default_user' AND adopted_by IS NULL",
        )
        .bind(owner_id)
        .bind(aionui_common::now_ms())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(moved)
    }

    async fn is_default_data_adopter(&self, owner_id: &str) -> Result<bool, DbError> {
        // The one-shot marker stamped by `adopt_system_default_data` is the
        // single source of truth for "who adopted": only that account may
        // re-run the on-disk file move.
        let (matches,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM users WHERE id = 'system_default_user' AND adopted_by = ?")
                .bind(owner_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(matches == 1)
    }

    async fn find_by_id(&self, id: &str) -> Result<Option<User>, DbError> {
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(user)
    }

    async fn find_active_by_id(&self, id: &str) -> Result<Option<User>, DbError> {
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ? AND status = 'active'")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(user)
    }

    async fn list_users(&self) -> Result<Vec<User>, DbError> {
        let users = sqlx::query_as::<_, User>("SELECT * FROM users")
            .fetch_all(&self.pool)
            .await?;

        Ok(users)
    }

    async fn count_users(&self) -> Result<i64, DbError> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?;

        Ok(row.0)
    }

    async fn update_password(&self, user_id: &str, password_hash: &str) -> Result<(), DbError> {
        let now = aionui_common::now_ms();
        let result = sqlx::query(
            "UPDATE users SET password_hash = ?, updated_at = ? \
             WHERE id = ? AND user_type = 'local'",
        )
        .bind(password_hash)
        .bind(now)
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("User '{user_id}' not found")));
        }

        Ok(())
    }

    async fn update_username(&self, user_id: &str, username: &str) -> Result<(), DbError> {
        let now = aionui_common::now_ms();
        let result = sqlx::query(
            "UPDATE users SET username = ?, updated_at = ? \
             WHERE id = ? AND user_type = 'local'",
        )
        .bind(username)
        .bind(now)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db_err) if is_unique_violation(db_err.as_ref()) => {
                DbError::Conflict(format!("Username '{username}' already exists"))
            }
            _ => DbError::Query(e),
        })?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("User '{user_id}' not found")));
        }

        Ok(())
    }

    async fn update_last_login(&self, user_id: &str) -> Result<(), DbError> {
        let now = aionui_common::now_ms();
        let result = sqlx::query("UPDATE users SET last_login = ?, updated_at = ? WHERE id = ?")
            .bind(now)
            .bind(now)
            .bind(user_id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("User '{user_id}' not found")));
        }

        Ok(())
    }

    async fn update_jwt_secret(&self, user_id: &str, jwt_secret: &str) -> Result<(), DbError> {
        let now = aionui_common::now_ms();
        let result = sqlx::query("UPDATE users SET jwt_secret = ?, updated_at = ? WHERE id = ?")
            .bind(jwt_secret)
            .bind(now)
            .bind(user_id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("User '{user_id}' not found")));
        }

        Ok(())
    }

    async fn set_status(&self, user_id: &str, status: UserStatus) -> Result<(), DbError> {
        let now = aionui_common::now_ms();
        let result = sqlx::query(
            "UPDATE users \
             SET status = ?, \
                 session_generation = CASE \
                     WHEN ? = 'disabled' AND status != 'disabled' THEN session_generation + 1 \
                     ELSE session_generation \
                 END, \
                 updated_at = ? \
             WHERE id = ?",
        )
        .bind(status.as_str())
        .bind(status.as_str())
        .bind(now)
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("User '{user_id}' not found")));
        }

        Ok(())
    }

    async fn increment_session_generation(&self, user_id: &str) -> Result<i64, DbError> {
        let now = aionui_common::now_ms();
        let result = sqlx::query(
            "UPDATE users \
             SET session_generation = session_generation + 1, updated_at = ? \
             WHERE id = ?",
        )
        .bind(now)
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("User '{user_id}' not found")));
        }

        let generation: i64 = sqlx::query_scalar("SELECT session_generation FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_one(&self.pool)
            .await?;

        Ok(generation)
    }
}

/// Checks if a SQLite database error is a UNIQUE constraint violation.
fn is_unique_violation(err: &dyn sqlx::error::DatabaseError) -> bool {
    // SQLite error code 2067 = SQLITE_CONSTRAINT_UNIQUE, 1555 = SQLITE_CONSTRAINT_PRIMARYKEY.
    err.code().is_some_and(|c| c == "2067" || c == "1555")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    async fn setup() -> (SqliteUserRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteUserRepository::new(db.pool().clone());
        (repo, db)
    }

    // -- Unit tests for is_unique_violation helper --

    #[test]
    fn unique_violation_code_detected() {
        // SQLite UNIQUE violation has code "2067"
        assert!(is_unique_violation(&FakeDbError("2067")));
    }

    #[test]
    fn non_unique_violation_code_rejected() {
        assert!(!is_unique_violation(&FakeDbError("1299")));
    }

    /// Minimal fake for testing is_unique_violation.
    struct FakeDbError(&'static str);

    impl std::fmt::Display for FakeDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "fake error")
        }
    }

    impl std::fmt::Debug for FakeDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "FakeDbError({})", self.0)
        }
    }

    impl std::error::Error for FakeDbError {}

    impl sqlx::error::DatabaseError for FakeDbError {
        fn message(&self) -> &str {
            "fake"
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::UniqueViolation
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(std::borrow::Cow::Borrowed(self.0))
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
    }

    // -- Integration tests that exercise the repository against in-memory SQLite --

    #[tokio::test]
    async fn create_user_returns_populated_fields() {
        let (repo, _db) = setup().await;
        let user = repo.create_user("alice", "hash123").await.unwrap();

        assert!(user.id.starts_with("user_"));
        assert_eq!(user.user_type, UserType::Local);
        assert_eq!(user.status, UserStatus::Active);
        assert_eq!(user.session_generation, 0);
        assert_eq!(user.username.as_deref(), Some("alice"));
        assert_eq!(user.password_hash.as_deref(), Some("hash123"));
        assert!(user.email.is_none());
        assert!(user.avatar_path.is_none());
        assert!(user.jwt_secret.is_none());
        assert!(user.last_login.is_none());
        assert!(user.created_at > 0);
        assert_eq!(user.created_at, user.updated_at);
    }

    #[tokio::test]
    async fn create_user_duplicate_username_returns_conflict() {
        let (repo, _db) = setup().await;
        repo.create_user("bob", "h1").await.unwrap();

        let err = repo.create_user("bob", "h2").await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn has_users_false_when_only_system_user() {
        let (repo, _db) = setup().await;
        assert!(!repo.has_users().await.unwrap());
    }

    #[tokio::test]
    async fn has_users_true_after_creating_real_user() {
        let (repo, _db) = setup().await;
        repo.create_user("real", "pass").await.unwrap();
        assert!(repo.has_users().await.unwrap());
    }

    #[tokio::test]
    async fn get_system_user_returns_default() {
        let (repo, _db) = setup().await;
        let user = repo.get_system_user().await.unwrap().unwrap();
        assert_eq!(user.id, "system_default_user");
        assert_eq!(user.user_type, UserType::Local);
        assert_eq!(user.status, UserStatus::Active);
        assert_eq!(user.username.as_deref(), Some("admin"));
    }

    #[tokio::test]
    async fn get_primary_webui_user_returns_system_user_first() {
        let (repo, _db) = setup().await;
        // Can't use "admin" here: the seeded system_default_user already owns that
        // username after the M6 default change. Any fresh user gets a different name.
        repo.create_user("other", "hash").await.unwrap();

        let user = repo.get_primary_webui_user().await.unwrap().unwrap();
        assert_eq!(user.id, "system_default_user");
    }

    #[tokio::test]
    async fn find_by_username_existing() {
        let (repo, _db) = setup().await;
        repo.create_user("charlie", "h").await.unwrap();

        let found = repo.find_by_username("charlie").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().username.as_deref(), Some("charlie"));
    }

    #[tokio::test]
    async fn find_by_username_missing() {
        let (repo, _db) = setup().await;
        assert!(repo.find_by_username("ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn find_by_id_existing() {
        let (repo, _db) = setup().await;
        let created = repo.create_user("dave", "h").await.unwrap();

        let found = repo.find_by_id(&created.id).await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, created.id);
    }

    #[tokio::test]
    async fn find_by_id_missing() {
        let (repo, _db) = setup().await;
        assert!(repo.find_by_id("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_users_includes_system_and_created() {
        let (repo, _db) = setup().await;
        repo.create_user("eve", "h").await.unwrap();
        repo.create_user("frank", "h").await.unwrap();

        let users = repo.list_users().await.unwrap();
        // system_default_user + eve + frank
        assert_eq!(users.len(), 3);
    }

    #[tokio::test]
    async fn count_users_includes_all() {
        let (repo, _db) = setup().await;
        repo.create_user("grace", "h").await.unwrap();

        // system_default_user + grace
        assert_eq!(repo.count_users().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn update_password_succeeds() {
        let (repo, _db) = setup().await;
        let user = repo.create_user("hal", "old_hash").await.unwrap();

        repo.update_password(&user.id, "new_hash").await.unwrap();

        let updated = repo.find_by_id(&user.id).await.unwrap().unwrap();
        assert_eq!(updated.password_hash.as_deref(), Some("new_hash"));
        assert!(updated.updated_at >= user.updated_at);
    }

    #[tokio::test]
    async fn update_password_nonexistent_user() {
        let (repo, _db) = setup().await;
        let err = repo.update_password("no_such_id", "h").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_username_succeeds() {
        let (repo, _db) = setup().await;
        let user = repo.create_user("ivan", "h").await.unwrap();

        repo.update_username(&user.id, "ivan_new").await.unwrap();

        let updated = repo.find_by_id(&user.id).await.unwrap().unwrap();
        assert_eq!(updated.username.as_deref(), Some("ivan_new"));
    }

    #[tokio::test]
    async fn update_username_conflict() {
        let (repo, _db) = setup().await;
        repo.create_user("jane", "h").await.unwrap();
        let other = repo.create_user("kate", "h").await.unwrap();

        let err = repo.update_username(&other.id, "jane").await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn update_last_login_sets_timestamp() {
        let (repo, _db) = setup().await;
        let user = repo.create_user("leo", "h").await.unwrap();
        assert!(user.last_login.is_none());

        repo.update_last_login(&user.id).await.unwrap();

        let updated = repo.find_by_id(&user.id).await.unwrap().unwrap();
        assert!(updated.last_login.is_some());
        assert!(updated.last_login.unwrap() > 0);
    }

    #[tokio::test]
    async fn update_jwt_secret_succeeds() {
        let (repo, _db) = setup().await;
        let user = repo.create_user("mike", "h").await.unwrap();
        assert!(user.jwt_secret.is_none());

        repo.update_jwt_secret(&user.id, "secret123").await.unwrap();

        let updated = repo.find_by_id(&user.id).await.unwrap().unwrap();
        assert_eq!(updated.jwt_secret.as_deref(), Some("secret123"));
    }

    #[tokio::test]
    async fn set_system_user_credentials_conflict_with_existing_username() {
        let (repo, _db) = setup().await;
        repo.create_user("taken", "h").await.unwrap();

        let err = repo.set_system_user_credentials("taken", "hash").await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn set_system_user_credentials_updates_fields() {
        let (repo, _db) = setup().await;

        repo.set_system_user_credentials("admin", "secure_hash").await.unwrap();

        let user = repo.get_system_user().await.unwrap().unwrap();
        assert_eq!(user.username.as_deref(), Some("admin"));
        assert_eq!(user.password_hash.as_deref(), Some("secure_hash"));
    }

    #[tokio::test]
    async fn ensure_external_user_is_idempotent_and_has_no_password() {
        let (repo, _db) = setup().await;
        let projection = ExternalUserProjection {
            username: Some("AionPro User".to_string()),
            email: Some("user@example.com".to_string()),
            avatar_path: Some("/avatar.png".to_string()),
        };

        let first = repo
            .ensure_external_user(UserType::Aionpro, "external-123", projection)
            .await
            .unwrap();
        let second = repo
            .ensure_external_user(
                UserType::Aionpro,
                "external-123",
                ExternalUserProjection {
                    username: Some("Different".to_string()),
                    email: None,
                    avatar_path: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(first.user_type, UserType::Aionpro);
        assert_eq!(first.external_user_id.as_deref(), Some("external-123"));
        assert_eq!(first.status, UserStatus::Active);
        assert_eq!(first.session_generation, 0);
        assert!(first.password_hash.is_none());
        assert!(repo.find_by_username("AionPro User").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn disabled_user_is_excluded_from_active_lookup() {
        let (repo, _db) = setup().await;
        let user = repo
            .ensure_external_user(
                UserType::Aionpro,
                "external-disabled",
                ExternalUserProjection::default(),
            )
            .await
            .unwrap();

        assert!(repo.find_active_by_id(&user.id).await.unwrap().is_some());
        repo.set_status(&user.id, UserStatus::Disabled).await.unwrap();

        let disabled = repo.find_by_id(&user.id).await.unwrap().unwrap();
        assert_eq!(disabled.status, UserStatus::Disabled);
        assert_eq!(disabled.session_generation, 1);
        assert!(repo.find_active_by_id(&user.id).await.unwrap().is_none());

        repo.set_status(&user.id, UserStatus::Disabled).await.unwrap();
        let disabled_again = repo.find_by_id(&user.id).await.unwrap().unwrap();
        assert_eq!(disabled_again.session_generation, 1);
    }

    #[tokio::test]
    async fn increment_session_generation_returns_new_value() {
        let (repo, _db) = setup().await;
        let user = repo.create_user("session-user", "h").await.unwrap();

        let generation = repo.increment_session_generation(&user.id).await.unwrap();

        assert_eq!(generation, 1);
        let updated = repo.find_by_id(&user.id).await.unwrap().unwrap();
        assert_eq!(updated.session_generation, 1);
    }

    async fn seed_legacy_conversation(db: &crate::Database, id: &str) {
        let now = aionui_common::now_ms();
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
             VALUES (?, 'system_default_user', 'legacy', 'aionrs', ?, ?)",
        )
        .bind(id)
        .bind(now)
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();
    }

    async fn conversation_owner(db: &crate::Database, id: &str) -> String {
        let (owner,): (String,) = sqlx::query_as("SELECT user_id FROM conversations WHERE id = ?")
            .bind(id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        owner
    }

    #[tokio::test]
    async fn adopt_moves_legacy_data_to_sole_external_user_once() {
        let (repo, db) = setup().await;
        seed_legacy_conversation(&db, "conv-legacy").await;
        let user = repo
            .ensure_external_user(UserType::Aionpro, "ext-1", ExternalUserProjection::default())
            .await
            .unwrap();

        let moved = repo.adopt_system_default_data(&user.id).await.unwrap();
        assert!(moved >= 1);
        assert_eq!(conversation_owner(&db, "conv-legacy").await, user.id);

        // Self-idempotent: the source set is now empty.
        let moved_again = repo.adopt_system_default_data(&user.id).await.unwrap();
        assert_eq!(moved_again, 0);
    }

    #[tokio::test]
    async fn adopt_is_refused_once_a_second_external_user_exists() {
        let (repo, db) = setup().await;
        seed_legacy_conversation(&db, "conv-legacy-2").await;
        let first = repo
            .ensure_external_user(UserType::Aionpro, "ext-a", ExternalUserProjection::default())
            .await
            .unwrap();
        let second = repo
            .ensure_external_user(UserType::Aionpro, "ext-b", ExternalUserProjection::default())
            .await
            .unwrap();

        assert_eq!(repo.adopt_system_default_data(&first.id).await.unwrap(), 0);
        assert_eq!(repo.adopt_system_default_data(&second.id).await.unwrap(), 0);
        assert_eq!(conversation_owner(&db, "conv-legacy-2").await, "system_default_user");
    }

    #[tokio::test]
    async fn adopt_is_refused_for_local_or_unknown_owner() {
        let (repo, db) = setup().await;
        seed_legacy_conversation(&db, "conv-legacy-3").await;

        // No external user at all.
        assert_eq!(repo.adopt_system_default_data("system_default_user").await.unwrap(), 0);

        // Owner id that is not the external user.
        repo.ensure_external_user(UserType::Aionpro, "ext-c", ExternalUserProjection::default())
            .await
            .unwrap();
        assert_eq!(repo.adopt_system_default_data("someone-else").await.unwrap(), 0);
        assert_eq!(conversation_owner(&db, "conv-legacy-3").await, "system_default_user");
    }

    #[tokio::test]
    async fn adopt_discovers_all_known_user_scoped_tables() {
        let (_repo, db) = setup().await;
        let tables: Vec<(String,)> = sqlx::query_as(
            "SELECT m.name FROM sqlite_master m \
             WHERE m.type = 'table' \
               AND m.name NOT LIKE 'sqlite_%' \
               AND m.name != 'users' \
               AND EXISTS (SELECT 1 FROM pragma_table_info(m.name) p WHERE p.name = 'user_id')",
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        let names: Vec<&str> = tables.iter().map(|(n,)| n.as_str()).collect();

        // Sentinel: the discovery convention (`user_id` column == ownership)
        // must keep matching the core scope tables. If this fails, either a
        // migration renamed an ownership column or the convention broke —
        // both must be looked at before shipping.
        for expected in [
            "conversations",
            "teams",
            "cron_jobs",
            "skills",
            "mcp_servers",
            "providers",
            "assistants",
            "system_settings",
            "client_preferences",
        ] {
            assert!(
                names.contains(&expected),
                "expected user-scoped table '{expected}' to be discovered"
            );
        }
    }

    #[tokio::test]
    async fn adopt_skips_rows_colliding_with_the_new_owner() {
        let (repo, db) = setup().await;
        let now = aionui_common::now_ms();
        let user = repo
            .ensure_external_user(UserType::Aionpro, "ext-d", ExternalUserProjection::default())
            .await
            .unwrap();

        // Both the legacy user and the new owner have a system_settings row
        // (PRIMARY KEY user_id) — the owner's row must win, the legacy row
        // must be left behind rather than erroring the whole adoption.
        sqlx::query(
            "INSERT INTO system_settings (user_id, language, updated_at) VALUES ('system_default_user', 'zh-CN', ?)",
        )
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("INSERT INTO system_settings (user_id, language, updated_at) VALUES (?, 'en-US', ?)")
            .bind(&user.id)
            .bind(now)
            .execute(db.pool())
            .await
            .unwrap();
        seed_legacy_conversation(&db, "conv-legacy-4").await;

        repo.adopt_system_default_data(&user.id).await.unwrap();

        let (language,): (String,) = sqlx::query_as("SELECT language FROM system_settings WHERE user_id = ?")
            .bind(&user.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(language, "en-US");
        assert_eq!(conversation_owner(&db, "conv-legacy-4").await, user.id);
    }
}
