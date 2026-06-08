//! Runtime capability modules shared across agent managers.
//!
//! These modules provide reusable primitives (CLI process supervision,
//! skill indexing, backend output/protocol sinks, first-message injection,
//! agent binary resolution, solo-team guide prompts) that any agent
//! implementation can compose.

pub(crate) mod agent_binary_resolver;
pub(crate) mod backend_output_sink;
pub(crate) mod backend_protocol_sink;
pub(crate) mod cli_process;
pub(crate) mod first_message_injector;
pub mod model_identity_reminder;
pub mod prompt_pipeline;
pub(crate) mod skill_manager;
pub(crate) mod team_guide_prompt;

pub use prompt_pipeline::{PostRecvHook, PreSendHook, PromptCtx, PromptPipeline};
