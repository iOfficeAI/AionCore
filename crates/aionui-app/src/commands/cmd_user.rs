//! `aioncore user` subcommand: offline local-account management.
//!
//! Short-lived, like `doctor`: opens the data-dir SQLite database directly,
//! performs the operation, and exits — it never starts the HTTP server and
//! never goes through an identity-mode auth gate. This is the bootstrap path
//! for self-hosted WebUi deployments, where the HTTP set-password endpoints
//! are `ensure_local_mode`-gated and therefore unreachable without `--local`
//! (which disables authentication entirely), leaving a remote WebUi node with
//! no way to set its own first password.
//!
//! This file is a thin CLI adapter: it reads input, delegates the actual
//! account mutation to the `aionui-auth` domain services (`set_local_password`
//! for the upsert path, `create_local_user` for strict create), maps that
//! service's `AccountError` onto the CLI boundary error, and renders the
//! outcome. No account business logic (validation, hashing, seed-vs-named-user
//! resolution) lives here.
//!
//! `set-password` does NOT bump `session_generation`, so changing a password
//! while a server is running will not revoke live sessions — restart the
//! server after a hot change. For fresh bootstrap (server not yet started)
//! this is irrelevant.

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use aionui_auth::{AccountError, PasswordOutcome, create_local_user, set_local_password, validate_username};
use aionui_db::{
    Database, IUserRepository, SqliteUserRepository, init_database, maybe_copy_legacy_database,
    models::{User, UserStatus},
};

use crate::cli::{Cli, CreateUserArgs, SetPasswordArgs, UserArgs, UserCommand, UserStatusArgs};
use crate::commands::error::{CliBoundaryCode, CliBoundaryError};

const SUBCOMMAND: &str = "user";

pub async fn run_user(cli: &Cli, args: &UserArgs) -> Result<ExitCode, CliBoundaryError> {
    match &args.command {
        UserCommand::SetPassword(set) => set_password(cli, set).await,
        UserCommand::Create(create) => create_user(cli, create).await,
        UserCommand::List => list_all(cli).await,
        UserCommand::Disable(args) => set_user_status(cli, args, UserStatus::Disabled).await,
        UserCommand::Enable(args) => set_user_status(cli, args, UserStatus::Active).await,
    }
}

async fn set_password(cli: &Cli, args: &SetPasswordArgs) -> Result<ExitCode, CliBoundaryError> {
    // Courtesy fail-fast: reject an obviously malformed username before we ask
    // the operator to type a password. `set_local_password` re-validates and
    // remains the authoritative gate — this only improves the interactive UX.
    if let Some(name) = args.username.as_deref() {
        validate_username(name).map_err(|_| input_error("username does not meet the required format"))?;
    }

    let password = read_new_password(args.password_stdin)?;

    let database = open_database(cli).await?;
    let repo = SqliteUserRepository::new(database.pool().clone());
    let outcome = set_local_password(&repo, args.username.as_deref(), &password).await;
    database.close().await;
    let outcome = outcome.map_err(map_account_error)?;

    println!("Password set for {}.", describe_outcome(&outcome));
    Ok(ExitCode::SUCCESS)
}

/// CLI adapter for `user create`: read the new password, then delegate to the
/// `create_local_user` domain service, which refuses to overwrite an existing
/// account. Like the rest of this subcommand it never starts the HTTP server.
async fn create_user(cli: &Cli, args: &CreateUserArgs) -> Result<ExitCode, CliBoundaryError> {
    // Courtesy fail-fast, mirroring `set_password`: reject an obviously malformed
    // username before we ask the operator to type a password. `create_local_user`
    // re-validates and remains the authoritative gate.
    validate_username(&args.username).map_err(|_| input_error("username does not meet the required format"))?;

    let password = read_new_password(args.password_stdin)?;

    let database = open_database(cli).await?;
    let repo = SqliteUserRepository::new(database.pool().clone());
    let outcome = create_local_user(&repo, &args.username, &password).await;
    database.close().await;
    let outcome = outcome.map_err(map_account_error)?;

    // `create_local_user` only ever yields `Created` on success; match rather
    // than reuse `describe_outcome` (whose "newly created" wording would read as
    // "Created newly created user …"), with a sane fallback regardless.
    match &outcome {
        PasswordOutcome::Created { id, username } => println!("Created user {username} (id={id})."),
        other => println!("Created {}.", describe_outcome(other)),
    }
    Ok(ExitCode::SUCCESS)
}

/// Render a human-readable label for the affected account (never the
/// password/hash) from the domain service's structured outcome.
fn describe_outcome(outcome: &PasswordOutcome) -> String {
    match outcome {
        PasswordOutcome::DefaultUser => "default user (id=system_default_user, username=admin)".to_owned(),
        PasswordOutcome::UpdatedExisting { id, username } => format!("user {username} (id={id})"),
        PasswordOutcome::Created { id, username } => format!("newly created user {username} (id={id})"),
    }
}

/// Map the `aionui-auth` domain error onto the CLI boundary error.
///
/// Input-shaped failures (bad username, weak password, an already-taken
/// username) become a config-class exit; hashing and database failures become
/// internal-class exits. The static messages here intentionally avoid echoing
/// the offending value so nothing sensitive reaches stderr.
fn map_account_error(err: AccountError) -> CliBoundaryError {
    match err {
        AccountError::InvalidUsername(_) => input_error("username does not meet the required format"),
        AccountError::WeakPassword(_) => input_error("password does not meet strength requirements"),
        AccountError::AlreadyExists(_) => already_exists_error(),
        AccountError::Hash => hash_error(),
        AccountError::Db(_) => database_error(),
    }
}

async fn list_all(cli: &Cli) -> Result<ExitCode, CliBoundaryError> {
    let database = open_database(cli).await?;
    let repo = SqliteUserRepository::new(database.pool().clone());
    let users = repo.list_users().await.map_err(|_| database_error());
    database.close().await;

    for line in format_user_lines(&users?) {
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Render one non-sensitive line per user for `user list`.
///
/// Deliberately excludes `password_hash`, `jwt_secret`, and
/// `encryption_secret` — these must never reach stdout. Adding a column here
/// that surfaces any of them is a security bug.
fn format_user_lines(users: &[User]) -> Vec<String> {
    let mut lines = Vec::with_capacity(users.len() + 1);
    lines.push(format!(
        "{:<24}  {:<8}  {:<20}  {:<9}  {:<14}  {}",
        "ID", "TYPE", "USERNAME", "STATUS", "CREATED_MS", "LAST_LOGIN_MS"
    ));
    for user in users {
        let last_login = user
            .last_login
            .map(|ms| ms.to_string())
            .unwrap_or_else(|| "-".to_owned());
        lines.push(format!(
            "{:<24}  {:<8}  {:<20}  {:<9}  {:<14}  {}",
            user.id,
            user.user_type.as_str(),
            user.username.as_deref().unwrap_or("-"),
            user.status.as_str(),
            user.created_at,
            last_login,
        ));
    }
    lines
}

/// CLI adapter for `user disable` / `user enable`: open the data-dir database,
/// flip the named account's status via the repo-driven core, then close and
/// report. Like the rest of this subcommand it never starts the HTTP server.
async fn set_user_status(cli: &Cli, args: &UserStatusArgs, status: UserStatus) -> Result<ExitCode, CliBoundaryError> {
    let database = open_database(cli).await?;
    let repo = SqliteUserRepository::new(database.pool().clone());
    let outcome = apply_user_status(&repo, &args.username, status).await;
    database.close().await;
    let label = outcome?;

    // Past tense mirrors the resulting state, not the requested action.
    let verb = match status {
        UserStatus::Active => "enabled",
        UserStatus::Disabled => "disabled",
    };
    println!("User {label} {verb}.");
    Ok(ExitCode::SUCCESS)
}

/// Resolve the local password account by username and set its status, returning
/// a non-sensitive label for the affected account.
///
/// Repo-driven (not `&Cli`-driven) so it is unit-testable over an in-memory
/// database. `find_by_username` only matches local accounts that have a
/// password, so external identity-provider users are intentionally out of
/// reach here. An unknown name maps to `CLI_USER_NOT_FOUND` (a usage error),
/// distinct from an internal database failure. The session-revocation on
/// disable is handled inside `set_status` (it bumps `session_generation` only
/// on an active→disabled transition), so re-disabling is a harmless no-op.
async fn apply_user_status(
    repo: &dyn IUserRepository,
    username: &str,
    status: UserStatus,
) -> Result<String, CliBoundaryError> {
    let user = repo
        .find_by_username(username)
        .await
        .map_err(|_| database_error())?
        .ok_or_else(user_not_found_error)?;
    repo.set_status(&user.id, status).await.map_err(|_| database_error())?;
    Ok(format!("{} (id={})", user.username.as_deref().unwrap_or("-"), user.id))
}

async fn open_database(cli: &Cli) -> Result<Database, CliBoundaryError> {
    let db_path = cli.data_dir.join("aionui-backend.db");
    maybe_copy_legacy_database(&db_path).map_err(|_| database_error())?;
    init_database(&db_path).await.map_err(|_| database_error())
}

/// Read a new password without echoing it to the terminal.
///
/// - `--password-stdin`: read one line from stdin (pipeline/docker use);
///   a single trailing `\n`/`\r\n` is stripped, everything else is verbatim.
/// - interactive TTY: prompt twice with no echo and require the two to match.
/// - non-TTY without `--password-stdin`: refuse, since there is no safe way
///   to read a password (and a bare arg would leak it into the process list).
fn read_new_password(from_stdin: bool) -> Result<String, CliBoundaryError> {
    if from_stdin {
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|_| input_error("failed to read password from stdin"))?;
        return parse_stdin_password(&line);
    }

    if !io::stdin().is_terminal() {
        return Err(input_error(
            "stdin is not a terminal; pass --password-stdin to read the password from a pipe",
        ));
    }

    let password = rpassword::prompt_password("New password: ").map_err(|_| input_error("failed to read password"))?;
    let confirmation =
        rpassword::prompt_password("Confirm password: ").map_err(|_| input_error("failed to read password"))?;
    if password != confirmation {
        return Err(input_error("passwords do not match"));
    }
    Ok(password)
}

/// Strip a single trailing newline (`\n`, or `\r\n`) from a stdin line and
/// reject an empty result. Kept pure and separate from the IO so the trimming
/// contract is unit-testable: exactly ONE trailing newline is removed and every
/// other byte — including interior and trailing spaces — is preserved verbatim.
/// A regression to `trim_end()` would silently mutate a password ending in
/// whitespace into a different value, so this behavior stays locked by a test.
fn parse_stdin_password(line: &str) -> Result<String, CliBoundaryError> {
    let password = line.strip_suffix('\n').unwrap_or(line);
    let password = password.strip_suffix('\r').unwrap_or(password);
    if password.is_empty() {
        return Err(input_error("received an empty password on stdin"));
    }
    Ok(password.to_owned())
}

fn database_error() -> CliBoundaryError {
    CliBoundaryError::new(
        CliBoundaryCode::CliUserDatabaseFailed,
        SUBCOMMAND,
        "user command failed to access the application database",
    )
}

fn input_error(message: &'static str) -> CliBoundaryError {
    CliBoundaryError::new(CliBoundaryCode::CliUserInputInvalid, SUBCOMMAND, message)
}

fn user_not_found_error() -> CliBoundaryError {
    CliBoundaryError::new(
        CliBoundaryCode::CliUserNotFound,
        SUBCOMMAND,
        "no local user exists with the given username",
    )
}

fn already_exists_error() -> CliBoundaryError {
    CliBoundaryError::new(
        CliBoundaryCode::CliUserAlreadyExists,
        SUBCOMMAND,
        "a local user with the given username already exists",
    )
}

fn hash_error() -> CliBoundaryError {
    CliBoundaryError::new(
        CliBoundaryCode::CliUserHashFailed,
        SUBCOMMAND,
        "user command failed to hash the password",
    )
}

#[cfg(test)]
#[path = "cmd_user_test.rs"]
mod cmd_user_test;
