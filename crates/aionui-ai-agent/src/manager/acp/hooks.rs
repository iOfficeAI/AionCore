//! Built-in `PreSendHook`s for the ACP prompt pipeline.
//!
//! Each hook reads a one-shot flag on `AcpSession` (or a `pending_*`
//! field), consumes it, and prepends its block to the prompt. Failures
//! are reported via `ctx.runtime.emit(AgentStreamEvent::AcpPromptHookWarning(..))`
//! and the prompt is returned in a gracefully-degraded form.

use crate::capability::first_message_injector::{InjectionConfig, inject_first_message_prefix};
use crate::capability::prompt_pipeline::{PreSendHook, PromptCtx};
use crate::protocol::events::AgentStreamEvent;
use aionui_api_types::AcpPromptHookWarningPayload;

#[derive(Default)]
pub struct SessionNewPreludeHook;

#[async_trait::async_trait]
impl PreSendHook for SessionNewPreludeHook {
    async fn pre_send(&self, ctx: &mut PromptCtx<'_>, prompt: String) -> String {
        if !ctx.session.take_pending_session_new_prelude() {
            return prompt;
        }

        let metadata = &ctx.params.metadata;
        let config = InjectionConfig {
            preset_context: ctx.params.preset_context.as_deref(),
            skills: &ctx.params.config.skills,
            custom_workspace: ctx.params.workspace.is_custom,
            native_skill_support: metadata
                .native_skills_dirs
                .as_ref()
                .is_some_and(|v: &Vec<String>| !v.is_empty()),
        };

        // inject_first_message_prefix currently swallows I/O errors and
        // downgrades internally; any failure surfaces as an unchanged
        // prompt. Wrap a catch_unwind-style boundary so once we add
        // explicit failure signalling, this hook stays the policy owner.
        inject_first_message_prefix(&prompt, ctx.skill_manager, config).await
    }
}

/// Emit a non-blocking toast warning back to the UI via the stream
/// channel. Used by hook adapters when their underlying helper fails
/// but the pipeline must keep the prompt flowing.
#[allow(dead_code)] // Seed for future hook-failure surfacing; Task 7's ignored skeleton unlocks this.
pub(crate) fn emit_hook_warning(ctx: &PromptCtx<'_>, hook: &'static str, message: impl Into<String>) {
    let payload = AcpPromptHookWarningPayload {
        hook: hook.to_owned(),
        message: message.into(),
    };
    let value = serde_json::to_value(payload).unwrap_or(serde_json::Value::Null);
    ctx.runtime.emit(AgentStreamEvent::AcpPromptHookWarning(value));
}

#[cfg(test)]
mod tests {
    //! Full-path hook tests live in tests/prompt_pipeline_integration.rs
    //! where a real AcpSession + AcpSessionParams + AgentRuntime triple
    //! is already wired for assertion. This module keeps unit-level
    //! property checks around the helpers that don't need ctx.
    use super::*;

    #[test]
    fn emit_hook_warning_payload_shape() {
        let payload = AcpPromptHookWarningPayload {
            hook: "session_new_prelude".into(),
            message: "boom".into(),
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert_eq!(v["hook"], "session_new_prelude");
        assert_eq!(v["message"], "boom");
    }
}
