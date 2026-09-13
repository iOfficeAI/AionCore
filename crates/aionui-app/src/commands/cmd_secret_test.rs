use aionui_auth::{SecretError, SecretSource, SecretStatus, SecretWriteOutcome, secret_fingerprint};
use aionui_db::DbError;

use super::{CliBoundaryCode, describe_write_outcome, format_status_lines, map_secret_error, parse_stdin_secret};

// The secret business logic (resolution, overwrite guard, fingerprinting) lives
// in `aionui-auth`'s `secret` module and is covered by
// `crates/aionui-auth/src/secret_test.rs`. This file only guards the CLI's
// presentation layer: it must render fingerprints and warnings, never the
// secret itself.

// Security: the status report must surface fingerprints and the "unpersisted"
// warning, but never the secret value.
#[test]
fn unpersisted_env_status_warns_without_leaking_secret() {
    let secret = "env-only-super-secret-value";
    let status = SecretStatus {
        env_present: true,
        env_fingerprint: Some(secret_fingerprint(secret)),
        db_present: false,
        db_fingerprint: None,
        effective_source: SecretSource::Environment,
    };

    let joined = format_status_lines(&status).join("\n");

    assert!(
        joined.contains(&secret_fingerprint(secret)),
        "fingerprint should render"
    );
    assert!(!joined.contains(secret), "the secret must never appear");
    assert!(joined.contains("NOT persisted"), "must warn about the persistence gap");
    assert!(joined.contains("secret set --secret-stdin"), "must suggest the fix");
}

// A divergence between env and database is flagged, again without leaking
// either secret.
#[test]
fn mismatch_status_warns_without_leaking_secret() {
    let env = "environment-secret-value";
    let db = "database-secret-value";
    let status = SecretStatus {
        env_present: true,
        env_fingerprint: Some(secret_fingerprint(env)),
        db_present: true,
        db_fingerprint: Some(secret_fingerprint(db)),
        effective_source: SecretSource::Environment,
    };

    let joined = format_status_lines(&status).join("\n");

    assert!(joined.contains("differ"), "must warn about the mismatch");
    assert!(
        joined.contains("secret set --secret-stdin --force"),
        "must suggest the reconcile command so the operator has an actionable fix"
    );
    assert!(joined.contains(&secret_fingerprint(env)));
    assert!(joined.contains(&secret_fingerprint(db)));
    assert!(!joined.contains(env), "the env secret must never appear");
    assert!(!joined.contains(db), "the db secret must never appear");
}

// When neither source has a secret, the report says so and explains the
// generate-on-start fallback, with no warnings.
#[test]
fn none_status_reports_generation_fallback() {
    let status = SecretStatus {
        env_present: false,
        env_fingerprint: None,
        db_present: false,
        db_fingerprint: None,
        effective_source: SecretSource::None,
    };

    let joined = format_status_lines(&status).join("\n");

    assert!(joined.contains("not set"));
    assert!(
        joined.contains("generated"),
        "must explain the generate-on-start fallback"
    );
    assert!(!joined.contains("WARNING"), "nothing is wrong yet");
}

// An upgraded-not-booted database (no env, empty encryption_secret column, but a
// signing secret to seed from): the effective source is reported as the signing
// seed so the report does NOT invite a destructive `generate`, and there is
// nothing to warn about — the seed root is stable across restarts.
#[test]
fn seeded_from_signing_status_describes_root_without_warning() {
    let status = SecretStatus {
        env_present: false,
        env_fingerprint: None,
        db_present: false,
        db_fingerprint: None,
        effective_source: SecretSource::SeededFromSigning,
    };

    let joined = format_status_lines(&status).join("\n");

    assert!(
        joined.contains("seeded from the signing secret"),
        "must report the effective seed root, not 'none': {joined}"
    );
    assert!(
        !joined.contains("WARNING"),
        "the seed root is stable across restarts; nothing to warn about: {joined}"
    );
}

// Write outcomes are described by fingerprint only.
#[test]
fn write_outcomes_render_fingerprints_not_secrets() {
    let old = "old-secret-value";
    let new = "new-secret-value";

    let created = describe_write_outcome(&SecretWriteOutcome::Created {
        fingerprint: secret_fingerprint(new),
    });
    assert!(created.contains(&secret_fingerprint(new)));
    assert!(!created.contains(new));

    let overwritten = describe_write_outcome(&SecretWriteOutcome::Overwritten {
        old_fingerprint: secret_fingerprint(old),
        new_fingerprint: secret_fingerprint(new),
    });
    assert!(overwritten.contains(&secret_fingerprint(old)));
    assert!(overwritten.contains(&secret_fingerprint(new)));
    assert!(!overwritten.contains(old));
    assert!(!overwritten.contains(new));

    let unchanged = describe_write_outcome(&SecretWriteOutcome::Unchanged {
        fingerprint: secret_fingerprint(new),
    });
    assert!(unchanged.contains(&secret_fingerprint(new)));
    assert!(!unchanged.contains(new));
}

// The conflict error carries the current secret's fingerprint (non-sensitive)
// on its stderr line, never the secret, and maps to the conflict exit code.
#[test]
fn conflict_error_exposes_fingerprint_not_secret() {
    let current = "currently-effective-secret";
    let err = map_secret_error(SecretError::AlreadyExists {
        current_fingerprint: secret_fingerprint(current),
    });

    assert_eq!(err.code(), CliBoundaryCode::CliSecretConflict);
    let line = err.stderr_line();
    assert!(line.contains("CLI_SECRET_CONFLICT"));
    assert!(
        line.contains(&secret_fingerprint(current)),
        "fingerprint should be reported"
    );
    assert!(!line.contains(current), "the secret must never appear");
    assert!(line.contains("--force"), "must tell the operator how to proceed");
}

// An empty-secret error maps to the input-invalid code, not the conflict code.
#[test]
fn empty_error_maps_to_input_invalid() {
    let err = map_secret_error(SecretError::Empty);
    assert_eq!(err.code(), CliBoundaryCode::CliSecretInputInvalid);
    assert!(err.stderr_line().contains("CLI_SECRET_INPUT_INVALID"));
}

// A database error maps to the stable database-failed code and must NOT leak the
// inner DbError text (which can carry paths/SQL detail) onto the stderr line.
// This is the one error arm with a leak dimension, so it is locked explicitly.
#[test]
fn db_error_maps_to_stable_code_without_leaking_inner_detail() {
    let err = map_secret_error(SecretError::Db(DbError::NotFound(
        "system_default_user row detail that must not leak".to_owned(),
    )));

    assert_eq!(err.code(), CliBoundaryCode::CliSecretDatabaseFailed);
    let line = err.stderr_line();
    assert!(line.contains("CLI_SECRET_DATABASE_FAILED"));
    assert!(
        !line.contains("must not leak"),
        "inner DbError detail must never reach the boundary line: {line}"
    );
}

// The stdin parser strips exactly ONE trailing newline and preserves every other
// byte. A trailing space is part of the secret and must survive — a regression to
// trim_end() would silently corrupt such a secret and break decryption.
#[test]
fn parse_stdin_secret_strips_one_newline_and_preserves_bytes() {
    assert_eq!(parse_stdin_secret("plain-secret\n").unwrap(), "plain-secret");
    assert_eq!(parse_stdin_secret("crlf-secret\r\n").unwrap(), "crlf-secret");
    assert_eq!(parse_stdin_secret("no-newline").unwrap(), "no-newline");
    assert_eq!(
        parse_stdin_secret("trailing-space \n").unwrap(),
        "trailing-space ",
        "a trailing space is part of the secret and must be preserved"
    );
    assert_eq!(
        parse_stdin_secret("a\nb\n").unwrap(),
        "a\nb",
        "only the final newline is stripped; interior newlines stay"
    );
}

// An empty or newline-only stdin line is rejected as invalid input, never
// persisted as an empty secret.
#[test]
fn parse_stdin_secret_rejects_empty_input() {
    for line in ["", "\n", "\r\n"] {
        let err = parse_stdin_secret(line).unwrap_err();
        assert_eq!(
            err.code(),
            CliBoundaryCode::CliSecretInputInvalid,
            "empty stdin line {line:?} must be rejected as invalid input"
        );
    }
}
