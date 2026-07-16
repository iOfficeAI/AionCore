use super::*;

impl TeamSessionService {
    pub(super) async fn build_team_response(&self, team: &Team) -> Result<TeamResponse, TeamError> {
        let mut agents = Vec::with_capacity(team.agents.len());
        for agent in &team.agents {
            agents.push(self.build_agent_response(agent).await?);
        }
        let source_metadata = match self.team_row_source_metadata(team) {
            Some(metadata) => Some(metadata),
            None => self.resolve_team_source_metadata(team).await?,
        };

        Ok(TeamResponse {
            id: team.id.clone(),
            name: team.name.clone(),
            workspace: team.workspace.clone(),
            workspace_mode: team.workspace_mode,
            workspace_leases: vec![],
            assistants: agents,
            leader_assistant_id: team.lead_agent_id.clone(),
            source_channel: source_metadata
                .as_ref()
                .and_then(|metadata| metadata.source_channel.clone()),
            source_channel_id: source_metadata
                .as_ref()
                .and_then(|metadata| metadata.source_channel_id.clone()),
            source_chat_id: source_metadata
                .as_ref()
                .and_then(|metadata| metadata.source_chat_id.clone()),
            source_user_id: source_metadata
                .as_ref()
                .and_then(|metadata| metadata.source_user_id.clone()),
            source_label: source_metadata
                .as_ref()
                .and_then(|metadata| metadata.source_label.clone()),
            created_from: source_metadata
                .as_ref()
                .and_then(|metadata| metadata.created_from.clone()),
            created_at: team.created_at,
            updated_at: team.updated_at,
        })
    }

    fn team_row_source_metadata(&self, team: &Team) -> Option<crate::provisioning::TeamSourceMetadata> {
        if team
            .source_channel
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            return None;
        }
        Some(crate::provisioning::TeamSourceMetadata {
            source_channel: team.source_channel.clone(),
            source_channel_id: team.source_channel_id.clone(),
            source_chat_id: team.source_chat_id.clone(),
            source_user_id: team.source_user_id.clone(),
            source_label: team.source_label.clone(),
            created_from: team.created_from.clone(),
        })
    }

    async fn resolve_team_source_metadata(
        &self,
        team: &Team,
    ) -> Result<Option<crate::provisioning::TeamSourceMetadata>, TeamError> {
        let lead = team
            .lead_agent_id
            .as_deref()
            .and_then(|lead_id| team.agents.iter().find(|agent| agent.slot_id == lead_id))
            .or_else(|| {
                team.agents
                    .iter()
                    .find(|agent| agent.role == crate::types::TeammateRole::Lead)
            })
            .or_else(|| team.agents.first());
        let Some(lead) = lead else {
            return Ok(None);
        };
        self.conversation_port
            .conversation_source_metadata(&lead.conversation_id)
            .await
    }

    pub(super) async fn build_agent_response(
        &self,
        agent: &TeamAgent,
    ) -> Result<aionui_api_types::TeamAgentResponse, TeamError> {
        let icon = self.resolve_agent_icon(agent).await?;
        let mut response = agent.to_response_with_icon(icon);
        response.pending_confirmations = self.pending_confirmation_count(&agent.conversation_id);
        Ok(response)
    }

    fn pending_confirmation_count(&self, conversation_id: &str) -> usize {
        self.task_manager
            .get_task(conversation_id)
            .map(|agent| agent.get_confirmations().len())
            .unwrap_or(0)
    }

    async fn resolve_agent_icon(&self, agent: &TeamAgent) -> Result<Option<String>, TeamError> {
        if let Some(assistant_id) = agent.assistant_id.as_deref()
            && let Some(definition) = self.assistant_definition_repo.get_by_assistant_id(assistant_id).await?
            && let Some(icon) = assistant_icon(
                definition.assistant_id.as_str(),
                &definition.avatar_type,
                definition.avatar_value.as_deref(),
            )
        {
            return Ok(Some(icon));
        }

        if let Some(row) = self
            .agent_metadata_repo
            .find_builtin_by_backend(agent.backend.as_str())
            .await?
            && row.icon.is_some()
        {
            return Ok(row.icon);
        }

        if agent.backend == "acp"
            && let Some(row) = self
                .agent_metadata_repo
                .find_builtin_by_backend(agent.model.as_str())
                .await?
        {
            return Ok(row.icon);
        }

        Ok(None)
    }
}

fn assistant_icon(assistant_id: &str, avatar_type: &str, avatar_value: Option<&str>) -> Option<String> {
    match avatar_type {
        "builtin_asset" | "user_asset" => avatar_value.map(|value| {
            if is_direct_avatar_url(value) {
                value.to_string()
            } else {
                format!("/api/assistants/{assistant_id}/avatar")
            }
        }),
        _ => None,
    }
}

fn is_direct_avatar_url(value: &str) -> bool {
    value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("data:")
        || value.starts_with("file://")
        || value.starts_with("/api/assistants/")
}
