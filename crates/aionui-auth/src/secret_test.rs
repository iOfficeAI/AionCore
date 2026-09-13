use aionui_db::{IUserRepository, SqliteUserRepository, init_database_memory};

use super::{
    SecretError, SecretSource, SecretWriteOutcome, generate_secret, read_secret_status, secret_fingerprint, set_secret,
};

async fn repo() -> SqliteUserRepository {
    let db = init_database_memory().await.unwrap();
    SqliteUserRepository::new(db.pool().clone())
}

// Read the raw persisted secret straight from the row. Test-only: production
// code never reads the secret back out of this module.
async fn stored_secret(repo: &SqliteUserRepository) -> Option<String> {
    let user = repo.get_system_user().await.unwrap().expect("seed system user present");
    user.encryption_secret.filter(|s| !s.is_empty())
}

// Model a database upgraded before the encryption/signing split but not yet
// booted since: the `encryption_secret` column is still empty, but the system
// user carries a `jwt_secret` — the real root the server would seed from.
async fn seed_jwt_secret(repo: &SqliteUserRepository, secret: &str) {
    let user = repo.get_system_user().await.unwrap().expect("seed system user present");
    repo.update_jwt_secret(&user.id, secret).await.unwrap();
}

// The fingerprint is deterministic, 12 hex chars, distinguishes different
// secrets, and never contains the secret itself. This is the property the
// whole no-leak guarantee rests on.
#[test]
fn fingerprint_is_stable_and_hides_secret() {
    let secret = "super-secret-value";
    let fp = secret_fingerprint(secret);

    assert_eq!(fp, secret_fingerprint(secret), "must be deterministic");
    assert_eq!(fp.len(), 12, "expected 12 hex chars, got {fp:?}");
    assert!(fp.chars().all(|c| c.is_ascii_hexdigit()), "non-hex output {fp:?}");
    assert_ne!(fp, secret_fingerprint("different-secret"), "must distinguish secrets");
    assert!(!fp.contains(secret), "fingerprint must not embed the secret");
}

// Fresh install, no env: nothing is present and the resolver would fall through
// to generation.
#[tokio::test]
async fn status_is_none_when_unset() {
    let repo = repo().await;

    let status = read_secret_status(&repo, None, None).await.unwrap();

    assert!(!status.env_present);
    assert!(!status.db_present);
    assert_eq!(status.effective_source, SecretSource::None);
    assert!(!status.env_db_mismatch());
    assert!(!status.effective_secret_is_unpersisted());
}

// An empty env var is treated as absent — it cannot serve as a usable secret.
#[tokio::test]
async fn status_treats_empty_env_as_absent() {
    let repo = repo().await;

    let status = read_secret_status(&repo, Some(""), None).await.unwrap();

    assert!(!status.env_present);
    assert_eq!(status.effective_source, SecretSource::None);
}

// The core "secret gap": env supplies the effective secret, but nothing is
// persisted, so a restart without the env var loses it.
#[tokio::test]
async fn status_flags_unpersisted_env_secret() {
    let repo = repo().await;

    let status = read_secret_status(&repo, Some("env-secret"), None).await.unwrap();

    assert!(status.env_present);
    assert!(!status.db_present);
    assert_eq!(status.effective_source, SecretSource::Environment);
    assert!(status.effective_secret_is_unpersisted());
    assert!(!status.env_db_mismatch(), "no db value means no mismatch");
    assert_eq!(
        status.env_fingerprint.as_deref(),
        Some(secret_fingerprint("env-secret").as_str())
    );
}

// When env and database disagree, the mismatch is surfaced and env wins as the
// effective source.
#[tokio::test]
async fn status_detects_env_db_mismatch() {
    let repo = repo().await;
    set_secret(&repo, None, None, "db-secret", false).await.unwrap();

    let status = read_secret_status(&repo, Some("env-secret"), None).await.unwrap();

    assert!(status.env_present);
    assert!(status.db_present);
    assert_eq!(status.effective_source, SecretSource::Environment);
    assert!(status.env_db_mismatch());
    assert!(!status.effective_secret_is_unpersisted(), "db value is present");
}

// Generate on a genuinely fresh system persists a new secret.
#[tokio::test]
async fn generate_creates_on_fresh_system() {
    let repo = repo().await;

    let outcome = generate_secret(&repo, None, None, false).await.unwrap();
    assert!(matches!(outcome, SecretWriteOutcome::Created { .. }), "got {outcome:?}");

    let stored = stored_secret(&repo).await.expect("secret persisted");
    let status = read_secret_status(&repo, None, None).await.unwrap();
    assert!(status.db_present);
    assert_eq!(status.effective_source, SecretSource::Database);
    assert_eq!(
        status.db_fingerprint.as_deref(),
        Some(secret_fingerprint(&stored).as_str())
    );
}

// Generate refuses to clobber an existing database secret unless forced —
// overwriting would orphan every credential encrypted under the old key.
#[tokio::test]
async fn generate_refuses_existing_db_secret_without_force() {
    let repo = repo().await;
    set_secret(&repo, None, None, "existing", false).await.unwrap();

    let err = generate_secret(&repo, None, None, false).await.unwrap_err();
    match err {
        SecretError::AlreadyExists { current_fingerprint } => {
            assert_eq!(current_fingerprint, secret_fingerprint("existing"));
        }
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    // The existing secret is untouched.
    assert_eq!(stored_secret(&repo).await.as_deref(), Some("existing"));

    // With force, it is replaced.
    let outcome = generate_secret(&repo, None, None, true).await.unwrap();
    assert!(
        matches!(outcome, SecretWriteOutcome::Overwritten { .. }),
        "got {outcome:?}"
    );
    assert_ne!(stored_secret(&repo).await.as_deref(), Some("existing"));
}

// Generate refuses when an env secret is effective, even though the database is
// empty: a random value would diverge from the working env secret.
#[tokio::test]
async fn generate_refuses_when_env_effective_without_force() {
    let repo = repo().await;

    let err = generate_secret(&repo, Some("env-secret"), None, false)
        .await
        .unwrap_err();
    match err {
        SecretError::AlreadyExists { current_fingerprint } => {
            assert_eq!(current_fingerprint, secret_fingerprint("env-secret"));
        }
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    assert!(stored_secret(&repo).await.is_none(), "nothing should have been written");
}

// An empty secret is rejected before any write.
#[tokio::test]
async fn set_rejects_empty_secret() {
    let repo = repo().await;

    let err = set_secret(&repo, None, None, "", false).await.unwrap_err();
    assert!(matches!(err, SecretError::Empty), "got {err:?}");
    assert!(stored_secret(&repo).await.is_none());
}

// Set persists the value and is idempotent when re-run with the same value.
#[tokio::test]
async fn set_persists_and_is_idempotent() {
    let repo = repo().await;

    let created = set_secret(&repo, None, None, "chosen-secret", false).await.unwrap();
    assert_eq!(
        created,
        SecretWriteOutcome::Created {
            fingerprint: secret_fingerprint("chosen-secret")
        }
    );
    assert_eq!(stored_secret(&repo).await.as_deref(), Some("chosen-secret"));

    let again = set_secret(&repo, None, None, "chosen-secret", false).await.unwrap();
    assert_eq!(
        again,
        SecretWriteOutcome::Unchanged {
            fingerprint: secret_fingerprint("chosen-secret")
        }
    );
}

// Set refuses to replace a different effective secret unless forced.
#[tokio::test]
async fn set_refuses_different_secret_without_force() {
    let repo = repo().await;
    set_secret(&repo, None, None, "original", false).await.unwrap();

    let err = set_secret(&repo, None, None, "replacement", false).await.unwrap_err();
    match err {
        SecretError::AlreadyExists { current_fingerprint } => {
            assert_eq!(current_fingerprint, secret_fingerprint("original"));
        }
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    assert_eq!(
        stored_secret(&repo).await.as_deref(),
        Some("original"),
        "must not overwrite"
    );

    let forced = set_secret(&repo, None, None, "replacement", true).await.unwrap();
    assert_eq!(
        forced,
        SecretWriteOutcome::Overwritten {
            old_fingerprint: secret_fingerprint("original"),
            new_fingerprint: secret_fingerprint("replacement"),
        }
    );
    assert_eq!(stored_secret(&repo).await.as_deref(), Some("replacement"));
}

// Reconcile a divergence: the env supplies the effective secret while the
// database still holds a different, stale value. Persisting the *effective* env
// value is allowed without force — the effective secret (what credentials are
// encrypted under) does not change, only the stale database copy is replaced —
// and the outcome reports the database transition as Overwritten. This is the
// env-present + db-present-different case the other write tests (all env=None or
// empty-db) never exercise; the guard must compare against the effective secret,
// not against db_before, for it to pass.
#[tokio::test]
async fn set_reconciles_env_over_divergent_db_without_force() {
    let repo = repo().await;
    set_secret(&repo, None, None, "db-secret", false).await.unwrap();

    let outcome = set_secret(&repo, Some("env-secret"), None, "env-secret", false)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        SecretWriteOutcome::Overwritten {
            old_fingerprint: secret_fingerprint("db-secret"),
            new_fingerprint: secret_fingerprint("env-secret"),
        }
    );
    assert_eq!(stored_secret(&repo).await.as_deref(), Some("env-secret"));
}

// The gap-fix flow: persist the currently-effective env secret into the
// database. Allowed without force because the value does not change, and it
// makes the secret survive a restart.
#[tokio::test]
async fn set_persisting_effective_env_secret_closes_gap() {
    let repo = repo().await;

    // Before: env supplies the secret, database is empty.
    let before = read_secret_status(&repo, Some("env-secret"), None).await.unwrap();
    assert!(before.effective_secret_is_unpersisted());

    let outcome = set_secret(&repo, Some("env-secret"), None, "env-secret", false)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        SecretWriteOutcome::Created {
            fingerprint: secret_fingerprint("env-secret")
        }
    );

    // After: the same secret now lives in the database; no gap, no mismatch.
    let after = read_secret_status(&repo, Some("env-secret"), None).await.unwrap();
    assert!(after.db_present);
    assert!(!after.effective_secret_is_unpersisted());
    assert!(!after.env_db_mismatch());
    assert_eq!(stored_secret(&repo).await.as_deref(), Some("env-secret"));
}

// Upgraded-not-booted database: no env, `encryption_secret` empty, but a
// `jwt_secret` is present. The status must report that an effective root already
// exists (SeededFromSigning) rather than "None" — otherwise it would invite a
// destructive `generate`. It is not "db_present" (the column is empty) and there
// is nothing unpersisted or mismatched to warn about.
#[tokio::test]
async fn status_reports_seeded_from_signing_on_upgraded_db() {
    let repo = repo().await;
    seed_jwt_secret(&repo, "signing-secret-value").await;

    let status = read_secret_status(&repo, None, None).await.unwrap();

    assert!(!status.env_present);
    assert!(!status.db_present, "the encryption_secret column is still empty");
    assert_eq!(status.effective_source, SecretSource::SeededFromSigning);
    assert!(!status.effective_secret_is_unpersisted());
    assert!(!status.env_db_mismatch());
}

// The core footgun fix: on an upgraded-not-booted database the real root is the
// signing secret, so `generate` (which produces a different value) must refuse
// without force — even though the `encryption_secret` column is empty. The
// current fingerprint is the signing secret's, and nothing is written.
#[tokio::test]
async fn generate_refuses_when_only_jwt_seed_present_without_force() {
    let repo = repo().await;
    seed_jwt_secret(&repo, "signing-secret-value").await;

    let err = generate_secret(&repo, None, None, false).await.unwrap_err();
    match err {
        SecretError::AlreadyExists { current_fingerprint } => {
            assert_eq!(current_fingerprint, secret_fingerprint("signing-secret-value"));
        }
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    assert!(
        stored_secret(&repo).await.is_none(),
        "the encryption_secret column must stay empty on a refused generate"
    );

    // With force the operator accepts orphaning the seed root; because the
    // column was empty the physical transition is a Created.
    let outcome = generate_secret(&repo, None, None, true).await.unwrap();
    assert!(matches!(outcome, SecretWriteOutcome::Created { .. }), "got {outcome:?}");
    assert!(stored_secret(&repo).await.is_some());
    assert_ne!(
        stored_secret(&repo).await.as_deref(),
        Some("signing-secret-value"),
        "forced generate writes a fresh random root, not the seed"
    );
}

// Same guard on the `set` path: a value that diverges from the signing-seed root
// is refused without force, reporting the seed's fingerprint.
#[tokio::test]
async fn set_refuses_divergent_secret_over_jwt_seed_without_force() {
    let repo = repo().await;
    seed_jwt_secret(&repo, "signing-secret-value").await;

    let err = set_secret(&repo, None, None, "operator-chosen-different", false)
        .await
        .unwrap_err();
    match err {
        SecretError::AlreadyExists { current_fingerprint } => {
            assert_eq!(current_fingerprint, secret_fingerprint("signing-secret-value"));
        }
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    assert!(stored_secret(&repo).await.is_none(), "nothing should have been written");
}

// Pinning the seed root explicitly is safe and allowed without force: persisting
// the signing-secret value into `encryption_secret` does not change the derived
// key, it just makes the root survive a later signing-secret rotation. The
// physical column had no value, so the transition is a Created.
#[tokio::test]
async fn set_persisting_jwt_seed_value_is_allowed_without_force() {
    let repo = repo().await;
    seed_jwt_secret(&repo, "signing-secret-value").await;

    let outcome = set_secret(&repo, None, None, "signing-secret-value", false)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        SecretWriteOutcome::Created {
            fingerprint: secret_fingerprint("signing-secret-value")
        }
    );
    assert_eq!(stored_secret(&repo).await.as_deref(), Some("signing-secret-value"));

    // Now that it is pinned, the status reports it as a persisted Database root.
    let status = read_secret_status(&repo, None, None).await.unwrap();
    assert!(status.db_present);
    assert_eq!(status.effective_source, SecretSource::Database);
}

// The env-signing deployment: `JWT_SECRET` is supplied via the environment and
// the `jwt_secret` column was never persisted (env-run nodes don't persist it).
// The server seeds the encryption root from the *effective* signing secret — the
// env value — so the CLI must report SeededFromSigning even with both database
// columns empty. Reading only the column (as the first cut did) would say "None"
// and invite a destructive `generate`.
#[tokio::test]
async fn status_reports_seeded_from_signing_from_jwt_env() {
    let repo = repo().await;

    let status = read_secret_status(&repo, None, Some("signing-env-secret"))
        .await
        .unwrap();

    assert!(!status.env_present, "AIONUI_ENCRYPTION_SECRET is not set");
    assert!(!status.db_present, "the encryption_secret column is empty");
    assert_eq!(status.effective_source, SecretSource::SeededFromSigning);
    assert!(!status.effective_secret_is_unpersisted());
    assert!(!status.env_db_mismatch());
}

// The env-signing footgun: `JWT_SECRET` in the env, both database columns empty.
// `generate` must refuse without force and fingerprint the env signing secret —
// otherwise it writes a divergent encryption root that orphans every credential
// the running server encrypted under the env-seeded key.
#[tokio::test]
async fn generate_refuses_when_only_jwt_env_present_without_force() {
    let repo = repo().await;

    let err = generate_secret(&repo, None, Some("signing-env-secret"), false)
        .await
        .unwrap_err();
    match err {
        SecretError::AlreadyExists { current_fingerprint } => {
            assert_eq!(current_fingerprint, secret_fingerprint("signing-env-secret"));
        }
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    assert!(stored_secret(&repo).await.is_none(), "nothing should have been written");
}

// Priority within the signing seed mirrors `resolve_jwt_secret`: when both the
// `JWT_SECRET` env var and the `jwt_secret` column are present but differ, the
// server signs with (and thus seeds the encryption root from) the env value. The
// guard must therefore refuse against the env seed's fingerprint, not the
// column's — pinning the column value would still diverge from the live key.
#[tokio::test]
async fn jwt_env_takes_precedence_over_jwt_column_for_seed() {
    let repo = repo().await;
    seed_jwt_secret(&repo, "column-signing-secret").await;

    let err = generate_secret(&repo, None, Some("env-signing-secret"), false)
        .await
        .unwrap_err();
    match err {
        SecretError::AlreadyExists { current_fingerprint } => {
            assert_eq!(
                current_fingerprint,
                secret_fingerprint("env-signing-secret"),
                "the env signing secret wins over the column, matching resolve_jwt_secret"
            );
        }
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    assert!(stored_secret(&repo).await.is_none(), "nothing should have been written");
}

// Branch-ordering invariant for `read_secret_status`'s if-chain: when the
// encryption env is set it short-circuits to `Environment` before the seed tier
// is consulted, so introducing the `SeededFromSigning` branch cannot steal
// priority from an explicit encryption secret. NOTE: this is an *ordering* guard
// only. Because the encryption-env branch never reads the jwt seed, this test is
// deliberately insensitive to the `effective_jwt_seed` fold — it would stay green
// if the fold were reverted. The fold's own regression is covered by the three
// `env = None` tests above (status_reports_seeded_from_signing_from_jwt_env,
// generate_refuses_when_only_jwt_env_present_without_force,
// jwt_env_takes_precedence_over_jwt_column_for_seed), where the seed tier is the
// one that decides the outcome.
#[tokio::test]
async fn encryption_env_outranks_jwt_seed() {
    let repo = repo().await;

    let status = read_secret_status(&repo, Some("enc-secret"), Some("signing-env-secret"))
        .await
        .unwrap();

    assert_eq!(status.effective_source, SecretSource::Environment);
    assert!(
        status.effective_secret_is_unpersisted(),
        "the encryption env is not persisted"
    );
    assert_eq!(
        status.env_fingerprint.as_deref(),
        Some(secret_fingerprint("enc-secret").as_str())
    );
}
