//! Channel A: the RUNTIME CONSUMPTION side of skills.
//!
//! An agent that can execute commands reaches this through
//! `aioncore skills list|show|cat`, which is a normal tool call rather than the
//! text-protocol round trip channel B needs. Channel B stays as the fallback for
//! agents that cannot run commands at all (plan mode, read-only, cron), and the
//! agent picks -- we deliberately do not try to predict which, because permission
//! mode is agent-side runtime state no CLI capability query reveals.
//!
//! Deliberately NOT merged with `config skills *` in `aionui-extension`, which
//! lists every importable skill and can write. This domain is read-only and
//! scoped to one conversation's snapshot. Different semantics, different
//! authority; merging them would let a conversation-scoped runtime token reach
//! the installation-wide management surface.
//!
//! It also cannot LIVE in `aionui-extension`: runtime-token validation needs
//! `aionui-ai-agent`, which sits above the extension crate, so the dependency
//! would invert the layering.

pub mod error;
pub mod routes;
pub mod service;
pub mod state;

pub use error::SkillRuntimeError;
pub use routes::skill_runtime_routes;
pub use service::SkillRuntimeService;
pub use state::SkillRuntimeRouterState;
