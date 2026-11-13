//! Antigravity (`agy` CLI) direct-CLI backend.
//!
//! Process model: ONE agy process per conversation turn-burst, driven via
//! `--input-format stream-json --output-format stream-json -p ""` (`agy
//! changelog` 1.1.15; live-verified on `agy 1.5.8`). Each turn starts a warm
//! process with retained stdin so mid-turn `Command::Steer` writes
//! (`{"event":"user","message":{"role":"user","content":...}}`) run
//! back-to-back in the same process without a ~6s respawn; once the active turn
//! and any steered follow-ups settle, stdin is closed (`EOF`) so the process
//! exits cleanly, and later idle turns resume via `--conversation <id>`.
//!
//! Wire shapes here are verified against captured samples in
//! `~/aion/protocols/samples/antigravity-cli/1.1.8/` and live `agy 1.5.8`
//! `--input-format stream-json` captures.

mod argv;
/// i18n key for the "some steps in this turn did not take effect" notice,
/// interpolating `{{count}}`. agy-specific: no other backend reports steps that
/// silently failed while the turn still ends in success.
pub(crate) const CODE_STEPS_FAILED: &str = "ANTIGRAVITY_STEPS_FAILED";

mod conn;
mod models;
mod skills;
mod translate;
mod wire;

pub use conn::{AntigravityConnection, AntigravitySessionBackend, antigravity_capabilities};
