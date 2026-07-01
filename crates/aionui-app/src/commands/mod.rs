//! Subcommand implementations for the `aioncore` binary.
//!
//! This file is a façade — module declarations and re-exports only.
//! All logic lives in the submodules.

pub(crate) mod cmd_cron_helper;
pub(crate) mod cmd_doctor;
pub(crate) mod cmd_mcp_bridge;
pub(crate) mod cmd_prepare_managed_resources;
pub(crate) mod cmd_server;
pub(crate) mod cmd_team_stdio;
pub(crate) mod error;

pub(crate) use cmd_cron_helper::run_cron_helper;
pub(crate) use cmd_doctor::run_doctor;
pub(crate) use cmd_mcp_bridge::run_mcp_bridge;
pub(crate) use cmd_prepare_managed_resources::run_prepare_managed_resources;
pub(crate) use cmd_server::{bind_http_listener, run_server};
pub(crate) use cmd_team_stdio::run_team_stdio;
pub(crate) use error::{CliBoundaryCode, CliBoundaryError};
