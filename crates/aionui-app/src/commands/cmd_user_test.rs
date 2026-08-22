use std::process::ExitCode;

use aionui_auth::AccountError;
use aionui_db::DbError;
use aionui_db::models::{User, UserStatus, UserType};
use aionui_db::{IUserRepository, SqliteUserRepository, init_database_memory};

use super::{apply_user_status, format_user_lines, map_account_error, parse_stdin_password};
use crate::commands::error::CliBoundaryCode;

// Security: the `user list` rendering must never surface `password_hash`,
// `jwt_secret`, or `encryption_secret`, even when all are populated. Adding a
// leaking column here should turn this test red.
//
// The account-mutation behaviour (default vs named user, hashing, validation)
// lives in `aionui-auth`'s `set_local_password` and is covered by
// `crates/aionui-auth/src/account_test.rs`; this file only guards the CLI's
// presentation layer.
#[test]
fn format_user_lines_never_leaks_secrets() {
    let user = User {
        id: "u-1".to_owned(),
        user_type: UserType::Local,
        external_user_id: None,
        username: Some("alice".to_owned()),
        email: Some("a@example.com".to_owned()),
        password_hash: Some("$2b$12$SUPERSECRETHASHVALUE".to_owned()),
        avatar_path: None,
        jwt_secret: Some("TOPSECRETJWTKEY".to_owned()),
        encryption_secret: Some("TOPSECRETENCKEY".to_owned()),
        status: UserStatus::Active,
        session_generation: 0,
        created_at: 111,
        updated_at: 222,
        last_login: Some(333),
    };

    let joined = format_user_lines(&[user]).join("\n");

    // Non-sensitive fields render.
    assert!(joined.contains("u-1"));
    assert!(joined.contains("alice"));
    assert!(joined.contains("local"));
    assert!(joined.contains("active"));
    // Sensitive fields must never appear.
    assert!(!joined.contains("SUPERSECRETHASHVALUE"));
    assert!(!joined.contains("$2b$12$"));
    assert!(!joined.contains("TOPSECRETJWTKEY"));
    assert!(!joined.contains("TOPSECRETENCKEY"));
}

// End-to-end over a real in-memory DB: the seed user shows up in the listing,
// and a secret it carries is not leaked by the renderer.
#[tokio::test]
async fn list_lines_include_seed_user_without_secrets() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUserRepository::new(db.pool().clone());

    // A bare init leaves `jwt_secret` NULL, which would make the leak assertion
    // below vacuous. Seed a real secret onto the system user first so the
    // renderer is actually exercised against a populated, sensitive column.
    let seed = repo.get_system_user().await.unwrap().expect("seed system user present");
    let secret = "SEEDED-JWT-SECRET-VALUE";
    repo.update_jwt_secret(&seed.id, secret).await.unwrap();

    let users = repo.list_users().await.unwrap();
    let joined = format_user_lines(&users).join("\n");

    assert!(joined.contains("system_default_user"));
    assert!(
        !joined.contains(secret),
        "the system user's jwt secret must never appear in the listing"
    );
}

// Happy path: disabling an active account flips its status AND revokes live
// sessions (session_generation bumps once); re-enabling restores login without
// a second bump.
#[tokio::test]
async fn disable_then_enable_flips_status_and_revokes_sessions_once() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUserRepository::new(db.pool().clone());
    let created = repo.create_user("carol", "hash").await.unwrap();
    assert_eq!(created.status, UserStatus::Active);
    let gen0 = created.session_generation;

    let label = apply_user_status(&repo, "carol", UserStatus::Disabled).await.unwrap();
    assert!(label.contains("carol"));
    assert!(label.contains(&created.id));
    let disabled = repo.find_by_id(&created.id).await.unwrap().unwrap();
    assert_eq!(disabled.status, UserStatus::Disabled);
    assert_eq!(
        disabled.session_generation,
        gen0 + 1,
        "disabling must revoke live sessions by bumping the generation"
    );

    apply_user_status(&repo, "carol", UserStatus::Active).await.unwrap();
    let enabled = repo.find_by_id(&created.id).await.unwrap().unwrap();
    assert_eq!(enabled.status, UserStatus::Active);
    assert_eq!(
        enabled.session_generation,
        gen0 + 1,
        "enabling must NOT bump the generation again"
    );
}

// Bad path: an unknown username is a usage error (CLI_USER_NOT_FOUND, exit 2),
// not an internal database failure.
#[tokio::test]
async fn disable_unknown_user_maps_to_not_found() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUserRepository::new(db.pool().clone());

    let err = apply_user_status(&repo, "ghost", UserStatus::Disabled)
        .await
        .unwrap_err();
    assert_eq!(err.code(), CliBoundaryCode::CliUserNotFound);
    assert_eq!(err.exit_code(), ExitCode::from(2));
}

// The domain error maps onto the right CLI boundary code and exit class: a bad
// username or weak password is a config-class usage error (exit 2), while a
// hashing or database failure is internal-class (exit 1). Locking the exit code
// too, because callers script on it.
#[test]
fn map_account_error_classifies_by_category() {
    let invalid_username = map_account_error(AccountError::InvalidUsername("bad name".to_owned()));
    assert_eq!(invalid_username.code(), CliBoundaryCode::CliUserInputInvalid);
    assert_eq!(invalid_username.exit_code(), ExitCode::from(2));

    let weak = map_account_error(AccountError::WeakPassword("too short".to_owned()));
    assert_eq!(weak.code(), CliBoundaryCode::CliUserInputInvalid);
    assert_eq!(weak.exit_code(), ExitCode::from(2));

    // An already-taken username is a config-class usage error (exit 2), NOT an
    // internal database failure — the operator can act on it.
    let exists = map_account_error(AccountError::AlreadyExists("alice".to_owned()));
    assert_eq!(exists.code(), CliBoundaryCode::CliUserAlreadyExists);
    assert_eq!(exists.exit_code(), ExitCode::from(2));

    let hash = map_account_error(AccountError::Hash);
    assert_eq!(hash.code(), CliBoundaryCode::CliUserHashFailed);
    assert_eq!(hash.exit_code(), ExitCode::from(1));

    let db = map_account_error(AccountError::Db(DbError::NotFound("x".to_owned())));
    assert_eq!(db.code(), CliBoundaryCode::CliUserDatabaseFailed);
    assert_eq!(db.exit_code(), ExitCode::from(1));
}

// A database error maps to the stable database-failed code and must NOT leak the
// inner DbError text (which can carry paths/SQL detail) onto the stderr line.
// This is the one error arm with a leak dimension, so it is locked explicitly.
#[test]
fn map_account_error_db_does_not_leak_inner_detail() {
    let err = map_account_error(AccountError::Db(DbError::NotFound(
        "system_default_user row detail that must not leak".to_owned(),
    )));

    assert_eq!(err.code(), CliBoundaryCode::CliUserDatabaseFailed);
    let line = err.stderr_line();
    assert!(line.contains("CLI_USER_DATABASE_FAILED"));
    assert!(
        !line.contains("must not leak"),
        "inner DbError detail must never reach the boundary line: {line}"
    );
}

// The stdin parser strips exactly ONE trailing newline and preserves every other
// byte. A trailing space is part of the password and must survive — a regression
// to trim_end() would silently corrupt such a password.
#[test]
fn parse_stdin_password_strips_one_newline_and_preserves_bytes() {
    assert_eq!(parse_stdin_password("plain-pass\n").unwrap(), "plain-pass");
    assert_eq!(parse_stdin_password("crlf-pass\r\n").unwrap(), "crlf-pass");
    assert_eq!(parse_stdin_password("no-newline").unwrap(), "no-newline");
    assert_eq!(
        parse_stdin_password("trailing-space \n").unwrap(),
        "trailing-space ",
        "a trailing space is part of the password and must be preserved"
    );
    assert_eq!(
        parse_stdin_password("a\nb\n").unwrap(),
        "a\nb",
        "only the final newline is stripped; interior newlines stay"
    );
}

// An empty or newline-only stdin line is rejected as invalid input, never
// accepted as an empty password.
#[test]
fn parse_stdin_password_rejects_empty_input() {
    for line in ["", "\n", "\r\n"] {
        let err = parse_stdin_password(line).unwrap_err();
        assert_eq!(
            err.code(),
            CliBoundaryCode::CliUserInputInvalid,
            "empty stdin line {line:?} must be rejected as invalid input"
        );
    }
}
