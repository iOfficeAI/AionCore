#![warn(clippy::disallowed_types)]

//! System services: provider management, model fetching, settings, and version checks.
pub mod bedrock_probe;
pub mod client_pref;
pub mod diagnostics;
pub mod error;
pub mod model_fetcher;
pub mod protocol;
pub mod provider;
pub mod routes;
pub mod runtime_prepare;
pub mod settings;
pub mod sysinfo;
pub mod version;

pub use bedrock_probe::{ConnectionTestRouterState, ConnectionTestService, connection_test_routes};
pub use client_pref::ClientPrefService;
pub use diagnostics::FeedbackDiagnosticsService;
pub use error::SystemError;
pub use model_fetcher::ModelFetchService;
pub use protocol::ProtocolDetectionService;
pub use provider::ProviderService;
pub use routes::{SystemRouterState, settings_routes, system_routes};
pub use runtime_prepare::RuntimePrepareService;
pub use settings::SettingsService;
pub use version::VersionCheckService;
