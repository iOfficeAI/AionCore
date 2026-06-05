use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use semver::Version;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeTool {
    Node,
    Npm,
    Npx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedNodeSource {
    Bundled,
    Managed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeCommandProbe {
    ExplicitPath { path: PathBuf },
    PathLookup { command: String },
    NodeTool { tool: NodeTool, command: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCommand {
    pub program: PathBuf,
    pub args_prefix: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
}

impl ResolvedCommand {
    pub fn plain(program: PathBuf) -> Self {
        Self {
            program,
            args_prefix: vec![],
            env: vec![],
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedNodeRuntime {
    pub source: ResolvedNodeSource,
    pub root: PathBuf,
    pub version: Version,
    pub node_path: PathBuf,
    pub npm_path: PathBuf,
    pub npm_args_prefix: Vec<OsString>,
    pub npx_path: PathBuf,
    pub npx_args_prefix: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
}

impl ResolvedNodeRuntime {
    pub fn npm_command(&self) -> ResolvedCommand {
        ResolvedCommand {
            program: self.npm_path.clone(),
            args_prefix: self.npm_args_prefix.clone(),
            env: self.env.clone(),
        }
    }

    pub fn npx_command(&self) -> ResolvedCommand {
        ResolvedCommand {
            program: self.npx_path.clone(),
            args_prefix: self.npx_args_prefix.clone(),
            env: self.env.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRuntimeProgressPhase {
    WaitingForLock,
    Downloading,
    Extracting,
    Validating,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRuntimeFailureKind {
    Timeout,
    DownloadFailed,
    HttpStatus,
    ValidationFailed,
    UnsupportedPlatform,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRuntimeProgress {
    pub phase: NodeRuntimeProgressPhase,
    pub failure_kind: Option<NodeRuntimeFailureKind>,
    pub message: Option<String>,
    pub status_code: Option<u16>,
}

impl NodeRuntimeProgress {
    pub fn waiting_for_lock(message: impl Into<String>) -> Self {
        Self {
            phase: NodeRuntimeProgressPhase::WaitingForLock,
            failure_kind: None,
            message: Some(message.into()),
            status_code: None,
        }
    }

    pub fn downloading(message: impl Into<String>) -> Self {
        Self {
            phase: NodeRuntimeProgressPhase::Downloading,
            failure_kind: None,
            message: Some(message.into()),
            status_code: None,
        }
    }

    pub fn extracting(message: impl Into<String>) -> Self {
        Self {
            phase: NodeRuntimeProgressPhase::Extracting,
            failure_kind: None,
            message: Some(message.into()),
            status_code: None,
        }
    }

    pub fn validating(message: impl Into<String>) -> Self {
        Self {
            phase: NodeRuntimeProgressPhase::Validating,
            failure_kind: None,
            message: Some(message.into()),
            status_code: None,
        }
    }

    pub fn ready(message: impl Into<String>) -> Self {
        Self {
            phase: NodeRuntimeProgressPhase::Ready,
            failure_kind: None,
            message: Some(message.into()),
            status_code: None,
        }
    }

    pub fn failed(kind: NodeRuntimeFailureKind, message: impl Into<String>) -> Self {
        Self {
            phase: NodeRuntimeProgressPhase::Failed,
            failure_kind: Some(kind),
            message: Some(message.into()),
            status_code: None,
        }
    }

    pub fn failed_with_status(kind: NodeRuntimeFailureKind, status_code: u16, message: impl Into<String>) -> Self {
        Self {
            phase: NodeRuntimeProgressPhase::Failed,
            failure_kind: Some(kind),
            message: Some(message.into()),
            status_code: Some(status_code),
        }
    }
}

pub trait NodeRuntimeProgressReporter: Send + Sync {
    fn report(&self, update: NodeRuntimeProgress);
}

impl<F> NodeRuntimeProgressReporter for F
where
    F: Fn(NodeRuntimeProgress) + Send + Sync,
{
    fn report(&self, update: NodeRuntimeProgress) {
        self(update);
    }
}

pub type SharedNodeRuntimeProgressReporter = Arc<dyn NodeRuntimeProgressReporter>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRuntimeSupport {
    pub supported: bool,
    pub detail: String,
}

impl NodeRuntimeSupport {
    pub fn is_supported(&self) -> bool {
        self.supported
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorRow {
    pub tool: String,
    pub source: String,
    pub detail: String,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct NodeRuntimeError {
    message: String,
}

impl NodeRuntimeError {
    pub fn system_invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn managed_invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn unsupported_platform(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn io_system(error: std::io::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}
