use std::process::ExitCode;

use crate::process_report::{ExitKind, ProcessReport};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootstrapErrorCode {
    ConfigInvalid,
    RuntimeInitFailed,
    LoggingInitFailed,
    BindFailed,
    DataInitFailed,
    ServiceInitFailed,
    ServerFailed,
    ShutdownFailed,
}

impl BootstrapErrorCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ConfigInvalid => "BOOTSTRAP_CONFIG_INVALID",
            Self::RuntimeInitFailed => "BOOTSTRAP_RUNTIME_INIT_FAILED",
            Self::LoggingInitFailed => "BOOTSTRAP_LOGGING_INIT_FAILED",
            Self::BindFailed => "BOOTSTRAP_BIND_FAILED",
            Self::DataInitFailed => "BOOTSTRAP_DATA_INIT_FAILED",
            Self::ServiceInitFailed => "BOOTSTRAP_SERVICE_INIT_FAILED",
            Self::ServerFailed => "BOOTSTRAP_SERVER_FAILED",
            Self::ShutdownFailed => "BOOTSTRAP_SHUTDOWN_FAILED",
        }
    }

    pub(crate) fn exit_kind(self) -> ExitKind {
        match self {
            Self::ConfigInvalid => ExitKind::Config,
            Self::BindFailed => ExitKind::Unavailable,
            Self::RuntimeInitFailed
            | Self::LoggingInitFailed
            | Self::DataInitFailed
            | Self::ServiceInitFailed
            | Self::ServerFailed
            | Self::ShutdownFailed => ExitKind::Internal,
        }
    }
}

#[derive(Debug)]
pub(crate) struct BootstrapError {
    code: BootstrapErrorCode,
    stage: &'static str,
    message: &'static str,
    source: Option<anyhow::Error>,
    fields: Vec<(&'static str, String)>,
}

impl BootstrapError {
    pub(crate) fn new(code: BootstrapErrorCode, stage: &'static str, message: &'static str) -> Self {
        Self {
            code,
            stage,
            message,
            source: None,
            fields: Vec::new(),
        }
    }

    pub(crate) fn with_source(mut self, source: impl Into<anyhow::Error>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub(crate) fn with_field(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.fields.push((key, value.into()));
        self
    }

    #[cfg(test)]
    pub(crate) fn code(&self) -> BootstrapErrorCode {
        self.code
    }

    #[cfg(test)]
    pub(crate) fn stage(&self) -> &'static str {
        self.stage
    }

    pub(crate) fn exit_code(&self) -> ExitCode {
        self.code.exit_kind().exit_code()
    }

    pub(crate) fn stderr_line(&self) -> String {
        let mut fields = vec![("stage", self.stage.to_owned())];
        fields.extend(self.fields.clone());
        ProcessReport {
            code: self.code.as_str(),
            message: self.message,
            exit_kind: self.code.exit_kind(),
            fields,
        }
        .stderr_line()
    }

    pub(crate) fn log_source(&self) {
        if self.code == BootstrapErrorCode::LoggingInitFailed {
            // Logging setup failed before a tracing subscriber was available.
            // Keep raw source private on the error object; public stderr remains
            // the stable boundary line only.
            return;
        }
        if let Some(source) = &self.source {
            tracing::error!(
                code = self.code.as_str(),
                stage = self.stage,
                error = %source,
                "bootstrap boundary failure"
            );
        }
    }
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for BootstrapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::ExitCode;

    #[test]
    fn bootstrap_error_renders_code_stage_and_exit() {
        let err = BootstrapError::new(
            BootstrapErrorCode::BindFailed,
            "bind.listener",
            "failed to bind HTTP listener",
        )
        .with_field("port", "13400");

        assert_eq!(err.code(), BootstrapErrorCode::BindFailed);
        assert_eq!(err.stage(), "bind.listener");
        assert_eq!(err.exit_code(), ExitCode::from(3));
        assert_eq!(
            err.stderr_line(),
            "BOOTSTRAP_BIND_FAILED stage=bind.listener port=13400: failed to bind HTTP listener"
        );
    }

    #[test]
    fn config_invalid_uses_exit_2() {
        let err = BootstrapError::new(
            BootstrapErrorCode::ConfigInvalid,
            "config.parse",
            "invalid application configuration",
        );

        assert_eq!(err.exit_code(), ExitCode::from(2));
    }

    #[test]
    fn source_is_preserved_but_not_rendered_to_stderr() {
        let err = BootstrapError::new(
            BootstrapErrorCode::DataInitFailed,
            "data.open",
            "failed to initialize data layer",
        )
        .with_source(anyhow::anyhow!("secret disk path /tmp/secret.db"));

        let stderr = err.stderr_line();
        assert!(!stderr.contains("secret"));
        assert!(!stderr.contains("/tmp/secret.db"));

        let source = std::error::Error::source(&err).expect("source should be preserved");
        assert!(source.to_string().contains("secret disk path /tmp/secret.db"));
    }

    #[test]
    fn logging_init_source_remains_private_when_tracing_is_unavailable() {
        let err = BootstrapError::new(
            BootstrapErrorCode::LoggingInitFailed,
            "logging.init",
            "failed to initialize logging",
        )
        .with_source(anyhow::anyhow!("secret log path /tmp/aion.log"));

        err.log_source();

        let stderr = err.stderr_line();
        assert!(stderr.contains("BOOTSTRAP_LOGGING_INIT_FAILED"));
        assert!(!stderr.contains("secret log path"));
        assert!(!stderr.contains("/tmp/aion.log"));
        assert!(
            std::error::Error::source(&err)
                .expect("source should be preserved")
                .to_string()
                .contains("secret log path")
        );
    }
}
