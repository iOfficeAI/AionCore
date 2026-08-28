//! `aioncore secret` subcommand: offline storage-encryption-secret provisioning.
//!
//! Short-lived, like `doctor` and `user`: opens the data-dir SQLite database
//! directly, performs the operation, and exits. It never starts the HTTP
//! server and never derives the encryption key — it only inspects and writes
//! the raw secret.
//!
//! The encryption secret is the storage-encryption root (the server derives an
//! AES-256-GCM key from it), decoupled from the JWT signing secret. It must
//! stay stable across restarts, but the server's resolver uses an
//! `AIONUI_ENCRYPTION_SECRET` env var *without persisting it*, so a node booted
//! from the environment loses the ability to decrypt stored credentials the
//! moment it restarts without that env var. These commands let an operator
//! persist the secret out-of-band so it survives restarts.
//!
//! This file is a thin CLI adapter: it reads input (env + stdin/TTY),
//! delegates the actual inspection/mutation to the `aionui-auth` domain
//! service (`read_secret_status`, `generate_secret`, `set_secret`), maps that
//! service's `SecretError` onto the CLI boundary error, and renders the
//! outcome. No secret business logic (resolution priority, overwrite guard,
//! fingerprinting) lives here, and the secret itself never reaches stdout.

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use aionui_auth::{
    SecretError, SecretSource, SecretStatus, SecretWriteOutcome, generate_secret, read_secret_status, set_secret,
};
use aionui_db::{Database, SqliteUserRepository, init_database, maybe_copy_legacy_database};

use crate::cli::{Cli, SecretArgs, SecretCommand, SetSecretArgs};
use crate::commands::error::{CliBoundaryCode, CliBoundaryError};

const SUBCOMMAND: &str = "secret";
const ENCRYPTION_SECRET_ENV: &str = "AIONUI_ENCRYPTION_SECRET";
/// The signing-secret env var the server reads (`services.rs`). Passed to the
/// domain so the CLI's "effective root" judgement matches the server's seed
/// fallback for env-signing nodes whose `jwt_secret` column is still empty.
const JWT_SECRET_ENV: &str = "JWT_SECRET";

pub async fn run_secret(cli: &Cli, args: &SecretArgs) -> Result<ExitCode, CliBoundaryError> {
    match &args.command {
        SecretCommand::Status => status(cli).await,
        SecretCommand::Generate(generate_args) => generate(cli, generate_args.force).await,
        SecretCommand::Set(set) => set_from_input(cli, set).await,
    }
}

async fn status(cli: &Cli) -> Result<ExitCode, CliBoundaryError> {
    let env_secret = env_secret();
    let jwt_env = jwt_env_secret();

    let database = open_database(cli).await?;
    let repo = SqliteUserRepository::new(database.pool().clone());
    let status = read_secret_status(&repo, env_secret.as_deref(), jwt_env.as_deref()).await;
    database.close().await;
    let status = status.map_err(map_secret_error)?;

    for line in format_status_lines(&status) {
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

async fn generate(cli: &Cli, force: bool) -> Result<ExitCode, CliBoundaryError> {
    let env_secret = env_secret();
    let jwt_env = jwt_env_secret();

    let database = open_database(cli).await?;
    let repo = SqliteUserRepository::new(database.pool().clone());
    let outcome = generate_secret(&repo, env_secret.as_deref(), jwt_env.as_deref(), force).await;
    database.close().await;
    let outcome = outcome.map_err(map_secret_error)?;

    println!(
        "Generated a new encryption secret: {}.",
        describe_write_outcome(&outcome)
    );
    Ok(ExitCode::SUCCESS)
}

async fn set_from_input(cli: &Cli, args: &SetSecretArgs) -> Result<ExitCode, CliBoundaryError> {
    let env_secret = env_secret();
    let jwt_env = jwt_env_secret();
    let secret = read_new_secret(args.secret_stdin)?;

    let database = open_database(cli).await?;
    let repo = SqliteUserRepository::new(database.pool().clone());
    let outcome = set_secret(&repo, env_secret.as_deref(), jwt_env.as_deref(), &secret, args.force).await;
    database.close().await;
    let outcome = outcome.map_err(map_secret_error)?;

    println!("Persisted the encryption secret: {}.", describe_write_outcome(&outcome));
    Ok(ExitCode::SUCCESS)
}

/// Read the `AIONUI_ENCRYPTION_SECRET` env var verbatim (the domain normalizes
/// an empty value to "absent"). Returns an owned `String` so the borrow
/// outlives the database await points.
fn env_secret() -> Option<String> {
    std::env::var(ENCRYPTION_SECRET_ENV).ok()
}

/// Read the `JWT_SECRET` env var verbatim, so the domain can fold it into the
/// effective-root judgement exactly as the server's resolver does (the encryption
/// root is seeded from the *effective* signing secret, which is this env value
/// when set). Owned `String` for the same borrow-lifetime reason as `env_secret`.
fn jwt_env_secret() -> Option<String> {
    std::env::var(JWT_SECRET_ENV).ok()
}

/// Render the non-sensitive status report. Prints only presence, fingerprints,
/// and the effective source — never the secret — plus an actionable warning
/// when the effective secret is unpersisted or diverges from the database.
fn format_status_lines(status: &SecretStatus) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push("Encryption secret status:".to_owned());
    lines.push(format!(
        "  environment ({ENCRYPTION_SECRET_ENV}):  {}",
        presence(status.env_present, status.env_fingerprint.as_deref())
    ));
    lines.push(format!(
        "  database (persisted):  {}",
        presence(status.db_present, status.db_fingerprint.as_deref())
    ));
    lines.push(format!(
        "  effective source:      {}",
        describe_source(status.effective_source)
    ));

    if status.effective_secret_is_unpersisted() {
        lines.push(String::new());
        lines.push("WARNING: the effective secret comes from the environment and is NOT persisted.".to_owned());
        lines.push(
            "  On a restart without AIONUI_ENCRYPTION_SECRET a different secret is used, making every \
             stored credential undecryptable."
                .to_owned(),
        );
        lines.push(format!(
            "  Persist it now:  echo \"${ENCRYPTION_SECRET_ENV}\" | aioncore secret set --secret-stdin"
        ));
    } else if status.env_db_mismatch() {
        lines.push(String::new());
        lines.push("WARNING: the environment and database secrets differ.".to_owned());
        lines.push(
            "  The server uses the environment secret; a restart without AIONUI_ENCRYPTION_SECRET \
             would switch to the database secret and break decryption."
                .to_owned(),
        );
        lines.push(format!(
            "  Reconcile by unsetting {ENCRYPTION_SECRET_ENV}, or overwrite the stored value:  \
             echo \"${ENCRYPTION_SECRET_ENV}\" | aioncore secret set --secret-stdin --force"
        ));
    }

    lines
}

fn presence(present: bool, fingerprint: Option<&str>) -> String {
    match (present, fingerprint) {
        (true, Some(fp)) => format!("present (fingerprint {fp})"),
        (true, None) => "present".to_owned(),
        (false, _) => "not set".to_owned(),
    }
}

fn describe_source(source: SecretSource) -> &'static str {
    match source {
        SecretSource::Environment => "environment",
        SecretSource::Database => "database",
        SecretSource::SeededFromSigning => {
            "seeded from the signing secret (an effective root already exists; the server persists this exact value on next start)"
        }
        SecretSource::None => "none (a secret will be generated and persisted on next start)",
    }
}

/// Describe a write outcome by fingerprint only — never the secret.
fn describe_write_outcome(outcome: &SecretWriteOutcome) -> String {
    match outcome {
        SecretWriteOutcome::Created { fingerprint } => format!("stored (fingerprint {fingerprint})"),
        SecretWriteOutcome::Overwritten {
            old_fingerprint,
            new_fingerprint,
        } => format!("replaced fingerprint {old_fingerprint} with {new_fingerprint}"),
        SecretWriteOutcome::Unchanged { fingerprint } => {
            format!("already stored (fingerprint {fingerprint}); no change")
        }
    }
}

/// Map the `aionui-auth` domain error onto the CLI boundary error.
///
/// The `AlreadyExists` fingerprint is non-sensitive, so it is attached as a
/// structured field to help the operator confirm the divergence without ever
/// exposing the secret.
fn map_secret_error(err: SecretError) -> CliBoundaryError {
    match err {
        SecretError::Empty => {
            CliBoundaryError::new(CliBoundaryCode::CliSecretInputInvalid, SUBCOMMAND, "the secret must not be empty")
        }
        SecretError::AlreadyExists { current_fingerprint } => CliBoundaryError::new(
            CliBoundaryCode::CliSecretConflict,
            SUBCOMMAND,
            "a different secret is already in effect; pass --force to overwrite (this breaks decryption of stored credentials)",
        )
        .with_field("current_fingerprint", current_fingerprint),
        SecretError::Db(_) => database_error(),
    }
}

async fn open_database(cli: &Cli) -> Result<Database, CliBoundaryError> {
    let db_path = cli.data_dir.join("aionui-backend.db");
    maybe_copy_legacy_database(&db_path).map_err(|_| database_error())?;
    init_database(&db_path).await.map_err(|_| database_error())
}

/// Read the new secret without echoing it to the terminal.
///
/// - `--secret-stdin`: read one line from stdin (pipeline/docker use); a
///   single trailing `\n`/`\r\n` is stripped, everything else is verbatim.
/// - interactive TTY: prompt once with no echo (a long base64 secret is
///   usually pasted, so a confirmation prompt would only add friction).
/// - non-TTY without `--secret-stdin`: refuse, since there is no safe way to
///   read the secret (and a bare arg would leak it into the process list).
fn read_new_secret(from_stdin: bool) -> Result<String, CliBoundaryError> {
    if from_stdin {
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|_| input_error("failed to read secret from stdin"))?;
        return parse_stdin_secret(&line);
    }

    if !io::stdin().is_terminal() {
        return Err(input_error(
            "stdin is not a terminal; pass --secret-stdin to read the secret from a pipe",
        ));
    }

    let secret =
        rpassword::prompt_password("New encryption secret: ").map_err(|_| input_error("failed to read secret"))?;
    if secret.is_empty() {
        return Err(input_error("received an empty secret"));
    }
    Ok(secret)
}

/// Strip a single trailing newline (`\n`, or `\r\n`) from a stdin line and
/// reject an empty result. Kept pure and separate from the IO so the trimming
/// contract is unit-testable: exactly ONE trailing newline is removed and every
/// other byte — including interior and trailing spaces — is preserved verbatim.
/// A regression to `trim_end()` would silently mutate a secret ending in
/// whitespace into a different value, breaking decryption of every stored
/// credential, so this behavior must stay locked by a test.
fn parse_stdin_secret(line: &str) -> Result<String, CliBoundaryError> {
    let secret = line.strip_suffix('\n').unwrap_or(line);
    let secret = secret.strip_suffix('\r').unwrap_or(secret);
    if secret.is_empty() {
        return Err(input_error("received an empty secret on stdin"));
    }
    Ok(secret.to_owned())
}

fn database_error() -> CliBoundaryError {
    CliBoundaryError::new(
        CliBoundaryCode::CliSecretDatabaseFailed,
        SUBCOMMAND,
        "secret command failed to access the application database",
    )
}

fn input_error(message: &'static str) -> CliBoundaryError {
    CliBoundaryError::new(CliBoundaryCode::CliSecretInputInvalid, SUBCOMMAND, message)
}

#[cfg(test)]
#[path = "cmd_secret_test.rs"]
mod cmd_secret_test;
