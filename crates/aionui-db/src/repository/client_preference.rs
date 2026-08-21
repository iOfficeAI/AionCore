use crate::error::DbError;
use crate::models::ClientPreference;

/// Client preference data access abstraction.
///
/// Provides CRUD operations on the generic key-value `client_preferences`
/// table. The table has two scopes (migration 031):
///
/// * **account** — per-user rows, addressed by `(user_id, key)`. The
///   `*_by_keys` / `get_all` / `upsert_batch` / `delete_keys` methods operate
///   exclusively on this scope.
/// * **device** — machine-level rows with `user_id = NULL`, addressed by `key`
///   alone. The `*_device*` methods operate exclusively on this scope.
///
/// Routing a key to the right scope is a service-layer decision; this trait
/// never infers a scope from the key name.
#[async_trait::async_trait]
pub trait IClientPreferenceRepository: Send + Sync {
    /// Returns all account-scope preferences for the given user.
    async fn get_all(&self, user_id: &str) -> Result<Vec<ClientPreference>, DbError>;

    /// Returns the user's account-scope preferences for the given keys only.
    /// Keys that don't exist are simply omitted from the result.
    async fn get_by_keys(&self, user_id: &str, keys: &[&str]) -> Result<Vec<ClientPreference>, DbError>;

    /// Inserts or updates a batch of account-scope key-value pairs.
    async fn upsert_batch(&self, user_id: &str, entries: &[(&str, &str)]) -> Result<(), DbError>;

    /// Deletes the given account-scope keys for the user.
    async fn delete_keys(&self, user_id: &str, keys: &[&str]) -> Result<(), DbError>;

    /// Returns all device-scope (machine-level) preferences.
    async fn get_all_device(&self) -> Result<Vec<ClientPreference>, DbError>;

    /// Returns the device-scope preferences for the given keys only.
    /// Keys that don't exist are simply omitted from the result.
    async fn get_device_by_keys(&self, keys: &[&str]) -> Result<Vec<ClientPreference>, DbError>;

    /// Inserts or updates a batch of device-scope key-value pairs.
    async fn upsert_device_batch(&self, entries: &[(&str, &str)]) -> Result<(), DbError>;

    /// Deletes the given device-scope keys.
    async fn delete_device_keys(&self, keys: &[&str]) -> Result<(), DbError>;
}
