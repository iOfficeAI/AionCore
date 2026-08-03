//! Antigravity (`agy` CLI) direct-CLI backend.
//!
//! Process model: ONE agy process per turn. agy has no `--input-format` (its
//! flag set is validated by Go's `flag` package, so an unknown flag is a hard
//! error — verified), which rules out the persistent-stdin FIFO shape the
//! claude lane uses. Multi-turn continuity comes from `--conversation <id>`,
//! which agy resumes from its own on-disk conversation store.
//!
//! Wire shapes here are verified against captured samples in
//! `~/aion/protocols/samples/antigravity-cli/1.1.8/`.

mod argv;
mod conn;
mod mcp_config;
mod models;
mod skills;
mod translate;
mod version;
mod wire;

pub use conn::{AntigravityConnection, AntigravitySessionBackend, antigravity_capabilities};
pub use version::{VersionDrift, version_drift};
