use std::collections::BTreeMap;

use aionui_api_types::{
    AgentIntegrationMode, PlanningIsolationLevel, PlanningIsolationResponse, PolicyDecision, PolicyDecisionKind,
    TaskSessionMode, ToolCapability,
};

pub const AION_STRICT_PLANNING_ALLOWED_TOOLS: [&str; 4] = ["Read", "Grep", "Glob", "ViewImage"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AionToolClassification {
    Capability(ToolCapability),
    DeniedControl,
}

/// Classify every tool registered by the pinned Aion bootstrap.
/// Dynamic names return `None` and are denied by the exact-name policy.
pub fn classify_aion_registered_tool(name: &str) -> Option<AionToolClassification> {
    match name {
        "Read" | "Grep" | "Glob" | "ViewImage" => {
            Some(AionToolClassification::Capability(ToolCapability::FilesystemRead))
        }
        "Write" | "Edit" => Some(AionToolClassification::Capability(ToolCapability::FilesystemWrite)),
        "ExecCommand" => Some(AionToolClassification::Capability(ToolCapability::ShellExecute)),
        "Skill" | "Spawn" | "ToolSearch" | "EnterPlanMode" | "ExitPlanMode" => {
            Some(AionToolClassification::DeniedControl)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyContext {
    pub task_id: String,
    pub project_id: Option<String>,
    pub task_mode: TaskSessionMode,
    pub agent_id: String,
    pub integration_mode: AgentIntegrationMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPolicyRequest {
    pub capability: Option<ToolCapability>,
    pub resource: Option<String>,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeEnforcementCapabilities {
    pub integration_mode: AgentIntegrationMode,
    pub mandatory_gateway: bool,
    pub all_registered_tools_classified: bool,
    pub filesystem_mutation_blocked: bool,
    pub shell_mutation_blocked: bool,
    pub git_mutation_blocked: bool,
    pub mcp_mutation_blocked: bool,
    pub external_network_blocked: bool,
    pub delegated_tools_inherit_policy: bool,
    pub unknown_tools_fail_closed: bool,
    pub mutation_hard_denied: bool,
    pub mcp_mediated: bool,
    pub known_bypass: bool,
}

impl RuntimeEnforcementCapabilities {
    pub const fn unknown() -> Self {
        Self {
            integration_mode: AgentIntegrationMode::Unknown,
            mandatory_gateway: false,
            all_registered_tools_classified: false,
            filesystem_mutation_blocked: false,
            shell_mutation_blocked: false,
            git_mutation_blocked: false,
            mcp_mutation_blocked: false,
            external_network_blocked: false,
            delegated_tools_inherit_policy: false,
            unknown_tools_fail_closed: false,
            mutation_hard_denied: false,
            mcp_mediated: false,
            known_bypass: true,
        }
    }
}

pub const fn audited_runtime_capabilities(integration_mode: AgentIntegrationMode) -> RuntimeEnforcementCapabilities {
    match integration_mode {
        AgentIntegrationMode::NativeSandbox => RuntimeEnforcementCapabilities {
            integration_mode,
            mandatory_gateway: false,
            all_registered_tools_classified: false,
            filesystem_mutation_blocked: true,
            shell_mutation_blocked: true,
            git_mutation_blocked: true,
            mcp_mutation_blocked: false,
            external_network_blocked: false,
            delegated_tools_inherit_policy: false,
            unknown_tools_fail_closed: false,
            mutation_hard_denied: false,
            mcp_mediated: false,
            known_bypass: false,
        },
        AgentIntegrationMode::NativePermissionMode => RuntimeEnforcementCapabilities {
            integration_mode,
            mandatory_gateway: false,
            all_registered_tools_classified: false,
            filesystem_mutation_blocked: true,
            shell_mutation_blocked: true,
            git_mutation_blocked: true,
            mcp_mutation_blocked: false,
            external_network_blocked: false,
            delegated_tools_inherit_policy: false,
            unknown_tools_fail_closed: false,
            mutation_hard_denied: false,
            mcp_mediated: false,
            known_bypass: false,
        },
        AgentIntegrationMode::GenericAcp | AgentIntegrationMode::InProcessToolRegistry => {
            RuntimeEnforcementCapabilities {
                integration_mode,
                known_bypass: true,
                ..RuntimeEnforcementCapabilities::unknown()
            }
        }
        AgentIntegrationMode::Unknown => RuntimeEnforcementCapabilities::unknown(),
    }
}

/// Audited capability set for the exact-name Aion strict-planning profile.
///
/// The profile advertises and executes only Read, Grep, Glob, and ViewImage.
/// Every other built-in, dynamically registered, or delegated tool is denied
/// before approval. MCP processes, hooks, and native plan tools are disabled
/// before bootstrap.
pub const fn audited_aion_strict_runtime_capabilities() -> RuntimeEnforcementCapabilities {
    RuntimeEnforcementCapabilities {
        integration_mode: AgentIntegrationMode::InProcessToolRegistry,
        mandatory_gateway: true,
        all_registered_tools_classified: true,
        filesystem_mutation_blocked: true,
        shell_mutation_blocked: true,
        git_mutation_blocked: true,
        mcp_mutation_blocked: true,
        external_network_blocked: true,
        delegated_tools_inherit_policy: true,
        unknown_tools_fail_closed: true,
        mutation_hard_denied: true,
        mcp_mediated: true,
        known_bypass: false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanningPolicy {
    internal_network_decision: PolicyDecisionKind,
}

impl PlanningPolicy {
    pub const fn new(internal_network_decision: PolicyDecisionKind) -> Self {
        Self {
            internal_network_decision,
        }
    }

    pub fn evaluate(&self, context: &PolicyContext, request: &ToolPolicyRequest) -> PolicyDecision {
        let result = match request.capability {
            None => decision(
                PolicyDecisionKind::Deny,
                "planning.unknown.deny",
                "Unknown tool capabilities are denied during planning",
            ),
            Some(ToolCapability::FilesystemRead) => allow("planning.filesystem.read"),
            Some(ToolCapability::FilesystemWrite) => deny("planning.filesystem.write"),
            Some(ToolCapability::ShellReadonly) => allow("planning.shell.readonly"),
            Some(ToolCapability::ShellExecute) => deny("planning.shell.execute"),
            Some(ToolCapability::GitRead) => allow("planning.git.read"),
            Some(ToolCapability::GitWrite) => deny("planning.git.write"),
            Some(ToolCapability::McpRead) => allow("planning.mcp.read"),
            Some(ToolCapability::McpWrite) => deny("planning.mcp.write"),
            Some(ToolCapability::NetworkInternal) => decision(
                self.internal_network_decision,
                "planning.network.internal",
                "Internal network access follows the configured planning policy",
            ),
            Some(ToolCapability::NetworkExternal) => deny("planning.network.external"),
        };
        tracing::info!(
            target: "aionui_policy_audit",
            audit_event = "planning.policy_decision",
            task_id = %context.task_id,
            agent_id = %context.agent_id,
            integration_mode = ?context.integration_mode,
            tool = request.metadata.get("tool").map(String::as_str).unwrap_or("unknown"),
            capability = ?request.capability,
            decision = ?result.decision,
            reason = %result.reason,
            rule_id = %result.rule_id,
            timestamp = aionui_common::now_ms(),
            "planning.policy_decision"
        );
        result
    }
}

impl Default for PlanningPolicy {
    fn default() -> Self {
        Self::new(PolicyDecisionKind::Deny)
    }
}

pub fn resolve_planning_isolation(
    runtime: RuntimeEnforcementCapabilities,
    evidence: Vec<String>,
) -> PlanningIsolationResponse {
    let all_mutation_paths_blocked = runtime.mandatory_gateway
        && runtime.all_registered_tools_classified
        && runtime.filesystem_mutation_blocked
        && runtime.shell_mutation_blocked
        && runtime.git_mutation_blocked
        && runtime.mcp_mutation_blocked
        && runtime.external_network_blocked
        && runtime.delegated_tools_inherit_policy
        && runtime.unknown_tools_fail_closed
        && runtime.mutation_hard_denied
        && runtime.mcp_mediated;

    let level = if all_mutation_paths_blocked && !runtime.known_bypass {
        PlanningIsolationLevel::Guaranteed
    } else if runtime.known_bypass {
        PlanningIsolationLevel::Unsupported
    } else {
        PlanningIsolationLevel::BestEffort
    };
    let reason = match level {
        PlanningIsolationLevel::Guaranteed => "All audited mutation paths have mandatory runtime enforcement",
        PlanningIsolationLevel::BestEffort => {
            "The runtime blocks some mutation paths but does not cover every audited path"
        }
        PlanningIsolationLevel::Unsupported => {
            "The runtime has a known mutation path outside WorkMate policy enforcement"
        }
    };

    PlanningIsolationResponse {
        level,
        integration_mode: runtime.integration_mode,
        automatic_planning_enabled: level == PlanningIsolationLevel::Guaranteed,
        reason: reason.to_owned(),
        evidence,
    }
}

pub fn classify_shell_argv(argv: &[String]) -> Option<ToolCapability> {
    let (program, args) = argv.split_first()?;
    let program = program.trim().to_ascii_lowercase();
    if program.is_empty() || args.iter().any(|arg| contains_shell_control(arg)) {
        return None;
    }

    if matches!(program.as_str(), "rg" | "grep" | "ls" | "cat" | "get-content") {
        if program == "rg" && args.iter().any(|arg| arg == "--pre" || arg.starts_with("--pre=")) {
            return Some(ToolCapability::ShellExecute);
        }
        return Some(ToolCapability::ShellReadonly);
    }
    if program == "find" {
        let mutating_or_executing = args.iter().any(|arg| {
            matches!(
                arg.to_ascii_lowercase().as_str(),
                "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-fls" | "-fprint" | "-fprint0"
            )
        });
        return Some(if mutating_or_executing {
            ToolCapability::ShellExecute
        } else {
            ToolCapability::ShellReadonly
        });
    }
    if program == "git" {
        let command = args.first()?.to_ascii_lowercase();
        let safe_command = matches!(command.as_str(), "status" | "log" | "show" | "diff" | "rev-parse")
            || (command == "branch"
                && args.get(1..).is_some_and(|rest| {
                    rest.is_empty()
                        || matches!(rest, [argument] if argument == "--show-current" || argument == "--list")
                }));
        let unsafe_option = args.iter().any(|arg| {
            let option = arg.to_ascii_lowercase();
            option.starts_with("--output")
                || option.starts_with("--exec")
                || option == "--ext-diff"
                || option == "--textconv"
                || option == "-c"
        });
        return Some(if safe_command && !unsafe_option {
            ToolCapability::GitRead
        } else {
            ToolCapability::GitWrite
        });
    }
    Some(ToolCapability::ShellExecute)
}

pub fn classify_mcp_capability(trusted_metadata: bool, declared: Option<ToolCapability>) -> Option<ToolCapability> {
    if !trusted_metadata {
        return None;
    }
    match declared {
        Some(ToolCapability::McpRead | ToolCapability::McpWrite) => declared,
        _ => None,
    }
}

fn contains_shell_control(value: &str) -> bool {
    value
        .chars()
        .any(|character| matches!(character, ';' | '&' | '|' | '>' | '<' | '`' | '\n' | '\r'))
}

fn allow(rule_id: &str) -> PolicyDecision {
    decision(
        PolicyDecisionKind::Allow,
        rule_id,
        "Read-only capability is allowed during planning",
    )
}

fn deny(rule_id: &str) -> PolicyDecision {
    decision(
        PolicyDecisionKind::Deny,
        rule_id,
        "Mutating capability is denied during planning",
    )
}

fn decision(decision: PolicyDecisionKind, rule_id: &str, reason: &str) -> PolicyDecision {
    PolicyDecision {
        decision,
        reason: reason.to_owned(),
        rule_id: rule_id.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> PolicyContext {
        PolicyContext {
            task_id: "task-1".into(),
            project_id: Some("project-1".into()),
            task_mode: TaskSessionMode::Plan,
            agent_id: "agent-1".into(),
            integration_mode: AgentIntegrationMode::NativeSandbox,
        }
    }

    fn request(capability: Option<ToolCapability>) -> ToolPolicyRequest {
        ToolPolicyRequest {
            capability,
            resource: None,
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn planning_policy_allows_reads_and_denies_mutations() {
        let policy = PlanningPolicy::default();
        for capability in [
            ToolCapability::FilesystemRead,
            ToolCapability::ShellReadonly,
            ToolCapability::GitRead,
            ToolCapability::McpRead,
        ] {
            assert_eq!(
                policy.evaluate(&context(), &request(Some(capability))).decision,
                PolicyDecisionKind::Allow
            );
        }
        for capability in [
            ToolCapability::FilesystemWrite,
            ToolCapability::ShellExecute,
            ToolCapability::GitWrite,
            ToolCapability::McpWrite,
            ToolCapability::NetworkExternal,
        ] {
            assert_eq!(
                policy.evaluate(&context(), &request(Some(capability))).decision,
                PolicyDecisionKind::Deny
            );
        }
        assert_eq!(
            policy.evaluate(&context(), &request(None)).decision,
            PolicyDecisionKind::Deny
        );
        assert_eq!(
            PlanningPolicy::new(PolicyDecisionKind::Ask)
                .evaluate(&context(), &request(Some(ToolCapability::NetworkInternal)))
                .decision,
            PolicyDecisionKind::Ask,
        );
    }

    #[test]
    fn only_complete_runtime_enforcement_is_guaranteed() {
        let guaranteed = RuntimeEnforcementCapabilities {
            integration_mode: AgentIntegrationMode::NativeSandbox,
            mandatory_gateway: true,
            all_registered_tools_classified: true,
            filesystem_mutation_blocked: true,
            shell_mutation_blocked: true,
            git_mutation_blocked: true,
            mcp_mutation_blocked: true,
            external_network_blocked: true,
            delegated_tools_inherit_policy: true,
            unknown_tools_fail_closed: true,
            mutation_hard_denied: true,
            mcp_mediated: true,
            known_bypass: false,
        };
        assert_eq!(
            resolve_planning_isolation(guaranteed, Vec::new()).level,
            PlanningIsolationLevel::Guaranteed
        );
        assert_eq!(
            resolve_planning_isolation(
                RuntimeEnforcementCapabilities {
                    mcp_mutation_blocked: false,
                    ..guaranteed
                },
                Vec::new()
            )
            .level,
            PlanningIsolationLevel::BestEffort,
        );
        assert_eq!(
            resolve_planning_isolation(
                RuntimeEnforcementCapabilities {
                    known_bypass: true,
                    ..guaranteed
                },
                Vec::new()
            )
            .level,
            PlanningIsolationLevel::Unsupported,
        );
    }

    #[test]
    fn aion_registry_inventory_classifies_every_builtin_and_fails_unknown_closed() {
        let registered = [
            "Read",
            "Write",
            "Edit",
            "ExecCommand",
            "Grep",
            "Glob",
            "ViewImage",
            "Skill",
            "Spawn",
            "EnterPlanMode",
            "ExitPlanMode",
            "ToolSearch",
        ];
        assert_eq!(
            registered
                .iter()
                .filter(|name| classify_aion_registered_tool(name).is_some())
                .count(),
            registered.len()
        );
        assert_eq!(classify_aion_registered_tool("dynamic_mcp_tool"), None);
    }

    #[test]
    fn audited_profiles_never_claim_guaranteed_without_complete_coverage() {
        for mode in [
            AgentIntegrationMode::NativeSandbox,
            AgentIntegrationMode::NativePermissionMode,
            AgentIntegrationMode::GenericAcp,
            AgentIntegrationMode::InProcessToolRegistry,
            AgentIntegrationMode::Unknown,
        ] {
            assert_ne!(
                resolve_planning_isolation(audited_runtime_capabilities(mode), Vec::new()).level,
                PlanningIsolationLevel::Guaranteed,
            );
        }
    }

    #[test]
    fn shell_classifier_is_allowlist_based_and_rejects_shell_syntax() {
        assert_eq!(
            classify_shell_argv(&["rg".into(), "needle".into()]),
            Some(ToolCapability::ShellReadonly)
        );
        assert_eq!(
            classify_shell_argv(&["git".into(), "status".into()]),
            Some(ToolCapability::GitRead),
        );
        assert_eq!(
            classify_shell_argv(&["git".into(), "diff".into(), "--output=patch".into()]),
            Some(ToolCapability::GitWrite),
        );
        assert_eq!(
            classify_shell_argv(&["git".into(), "branch".into(), "new-branch".into()]),
            Some(ToolCapability::GitWrite),
        );
        assert_eq!(
            classify_shell_argv(&["find".into(), ".".into(), "-delete".into()]),
            Some(ToolCapability::ShellExecute),
        );
        assert_eq!(
            classify_shell_argv(&["rg".into(), "--pre=generator".into(), "needle".into()]),
            Some(ToolCapability::ShellExecute),
        );
        assert_eq!(
            classify_shell_argv(&["rm".into(), "file".into()]),
            Some(ToolCapability::ShellExecute)
        );
        assert_eq!(classify_shell_argv(&["rg".into(), "x;touch".into()]), None);
    }

    #[test]
    fn mcp_requires_trusted_capability_metadata() {
        assert_eq!(classify_mcp_capability(false, Some(ToolCapability::McpRead)), None);
        assert_eq!(
            classify_mcp_capability(true, Some(ToolCapability::McpRead)),
            Some(ToolCapability::McpRead),
        );
        assert_eq!(
            classify_mcp_capability(true, Some(ToolCapability::FilesystemRead)),
            None
        );
    }
}
