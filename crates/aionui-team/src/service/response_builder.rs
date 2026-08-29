use super::*;

impl TeamSessionService {
    pub(super) async fn build_team_response(&self, user_id: &str, team: &Team) -> Result<TeamResponse, TeamError> {
        let mut agents = Vec::with_capacity(team.agents.len());
        for agent in &team.agents {
            agents.push(self.build_agent_response(user_id, &team.id, agent).await?);
        }

        Ok(TeamResponse {
            id: team.id.clone(),
            name: team.name.clone(),
            workspace: team.workspace.clone(),
            assistants: agents,
            leader_assistant_id: team.lead_agent_id.clone(),
            created_at: team.created_at,
            updated_at: team.updated_at,
        })
    }

    pub(super) async fn build_agent_response(
        &self,
        user_id: &str,
        team_id: &str,
        agent: &TeamAgent,
    ) -> Result<aionui_api_types::TeamAgentResponse, TeamError> {
        let icon = self.resolve_agent_icon(user_id, agent).await?;
        let mut response = agent.to_response_with_icon(icon);
        response.pending_confirmations = self.pending_confirmation_count(&agent.conversation_id);
        response.context_reset = self.context_reset_capability(user_id, team_id, agent).await?;
        Ok(response)
    }

    pub(super) async fn context_reset_capability(
        &self,
        user_id: &str,
        team_id: &str,
        agent: &TeamAgent,
    ) -> Result<aionui_api_types::TeamContextResetCapability, TeamError> {
        let session = self.sessions.get(team_id).map(|entry| Arc::clone(&entry.session));
        self.context_reset_capability_for_session(user_id, agent, session.as_deref())
            .await
    }

    pub(super) async fn context_reset_capability_for_session(
        &self,
        user_id: &str,
        agent: &TeamAgent,
        session: Option<&TeamSession>,
    ) -> Result<aionui_api_types::TeamContextResetCapability, TeamError> {
        use aionui_api_types::{TeamContextResetAvailability as Availability, TeamContextResetCapability};

        if agent.role == TeammateRole::Lead {
            return Ok(TeamContextResetCapability {
                supported: false,
                availability: Availability::LeaderNotTargetable,
            });
        }

        if !self
            .conversation_port
            .supports_context_reset(user_id, &agent.conversation_id)
            .await?
        {
            return Ok(TeamContextResetCapability {
                supported: false,
                availability: Availability::Unsupported,
            });
        }

        let Some(session) = session else {
            return Ok(TeamContextResetCapability {
                supported: true,
                availability: Availability::SessionStopped,
            });
        };

        let availability = match session.member_runtimes().snapshot(&agent.slot_id) {
            MemberRuntimeSnapshot::Absent => Availability::Dormant,
            MemberRuntimeSnapshot::Attaching { .. } => Availability::Initializing,
            MemberRuntimeSnapshot::Failed { .. } => Availability::Failed,
            MemberRuntimeSnapshot::Removing { .. } => Availability::Removing,
            MemberRuntimeSnapshot::SessionStopped => Availability::SessionStopped,
            MemberRuntimeSnapshot::Ready => {
                let work = session.work_coordinator().slot_snapshot(&agent.slot_id);
                if work.is_some_and(|slot| {
                    slot.active_batch.is_some() || slot.queued_foreground_count > 0 || slot.queued_background_count > 0
                }) {
                    Availability::Busy
                } else {
                    Availability::Ready
                }
            }
        };
        Ok(TeamContextResetCapability {
            supported: true,
            availability,
        })
    }

    fn pending_confirmation_count(&self, conversation_id: &str) -> usize {
        self.task_manager
            .get_task(conversation_id)
            .map(|agent| agent.get_confirmations().len())
            .unwrap_or(0)
    }

    async fn resolve_agent_icon(&self, user_id: &str, agent: &TeamAgent) -> Result<Option<String>, TeamError> {
        if let Some(assistant_id) = agent.assistant_id.as_deref()
            && let Some(definition) = self
                .assistant_definition_repo
                .get_by_assistant_id_for_user(user_id, assistant_id)
                .await?
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
            .find_builtin_by_backend_for_user(user_id, agent.backend.as_str())
            .await?
            && row.icon.is_some()
        {
            return Ok(row.icon);
        }

        if agent.backend == "acp"
            && let Some(row) = self
                .agent_metadata_repo
                .find_builtin_by_backend_for_user(user_id, agent.model.as_str())
                .await?
        {
            return Ok(row.icon);
        }

        Ok(None)
    }
}

fn assistant_icon(assistant_id: &str, avatar_type: &str, avatar_value: Option<&str>) -> Option<String> {
    aionui_api_types::assistant_avatar_response_value(avatar_type, avatar_value, assistant_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_icon_routes_user_asset_through_backend_even_without_value() {
        assert_eq!(
            assistant_icon("assistant-1", "user_asset", None).as_deref(),
            Some("/api/assistants/assistant-1/avatar")
        );
    }

    #[test]
    fn assistant_icon_does_not_pass_through_direct_asset_values() {
        assert_eq!(
            assistant_icon("assistant-1", "user_asset", Some("data:image/png;base64,abc")).as_deref(),
            Some("/api/assistants/assistant-1/avatar")
        );
        assert_eq!(
            assistant_icon("assistant-1", "user_asset", Some("https://example.invalid/avatar.png")).as_deref(),
            Some("/api/assistants/assistant-1/avatar")
        );
    }

    #[test]
    fn assistant_icon_returns_none_for_none_avatar_type() {
        assert_eq!(assistant_icon("assistant-1", "none", None), None);
    }
}
