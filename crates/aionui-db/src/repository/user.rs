use crate::error::DbError;
use crate::models::{ExternalUserProjection, User, UserStatus, UserType};

/// User data access abstraction.
///
/// All methods return `Result<_, DbError>` so callers can map database
/// failures into their owning error type.
///
/// Object-safe via `async_trait` to support `Arc<dyn IUserRepository>`.
#[async_trait::async_trait]
pub trait IUserRepository: Send + Sync {
    /// Returns `true` if at least one user with a non-empty password exists.
    ///
    /// The system default user (empty password_hash) does not count.
    async fn has_users(&self) -> Result<bool, DbError>;

    /// Returns the system default user (`id = "system_default_user"`).
    async fn get_system_user(&self) -> Result<Option<User>, DbError>;

    /// Returns the primary WebUI user.
    ///
    /// Priority: system default user first, then falls back to a user named "admin".
    async fn get_primary_webui_user(&self) -> Result<Option<User>, DbError>;

    /// Updates the system default user's username and password hash.
    ///
    /// Used during the initial bootstrap flow.
    async fn set_system_user_credentials(&self, username: &str, password_hash: &str) -> Result<(), DbError>;

    /// Creates a new user and returns the inserted row.
    ///
    /// Returns `DbError::Conflict` if the username already exists.
    async fn create_user(&self, username: &str, password_hash: &str) -> Result<User, DbError>;

    /// Finds an active local password user by username.
    async fn find_by_username(&self, username: &str) -> Result<Option<User>, DbError>;

    /// Idempotently creates or returns an external identity projection.
    async fn ensure_external_user(
        &self,
        user_type: UserType,
        external_user_id: &str,
        projection: ExternalUserProjection,
    ) -> Result<User, DbError>;

    /// Finds a user by external identity mapping.
    async fn find_by_external_user_id(
        &self,
        user_type: UserType,
        external_user_id: &str,
    ) -> Result<Option<User>, DbError>;

    /// One-time adoption of the machine's pre-multi-account data: while
    /// `owner_id` is the ONLY external (aionpro) user in this database,
    /// re-own every user-scoped row currently held by `system_default_user`
    /// to `owner_id`. Returns the number of rows moved.
    ///
    /// Self-idempotent: after the first successful adoption the source set is
    /// empty, so repeated calls move nothing. Once a second external user is
    /// provisioned the adoption window closes permanently. Rows that would
    /// collide with an existing row of the new owner (per-user PK/UNIQUE
    /// tables such as `system_settings`) are skipped, keeping the owner's own
    /// data authoritative.
    async fn adopt_system_default_data(&self, owner_id: &str) -> Result<u64, DbError>;

    /// Whether `owner_id` is the recorded one-shot adopter of the local
    /// default user's data (`users.adopted_by` on the `system_default_user`
    /// row). Exposed so the on-disk file adoption can re-run after a partial
    /// move: `adopt_system_default_data` fires only once, but leftover files
    /// under `users/system_default_user/` may still need moving on a later
    /// login. Only the stamped adopter ever matches, so files can never leak
    /// to any other account — including after more accounts are provisioned.
    async fn is_default_data_adopter(&self, owner_id: &str) -> Result<bool, DbError>;

    /// Finds a user by ID.
    async fn find_by_id(&self, id: &str) -> Result<Option<User>, DbError>;

    /// Finds an active user by ID.
    async fn find_active_by_id(&self, id: &str) -> Result<Option<User>, DbError>;

    /// Lists all users.
    async fn list_users(&self) -> Result<Vec<User>, DbError>;

    /// Returns the total number of users.
    async fn count_users(&self) -> Result<i64, DbError>;

    /// Updates a user's password hash.
    async fn update_password(&self, user_id: &str, password_hash: &str) -> Result<(), DbError>;

    /// Updates a user's username.
    ///
    /// Returns `DbError::Conflict` if the new username already exists.
    async fn update_username(&self, user_id: &str, username: &str) -> Result<(), DbError>;

    /// Updates a user's last login timestamp to the current time.
    async fn update_last_login(&self, user_id: &str) -> Result<(), DbError>;

    /// Updates a user's JWT secret.
    async fn update_jwt_secret(&self, user_id: &str, jwt_secret: &str) -> Result<(), DbError>;

    /// Updates a user's status. Transitioning into `disabled` also revokes
    /// existing sessions by incrementing `session_generation`.
    async fn set_status(&self, user_id: &str, status: UserStatus) -> Result<(), DbError>;

    /// Increments a user's session generation and returns the new value.
    async fn increment_session_generation(&self, user_id: &str) -> Result<i64, DbError>;
}
