#![warn(clippy::disallowed_types)]

//! Office document preview, format conversion, and proxy.
pub mod conversion;
pub mod error;
mod officecli_runtime;
pub mod port;
pub mod proxy;
pub mod routes;
pub mod state;
pub mod types;
pub mod watch_manager;

pub use conversion::ConversionService;
pub use error::OfficeError;
pub use proxy::{ProxyError, ProxyService};
pub use routes::{office_proxy_routes, office_routes};
pub use state::OfficeRouterState;
pub use types::{DocType, OfficecliStatus};
pub use watch_manager::{DefaultProcessSpawner, OfficecliWatchManager, ProcessHandle, ProcessSpawner};
