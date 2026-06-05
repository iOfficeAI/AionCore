use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use crate::{ResolvedCommand, ResolvedNodeRuntime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedAcpToolId {
    CodexAcp,
    ClaudeAgentAcp,
}

impl ManagedAcpToolId {
    pub fn slug(self) -> &'static str {
        match self {
            Self::CodexAcp => "codex-acp",
            Self::ClaudeAgentAcp => "claude-agent-acp",
        }
    }

    pub fn version(self) -> &'static str {
        match self {
            Self::CodexAcp => "0.14.0",
            Self::ClaudeAgentAcp => "0.39.0",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::CodexAcp => "Codex ACP",
            Self::ClaudeAgentAcp => "Claude ACP",
        }
    }

    pub fn package_name(self) -> &'static str {
        match self {
            Self::CodexAcp => "@zed-industries/codex-acp",
            Self::ClaudeAgentAcp => "@agentclientprotocol/claude-agent-acp",
        }
    }

    pub fn from_backend(backend: &str) -> Option<Self> {
        match backend {
            "codex" => Some(Self::CodexAcp),
            "claude" => Some(Self::ClaudeAgentAcp),
            _ => None,
        }
    }

    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "codex-acp" => Some(Self::CodexAcp),
            "claude-agent-acp" => Some(Self::ClaudeAgentAcp),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedManagedAcpTool {
    pub id: ManagedAcpToolId,
    pub version: String,
    pub root: PathBuf,
    pub entrypoint: PathBuf,
    pub env_path_entries: Vec<PathBuf>,
}

impl ResolvedManagedAcpTool {
    pub fn command(&self, node_runtime: &ResolvedNodeRuntime) -> ResolvedCommand {
        let mut env = node_runtime.env.clone();
        if !self.env_path_entries.is_empty() {
            let mut paths: Vec<PathBuf> = self.env_path_entries.clone();
            if let Some(existing_path) = env
                .iter()
                .find_map(|(key, value)| (key == "PATH").then(|| std::env::split_paths(value).collect::<Vec<_>>()))
            {
                paths.extend(existing_path);
            } else if let Some(current_path) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&current_path));
            }
            if let Ok(path) = std::env::join_paths(paths) {
                if let Some(path_env) = env.iter_mut().find(|(key, _)| key == "PATH") {
                    path_env.1 = path;
                } else {
                    env.push((OsString::from("PATH"), path));
                }
            }
        }

        ResolvedCommand {
            program: node_runtime.node_path.clone(),
            args_prefix: vec![self.entrypoint.clone().into_os_string()],
            env,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedAcpToolProgressPhase {
    WaitingForLock,
    Downloading,
    Extracting,
    Validating,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedAcpToolFailureKind {
    Timeout,
    DownloadFailed,
    HttpStatus,
    ChecksumMismatch,
    ValidationFailed,
    UnsupportedPlatform,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedAcpToolProgress {
    pub phase: ManagedAcpToolProgressPhase,
    pub failure_kind: Option<ManagedAcpToolFailureKind>,
    pub message: Option<String>,
    pub status_code: Option<u16>,
}

impl ManagedAcpToolProgress {
    pub fn waiting_for_lock(message: impl Into<String>) -> Self {
        Self {
            phase: ManagedAcpToolProgressPhase::WaitingForLock,
            failure_kind: None,
            message: Some(message.into()),
            status_code: None,
        }
    }

    pub fn downloading(message: impl Into<String>) -> Self {
        Self {
            phase: ManagedAcpToolProgressPhase::Downloading,
            failure_kind: None,
            message: Some(message.into()),
            status_code: None,
        }
    }

    pub fn extracting(message: impl Into<String>) -> Self {
        Self {
            phase: ManagedAcpToolProgressPhase::Extracting,
            failure_kind: None,
            message: Some(message.into()),
            status_code: None,
        }
    }

    pub fn validating(message: impl Into<String>) -> Self {
        Self {
            phase: ManagedAcpToolProgressPhase::Validating,
            failure_kind: None,
            message: Some(message.into()),
            status_code: None,
        }
    }

    pub fn ready(message: impl Into<String>) -> Self {
        Self {
            phase: ManagedAcpToolProgressPhase::Ready,
            failure_kind: None,
            message: Some(message.into()),
            status_code: None,
        }
    }

    pub fn failed(kind: ManagedAcpToolFailureKind, message: impl Into<String>) -> Self {
        Self {
            phase: ManagedAcpToolProgressPhase::Failed,
            failure_kind: Some(kind),
            message: Some(message.into()),
            status_code: None,
        }
    }

    pub fn failed_with_status(kind: ManagedAcpToolFailureKind, status_code: u16, message: impl Into<String>) -> Self {
        Self {
            phase: ManagedAcpToolProgressPhase::Failed,
            failure_kind: Some(kind),
            message: Some(message.into()),
            status_code: Some(status_code),
        }
    }
}

pub trait ManagedAcpToolProgressReporter: Send + Sync {
    fn report(&self, update: ManagedAcpToolProgress);
}

impl<F> ManagedAcpToolProgressReporter for F
where
    F: Fn(ManagedAcpToolProgress) + Send + Sync,
{
    fn report(&self, update: ManagedAcpToolProgress) {
        self(update);
    }
}

pub type SharedManagedAcpToolProgressReporter = Arc<dyn ManagedAcpToolProgressReporter>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedAcpToolSupport {
    pub supported: bool,
    pub detail: String,
}

impl ManagedAcpToolSupport {
    pub fn is_supported(&self) -> bool {
        self.supported
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct ManagedAcpToolError {
    message: String,
}

impl ManagedAcpToolError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn unsupported_platform(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn io(error: std::io::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}
