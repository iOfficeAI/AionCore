#![warn(clippy::disallowed_types)]

//! Backend-served static logo assets.
pub mod error;
pub mod routes;
pub mod service;
pub mod state;

pub use error::AssetError;
pub use routes::asset_routes;
pub use service::AssetService;
pub use state::AssetRouterState;
