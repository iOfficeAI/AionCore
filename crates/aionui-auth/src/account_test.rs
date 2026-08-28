use aionui_db::{IUserRepository, SqliteUserRepository, init_database_memory};

use super::{AccountError, PasswordOutcome, create_local_user, set_local_password};
use crate::verify_password;

// Happy path: no username targets the seed `system_default_user`, and the
// stored hash must verify against the plaintext — this is the bootstrap → login
// contract the whole feature exists to satisfy.
#[tokio::test]
async fn default_user_password_is_verifiable() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUserRepository::new(db.pool().clone());

    let outcome = set_local_password(&repo, None, "StrongP@ss1").await.unwrap();
    assert_eq!(outcome, PasswordOutcome::DefaultUser);

    let user = repo.get_system_user().await.unwrap().expect("seed system user present");
    let stored = user.password_hash.expect("password hash stored");
    assert!(verify_password("StrongP@ss1", &stored).unwrap());
}

// A named user is created on first set-password, then updated in place on the
// second — the new hash verifies and the old one no longer does.
#[tokio::test]
async fn named_user_created_then_updated() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUserRepository::new(db.pool().clone());

    let created = set_local_password(&repo, Some("alice"), "StrongP@ss1").await.unwrap();
    let created_id = match created {
        PasswordOutcome::Created { id, username } => {
            assert_eq!(username, "alice");
            id
        }
        other => panic!("expected Created, got {other:?}"),
    };

    let updated = set_local_password(&repo, Some("alice"), "An0ther#Pass").await.unwrap();
    match updated {
        PasswordOutcome::UpdatedExisting { id, username } => {
            assert_eq!(username, "alice");
            // Same row updated in place, not a second account.
            assert_eq!(id, created_id);
        }
        other => panic!("expected UpdatedExisting, got {other:?}"),
    }

    let user = repo.find_by_username("alice").await.unwrap().expect("alice exists");
    let stored = user.password_hash.expect("password hash stored");
    assert!(verify_password("An0ther#Pass", &stored).unwrap());
    assert!(!verify_password("StrongP@ss1", &stored).unwrap());
}

// Bad path: a password below the strength bar is rejected with a specific
// error and the seed user's (empty) password is left untouched.
#[tokio::test]
async fn weak_password_is_rejected_without_mutation() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUserRepository::new(db.pool().clone());

    let err = set_local_password(&repo, None, "short").await.unwrap_err();
    assert!(matches!(err, AccountError::WeakPassword(_)), "got {err:?}");

    // No write happened: the seed user still has no usable password.
    let user = repo.get_system_user().await.unwrap().expect("seed system user present");
    assert!(user.password_hash.as_deref().unwrap_or("").is_empty());
}

// Bad path: an ill-formed username is rejected before any user is created, with
// a specific error (not a generic failure).
#[tokio::test]
async fn invalid_username_is_rejected_without_creating_user() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUserRepository::new(db.pool().clone());

    let err = set_local_password(&repo, Some("ab"), "StrongP@ss1").await.unwrap_err();
    assert!(matches!(err, AccountError::InvalidUsername(_)), "got {err:?}");

    assert!(repo.find_by_username("ab").await.unwrap().is_none());
}

// Happy path: `create_local_user` creates a brand-new named account, reports it
// as `Created`, and the stored hash verifies against the plaintext.
#[tokio::test]
async fn create_local_user_creates_new_named_user() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUserRepository::new(db.pool().clone());

    let outcome = create_local_user(&repo, "alice", "StrongP@ss1").await.unwrap();
    match outcome {
        PasswordOutcome::Created { username, .. } => assert_eq!(username, "alice"),
        other => panic!("expected Created, got {other:?}"),
    }

    let user = repo.find_by_username("alice").await.unwrap().expect("alice exists");
    let stored = user.password_hash.expect("password hash stored");
    assert!(verify_password("StrongP@ss1", &stored).unwrap());
}

// Strict create: a second create for the same username is refused with
// `AlreadyExists`, and the FIRST account is left completely untouched — same
// row, original password still valid, the second password never took effect.
// This is the core "never silently overwrite" guarantee that separates `create`
// from the upsert `set_local_password`.
#[tokio::test]
async fn create_local_user_refuses_existing_user_without_touching_it() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUserRepository::new(db.pool().clone());

    let first = create_local_user(&repo, "alice", "StrongP@ss1").await.unwrap();
    let first_id = match first {
        PasswordOutcome::Created { id, .. } => id,
        other => panic!("expected Created, got {other:?}"),
    };

    let err = create_local_user(&repo, "alice", "An0ther#Pass").await.unwrap_err();
    assert!(
        matches!(&err, AccountError::AlreadyExists(name) if name == "alice"),
        "got {err:?}"
    );

    let user = repo.find_by_username("alice").await.unwrap().expect("alice exists");
    assert_eq!(user.id, first_id, "must be the same row, not a second account");
    let stored = user.password_hash.expect("password hash stored");
    assert!(
        verify_password("StrongP@ss1", &stored).unwrap(),
        "the original password must survive a refused create"
    );
    assert!(
        !verify_password("An0ther#Pass", &stored).unwrap(),
        "the refused create's password must never take effect"
    );
}

// The built-in `admin` seed owns username `admin`, so `create --username admin`
// must be refused as a conflict — never creating a second admin row nor
// overwriting the seed's (empty) password.
#[tokio::test]
async fn create_local_user_refuses_reserved_admin_seed() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUserRepository::new(db.pool().clone());

    let err = create_local_user(&repo, "admin", "StrongP@ss1").await.unwrap_err();
    assert!(
        matches!(&err, AccountError::AlreadyExists(name) if name == "admin"),
        "got {err:?}"
    );

    // The seed row is untouched: still named admin, still without a usable
    // password (create must not have written one onto it).
    let seed = repo.get_system_user().await.unwrap().expect("seed system user present");
    assert_eq!(seed.username.as_deref(), Some("admin"));
    assert!(
        seed.password_hash.as_deref().unwrap_or("").is_empty(),
        "the seed admin's password must stay empty after a refused create"
    );
}

// Bad path: validation runs before any insert, so an ill-formed username or a
// weak password is rejected with a specific error and creates nothing.
#[tokio::test]
async fn create_local_user_rejects_invalid_input_without_writing() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUserRepository::new(db.pool().clone());

    let bad_name = create_local_user(&repo, "ab", "StrongP@ss1").await.unwrap_err();
    assert!(matches!(bad_name, AccountError::InvalidUsername(_)), "got {bad_name:?}");

    let weak = create_local_user(&repo, "validname", "short").await.unwrap_err();
    assert!(matches!(weak, AccountError::WeakPassword(_)), "got {weak:?}");

    assert!(
        repo.find_by_username("validname").await.unwrap().is_none(),
        "no user may be created when validation fails"
    );
}
