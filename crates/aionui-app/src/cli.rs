//! CLI argument definitions for the `aioncore` binary.
//!
//! Kept separate from `main.rs` to isolate the clap surface (struct + enum +
//! attribute soup) from the runtime entry point. Visibility is `pub(crate)`
//! because only `main.rs` consumes it.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "aioncore", about = "AionUi Backend Server", version)]
pub(crate) struct Cli {
    /// Host address to listen on.
    #[arg(long, default_value_t = String::from(aionui_common::constants::DEFAULT_HOST))]
    pub host: String,

    /// Port number to listen on.
    #[arg(long, default_value_t = aionui_common::constants::DEFAULT_PORT)]
    pub port: u16,

    /// Data directory for database and file storage.
    #[arg(long, default_value = "data")]
    pub data_dir: PathBuf,

    /// Parent process ID used to terminate the backend when the desktop app dies.
    #[arg(long)]
    pub parent_pid: Option<u32>,

    /// Working directory for conversation workspaces.
    /// Falls back to AIONUI_WORK_DIR env, then to data-dir.
    #[arg(long)]
    pub work_dir: Option<PathBuf>,

    /// Host application version used for extension engine compatibility.
    #[arg(long, default_value_t = env!("CARGO_PKG_VERSION").to_string())]
    pub app_version: String,

    /// Run in local embedded mode (skip authentication, use system_default_user).
    #[arg(long)]
    pub local: bool,

    /// Identity source mode. AionPro mode requires AIONCORE_BOOTSTRAP_SECRET.
    #[arg(long, value_enum, default_value_t = IdentityModeArg::Webui)]
    pub identity_mode: IdentityModeArg,

    /// Directory for log files. Defaults to {data-dir}/logs/.
    #[arg(long)]
    pub log_dir: Option<PathBuf>,

    /// Log level filter (e.g. "info", "debug", "info,aionui_mcp=trace").
    #[arg(long)]
    pub log_level: Option<String>,

    /// Dump prompt diagnostics to {data-dir}/prompt-dumps.
    #[arg(long)]
    pub dump_prompts: bool,

    /// Explicitly back up a corruption-like local database and create a fresh database during startup.
    #[arg(long)]
    pub recover_corrupted_database: bool,

    /// Managed runtime resource source selection.
    #[arg(long, value_enum, default_value_t = ManagedResourcesModeArg::Download)]
    pub managed_resources_mode: ManagedResourcesModeArg,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedResourcesModeArg {
    Bundled,
    Download,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityModeArg {
    Local,
    Webui,
    Aionpro,
}

impl From<IdentityModeArg> for aionui_app::IdentityMode {
    fn from(value: IdentityModeArg) -> Self {
        match value {
            IdentityModeArg::Local => Self::Local,
            IdentityModeArg::Webui => Self::WebUi,
            IdentityModeArg::Aionpro => Self::AionPro,
        }
    }
}

impl From<ManagedResourcesModeArg> for aionui_runtime::ManagedResourcesMode {
    fn from(value: ManagedResourcesModeArg) -> Self {
        match value {
            ManagedResourcesModeArg::Bundled => Self::Bundled,
            ManagedResourcesModeArg::Download => Self::Download,
        }
    }
}

// `Mcp` prefix is load-bearing on Mcp* variants — clap derives the kebab-case
// subcommand name (`mcp-team-stdio`) that external callers (the ACP agent CLI,
// which spawns it from `session/new.mcpServers`) depend on verbatim.
#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// Print the top-level agent-facing CLI capability index.
    Capabilities,
    /// Agent-facing automation CLI for AionUi configuration.
    Config(ConfigArgs),
    /// Agent-facing read-only troubleshooting CLI for AionUi diagnosis.
    Diagnose(DiagnoseArgs),
    /// Agent-facing Team collaboration CLI fallback.
    Team(TeamArgs),
    /// Cross-session messaging: list deliverable conversations and deliver a
    /// message to one of them.
    Session(SessionArgs),
    /// Agent-facing read-only runtime CLI for THIS conversation's skills.
    /// Channel A of skill delivery: a normal tool call instead of the
    /// `[LOAD_SKILL]` text-protocol round trip.
    Skills(SkillsArgs),
    /// PreToolUse permission gate for the Antigravity CLI (spawned by agy).
    /// Reads the tool request on stdin, asks the running AionUi backend, and
    /// writes agy's decision to stdout.
    AntigravityHook,
    /// Stdio ↔ TCP bridge for the team MCP server (spawned by the ACP agent CLI).
    /// MCP stdio server for team tools (spawned by the ACP agent CLI).
    McpTeamStdio,
    /// Self-check: hydrate the agent registry, probe every CLI on `$PATH`,
    /// and print a per-agent availability table. Useful when the user
    /// reports "no agent works" — running this from the same shell the
    /// app launched from confirms whether each backend is detectable
    /// before involving server logs.
    Doctor,
    /// Prepare current-platform managed runtime resources under a bundle output root.
    PrepareManagedResources(PrepareManagedResourcesArgs),
    /// Manage local user accounts. Short-lived: opens the data-dir database
    /// directly and exits without starting the server. Bootstrap path for
    /// self-hosted deployments where the HTTP set-password endpoints are
    /// unreachable without `--local` (which disables auth entirely).
    User(UserArgs),
    /// Inspect and provision the storage-encryption secret — the root key that
    /// encrypts at-rest credentials (provider API keys, remote-agent tokens,
    /// channel config). Short-lived like `user`: opens the data-dir database
    /// directly and exits. Lets a self-hosted node persist its encryption
    /// secret so it survives a restart, instead of relying on an
    /// `AIONUI_ENCRYPTION_SECRET` env var that is used but never stored. This
    /// is independent of the JWT *signing* secret, which change-password may
    /// rotate freely without affecting decryption.
    Secret(SecretArgs),
}

impl Command {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Capabilities => "capabilities",
            Self::Config(_) => "config",
            Self::Diagnose(_) => "diagnose",
            Self::Team(_) => "team",
            Self::Session(_) => "session",
            Self::Skills(_) => "skills",
            Self::AntigravityHook => "antigravity-hook",
            Self::McpTeamStdio => "mcp-team-stdio",
            Self::Doctor => "doctor",
            Self::PrepareManagedResources(_) => "prepare-managed-resources",
            Self::User(_) => "user",
            Self::Secret(_) => "secret",
        }
    }

    pub(crate) fn need_runtime(&self) -> bool {
        matches!(self, Self::Doctor | Self::PrepareManagedResources(_))
    }
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseArgs {
    #[command(subcommand)]
    pub command: DiagnoseCommand,
}

#[derive(Args, Debug, Clone)]
#[command(disable_help_subcommand = true)]
pub(crate) struct TeamArgs {
    #[command(subcommand)]
    pub command: TeamCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum TeamCommand {
    Capabilities,
    Help,
    Context,
    Members,
    ReadMessages,
    SendMessage,
    InterruptAgent,
    Task(TeamTaskArgs),
    ListAssistants,
    DescribeAssistant,
    SpawnAgent,
    RenameAgent,
    ClearAgentContext,
    ShutdownAgent,
    #[command(external_subcommand)]
    Unknown(Vec<OsString>),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct TeamTaskArgs {
    #[command(subcommand)]
    pub command: TeamTaskCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum TeamTaskCommand {
    Create,
    Update,
    List,
    #[command(external_subcommand)]
    Unknown(Vec<OsString>),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct SessionArgs {
    #[command(subcommand)]
    pub command: SessionCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum SessionCommand {
    Capabilities,
    List,
    SendMessage,
    #[command(external_subcommand)]
    Unknown(Vec<OsString>),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct SkillsArgs {
    #[command(subcommand)]
    pub command: SkillsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum SkillsCommand {
    /// Print the agent-readable skills CLI capability contract.
    Capabilities,
    /// List the skills enabled in THIS conversation.
    List,
    // The stdin contract is spelled out in the help text below because omitting it
    // has a measured cost: live agents that tried `skills show <name>` and then
    // reached for `--help` found nothing about stdin, and spent three to five
    // failed tool calls guessing. Help text stays purely instructional -- this
    // rationale is for maintainers, so it does not belong on the page an agent
    // reads.
    /// Print a skill's full body plus its absolute directory.
    ///
    /// Takes no positional arguments. The skill name is read from stdin as a JSON
    /// object:
    ///
    /// {n}  printf '%s' '{"name":"mermaid"}' | aioncore skills show
    #[command(verbatim_doc_comment)]
    Show,
    /// Read one of a skill's supplementary files.
    ///
    /// Takes no positional arguments. The path is read from stdin as a JSON
    /// object, and must be `<skill-name>/<relative-path>`:
    ///
    /// {n}  printf '%s' '{"path":"mermaid/references/syntax.md"}' | aioncore skills cat
    #[command(verbatim_doc_comment)]
    Cat,
    #[command(external_subcommand)]
    Unknown(Vec<OsString>),
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum DiagnoseCommand {
    /// Print the agent-readable diagnose CLI capability contract.
    Capabilities,
    /// Print the current agent runtime context.
    Context,
    /// Read backend health.
    Health,
    /// Read a cross-domain diagnostic snapshot.
    Overview,
    /// Inspect conversation state and messages.
    Conversations(DiagnoseConversationsArgs),
    /// Inspect provider health summary.
    Providers(DiagnoseProvidersArgs),
    /// Inspect MCP server summary.
    Mcp(DiagnoseMcpArgs),
    /// Inspect scheduled task summary.
    Cron(DiagnoseCronArgs),
    /// Inspect team summary.
    Teams(DiagnoseTeamsArgs),
    /// Read aioncore logs.
    Logs(DiagnoseLogsArgs),
    /// Controlled HTTP read escape hatch.
    Http(DiagnoseHttpArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseConversationsArgs {
    #[command(subcommand)]
    pub command: DiagnoseConversationsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum DiagnoseConversationsCommand {
    List,
    Get,
    Messages,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseProvidersArgs {
    #[command(subcommand)]
    pub command: DiagnoseSummaryCommand,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseMcpArgs {
    #[command(subcommand)]
    pub command: DiagnoseSummaryCommand,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseCronArgs {
    #[command(subcommand)]
    pub command: DiagnoseSummaryCommand,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseTeamsArgs {
    #[command(subcommand)]
    pub command: DiagnoseSummaryCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum DiagnoseSummaryCommand {
    Summary,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseLogsArgs {
    #[command(subcommand)]
    pub command: DiagnoseLogsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum DiagnoseLogsCommand {
    Tail,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseHttpArgs {
    #[command(subcommand)]
    pub command: DiagnoseHttpCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum DiagnoseHttpCommand {
    Get,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigCommand {
    /// Print the agent-readable config CLI capability contract.
    Capabilities,
    /// Print the current agent runtime context.
    Context,
    /// Manage conversations.
    Conversation(ConfigConversationArgs),
    /// Manage assistants and assistant-owned behavior.
    Assistants(ConfigAssistantsArgs),
    /// Manage AionUi skills.
    Skills(ConfigSkillsArgs),
    /// Manage MCP servers and OAuth state.
    Mcp(ConfigMcpArgs),
    /// Manage model providers.
    Providers(ConfigProvidersArgs),
    /// Manage backend and client settings.
    Settings(ConfigSettingsArgs),
    /// Manage agent catalog and custom agents.
    Agents(ConfigAgentsArgs),
    /// Manage scheduled tasks.
    Cron(ConfigCronArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigAssistantsArgs {
    #[command(subcommand)]
    pub command: ConfigAssistantsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigAssistantsCommand {
    List,
    Get,
    Create,
    Update,
    Delete,
    Import,
    State,
    Rule(ConfigAssistantRuleArgs),
    Skill(ConfigAssistantSkillArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigAssistantRuleArgs {
    #[command(subcommand)]
    pub command: ConfigAssistantTextCommand,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigAssistantSkillArgs {
    #[command(subcommand)]
    pub command: ConfigAssistantTextCommand,
}

#[derive(Subcommand, Debug, Clone, Copy)]
pub(crate) enum ConfigAssistantTextCommand {
    Read,
    Write,
    Delete,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigSkillsArgs {
    #[command(subcommand)]
    pub command: ConfigSkillsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigSkillsCommand {
    List,
    Info,
    Paths,
    Import,
    Delete,
    Scan,
    ExternalPaths(ConfigSkillsExternalPathsArgs),
    Market(ConfigSkillsMarketArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigSkillsExternalPathsArgs {
    #[command(subcommand)]
    pub command: ConfigSkillsExternalPathsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigSkillsExternalPathsCommand {
    List,
    Add,
    Remove,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigSkillsMarketArgs {
    #[command(subcommand)]
    pub command: ConfigSkillsMarketCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigSkillsMarketCommand {
    Enable,
    Disable,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigMcpArgs {
    #[command(subcommand)]
    pub command: ConfigMcpCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigMcpCommand {
    Servers(ConfigMcpServersArgs),
    TestConnection,
    AgentConfigs,
    Oauth(ConfigMcpOauthArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigMcpServersArgs {
    #[command(subcommand)]
    pub command: ConfigMcpServersCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigMcpServersCommand {
    List,
    Get,
    Create,
    Update,
    Delete,
    Toggle,
    Import,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigMcpOauthArgs {
    #[command(subcommand)]
    pub command: ConfigMcpOauthCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigMcpOauthCommand {
    CheckStatus,
    Login,
    Logout,
    Authenticated,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigProvidersArgs {
    #[command(subcommand)]
    pub command: ConfigProvidersCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigProvidersCommand {
    List,
    Create,
    Update,
    Delete,
    DetectProtocol,
    FetchModels,
    Models(ConfigProviderModelsArgs),
    HealthCheck,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigProviderModelsArgs {
    #[command(subcommand)]
    pub command: ConfigProviderModelsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigProviderModelsCommand {
    Fetch,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigSettingsArgs {
    #[command(subcommand)]
    pub command: ConfigSettingsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigSettingsCommand {
    Get,
    Patch,
    Client(ConfigSettingsClientArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigSettingsClientArgs {
    #[command(subcommand)]
    pub command: ConfigSettingsClientCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigSettingsClientCommand {
    Get,
    Put,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigAgentsArgs {
    #[command(subcommand)]
    pub command: ConfigAgentsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigAgentsCommand {
    List,
    Enable,
    Overrides(ConfigAgentOverridesArgs),
    Custom(ConfigAgentCustomArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigAgentOverridesArgs {
    #[command(subcommand)]
    pub command: ConfigAgentOverridesCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigAgentOverridesCommand {
    Get,
    Set,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigAgentCustomArgs {
    #[command(subcommand)]
    pub command: ConfigAgentCustomCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigAgentCustomCommand {
    Create,
    Update,
    Delete,
    TryConnect,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigCronArgs {
    #[command(subcommand)]
    pub command: ConfigCronCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigCronCommand {
    Jobs(ConfigCronJobsArgs),
    Current(ConfigCronCurrentArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigCronJobsArgs {
    #[command(subcommand)]
    pub command: ConfigCronJobsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigCronJobsCommand {
    List,
    Get,
    Create,
    Update,
    Delete,
    Run,
    Skill(ConfigCronJobSkillArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigCronJobSkillArgs {
    #[command(subcommand)]
    pub command: ConfigCronJobSkillCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigCronJobSkillCommand {
    Get,
    Save,
    Delete,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigCronCurrentArgs {
    #[command(subcommand)]
    pub command: ConfigCronCurrentCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigCronCurrentCommand {
    List,
    Create,
    Update,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigConversationArgs {
    #[command(subcommand)]
    pub command: ConfigConversationCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigConversationCommand {
    Rename,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct PrepareManagedResourcesArgs {
    /// Bundle output root. Aioncore writes the managed resources under
    /// `<bundle-out>/{node,acp}/...` for packaging.
    #[arg(long)]
    pub bundle_out: PathBuf,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct UserArgs {
    #[command(subcommand)]
    pub command: UserCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum UserCommand {
    /// Set (or create) a local user's login password. With no `--username`,
    /// targets the built-in `system_default_user` (login name `admin`) — the
    /// bootstrap path for a fresh deployment. Does not revoke existing
    /// sessions; restart the server after a hot password change.
    SetPassword(SetPasswordArgs),
    /// Create a NEW local user with a login password. `--username` is required
    /// and must not already exist — a conflict (including the built-in `admin`
    /// seed) fails with `CLI_USER_ALREADY_EXISTS` and never overwrites the
    /// existing account. Use `set-password` to change an existing user's password.
    Create(CreateUserArgs),
    /// List all users with non-sensitive fields (id, type, username, status,
    /// timestamps). Never prints password hashes, JWT secrets, or
    /// encryption secrets.
    List,
    /// Disable a local user account: blocks future logins and revokes any live
    /// sessions (bumps the account's session generation). Reversible with
    /// `user enable`. `--username` is required — there is no default target, so
    /// the built-in admin cannot be disabled by accident. Only local password
    /// accounts can be disabled; external identity-provider users are managed
    /// upstream.
    Disable(UserStatusArgs),
    /// Re-enable a previously disabled local user account, restoring login.
    /// `--username` is required. Does not create sessions or reset the password.
    Enable(UserStatusArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct UserStatusArgs {
    /// Target username (required). Must name an existing local password
    /// account; unknown names fail with `CLI_USER_NOT_FOUND`.
    #[arg(long)]
    pub username: String,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct SetPasswordArgs {
    /// Target username. Omit to set the password of the built-in
    /// `system_default_user` (login name `admin`).
    #[arg(long)]
    pub username: Option<String>,

    /// Read the new password from stdin (one line) instead of prompting
    /// interactively. Use for scripted/containerized bootstrap:
    /// `echo "$PW" | aioncore user set-password --password-stdin`.
    #[arg(long)]
    pub password_stdin: bool,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct CreateUserArgs {
    /// Username for the new account (required). Must not already exist as a
    /// local user — the built-in `admin` seed counts — or the command fails
    /// with `CLI_USER_ALREADY_EXISTS`.
    #[arg(long)]
    pub username: String,

    /// Read the new password from stdin (one line) instead of prompting
    /// interactively. Use for scripted/containerized bootstrap:
    /// `echo "$PW" | aioncore user create --username alice --password-stdin`.
    #[arg(long)]
    pub password_stdin: bool,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct SecretArgs {
    #[command(subcommand)]
    pub command: SecretCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum SecretCommand {
    /// Report where the storage-encryption secret comes from (environment vs
    /// database), whether it is persisted, and warn about an env/database
    /// mismatch. Prints only presence and non-reversible fingerprints — never
    /// the secret itself.
    Status,
    /// Generate a random secret and persist it to the database. Refuses to
    /// clobber an already-effective secret unless `--force`, since a new
    /// secret changes the derived encryption key and orphans stored
    /// credentials. Use on a fresh install.
    Generate(GenerateArgs),
    /// Persist an operator-supplied secret to the database. The value is read
    /// from stdin or an interactive no-echo prompt, never from argv. Refuses
    /// to replace a different effective secret unless `--force`. Persisting
    /// the value that is already effective (e.g. an env-only secret) is the
    /// way to make it survive a restart.
    Set(SetSecretArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct GenerateArgs {
    /// Overwrite an existing effective secret. This changes the storage
    /// encryption key and makes every already-stored credential undecryptable
    /// — only use on a fresh install or when intentionally rotating.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct SetSecretArgs {
    /// Read the secret from stdin (one line) instead of prompting
    /// interactively. Use for scripted bootstrap:
    /// `echo "$AIONUI_ENCRYPTION_SECRET" | aioncore secret set --secret-stdin`.
    #[arg(long)]
    pub secret_stdin: bool,

    /// Overwrite a different existing effective secret. This changes the
    /// storage encryption key and makes every already-stored credential
    /// undecryptable — omit unless you intend to rotate.
    #[arg(long)]
    pub force: bool,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;
    use clap::error::ErrorKind;

    use super::{
        Cli, Command, ConfigArgs, ConfigCommand, ManagedResourcesModeArg, PrepareManagedResourcesArgs, SecretArgs,
        SecretCommand, SessionCommand, TeamCommand, UserArgs, UserCommand, UserStatusArgs,
    };

    #[test]
    fn long_version_flag_uses_workspace_package_version() {
        let result = Cli::try_parse_from(["aioncore", "--version"]);
        let err = match result {
            Ok(_) => panic!("expected --version to exit through clap DisplayVersion"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        let rendered = err.to_string();
        assert!(
            rendered.contains("aioncore"),
            "version output should contain binary name, got: {rendered:?}"
        );
        assert!(
            rendered.contains(env!("CARGO_PKG_VERSION")),
            "version output should contain package version {}, got: {rendered:?}",
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn short_version_flag_uses_workspace_package_version() {
        let result = Cli::try_parse_from(["aioncore", "-V"]);
        let err = match result {
            Ok(_) => panic!("expected -V to exit through clap DisplayVersion"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        let rendered = err.to_string();
        assert!(
            rendered.contains("aioncore"),
            "version output should contain binary name, got: {rendered:?}"
        );
        assert!(
            rendered.contains(env!("CARGO_PKG_VERSION")),
            "version output should contain package version {}, got: {rendered:?}",
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn prepare_managed_resources_accepts_bundle_out() {
        let cli = Cli::parse_from([
            "aioncore",
            "prepare-managed-resources",
            "--bundle-out",
            "/tmp/aioncore-bundle",
        ]);

        match cli.command {
            Some(Command::PrepareManagedResources(args)) => {
                assert_eq!(args.bundle_out, std::path::Path::new("/tmp/aioncore-bundle"));
            }
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn managed_resources_mode_defaults_to_download() {
        let cli = Cli::parse_from(["aioncore"]);
        assert_eq!(cli.managed_resources_mode, ManagedResourcesModeArg::Download);
    }

    #[test]
    fn managed_resources_mode_accepts_download() {
        let cli = Cli::parse_from(["aioncore", "--managed-resources-mode", "download"]);
        assert_eq!(cli.managed_resources_mode, ManagedResourcesModeArg::Download);
    }

    #[test]
    fn parent_pid_accepts_positive_integer() {
        let cli = Cli::parse_from(["aioncore", "--parent-pid", "4242"]);
        assert_eq!(cli.parent_pid, Some(4242));
    }

    #[test]
    fn dump_prompts_defaults_to_false() {
        let cli = Cli::parse_from(["aioncore"]);
        assert!(!cli.dump_prompts);
    }

    #[test]
    fn dump_prompts_accepts_flag() {
        let cli = Cli::parse_from(["aioncore", "--dump-prompts"]);
        assert!(cli.dump_prompts);
    }

    #[test]
    fn recover_corrupted_database_flag_defaults_to_false() {
        let cli = Cli::parse_from(["aioncore"]);
        assert!(!cli.recover_corrupted_database);
    }

    #[test]
    fn recover_corrupted_database_flag_is_accepted() {
        let cli = Cli::parse_from(["aioncore", "--recover-corrupted-database"]);
        assert!(cli.recover_corrupted_database);
    }

    #[test]
    fn command_as_str_returns_clap_subcommand_names() {
        let prepare_args = PrepareManagedResourcesArgs {
            bundle_out: PathBuf::from("/tmp/aioncore-bundle"),
        };

        let cases = [
            (
                Command::Config(ConfigArgs {
                    command: ConfigCommand::Context,
                }),
                "config",
            ),
            (Command::McpTeamStdio, "mcp-team-stdio"),
            (Command::AntigravityHook, "antigravity-hook"),
            (Command::Doctor, "doctor"),
            (
                Command::PrepareManagedResources(prepare_args),
                "prepare-managed-resources",
            ),
            (
                Command::User(UserArgs {
                    command: UserCommand::List,
                }),
                "user",
            ),
            (
                Command::Secret(SecretArgs {
                    command: SecretCommand::Status,
                }),
                "secret",
            ),
        ];

        for (command, expected) in cases {
            assert_eq!(command.as_str(), expected);
        }
    }

    #[test]
    fn config_cli_accepts_agent_facing_design_command_paths() {
        let commands: &[&[&str]] = &[
            &["aioncore", "config", "capabilities"],
            &["aioncore", "config", "context"],
            &["aioncore", "config", "conversation", "rename"],
            &["aioncore", "config", "assistants", "list"],
            &["aioncore", "config", "assistants", "get"],
            &["aioncore", "config", "assistants", "create"],
            &["aioncore", "config", "assistants", "update"],
            &["aioncore", "config", "assistants", "delete"],
            &["aioncore", "config", "assistants", "import"],
            &["aioncore", "config", "assistants", "state"],
            &["aioncore", "config", "assistants", "rule", "read"],
            &["aioncore", "config", "assistants", "rule", "write"],
            &["aioncore", "config", "assistants", "rule", "delete"],
            &["aioncore", "config", "assistants", "skill", "read"],
            &["aioncore", "config", "assistants", "skill", "write"],
            &["aioncore", "config", "assistants", "skill", "delete"],
            &["aioncore", "config", "skills", "list"],
            &["aioncore", "config", "skills", "info"],
            &["aioncore", "config", "skills", "paths"],
            &["aioncore", "config", "skills", "import"],
            &["aioncore", "config", "skills", "delete"],
            &["aioncore", "config", "skills", "scan"],
            &["aioncore", "config", "mcp", "servers", "list"],
            &["aioncore", "config", "mcp", "servers", "get"],
            &["aioncore", "config", "mcp", "servers", "create"],
            &["aioncore", "config", "mcp", "servers", "update"],
            &["aioncore", "config", "mcp", "servers", "delete"],
            &["aioncore", "config", "mcp", "servers", "toggle"],
            &["aioncore", "config", "mcp", "servers", "import"],
            &["aioncore", "config", "mcp", "test-connection"],
            &["aioncore", "config", "mcp", "agent-configs"],
            &["aioncore", "config", "mcp", "oauth", "check-status"],
            &["aioncore", "config", "mcp", "oauth", "login"],
            &["aioncore", "config", "mcp", "oauth", "logout"],
            &["aioncore", "config", "mcp", "oauth", "authenticated"],
            &["aioncore", "config", "providers", "list"],
            &["aioncore", "config", "providers", "create"],
            &["aioncore", "config", "providers", "update"],
            &["aioncore", "config", "providers", "delete"],
            &["aioncore", "config", "providers", "detect-protocol"],
            &["aioncore", "config", "providers", "fetch-models"],
            &["aioncore", "config", "providers", "models", "fetch"],
            &["aioncore", "config", "providers", "health-check"],
            &["aioncore", "config", "settings", "get"],
            &["aioncore", "config", "settings", "patch"],
            &["aioncore", "config", "settings", "client", "get"],
            &["aioncore", "config", "settings", "client", "put"],
            &["aioncore", "config", "agents", "list"],
            &["aioncore", "config", "agents", "enable"],
            &["aioncore", "config", "agents", "overrides", "get"],
            &["aioncore", "config", "agents", "overrides", "set"],
            &["aioncore", "config", "agents", "custom", "create"],
            &["aioncore", "config", "agents", "custom", "update"],
            &["aioncore", "config", "agents", "custom", "delete"],
            &["aioncore", "config", "agents", "custom", "try-connect"],
            &["aioncore", "config", "cron", "jobs", "list"],
            &["aioncore", "config", "cron", "jobs", "get"],
            &["aioncore", "config", "cron", "jobs", "create"],
            &["aioncore", "config", "cron", "jobs", "update"],
            &["aioncore", "config", "cron", "jobs", "delete"],
            &["aioncore", "config", "cron", "jobs", "run"],
            &["aioncore", "config", "cron", "jobs", "skill", "get"],
            &["aioncore", "config", "cron", "jobs", "skill", "save"],
            &["aioncore", "config", "cron", "jobs", "skill", "delete"],
            &["aioncore", "config", "skills", "external-paths", "list"],
            &["aioncore", "config", "skills", "external-paths", "add"],
            &["aioncore", "config", "skills", "external-paths", "remove"],
            &["aioncore", "config", "skills", "market", "enable"],
            &["aioncore", "config", "skills", "market", "disable"],
        ];

        for command in commands {
            let result = Cli::try_parse_from(*command);
            assert!(result.is_ok(), "command should parse: {command:?}");
        }
    }

    /// Resolves a `team ...` argv to its parsed subcommand, or `None` when clap
    /// swallowed it into `TeamCommand::Unknown`.
    ///
    /// `Unknown` is an `external_subcommand`, so a missing subcommand still
    /// parses successfully — asserting `is_ok()` alone proves nothing about
    /// whether the command is actually wired.
    fn parse_team_command(argv: &[&str]) -> Option<TeamCommand> {
        let cli = Cli::try_parse_from(argv).ok()?;
        let Some(Command::Team(args)) = cli.command else {
            return None;
        };
        match args.command {
            TeamCommand::Unknown(_) => None,
            command => Some(command),
        }
    }

    fn parse_session_command(argv: &[&str]) -> Option<SessionCommand> {
        let cli = Cli::try_parse_from(argv).ok()?;
        let Some(Command::Session(args)) = cli.command else {
            return None;
        };
        match args.command {
            SessionCommand::Unknown(_) => None,
            command => Some(command),
        }
    }

    /// Every tool in the session registry advertises a `cli_command`, and that
    /// path is printed by `session capabilities` and copied into the
    /// auto-inject skill. A registry entry with no wired subcommand sends
    /// agents at a command that can only fail.
    #[test]
    fn every_registry_tool_has_a_wired_session_cli_subcommand() {
        for tool in aionui_api_types::session_tool_descriptors() {
            let mut argv = vec!["aioncore", "session"];
            argv.extend(tool.cli_command.iter().map(String::as_str));
            assert!(
                parse_session_command(&argv).is_some(),
                "`{}` is advertised by tool {} but is not wired into SessionCommand",
                argv[1..].join(" "),
                tool.name
            );
        }
    }

    #[test]
    fn session_cli_accepts_capabilities() {
        assert!(parse_session_command(&["aioncore", "session", "capabilities"]).is_some());
    }

    #[test]
    fn unwired_session_subcommand_is_reported_as_unknown() {
        assert!(
            parse_session_command(&["aioncore", "session", "definitely-not-a-command"]).is_none(),
            "the guard above only works if an unwired path resolves to Unknown"
        );
    }

    /// Every tool in the shared Team registry advertises a `cli_command`, and
    /// that path is printed by `team capabilities`, `team help`, and the CLI
    /// transport prompt. A tool present in the registry but missing from
    /// `TeamCommand` sends agents at a command that can only fail, so the
    /// registry is the source of truth for this test rather than a hand-kept
    /// list that drifts when a tool is added.
    #[test]
    fn every_registry_tool_has_a_wired_team_cli_subcommand() {
        for tool in aionui_api_types::team_tool_descriptors() {
            let mut argv = vec!["aioncore", "team"];
            argv.extend(tool.cli_command.iter().map(String::as_str));
            assert!(
                parse_team_command(&argv).is_some(),
                "`{}` is advertised by tool {} but is not wired into TeamCommand",
                argv[1..].join(" "),
                tool.name
            );
        }
    }

    #[test]
    fn team_cli_accepts_agent_facing_command_paths() {
        // Registry-independent commands; the tool-backed paths are covered by
        // `every_registry_tool_has_a_wired_team_cli_subcommand`.
        let commands: &[&[&str]] = &[
            &["aioncore", "team", "capabilities"],
            &["aioncore", "team", "help"],
            &["aioncore", "team", "context"],
        ];

        for command in commands {
            assert!(
                parse_team_command(command).is_some(),
                "command should parse to a wired subcommand: {command:?}"
            );
        }
    }

    #[test]
    fn unwired_team_subcommand_is_reported_as_unknown() {
        assert!(
            parse_team_command(&["aioncore", "team", "definitely-not-a-command"]).is_none(),
            "the guard above only works if an unwired path resolves to Unknown"
        );
    }

    #[test]
    fn prepare_managed_resources_requires_bundle_out() {
        let err = match Cli::try_parse_from(["aioncore", "prepare-managed-resources"]) {
            Ok(_) => panic!("prepare-managed-resources should require --bundle-out"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn user_set_password_parses_without_username() {
        let cli = Cli::parse_from(["aioncore", "user", "set-password"]);
        match cli.command {
            Some(Command::User(UserArgs {
                command: UserCommand::SetPassword(args),
            })) => {
                assert_eq!(args.username, None);
                assert!(!args.password_stdin);
            }
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn user_set_password_parses_username_and_stdin() {
        let cli = Cli::parse_from([
            "aioncore",
            "user",
            "set-password",
            "--username",
            "alice",
            "--password-stdin",
        ]);
        match cli.command {
            Some(Command::User(UserArgs {
                command: UserCommand::SetPassword(args),
            })) => {
                assert_eq!(args.username.as_deref(), Some("alice"));
                assert!(args.password_stdin);
            }
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn user_list_parses() {
        let cli = Cli::parse_from(["aioncore", "user", "list"]);
        assert!(matches!(
            cli.command,
            Some(Command::User(UserArgs {
                command: UserCommand::List,
            }))
        ));
    }

    #[test]
    fn user_disable_parses_username() {
        let cli = Cli::parse_from(["aioncore", "user", "disable", "--username", "alice"]);
        match cli.command {
            Some(Command::User(UserArgs {
                command: UserCommand::Disable(UserStatusArgs { username }),
            })) => assert_eq!(username, "alice"),
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn user_enable_parses_username() {
        let cli = Cli::parse_from(["aioncore", "user", "enable", "--username", "bob"]);
        match cli.command {
            Some(Command::User(UserArgs {
                command: UserCommand::Enable(UserStatusArgs { username }),
            })) => assert_eq!(username, "bob"),
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn user_disable_requires_username() {
        let err = match Cli::try_parse_from(["aioncore", "user", "disable"]) {
            Ok(_) => panic!("user disable should require --username"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn user_create_parses_username_and_stdin() {
        let cli = Cli::parse_from(["aioncore", "user", "create", "--username", "alice", "--password-stdin"]);
        match cli.command {
            Some(Command::User(UserArgs {
                command: UserCommand::Create(args),
            })) => {
                assert_eq!(args.username, "alice");
                assert!(args.password_stdin);
            }
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    // The required, non-Option `--username` is exactly what separates strict
    // `create` from the upsert `set-password` (whose username is `Option`).
    // Lock it: a later change to `Option<String>` that silently loosened
    // strict-create semantics would turn this red.
    #[test]
    fn user_create_requires_username() {
        let err = match Cli::try_parse_from(["aioncore", "user", "create"]) {
            Ok(_) => panic!("user create should require --username"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn secret_status_parses() {
        let cli = Cli::parse_from(["aioncore", "secret", "status"]);
        assert!(matches!(
            cli.command,
            Some(Command::Secret(SecretArgs {
                command: SecretCommand::Status,
            }))
        ));
    }

    #[test]
    fn secret_generate_parses_force() {
        let cli = Cli::parse_from(["aioncore", "secret", "generate", "--force"]);
        match cli.command {
            Some(Command::Secret(SecretArgs {
                command: SecretCommand::Generate(args),
            })) => assert!(args.force),
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn secret_generate_defaults_without_force() {
        let cli = Cli::parse_from(["aioncore", "secret", "generate"]);
        match cli.command {
            Some(Command::Secret(SecretArgs {
                command: SecretCommand::Generate(args),
            })) => assert!(!args.force),
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn secret_set_parses_stdin_and_force() {
        let cli = Cli::parse_from(["aioncore", "secret", "set", "--secret-stdin", "--force"]);
        match cli.command {
            Some(Command::Secret(SecretArgs {
                command: SecretCommand::Set(args),
            })) => {
                assert!(args.secret_stdin);
                assert!(args.force);
            }
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    /// Every `aioncore skills …` command line the injected skills index teaches an
    /// agent must actually parse.
    ///
    /// This pins two crates together. The index text lives in `aionui-ai-agent`
    /// and the argument grammar lives here, so nothing structural stopped them
    /// from disagreeing -- and they did: the index taught `skills show <name>`
    /// while `Show` accepts no positional arguments at all. Unit tests on either
    /// side stayed green (the text side only asserted the string mentioned
    /// `$AIONUI_HELPER_BIN`), and the mismatch only surfaced against live agents,
    /// which spent three to five failed tool calls each recovering from it.
    #[test]
    fn skills_cli_commands_in_the_index_are_parseable() {
        let index = aionui_ai_agent::build_skills_index_text(&[aionui_ai_agent::SkillIndex {
            name: "mermaid".to_owned(),
            description: "Render Mermaid diagrams.".to_owned(),
        }]);

        // Pull out each taught invocation: the segment that starts at the helper
        // binary placeholder and runs to the end of its backticked command.
        let mut checked = 0usize;
        for segment in index.split('`') {
            let Some((_, after_bin)) = segment.split_once("\"$AIONUI_HELPER_BIN\"") else {
                continue;
            };
            let argv: Vec<&str> = after_bin.split_whitespace().collect();
            if argv.first() != Some(&"skills") {
                continue;
            }

            let parsed = Cli::try_parse_from(std::iter::once("aioncore").chain(argv.iter().copied()));
            assert!(
                parsed.is_ok(),
                "the skills index teaches `aioncore {}`, which the CLI rejects: {}",
                argv.join(" "),
                parsed.err().map(|e| e.to_string()).unwrap_or_default()
            );
            checked += 1;
        }

        assert!(
            checked >= 3,
            "expected the index to teach `skills show`, `skills cat` and `skills capabilities`; \
             only {checked} parseable invocation(s) were found -- if the wording changed, update \
             this extraction rather than dropping the guard"
        );

        // The failure mode this test exists for, stated directly: a positional
        // argument after `show` or `cat` is not a wording preference, it is a
        // command the binary refuses.
        assert!(
            Cli::try_parse_from(["aioncore", "skills", "show", "mermaid"]).is_err(),
            "`skills show <name>` must stay a parse error, so the index can never teach it again"
        );
    }
}
