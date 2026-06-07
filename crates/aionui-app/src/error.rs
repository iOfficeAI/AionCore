use std::process::ExitCode;

use crate::{bootstrap, commands};

#[derive(Debug)]
pub(crate) enum MainError {
    Bootstrap(bootstrap::BootstrapError),
    Cli(commands::CliBoundaryError),
    Other(anyhow::Error),
}

impl MainError {
    pub(crate) fn report(&self) {
        match self {
            Self::Bootstrap(err) => {
                err.log_source();
                eprintln!("{}", err.stderr_line());
            }
            Self::Cli(err) => {
                eprintln!("{}", err.stderr_line());
            }
            Self::Other(err) => {
                eprintln!("CLI_INTERNAL_ERROR: {err}");
            }
        }
    }

    pub(crate) fn exit_code(&self) -> ExitCode {
        match self {
            Self::Bootstrap(err) => err.exit_code(),
            Self::Cli(err) => err.exit_code(),
            Self::Other(_) => ExitCode::from(1),
        }
    }
}

impl From<bootstrap::BootstrapError> for MainError {
    fn from(error: bootstrap::BootstrapError) -> Self {
        Self::Bootstrap(error)
    }
}

impl From<commands::CliBoundaryError> for MainError {
    fn from(error: commands::CliBoundaryError) -> Self {
        Self::Cli(error)
    }
}

impl From<anyhow::Error> for MainError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
}
