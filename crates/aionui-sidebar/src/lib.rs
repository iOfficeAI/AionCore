//! Sidebar read model: one request renders the whole left panel.
//!
//! The backend owns classification (pinned / project / pseudo-dir / chats),
//! windowing, ordering, and pin truth (a `user_order` row's existence). The
//! frontend renders in the given order and runs no classification. See
//! `feat-project-design/temp/left-panel/` (`design.md`, `api-contract-sidebar.md`,
//! `boundary-rules.md`).

mod cascade;
mod ports;
mod service;
mod types;

pub mod routes;

pub use cascade::UserOrderDeleteHook;
pub use ports::RemoveProjectPorts;
pub use routes::{SidebarRouterState, sidebar_routes};
pub use service::SidebarService;
pub use types::SidebarError;
