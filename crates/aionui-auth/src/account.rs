//! Local-account password provisioning.
//!
//! The single business-logic home for "set (or create) a local user's login
//! password": validate → hash → persist. Its only caller today is the bootstrap
//! `aioncore user set-password` CLI — a thin adapter that just translates I/O
//! and errors around this function. Any future entry point (e.g. a WebUi
//! set-password handler) should delegate here too, so keeping the decision
//! logic here (not in the composition layer) means every entry point applies
//! the same validation, hashing, and default-user semantics.

use aionui_db::{DbError, IUserRepository};

use crate::password::hash_password;
use crate::validation::{validate_password, validate_username};

/// The always-present seed account (`id = "system_default_user"`, login name
/// `admin`, empty password until bootstrapped). Targeted when
/// [`set_local_password`] runs without an explicit username — the fresh-deploy
/// bootstrap path. Also the single row that carries the process-wide JWT
/// secret, so [`crate::secret`] provisioning targets the same id.
pub(crate) const SYSTEM_DEFAULT_USER_ID: &str = "system_default_user";

/// Which account [`set_local_password`] acted on, so callers can report the
/// result without re-querying. Carries no secret material (never the password
/// or its hash) — only the non-sensitive identity of the affected user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordOutcome {
    /// The built-in `system_default_user` (login name `admin`) had its password
    /// set. This is the no-username bootstrap path.
    DefaultUser,
    /// An existing named user's password was updated in place.
    UpdatedExisting { id: String, username: String },
    /// A new named user was created with the given password.
    Created { id: String, username: String },
}

/// Errors from [`set_local_password`], one variant per failure category so
/// adapters can map to their own boundary codes without string matching.
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("invalid username: {0}")]
    InvalidUsername(String),
    #[error("weak password: {0}")]
    WeakPassword(String),
    /// A local user with the requested username already exists. Distinct from a
    /// generic [`AccountError::Db`] failure so adapters can report a
    /// config-class conflict (not an internal error), and callers know the
    /// existing account was left untouched.
    #[error("a local user named {0} already exists")]
    AlreadyExists(String),
    #[error("failed to hash password")]
    Hash,
    #[error("database error: {0}")]
    Db(#[from] DbError),
}

/// Set (or create) a local user's login password.
///
/// - `username == None` → set the password of the built-in
///   `system_default_user` (login name `admin`). The bootstrap path; the seed
///   row always exists, so this updates in place and never changes the
///   username.
/// - `username == Some(existing)` → update that user's password in place.
/// - `username == Some(new)` → create the user with this password.
///
/// Validates the username (when named) and the password strength, then hashes
/// with bcrypt on a blocking thread (CPU-bound; kept off the async reactor)
/// before touching the database.
///
/// Does NOT bump `session_generation`: changing a password while a server is
/// running will not revoke live sessions — restart after a hot change. For
/// fresh bootstrap (server not yet started) this is irrelevant.
pub async fn set_local_password(
    repo: &dyn IUserRepository,
    username: Option<&str>,
    plaintext: &str,
) -> Result<PasswordOutcome, AccountError> {
    if let Some(name) = username {
        validate_username(name).map_err(|e| AccountError::InvalidUsername(e.to_string()))?;
    }
    validate_password(plaintext).map_err(|e| AccountError::WeakPassword(e.to_string()))?;

    // bcrypt hashing is CPU-bound and blocking; keep it off the async reactor.
    let owned = plaintext.to_owned();
    let hash = tokio::task::spawn_blocking(move || hash_password(&owned))
        .await
        .map_err(|_| AccountError::Hash)?
        .map_err(|_| AccountError::Hash)?;

    let outcome = match username {
        None => {
            repo.update_password(SYSTEM_DEFAULT_USER_ID, &hash).await?;
            PasswordOutcome::DefaultUser
        }
        Some(name) => match repo.find_by_username(name).await? {
            Some(user) => {
                repo.update_password(&user.id, &hash).await?;
                PasswordOutcome::UpdatedExisting {
                    id: user.id,
                    username: name.to_owned(),
                }
            }
            None => {
                let user = repo.create_user(name, &hash).await?;
                PasswordOutcome::Created {
                    id: user.id,
                    username: name.to_owned(),
                }
            }
        },
    };

    // Non-sensitive: logs the affected user's id/username, never the password.
    tracing::info!(outcome = ?outcome, "local account password set");
    Ok(outcome)
}

/// Create a *new* local user with the given login password.
///
/// Unlike [`set_local_password`], this never touches an existing account: if a
/// local user named `username` already exists (including the built-in `admin`
/// seed, which owns that username), it returns [`AccountError::AlreadyExists`]
/// and writes nothing — a `create` can never silently overwrite an existing
/// account's password. Use [`set_local_password`] for the intentional
/// set-or-update (upsert) path.
///
/// The username is required (there is no default-user fallback — that is a
/// `set` concept, not a `create` one). Validation and hashing are identical to
/// [`set_local_password`].
///
/// Does NOT bump `session_generation`: a brand-new account has no live sessions
/// to revoke.
pub async fn create_local_user(
    repo: &dyn IUserRepository,
    username: &str,
    plaintext: &str,
) -> Result<PasswordOutcome, AccountError> {
    validate_username(username).map_err(|e| AccountError::InvalidUsername(e.to_string()))?;
    validate_password(plaintext).map_err(|e| AccountError::WeakPassword(e.to_string()))?;

    // bcrypt hashing is CPU-bound and blocking; keep it off the async reactor.
    let owned = plaintext.to_owned();
    let hash = tokio::task::spawn_blocking(move || hash_password(&owned))
        .await
        .map_err(|_| AccountError::Hash)?
        .map_err(|_| AccountError::Hash)?;

    // A unique-constraint violation means the username is already taken (a named
    // user or the `admin` seed). Surface it as a distinct conflict rather than a
    // generic database failure, leaving the existing row untouched.
    let user = repo.create_user(username, &hash).await.map_err(|e| match e {
        DbError::Conflict(_) => AccountError::AlreadyExists(username.to_owned()),
        other => AccountError::Db(other),
    })?;

    let outcome = PasswordOutcome::Created {
        id: user.id,
        username: username.to_owned(),
    };

    // Non-sensitive: logs the created user's id/username, never the password.
    tracing::info!(outcome = ?outcome, "local account created");
    Ok(outcome)
}

#[cfg(test)]
#[path = "account_test.rs"]
mod account_test;
