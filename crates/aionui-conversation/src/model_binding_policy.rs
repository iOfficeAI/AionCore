//! Model-binding rules for conversation runtimes.
//!
//! `conversation.model` is the canonical durable provider/model binding. Most
//! ACP agents own their model configuration through ACP runtime options, while
//! Aion-managed Hermes needs this binding before its process is spawned.

use aionui_common::{AgentType, ProviderWithModel};

use crate::ConversationError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConversationModelPolicy {
    /// Aionrs owns its provider/model through `conversation.model`.
    Optional,
    /// Aion-managed builtin Hermes cannot start without a provider/model
    /// binding. The caller must provide one at conversation creation.
    Required,
    /// Other ACP implementations own model selection through their native
    /// runtime configuration and must not write the conversation binding.
    Forbidden,
}

pub(crate) fn for_agent(
    agent_type: AgentType,
    agent_source: Option<&str>,
    runtime_backend: Option<&str>,
) -> ConversationModelPolicy {
    if agent_type == AgentType::Aionrs {
        return ConversationModelPolicy::Optional;
    }

    if agent_type == AgentType::Acp
        && agent_source.is_some_and(|source| source.trim() == "builtin")
        && runtime_backend.is_some_and(|backend| backend.trim() == "hermes")
    {
        ConversationModelPolicy::Required
    } else {
        ConversationModelPolicy::Forbidden
    }
}

pub(crate) fn validate_required_binding(model: Option<&ProviderWithModel>) -> Result<(), ConversationError> {
    let Some(model) = model else {
        return Err(ConversationError::HermesProviderRequired);
    };

    if model.provider_id.trim().is_empty() {
        return Err(ConversationError::HermesProviderRequired);
    }

    let selected_model = model
        .use_model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| model.model.trim());
    if selected_model.is_empty() {
        return Err(ConversationError::HermesModelRequired);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(provider_id: &str, model: &str, use_model: Option<&str>) -> ProviderWithModel {
        ProviderWithModel {
            provider_id: provider_id.to_owned(),
            model: model.to_owned(),
            use_model: use_model.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn builtin_hermes_requires_a_binding() {
        assert_eq!(
            for_agent(AgentType::Acp, Some("builtin"), Some("hermes")),
            ConversationModelPolicy::Required
        );
    }

    #[test]
    fn non_builtin_hermes_cannot_unlock_the_binding() {
        assert_eq!(
            for_agent(AgentType::Acp, Some("custom"), Some("hermes")),
            ConversationModelPolicy::Forbidden
        );
    }

    #[test]
    fn required_binding_needs_provider_and_model() {
        assert!(matches!(
            validate_required_binding(None),
            Err(ConversationError::HermesProviderRequired)
        ));
        assert!(matches!(
            validate_required_binding(Some(&binding("", "model", None))),
            Err(ConversationError::HermesProviderRequired)
        ));
        assert!(matches!(
            validate_required_binding(Some(&binding("provider", "", None))),
            Err(ConversationError::HermesModelRequired)
        ));
        assert!(validate_required_binding(Some(&binding("provider", "", Some("model")))).is_ok());
    }
}
