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
/// i18n key for the "some steps in this turn did not take effect" notice,
/// interpolating `{{count}}`. agy-specific: no other backend reports steps that
/// silently failed while the turn still ends in success.
pub(crate) const CODE_STEPS_FAILED: &str = "ANTIGRAVITY_STEPS_FAILED";

mod conn;
mod mcp_config;
mod models;
mod skills;
mod translate;
mod wire;

pub use conn::{AntigravityConnection, AntigravitySessionBackend, antigravity_capabilities};
