use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DevelopmentPolicyRules {
    pub allowed_commands: Vec<String>,
    pub protected_paths: Vec<String>,
    pub allowed_network_hosts: Vec<String>,
    pub protected_branches: Vec<String>,
    pub dangerous_confirmation_count: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PolicyOperation {
    Command { program: String },
    Path { path: String, write: bool },
    Network { host: String },
    Git { operation: String, branch: Option<String> },
    Deploy { target: String },
    Delete { path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PolicyDecision {
    Allowed,
    Denied { reason: String },
    ConfirmationRequired { remaining: u8 },
}

pub struct PolicyEngine;

impl PolicyEngine {
    pub fn evaluate(rules: &DevelopmentPolicyRules, operation: &PolicyOperation, confirmations: u8) -> PolicyDecision {
        match operation {
            PolicyOperation::Command { program } => {
                let command = Path::new(program)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or(program);
                if rules.allowed_commands.iter().any(|allowed| allowed == command) {
                    PolicyDecision::Allowed
                } else {
                    PolicyDecision::Denied {
                        reason: "command is outside the Project allowlist".into(),
                    }
                }
            }
            PolicyOperation::Path { path, write } => {
                if unsafe_path(path) {
                    return PolicyDecision::Denied {
                        reason: "path escapes the Project boundary".into(),
                    };
                }
                if *write
                    && rules
                        .protected_paths
                        .iter()
                        .any(|protected| path_matches(protected, path))
                {
                    PolicyDecision::Denied {
                        reason: "path is protected".into(),
                    }
                } else {
                    PolicyDecision::Allowed
                }
            }
            PolicyOperation::Network { host } => {
                if rules
                    .allowed_network_hosts
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(host))
                {
                    PolicyDecision::Allowed
                } else {
                    PolicyDecision::Denied {
                        reason: "network destination is outside the Project allowlist".into(),
                    }
                }
            }
            PolicyOperation::Git { operation, branch } => {
                let dangerous = matches!(operation.as_str(), "push" | "merge" | "force_push" | "tag")
                    || branch
                        .as_ref()
                        .is_some_and(|branch| rules.protected_branches.iter().any(|protected| protected == branch));
                confirmation(rules, confirmations, dangerous)
            }
            PolicyOperation::Deploy { .. } | PolicyOperation::Delete { .. } => confirmation(rules, confirmations, true),
        }
    }
}

fn confirmation(rules: &DevelopmentPolicyRules, confirmations: u8, required: bool) -> PolicyDecision {
    if !required {
        return PolicyDecision::Allowed;
    }
    let required = rules.dangerous_confirmation_count.max(1);
    if confirmations >= required {
        PolicyDecision::Allowed
    } else {
        PolicyDecision::ConfirmationRequired {
            remaining: required - confirmations,
        }
    }
}

fn unsafe_path(value: &str) -> bool {
    let path = Path::new(value);
    path.is_absolute() || path.components().any(|component| component == Component::ParentDir)
}

fn path_matches(protected: &str, candidate: &str) -> bool {
    candidate == protected
        || candidate
            .strip_prefix(protected)
            .is_some_and(|remainder| remainder.starts_with('/') || remainder.starts_with('\\'))
}
