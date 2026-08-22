//! Storage-encryption-secret provisioning and inspection for self-hosted
//! deployments.
//!
//! The encryption secret is the storage-encryption root: the server derives an
//! AES-256-GCM key from it (`derive_encryption_key`) to encrypt provider API
//! keys, channel credentials, and remote-agent tokens. It is decoupled from the
//! JWT *signing* secret (which may rotate freely, e.g. on change-password), so
//! it must be **stable across restarts** — if the derived key changes, every
//! stored credential becomes silently undecryptable.
//!
//! The server resolves its encryption secret in priority order:
//! `AIONUI_ENCRYPTION_SECRET` env → `system_default_user.encryption_secret` in
//! the database → seeded from the effective JWT secret (then persisted). The
//! gap this module closes: an env-provided secret is *used but never persisted*.
//! A node booted with `AIONUI_ENCRYPTION_SECRET` set works, but the moment it
//! restarts without that env var, the resolver falls back to a *different*
//! persisted (or seeded) value, and every credential encrypted under the old
//! key is lost.
//!
//! These functions let an operator inspect and persist the secret out-of-band
//! (the bootstrap `aioncore secret …` CLI), so the effective secret lives in
//! the database and survives restarts regardless of the environment.
//!
//! Business rules that live here (not in the CLI adapter):
//! - The secret is **never** returned or printed; only non-reversible
//!   fingerprints and presence booleans cross this boundary.
//! - A write that would replace the currently-effective secret with a
//!   *different* value is refused unless the caller passes `force`, because
//!   doing so breaks decryption of everything already stored.

use aionui_db::{DbError, IUserRepository};
use sha2::{Digest, Sha256};

use crate::account::SYSTEM_DEFAULT_USER_ID;
use crate::jwt::generate_random_secret_string;

/// Which source supplies the secret the running server would actually use,
/// mirroring the resolver's env → `encryption_secret` column → seed-from-signing
/// → (none yet) priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretSource {
    /// The `AIONUI_ENCRYPTION_SECRET` environment variable (highest priority).
    /// Note this is used but not persisted by the server — see the module docs.
    Environment,
    /// The `system_default_user.encryption_secret` database column.
    Database,
    /// The `encryption_secret` column is empty, but the system user carries a
    /// `jwt_secret`: the server seeds the encryption root from it (the
    /// zero-re-encrypt upgrade path) and persists that exact value on next
    /// start. Crucially, an effective root ALREADY exists, so generating or
    /// setting a divergent one without `force` would orphan every stored
    /// credential — the overwrite guard treats this the same as `Database`.
    SeededFromSigning,
    /// Neither a secret nor a signing seed is present: the server would generate
    /// and persist a random secret on next start.
    None,
}

/// Non-sensitive snapshot of secret provisioning state.
///
/// Carries only presence booleans, fingerprints, and the effective source —
/// never the secret itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretStatus {
    /// A usable (non-empty) secret is present in the environment.
    pub env_present: bool,
    /// Fingerprint of the environment secret, when present.
    pub env_fingerprint: Option<String>,
    /// A usable (non-empty) secret is persisted in the database.
    pub db_present: bool,
    /// Fingerprint of the database secret, when present.
    pub db_fingerprint: Option<String>,
    /// The source the server would resolve to right now.
    pub effective_source: SecretSource,
}

impl SecretStatus {
    /// True when both env and database hold a secret and they differ.
    ///
    /// A mismatch means the server currently runs on the env secret, but a
    /// restart without the env var would fall back to a *different* database
    /// secret — changing the derived key and breaking decryption. The operator
    /// should reconcile them (persist the env value, or unset the env var).
    pub fn env_db_mismatch(&self) -> bool {
        match (&self.env_fingerprint, &self.db_fingerprint) {
            (Some(env), Some(db)) => env != db,
            _ => false,
        }
    }

    /// True when the effective secret exists only in the environment and is not
    /// persisted. This is the "secret gap": it works now but is lost on a
    /// restart that does not re-supply the env var.
    pub fn effective_secret_is_unpersisted(&self) -> bool {
        self.effective_source == SecretSource::Environment && !self.db_present
    }
}

/// The result of a secret write, describing the database transition. Carries
/// fingerprints only, never the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretWriteOutcome {
    /// A secret was persisted where the database previously had none.
    Created { fingerprint: String },
    /// An existing database secret was replaced (only reachable with `force`,
    /// or when persisting the already-effective env secret).
    Overwritten {
        old_fingerprint: String,
        new_fingerprint: String,
    },
    /// The database already held exactly this secret — no write performed.
    Unchanged { fingerprint: String },
}

/// Errors from the secret provisioning functions.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret must not be empty")]
    Empty,
    /// A different secret is already effective; refusing to overwrite without
    /// `force`. The fingerprint identifies the current secret without exposing
    /// it, so the operator can tell whether the divergence is expected.
    #[error("a different secret is already in effect (fingerprint {current_fingerprint}); pass force to overwrite")]
    AlreadyExists { current_fingerprint: String },
    #[error("database error: {0}")]
    Db(#[from] DbError),
}

/// A short, non-reversible fingerprint of a secret, for human comparison.
///
/// The first 6 bytes of a domain-separated SHA-256, hex-encoded (12 chars).
/// Enough to tell two secrets apart or confirm two sources match, without
/// printing or being able to recover either. The domain-separation prefix is
/// deliberately distinct from `derive_encryption_key`'s, so a fingerprint can
/// never coincide with (a prefix of) the actual encryption key.
pub fn secret_fingerprint(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"aionui-secret-fingerprint:");
    hasher.update(secret.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(12);
    for byte in &digest[..6] {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Normalize an incoming environment secret: an empty value is treated as
/// absent, since it cannot serve as a usable encryption root. This mirrors the
/// database side, which the server also filters non-empty.
fn normalize(secret: Option<&str>) -> Option<&str> {
    secret.filter(|s| !s.is_empty())
}

/// The system user's persisted encryption-relevant roots, read in one query.
///
/// Mirrors what the server's resolver sees on the database side, so the CLI's
/// judgement of "what root is effective" cannot drift from the running server's.
struct DbRoots {
    /// Non-empty `encryption_secret` column — the explicitly persisted root.
    encryption: Option<String>,
    /// Non-empty `jwt_secret` column — the seed root the server falls back to
    /// when `encryption` is absent (the zero-re-encrypt upgrade path). Read only
    /// to judge presence/fingerprint; it is never surfaced as a secret.
    jwt_seed: Option<String>,
}

/// Read the system default user's persisted encryption root and its signing-seed
/// fallback together. Both are normalized: an empty column is treated as absent.
async fn read_db_roots(repo: &dyn IUserRepository) -> Result<DbRoots, SecretError> {
    let user = repo.get_system_user().await?;
    let (encryption, jwt_seed) = match user {
        Some(u) => (
            u.encryption_secret.filter(|s| !s.is_empty()),
            u.jwt_secret.filter(|s| !s.is_empty()),
        ),
        None => (None, None),
    };
    Ok(DbRoots { encryption, jwt_seed })
}

/// The effective signing-seed root the server would fall back to for the
/// encryption key when no encryption secret is set, in the resolver's own
/// non-generating priority: `JWT_SECRET` env → the `jwt_secret` column.
///
/// This must mirror [`crate::jwt::resolve_jwt_secret`]'s env-over-column order,
/// because the server seeds the encryption root from the *effective* signing
/// secret — which is the env value when `JWT_SECRET` is set, not the column. A
/// CLI that judged the seed from the column alone would miss this and let a
/// `generate`/`set` silently orphan the credentials of an env-signing node whose
/// column is still empty (the common "secrets via env" deployment).
///
/// If both are empty the server would *generate* a fresh signing secret on a
/// genuinely fresh install — nothing is encrypted yet, so there is no root to
/// protect and this yields `None`.
fn effective_jwt_seed(jwt_env: Option<&str>, jwt_column: Option<String>) -> Option<String> {
    normalize(jwt_env).map(str::to_owned).or(jwt_column)
}

/// Inspect the current secret provisioning without exposing any secret.
///
/// `env_secret` is the raw `AIONUI_ENCRYPTION_SECRET` value (or `None`) and
/// `jwt_env` the raw `JWT_SECRET` value (or `None`); an empty string is treated
/// as absent for both. The database side reads
/// `system_default_user.encryption_secret`, and — when that column is empty —
/// falls back to the effective signing seed (`JWT_SECRET` env → `jwt_secret`
/// column) to report the effective source exactly as the server would resolve it.
pub async fn read_secret_status(
    repo: &dyn IUserRepository,
    env_secret: Option<&str>,
    jwt_env: Option<&str>,
) -> Result<SecretStatus, SecretError> {
    let env = normalize(env_secret);
    let roots = read_db_roots(repo).await?;
    // `db_present`/`db_fingerprint` describe the *persisted* `encryption_secret`
    // column only, so the unpersisted/mismatch warnings keep their meaning. The
    // signing seed (env or column) feeds only the effective-source verdict below.
    let db = roots.encryption;
    let jwt_seed = effective_jwt_seed(jwt_env, roots.jwt_seed);

    let effective_source = if env.is_some() {
        SecretSource::Environment
    } else if db.is_some() {
        SecretSource::Database
    } else if jwt_seed.is_some() {
        SecretSource::SeededFromSigning
    } else {
        SecretSource::None
    };

    Ok(SecretStatus {
        env_present: env.is_some(),
        env_fingerprint: env.map(secret_fingerprint),
        db_present: db.is_some(),
        db_fingerprint: db.as_deref().map(secret_fingerprint),
        effective_source,
    })
}

/// Persist an operator-supplied secret to the database.
///
/// Refuses (with [`SecretError::AlreadyExists`]) to replace a *different*
/// currently-effective secret unless `force` is set, because overwriting the
/// secret changes the derived encryption key and makes every stored credential
/// undecryptable. Persisting the value that is already effective (e.g. moving
/// an env-only secret into the database — the gap fix) is always allowed.
pub async fn set_secret(
    repo: &dyn IUserRepository,
    env_secret: Option<&str>,
    jwt_env: Option<&str>,
    new_secret: &str,
    force: bool,
) -> Result<SecretWriteOutcome, SecretError> {
    if new_secret.is_empty() {
        return Err(SecretError::Empty);
    }
    persist(repo, env_secret, jwt_env, new_secret, force).await
}

/// Generate a fresh random secret and persist it.
///
/// On a genuinely fresh system (no env and no database secret) this is safe.
/// If any secret is already effective, the random value necessarily differs
/// from it, so this refuses unless `force` is set — otherwise it would orphan
/// every stored credential. To keep an existing env secret, persist it with
/// [`set_secret`] instead of generating a new one.
pub async fn generate_secret(
    repo: &dyn IUserRepository,
    env_secret: Option<&str>,
    jwt_env: Option<&str>,
    force: bool,
) -> Result<SecretWriteOutcome, SecretError> {
    let new_secret = generate_random_secret_string();
    persist(repo, env_secret, jwt_env, &new_secret, force).await
}

/// Shared write path for [`set_secret`] and [`generate_secret`].
///
/// The overwrite guard compares against the *effective* secret (env takes
/// priority over the database), because that is the key existing credentials
/// were encrypted under. The reported outcome, by contrast, reflects the
/// database transition (what physically changed).
async fn persist(
    repo: &dyn IUserRepository,
    env_secret: Option<&str>,
    jwt_env: Option<&str>,
    new_secret: &str,
    force: bool,
) -> Result<SecretWriteOutcome, SecretError> {
    let roots = read_db_roots(repo).await?;
    let db_before = roots.encryption;
    let jwt_seed = effective_jwt_seed(jwt_env, roots.jwt_seed);

    // Guard against the *effective* root the running server would use, in the
    // resolver's own priority: encryption env → persisted `encryption_secret` →
    // seed from the signing secret (`JWT_SECRET` env → `jwt_secret` column).
    // Folding in the jwt seed is the safety-critical part: on a database upgraded
    // before the split but not yet booted, `encryption_secret` is still empty
    // while the real root is the signing secret, so without this a `generate`/`set`
    // would compare against `None`, skip the guard, and write a divergent root —
    // silently orphaning every stored credential. The reported outcome, by
    // contrast, reflects the physical `encryption_secret` transition.
    let effective = normalize(env_secret)
        .map(str::to_owned)
        .or_else(|| db_before.clone())
        .or(jwt_seed);

    // Guard: refuse to diverge from the currently-effective secret unless forced.
    if let Some(current) = &effective
        && current != new_secret
        && !force
    {
        return Err(SecretError::AlreadyExists {
            current_fingerprint: secret_fingerprint(current),
        });
    }

    // No-op fast path: the `encryption_secret` column already holds this value.
    if db_before.as_deref() == Some(new_secret) {
        return Ok(SecretWriteOutcome::Unchanged {
            fingerprint: secret_fingerprint(new_secret),
        });
    }

    repo.update_encryption_secret(SYSTEM_DEFAULT_USER_ID, new_secret)
        .await?;

    let outcome = match db_before {
        Some(old) => SecretWriteOutcome::Overwritten {
            old_fingerprint: secret_fingerprint(&old),
            new_fingerprint: secret_fingerprint(new_secret),
        },
        None => SecretWriteOutcome::Created {
            fingerprint: secret_fingerprint(new_secret),
        },
    };

    // Non-sensitive: logs fingerprints and the transition, never the secret.
    tracing::info!(outcome = ?outcome, forced = force, "encryption secret persisted");
    Ok(outcome)
}

#[cfg(test)]
#[path = "secret_test.rs"]
mod secret_test;
