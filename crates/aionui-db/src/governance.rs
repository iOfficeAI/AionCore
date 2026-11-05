//! M8 E12-3 governance: roles, agent run permissions and the global policy
//! switches, plus [`GovernanceService`], the one place launch paths ask
//! "may this run?".
//!
//! - Roles only apply in multi-user identity modes (D12-6). In the desktop
//!   single-user mode every caller is [`UserRole::Owner`] and the
//!   `user_roles` / `agent_run_permissions` tables are not consulted.
//! - Policies (D12-7) are global and apply in every mode. All default off.
//! - A denied launch and every governance change write an audit row.
use std::sync::Arc;

use aionui_common::AgentType;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tracing::warn;

use crate::audit::AuditSink;
use crate::error::DbError;
use crate::repository::audit_event::{AuditActorKind, AuditVia, NewAuditEvent};
use crate::repository::governance::{AgentRunPermissionRow, IGovernanceRepository, PolicyRow, UserRoleRow};

/// The built-in seed account; the bootstrap owner in multi-user modes.
pub const BOOTSTRAP_OWNER_USER_ID: &str = "system_default_user";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    Owner,
    Admin,
    Member,
}

impl UserRole {
    pub const ALL: [UserRole; 3] = [UserRole::Owner, UserRole::Admin, UserRole::Member];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "owner" => Some(Self::Owner),
            "admin" => Some(Self::Admin),
            "member" => Some(Self::Member),
            _ => None,
        }
    }

    /// Change policies and agent run permissions; see every audit row.
    pub fn can_manage_policies(self) -> bool {
        matches!(self, Self::Owner | Self::Admin)
    }

    /// Grant or change roles.
    pub fn can_manage_roles(self) -> bool {
        matches!(self, Self::Owner)
    }

    pub fn can_view_all_audit(self) -> bool {
        self.can_manage_policies()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyKey {
    NoYolo,
    McpStrictOnly,
    BannedBackends,
    ForceSandbox,
}

impl PolicyKey {
    pub const ALL: [PolicyKey; 4] = [
        PolicyKey::NoYolo,
        PolicyKey::McpStrictOnly,
        PolicyKey::BannedBackends,
        PolicyKey::ForceSandbox,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoYolo => "no_yolo",
            Self::McpStrictOnly => "mcp_strict_only",
            Self::BannedBackends => "banned_backends",
            Self::ForceSandbox => "force_sandbox",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.as_str() == raw.trim())
    }
}

/// Effective policy switches. `Default` is "everything off".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySet {
    /// Refuse full-auto (yolo) session modes everywhere.
    pub no_yolo: bool,
    /// Refuse sessions whose MCP surface is not WorkMate-managed and strictly
    /// isolated (only aionrs, Claude and Gemini can isolate).
    pub mcp_strict_only: bool,
    /// Agent backends / types that may not be started (lower-case names, e.g.
    /// `codex`, `antigravity`).
    pub banned_backends: Vec<String>,
    /// Refuse modes that disable the agent's sandbox, and batch runs that
    /// edit a workspace in place instead of a managed worktree.
    pub force_sandbox: bool,
}

impl PolicySet {
    /// Build from stored rows; unknown keys and malformed values are ignored
    /// (logged) so a bad row can never turn a policy on by accident.
    pub fn from_rows(rows: &[PolicyRow]) -> Self {
        let mut set = Self::default();
        for row in rows {
            let Some(key) = PolicyKey::parse(&row.key) else {
                continue;
            };
            let value = match serde_json::from_str::<Value>(&row.value) {
                Ok(value) => value,
                Err(err) => {
                    warn!(key = %row.key, error = %err, "ignoring malformed policy value");
                    continue;
                }
            };
            match normalize_policy_value(key, &value) {
                Ok(value) => set.apply(key, &value),
                Err(err) => warn!(key = %row.key, error = %err, "ignoring invalid policy value"),
            }
        }
        set
    }

    fn apply(&mut self, key: PolicyKey, value: &Value) {
        match key {
            PolicyKey::NoYolo => self.no_yolo = value.as_bool().unwrap_or(false),
            PolicyKey::McpStrictOnly => self.mcp_strict_only = value.as_bool().unwrap_or(false),
            PolicyKey::ForceSandbox => self.force_sandbox = value.as_bool().unwrap_or(false),
            PolicyKey::BannedBackends => {
                self.banned_backends = value
                    .as_array()
                    .map(|items| items.iter().filter_map(|v| v.as_str().map(ToOwned::to_owned)).collect())
                    .unwrap_or_default();
            }
        }
    }

    pub fn value_of(&self, key: PolicyKey) -> Value {
        match key {
            PolicyKey::NoYolo => json!(self.no_yolo),
            PolicyKey::McpStrictOnly => json!(self.mcp_strict_only),
            PolicyKey::ForceSandbox => json!(self.force_sandbox),
            PolicyKey::BannedBackends => json!(self.banned_backends),
        }
    }

    pub fn is_backend_banned(&self, name: &str) -> bool {
        let name = name.trim().to_ascii_lowercase();
        !name.is_empty() && self.banned_backends.contains(&name)
    }
}

/// Validate and normalise a policy value: booleans for the switches; a list
/// of distinct, trimmed, lower-case names (at most 64, each ≤ 64 chars) for
/// `banned_backends`.
pub fn normalize_policy_value(key: PolicyKey, value: &Value) -> Result<Value, String> {
    match key {
        PolicyKey::NoYolo | PolicyKey::McpStrictOnly | PolicyKey::ForceSandbox => value
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| format!("{} must be a boolean", key.as_str())),
        PolicyKey::BannedBackends => {
            let items = value
                .as_array()
                .ok_or_else(|| "banned_backends must be an array of strings".to_owned())?;
            if items.len() > 64 {
                return Err("banned_backends accepts at most 64 entries".into());
            }
            let mut names: Vec<String> = Vec::new();
            for item in items {
                let name = item
                    .as_str()
                    .ok_or_else(|| "banned_backends must be an array of strings".to_owned())?
                    .trim()
                    .to_ascii_lowercase();
                if name.is_empty() || name.len() > 64 {
                    return Err("banned_backends entries must be 1-64 characters".into());
                }
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            Ok(json!(names))
        }
    }
}

/// Mode aliases and native ids that mean "full-auto" regardless of backend.
const FULL_AUTO_MODE_IDS: &[&str] = &[
    "yolo",
    "yoloNoSandbox",
    "bypassPermissions",
    "agent-full-access",
    "full-access",
];
/// Modes that turn the agent's sandbox off (codex full access, the
/// `yoloNoSandbox` alias).
const NO_SANDBOX_MODE_IDS: &[&str] = &["yoloNoSandbox", "agent-full-access", "full-access"];

/// Whether `mode` is a full-auto (yolo) mode for this agent. Besides the
/// common aliases this matches the backend's own full-auto id
/// ([`AgentType::full_auto_mode_id`], e.g. opencode `build`), except where
/// that id is the plain `default` (hermes has no full-auto mode).
pub fn is_full_auto_mode(mode: &str, agent_type: Option<AgentType>, backend: Option<&str>) -> bool {
    let mode = mode.trim();
    if mode.is_empty() {
        return false;
    }
    if FULL_AUTO_MODE_IDS.contains(&mode) {
        return true;
    }
    match agent_type {
        Some(agent_type) => {
            let native = agent_type.full_auto_mode_id(backend);
            native != "default" && native == mode
        }
        None => false,
    }
}

pub fn is_no_sandbox_mode(mode: &str) -> bool {
    NO_SANDBOX_MODE_IDS.contains(&mode.trim())
}

/// Whether the backend can strictly isolate its MCP surface (B-09 / M7
/// matrix: aionrs, Claude Code, Gemini).
pub fn backend_supports_strict_mcp(agent_type: Option<AgentType>, backend: Option<&str>) -> bool {
    match agent_type {
        Some(AgentType::Aionrs) => true,
        Some(AgentType::Acp) => matches!(backend.map(str::trim), Some("claude") | Some("gemini")),
        _ => false,
    }
}

/// Where a launch is being checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchSurface {
    /// A new conversation (UI, batch, channel, runtime create, …).
    ConversationCreate,
    /// A session-mode change on an existing conversation.
    ModeChange,
    /// A cron job being created, updated or run.
    Cron,
    /// A batch job being created.
    Batch,
}

impl LaunchSurface {
    fn as_str(self) -> &'static str {
        match self {
            Self::ConversationCreate => "conversation",
            Self::ModeChange => "mode_change",
            Self::Cron => "cron",
            Self::Batch => "batch",
        }
    }
}

/// What is about to run. Fields that do not apply are `None` and skip the
/// matching checks.
#[derive(Debug, Clone, Copy)]
pub struct LaunchFacts<'a> {
    pub surface: LaunchSurface,
    /// Audit target (`conversation`, `cron_job`, `batch_job`) and id.
    pub target: Option<(&'a str, &'a str)>,
    pub agent_type: Option<AgentType>,
    pub backend: Option<&'a str>,
    pub assistant_id: Option<&'a str>,
    pub mode: Option<&'a str>,
    /// Session MCP is WorkMate-managed (`extra.mcp_strict`). `None` = not a
    /// session launch; the MCP check is skipped.
    pub mcp_strict: Option<bool>,
    /// The run edits a user workspace in place (batch `workspace` target).
    pub in_place_workspace: bool,
    /// The user confirmed full-auto for this launch (Autopilot
    /// `autopilot_yolo`), whatever `mode` says. Refused under `no_yolo`.
    pub full_auto: bool,
    pub via: Option<AuditVia>,
}

impl<'a> LaunchFacts<'a> {
    pub fn new(surface: LaunchSurface) -> Self {
        Self {
            surface,
            target: None,
            agent_type: None,
            backend: None,
            assistant_id: None,
            mode: None,
            mcp_strict: None,
            in_place_workspace: false,
            full_auto: false,
            via: None,
        }
    }
}

/// Why a launch was refused. `key` is the policy key (or
/// `agent_run_permission`); `message` is user-facing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyDenial {
    pub key: String,
    pub message: String,
}

impl std::fmt::Display for PolicyDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn agent_type_name(agent_type: AgentType) -> String {
    serde_json::to_value(agent_type)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_default()
}

/// Pure policy evaluation (no roles). First violation wins.
pub fn evaluate_policies(policy: &PolicySet, facts: &LaunchFacts<'_>) -> Option<PolicyDenial> {
    let deny = |key: PolicyKey, message: String| {
        Some(PolicyDenial {
            key: key.as_str().to_owned(),
            message,
        })
    };
    if !policy.banned_backends.is_empty() {
        let names = [facts.backend.map(str::to_owned), facts.agent_type.map(agent_type_name)];
        if let Some(name) = names.iter().flatten().find(|name| policy.is_backend_banned(name)) {
            return deny(
                PolicyKey::BannedBackends,
                format!("Policy blocks the `{name}` agent backend (banned_backends)."),
            );
        }
    }
    if policy.no_yolo && facts.full_auto {
        return deny(
            PolicyKey::NoYolo,
            "Policy forbids Autopilot full-auto (no_yolo); switch the job to Ask.".into(),
        );
    }
    if let Some(mode) = facts.mode {
        if policy.no_yolo && is_full_auto_mode(mode, facts.agent_type, facts.backend) {
            return deny(
                PolicyKey::NoYolo,
                format!("Policy forbids full-auto mode `{}` (no_yolo).", mode.trim()),
            );
        }
        if policy.force_sandbox && is_no_sandbox_mode(mode) {
            return deny(
                PolicyKey::ForceSandbox,
                format!(
                    "Policy forbids mode `{}`, which turns the sandbox off (force_sandbox).",
                    mode.trim()
                ),
            );
        }
    }
    if policy.force_sandbox && facts.in_place_workspace {
        return deny(
            PolicyKey::ForceSandbox,
            "Policy requires batch runs to use a managed worktree, not edit a workspace in place (force_sandbox)."
                .into(),
        );
    }
    if policy.mcp_strict_only
        && let Some(mcp_strict) = facts.mcp_strict
    {
        if !backend_supports_strict_mcp(facts.agent_type, facts.backend) {
            let name = facts
                .backend
                .map(str::to_owned)
                .or_else(|| facts.agent_type.map(agent_type_name))
                .unwrap_or_else(|| "this".into());
            return deny(
                PolicyKey::McpStrictOnly,
                format!(
                    "Policy only allows agents that isolate MCP strictly; `{name}` may load local MCP servers (mcp_strict_only)."
                ),
            );
        }
        if !mcp_strict {
            return deny(
                PolicyKey::McpStrictOnly,
                "Policy only allows sessions with a managed MCP list; start from an assistant or choose MCP servers explicitly (mcp_strict_only)."
                    .into(),
            );
        }
    }
    None
}

#[derive(Debug, thiserror::Error)]
pub enum GovernanceError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Db(#[from] DbError),
}

/// Roles, run permissions and policies, with audit.
pub struct GovernanceService {
    repo: Arc<dyn IGovernanceRepository>,
    audit: Option<AuditSink>,
    roles_active: bool,
    default_via: AuditVia,
    cache: RwLock<Option<PolicySet>>,
}

impl std::fmt::Debug for GovernanceService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GovernanceService")
            .field("roles_active", &self.roles_active)
            .finish_non_exhaustive()
    }
}

impl GovernanceService {
    /// `roles_active` is true in multi-user identity modes (WebUI / AionPro).
    pub fn new(repo: Arc<dyn IGovernanceRepository>, audit: Option<AuditSink>, roles_active: bool) -> Self {
        Self {
            repo,
            audit,
            roles_active,
            default_via: if roles_active {
                AuditVia::WebUi
            } else {
                AuditVia::Desktop
            },
            cache: RwLock::new(None),
        }
    }

    pub fn roles_active(&self) -> bool {
        self.roles_active
    }

    pub fn repository(&self) -> &Arc<dyn IGovernanceRepository> {
        &self.repo
    }

    /// Effective role. Single-user mode: always owner. Multi-user: the stored
    /// row; without one, the bootstrap account is owner and anyone else a
    /// member.
    pub async fn role_of(&self, user_id: &str) -> Result<UserRole, DbError> {
        if !self.roles_active {
            return Ok(UserRole::Owner);
        }
        if let Some(row) = self.repo.get_role(user_id).await?
            && let Some(role) = UserRole::parse(&row.role)
        {
            return Ok(role);
        }
        Ok(if user_id == BOOTSTRAP_OWNER_USER_ID {
            UserRole::Owner
        } else {
            UserRole::Member
        })
    }

    pub async fn policies(&self) -> Result<PolicySet, DbError> {
        if let Some(set) = self.cache.read().await.as_ref() {
            return Ok(set.clone());
        }
        let rows = self.repo.list_policies().await?;
        let set = PolicySet::from_rows(&rows);
        *self.cache.write().await = Some(set.clone());
        Ok(set)
    }

    pub async fn policy_rows(&self) -> Result<Vec<PolicyRow>, DbError> {
        self.repo.list_policies().await
    }

    async fn audit(&self, event: NewAuditEvent) {
        if let Some(sink) = self.audit.as_ref() {
            sink.record(event).await;
        }
    }

    /// Set one policy. Records `policy.update` with the before/after value.
    pub async fn set_policy(
        &self,
        actor_user_id: &str,
        key: PolicyKey,
        value: &Value,
        via: Option<AuditVia>,
    ) -> Result<PolicySet, GovernanceError> {
        let value = normalize_policy_value(key, value).map_err(GovernanceError::Invalid)?;
        let before = self.policies().await?.value_of(key);
        let text = serde_json::to_string(&value).map_err(|e| GovernanceError::Invalid(e.to_string()))?;
        self.repo.upsert_policy(key.as_str(), &text, actor_user_id).await?;
        let rows = self.repo.list_policies().await?;
        let set = PolicySet::from_rows(&rows);
        *self.cache.write().await = Some(set.clone());
        self.audit(
            NewAuditEvent::new("policy.update", AuditActorKind::User, via.unwrap_or(self.default_via))
                .with_user(actor_user_id)
                .with_actor(actor_user_id)
                .with_target("policy", key.as_str())
                .with_summary(format!("Policy {} set to {}", key.as_str(), text))
                .with_diff(json!({ "before": before, "after": value })),
        )
        .await;
        Ok(set)
    }

    /// Grant a role. Refuses to leave the system without an owner.
    pub async fn set_role(
        &self,
        actor_user_id: &str,
        target_user_id: &str,
        role: UserRole,
        via: Option<AuditVia>,
    ) -> Result<UserRoleRow, GovernanceError> {
        let before = self.role_of(target_user_id).await?;
        if before == UserRole::Owner && role != UserRole::Owner {
            let rows = self.repo.list_roles().await?;
            let mut owners: Vec<&str> = rows
                .iter()
                .filter(|row| row.role == UserRole::Owner.as_str())
                .map(|row| row.user_id.as_str())
                .collect();
            // The bootstrap account is an owner even without a stored row.
            if !rows.iter().any(|row| row.user_id == BOOTSTRAP_OWNER_USER_ID) {
                owners.push(BOOTSTRAP_OWNER_USER_ID);
            }
            if owners.iter().all(|owner| *owner == target_user_id) {
                return Err(GovernanceError::Conflict(
                    "at least one owner must remain; grant another owner first".into(),
                ));
            }
        }
        let row = self
            .repo
            .upsert_role(target_user_id, role.as_str(), actor_user_id)
            .await?;
        self.audit(
            NewAuditEvent::new("role.update", AuditActorKind::User, via.unwrap_or(self.default_via))
                .with_user(actor_user_id)
                .with_actor(actor_user_id)
                .with_target("user", target_user_id)
                .with_summary(format!("Role of {target_user_id} set to {}", role.as_str()))
                .with_diff(json!({ "before": before.as_str(), "after": role.as_str() })),
        )
        .await;
        Ok(row)
    }

    /// Set which roles may start an assistant / agent type. `None` lifts the
    /// restriction (the row is kept with a NULL list).
    pub async fn set_agent_run_permission(
        &self,
        actor_user_id: &str,
        subject_kind: &str,
        subject_id: &str,
        allowed_roles: Option<&[UserRole]>,
        via: Option<AuditVia>,
    ) -> Result<AgentRunPermissionRow, GovernanceError> {
        if !matches!(subject_kind, "assistant" | "agent_type") {
            return Err(GovernanceError::Invalid(
                "subject_kind must be `assistant` or `agent_type`".into(),
            ));
        }
        let subject_id = subject_id.trim();
        if subject_id.is_empty() || subject_id.len() > 128 {
            return Err(GovernanceError::Invalid("subject_id must be 1-128 characters".into()));
        }
        let subject_id = if subject_kind == "agent_type" {
            subject_id.to_ascii_lowercase()
        } else {
            subject_id.to_owned()
        };
        let before = self
            .repo
            .get_agent_run_permission(subject_kind, &subject_id)
            .await?
            .and_then(|row| row.allowed_roles)
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .unwrap_or(Value::Null);
        let after = allowed_roles.map(|roles| {
            let mut names: Vec<&str> = Vec::new();
            for role in UserRole::ALL {
                if roles.contains(&role) {
                    names.push(role.as_str());
                }
            }
            json!(names)
        });
        let text = after.as_ref().map(Value::to_string);
        let row = self
            .repo
            .upsert_agent_run_permission(subject_kind, &subject_id, text.as_deref(), actor_user_id)
            .await?;
        self.audit(
            NewAuditEvent::new(
                "agent_permission.update",
                AuditActorKind::User,
                via.unwrap_or(self.default_via),
            )
            .with_user(actor_user_id)
            .with_actor(actor_user_id)
            .with_target(subject_kind, subject_id.clone())
            .with_summary(format!(
                "Run permission for {subject_kind} {subject_id} set to {}",
                text.as_deref().unwrap_or("unrestricted")
            ))
            .with_diff(json!({ "before": before, "after": after.unwrap_or(Value::Null) })),
        )
        .await;
        Ok(row)
    }

    /// Members in multi-user mode get a narrowed assistant / agent catalog.
    /// Owner/admin and single-user mode always see the full catalog.
    pub async fn catalog_filter_active(&self, user_id: &str) -> Result<bool, DbError> {
        if !self.roles_active {
            return Ok(false);
        }
        Ok(self.role_of(user_id).await? == UserRole::Member)
    }

    /// Whether `user_id` may run (and therefore see) one permission subject.
    ///
    /// Mirrors [`Self::check_run_permission`] for a single `(kind, id)`:
    /// missing row or `allowed_roles = null` means unrestricted.
    pub async fn is_run_subject_allowed(
        &self,
        user_id: &str,
        subject_kind: &str,
        subject_id: &str,
    ) -> Result<bool, DbError> {
        if !self.roles_active {
            return Ok(true);
        }
        let id = subject_id.trim();
        if id.is_empty() {
            return Ok(true);
        }
        let id = if subject_kind == "agent_type" {
            id.to_ascii_lowercase()
        } else {
            id.to_owned()
        };
        let role = self.role_of(user_id).await?;
        if matches!(role, UserRole::Owner | UserRole::Admin) {
            return Ok(true);
        }
        let Some(row) = self.repo.get_agent_run_permission(subject_kind, &id).await? else {
            return Ok(true);
        };
        let Some(raw) = row.allowed_roles else {
            return Ok(true);
        };
        let allowed: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
        Ok(allowed.iter().any(|name| name == role.as_str()))
    }

    /// Catalog visibility for an assistant, including its bound agent type /
    /// ACP backend when present (same subjects `check_launch` consults).
    pub async fn may_see_assistant(
        &self,
        user_id: &str,
        assistant_id: &str,
        agent_type: Option<AgentType>,
        backend: Option<&str>,
    ) -> Result<bool, DbError> {
        if !self.catalog_filter_active(user_id).await? {
            return Ok(true);
        }
        if !self.is_run_subject_allowed(user_id, "assistant", assistant_id).await? {
            return Ok(false);
        }
        if let Some(backend) = backend.filter(|b| !b.trim().is_empty())
            && !self.is_run_subject_allowed(user_id, "agent_type", backend).await?
        {
            return Ok(false);
        }
        if let Some(agent_type) = agent_type {
            let name = agent_type_name(agent_type);
            if !self.is_run_subject_allowed(user_id, "agent_type", &name).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// User-facing denial when a member GETs an assistant outside their scope.
    pub fn catalog_deny_message(&self, role: UserRole, subject_kind: &str, subject_id: &str) -> String {
        format!(
            "Your role ({}) may not start {subject_kind} `{subject_id}`.",
            role.as_str()
        )
    }

    async fn check_run_permission(
        &self,
        user_id: &str,
        facts: &LaunchFacts<'_>,
    ) -> Result<Option<PolicyDenial>, DbError> {
        if !self.roles_active {
            return Ok(None);
        }
        let mut subjects: Vec<(&str, String)> = Vec::new();
        if let Some(id) = facts.assistant_id.filter(|id| !id.trim().is_empty()) {
            subjects.push(("assistant", id.trim().to_owned()));
        }
        if let Some(backend) = facts.backend.filter(|b| !b.trim().is_empty()) {
            subjects.push(("agent_type", backend.trim().to_ascii_lowercase()));
        }
        if let Some(agent_type) = facts.agent_type {
            subjects.push(("agent_type", agent_type_name(agent_type)));
        }
        if subjects.is_empty() {
            return Ok(None);
        }
        let role = self.role_of(user_id).await?;
        for (kind, id) in subjects {
            let Some(row) = self.repo.get_agent_run_permission(kind, &id).await? else {
                continue;
            };
            let Some(raw) = row.allowed_roles else { continue };
            let allowed: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
            if !allowed.iter().any(|name| name == role.as_str()) {
                return Ok(Some(PolicyDenial {
                    key: "agent_run_permission".into(),
                    message: format!("Your role ({}) may not start {kind} `{id}`.", role.as_str()),
                }));
            }
        }
        Ok(None)
    }

    /// Check a launch against run permissions and policies. A denial is
    /// audited as `policy.deny`. Storage errors fail closed.
    pub async fn check_launch(&self, user_id: &str, facts: &LaunchFacts<'_>) -> Result<(), PolicyDenial> {
        let outcome = async {
            if let Some(denial) = self.check_run_permission(user_id, facts).await? {
                return Ok::<_, DbError>(Some(denial));
            }
            let policy = self.policies().await?;
            Ok(evaluate_policies(&policy, facts))
        }
        .await;
        let denial = match outcome {
            Ok(None) => return Ok(()),
            Ok(Some(denial)) => denial,
            Err(err) => {
                warn!(error = %err, "governance check failed; refusing launch");
                PolicyDenial {
                    key: "governance_unavailable".into(),
                    message: "Governance policy could not be read; try again.".into(),
                }
            }
        };
        let mut event = NewAuditEvent::new(
            "policy.deny",
            AuditActorKind::User,
            facts.via.unwrap_or(self.default_via),
        )
        .with_user(user_id)
        .with_actor(user_id)
        .with_summary(denial.message.clone())
        .with_diff(json!({
            "key": denial.key,
            "surface": facts.surface.as_str(),
            "agent_type": facts.agent_type.map(agent_type_name),
            "backend": facts.backend,
            "assistant_id": facts.assistant_id,
            "mode": facts.mode,
            "mcp_strict": facts.mcp_strict,
            "in_place_workspace": facts.in_place_workspace,
            "full_auto": facts.full_auto,
        }));
        if let Some((kind, id)) = facts.target {
            event = event.with_target(kind, id);
        }
        self.audit(event).await;
        Err(denial)
    }
}
