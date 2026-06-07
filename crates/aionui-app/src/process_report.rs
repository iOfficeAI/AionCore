// Consumed by following CLI/bootstrap boundary tasks.
#![allow(dead_code)]

use std::process::ExitCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitKind {
    Internal,
    Config,
    Unavailable,
}

impl ExitKind {
    pub(crate) fn exit_code(self) -> ExitCode {
        match self {
            Self::Internal => ExitCode::from(1),
            Self::Config => ExitCode::from(2),
            Self::Unavailable => ExitCode::from(3),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessReport {
    pub(crate) code: &'static str,
    pub(crate) message: &'static str,
    pub(crate) exit_kind: ExitKind,
    pub(crate) fields: Vec<(&'static str, String)>,
}

impl ProcessReport {
    pub(crate) fn stderr_line(&self) -> String {
        let mut line = self.code.to_owned();
        for (key, value) in &self.fields {
            line.push(' ');
            line.push_str(key);
            line.push('=');
            line.push_str(&sanitize_field(value));
        }
        line.push_str(": ");
        line.push_str(self.message);
        line
    }
}

fn sanitize_field(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_ascii_graphic() && ch != ':' { ch } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_kind_maps_to_process_exit_codes() {
        assert_eq!(ExitKind::Internal.exit_code(), ExitCode::from(1));
        assert_eq!(ExitKind::Config.exit_code(), ExitCode::from(2));
        assert_eq!(ExitKind::Unavailable.exit_code(), ExitCode::from(3));
    }

    #[test]
    fn stderr_line_starts_with_stable_code() {
        let report = ProcessReport {
            code: "MCP_ENV_MISSING",
            message: "missing required environment variable",
            exit_kind: ExitKind::Config,
            fields: vec![
                ("subcommand", "mcp-guide-stdio".into()),
                ("env", "AION_MCP_PORT".into()),
            ],
        };

        assert_eq!(
            report.stderr_line(),
            "MCP_ENV_MISSING subcommand=mcp-guide-stdio env=AION_MCP_PORT: missing required environment variable"
        );
        assert_eq!(report.exit_kind.exit_code(), ExitCode::from(2));
    }

    #[test]
    fn stderr_line_sanitizes_field_values() {
        let report = ProcessReport {
            code: "BOOTSTRAP_BIND_FAILED",
            message: "failed to bind HTTP listener",
            exit_kind: ExitKind::Unavailable,
            fields: vec![("stage", "bind.listener\nsecret".into())],
        };

        assert_eq!(
            report.stderr_line(),
            "BOOTSTRAP_BIND_FAILED stage=bind.listener_secret: failed to bind HTTP listener"
        );
    }
}
