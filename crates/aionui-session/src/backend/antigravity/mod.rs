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
/// i18n key for the live silent-reasoning status notice (`agy` streams no
/// thinking prose, so stdout goes silent until the first response token).
pub(crate) const CODE_THINKING: &str = "ANTIGRAVITY_THINKING";
/// i18n key for the settled past-tense notice replacing a live thinking banner
/// once the model phase completes (`{{duration_seconds}}`, `{{thinking_tokens}}`).
pub(crate) const CODE_THOUGHT_COMPLETE: &str = "ANTIGRAVITY_THOUGHT_COMPLETE";
/// i18n key for the live context-compaction status notice (`/compact` or
/// high-context silent window near `agy`'s auto-compaction ceiling).
pub(crate) const CODE_COMPACTING: &str = "ANTIGRAVITY_COMPACTING";
/// i18n key for the settled context-compaction completion notice emitted when
/// `agy` finishes a `checkpoint` step (`{{duration_seconds}}`).
pub const CODE_COMPACTED: &str = "ANTIGRAVITY_COMPACTED";
/// Unscoped `supersedes_key` sentinel emitted by `Translator` for in-place
/// status/compaction notices; `conn::drive_turn_stdout` scopes it to the
/// concrete session, turn, and model phase before broadcast.
pub(crate) const STATUS_NOTICE_KEY: &str = "antigravity.status";

mod conn;
mod models;
mod skills;
mod translate;
mod wire;

pub use conn::{AntigravityConnection, AntigravitySessionBackend, antigravity_capabilities};
