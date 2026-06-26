// Consumed by following CLI/MCP boundary wiring tasks.
#![allow(dead_code)]
#![allow(clippy::enum_variant_names)]

use std::process::ExitCode;

use crate::process_report::{ExitKind, ProcessReport};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CliBoundaryCode {
    CliRuntimeInitFailed,
    CliDoctorDatabaseFailed,
    CliDoctorRegistryHydrateFailed,
    CliPrepareManagedResourcesFailed,
    McpEnvMissing,
    McpEnvInvalidPort,
    McpStdinTty,
    McpStdinReadFailed,
    McpStdinFrameInvalid,
    McpStdinJsonInvalid,
    McpFrameTooLarge,
    McpJsonSerializeFailed,
    McpTcpConnectFailed,
    McpTcpWriteFailed,
    McpTcpReadFailed,
    McpStdoutWriteFailed,
    McpStdioServeFailed,
    McpSessionEndedWithError,
    McpTaskJoinPanic,
    McpHttpConnectOrTimeout,
    McpHttpResponseReadFailed,
    McpHttpStatusError,
    McpToolRemoteError,
    McpToolResponseUnexpected,
}

impl CliBoundaryCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::CliRuntimeInitFailed => "CLI_RUNTIME_INIT_FAILED",
            Self::CliDoctorDatabaseFailed => "CLI_DOCTOR_DATABASE_FAILED",
            Self::CliDoctorRegistryHydrateFailed => "CLI_DOCTOR_REGISTRY_HYDRATE_FAILED",
            Self::CliPrepareManagedResourcesFailed => "CLI_PREPARE_MANAGED_RESOURCES_FAILED",
            Self::McpEnvMissing => "MCP_ENV_MISSING",
            Self::McpEnvInvalidPort => "MCP_ENV_INVALID_PORT",
            Self::McpStdinTty => "MCP_STDIN_TTY",
            Self::McpStdinReadFailed => "MCP_STDIN_READ_FAILED",
            Self::McpStdinFrameInvalid => "MCP_STDIN_FRAME_INVALID",
            Self::McpStdinJsonInvalid => "MCP_STDIN_JSON_INVALID",
            Self::McpFrameTooLarge => "MCP_FRAME_TOO_LARGE",
            Self::McpJsonSerializeFailed => "MCP_JSON_SERIALIZE_FAILED",
            Self::McpTcpConnectFailed => "MCP_TCP_CONNECT_FAILED",
            Self::McpTcpWriteFailed => "MCP_TCP_WRITE_FAILED",
            Self::McpTcpReadFailed => "MCP_TCP_READ_FAILED",
            Self::McpStdoutWriteFailed => "MCP_STDOUT_WRITE_FAILED",
            Self::McpStdioServeFailed => "MCP_STDIO_SERVE_FAILED",
            Self::McpSessionEndedWithError => "MCP_SESSION_ENDED_WITH_ERROR",
            Self::McpTaskJoinPanic => "MCP_TASK_JOIN_PANIC",
            Self::McpHttpConnectOrTimeout => "MCP_HTTP_CONNECT_OR_TIMEOUT",
            Self::McpHttpResponseReadFailed => "MCP_HTTP_RESPONSE_READ_FAILED",
            Self::McpHttpStatusError => "MCP_HTTP_STATUS_ERROR",
            Self::McpToolRemoteError => "MCP_TOOL_REMOTE_ERROR",
            Self::McpToolResponseUnexpected => "MCP_TOOL_RESPONSE_UNEXPECTED",
        }
    }

    fn exit_kind(self) -> ExitKind {
        match self {
            Self::McpEnvMissing
            | Self::McpEnvInvalidPort
            | Self::McpStdinTty
            | Self::McpStdinFrameInvalid
            | Self::McpStdinJsonInvalid
            | Self::McpFrameTooLarge => ExitKind::Config,
            Self::McpTcpConnectFailed | Self::McpHttpConnectOrTimeout | Self::McpHttpStatusError => {
                ExitKind::Unavailable
            }
            _ => ExitKind::Internal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CliBoundaryError {
    code: CliBoundaryCode,
    subcommand: &'static str,
    message: &'static str,
    fields: Vec<(&'static str, String)>,
}

impl CliBoundaryError {
    pub(crate) fn new(code: CliBoundaryCode, subcommand: &'static str, message: &'static str) -> Self {
        Self {
            code,
            subcommand,
            message,
            fields: Vec::new(),
        }
    }

    pub(crate) fn with_field(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.fields.push((key, value.into()));
        self
    }

    pub(crate) fn code(&self) -> CliBoundaryCode {
        self.code
    }

    pub(crate) fn exit_code(&self) -> ExitCode {
        self.code.exit_kind().exit_code()
    }

    pub(crate) fn stderr_line(&self) -> String {
        let mut fields = vec![("subcommand", self.subcommand.to_owned())];
        fields.extend(self.fields.clone());
        ProcessReport {
            code: self.code.as_str(),
            message: self.message,
            exit_kind: self.code.exit_kind(),
            fields,
        }
        .stderr_line()
    }
}

pub(crate) fn missing_env(subcommand: &'static str, env: &'static str) -> CliBoundaryError {
    CliBoundaryError::new(
        CliBoundaryCode::McpEnvMissing,
        subcommand,
        "missing required environment variable",
    )
    .with_field("env", env)
}

pub(crate) fn invalid_port(subcommand: &'static str, env: &'static str) -> CliBoundaryError {
    CliBoundaryError::new(
        CliBoundaryCode::McpEnvInvalidPort,
        subcommand,
        "invalid MCP helper port",
    )
    .with_field("env", env)
}

pub(crate) fn parse_required_port(
    subcommand: &'static str,
    env: &'static str,
    value: &str,
) -> Result<u16, CliBoundaryError> {
    value.parse::<u16>().map_err(|_| invalid_port(subcommand, env))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_env_renders_stable_stderr_and_exit_2() {
        let err = missing_env("mcp-team-stdio", "TEAM_MCP_PORT");
        assert_eq!(err.code(), CliBoundaryCode::McpEnvMissing);
        assert_eq!(err.exit_code(), ExitCode::from(2));
        assert_eq!(
            err.stderr_line(),
            "MCP_ENV_MISSING subcommand=mcp-team-stdio env=TEAM_MCP_PORT: missing required environment variable"
        );
    }

    #[test]
    fn invalid_port_renders_stable_stderr_and_exit_2() {
        let err = parse_required_port("mcp-team-stdio", "TEAM_MCP_PORT", "not-a-port").unwrap_err();
        assert_eq!(err.code(), CliBoundaryCode::McpEnvInvalidPort);
        assert_eq!(err.exit_code(), ExitCode::from(2));
        assert_eq!(
            err.stderr_line(),
            "MCP_ENV_INVALID_PORT subcommand=mcp-team-stdio env=TEAM_MCP_PORT: invalid MCP helper port"
        );
    }

    #[test]
    fn tcp_connect_failed_uses_exit_3() {
        let err = CliBoundaryError::new(
            CliBoundaryCode::McpTcpConnectFailed,
            "mcp-bridge",
            "failed to connect to local MCP TCP listener",
        );
        assert_eq!(err.exit_code(), ExitCode::from(3));
    }
}
