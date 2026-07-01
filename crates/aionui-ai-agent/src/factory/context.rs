//! Workspace information shared across factory builders. The conversation
//! domain has already decoded raw DB state into typed context before this
//! layer sees it.

use crate::error::AgentError;
use crate::session_context::AgentSessionContext;

pub(super) struct FactoryContext {
    pub conversation_id: String,
    pub workspace: String,
    pub is_custom_workspace: bool,
    pub runtime_env: Vec<(String, String)>,
}

impl FactoryContext {
    pub async fn resolve(context: &AgentSessionContext) -> Result<Self, AgentError> {
        Ok(Self {
            conversation_id: context.conversation.conversation_id.clone(),
            workspace: context.workspace.path.clone(),
            is_custom_workspace: context.workspace.is_custom,
            runtime_env: context.runtime_env.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_context::{
        AcpSessionBuildContext, AgentSessionContext, AgentSessionKind, ConversationContext, WorkspaceContext,
    };
    use aionui_common::{AgentType, ProviderWithModel};

    #[tokio::test]
    async fn resolve_preserves_runtime_env() {
        let context = AgentSessionContext {
            conversation: ConversationContext {
                conversation_id: "conv-1".into(),
                user_id: "user-1".into(),
                agent_type: AgentType::Acp,
                source: None,
            },
            workspace: WorkspaceContext {
                path: "/tmp/workspace".into(),
                stored_path: "/tmp/workspace".into(),
                is_custom: true,
            },
            model: ProviderWithModel {
                provider_id: "provider".into(),
                model: "model".into(),
                use_model: None,
            },
            skills: vec![],
            runtime_env: vec![("AIONUI_USER_ID".into(), "user-1".into())],
            team: None,
            kind: AgentSessionKind::Acp(Box::new(AcpSessionBuildContext {
                config: Default::default(),
                team: None,
                belongs_to_team: false,
                session_id: None,
                session_snapshot: None,
            })),
        };

        let ctx = FactoryContext::resolve(&context).await.unwrap();

        assert_eq!(
            ctx.runtime_env,
            vec![("AIONUI_USER_ID".to_owned(), "user-1".to_owned())]
        );
    }
}
