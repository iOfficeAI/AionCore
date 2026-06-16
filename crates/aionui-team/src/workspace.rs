use std::sync::Arc;

use aionui_common::{WorkspacePathValidationError, validate_workspace_path_availability};
use aionui_db::models::TeamRow;
use aionui_db::{ITeamRepository, UpdateTeamParams};
use tracing::warn;

use crate::error::TeamError;
use crate::provisioning::TeamConversationProvisioningPort;
use crate::types::{Team, TeammateRole};

pub(crate) fn validate_create_workspace_path(workspace: &str) -> Result<String, TeamError> {
    validate_workspace_path_availability(workspace).map_err(|error| match error {
        WorkspacePathValidationError::Empty => TeamError::InvalidRequest("Workspace directory is empty".into()),
        WorkspacePathValidationError::DoesNotExist(path)
        | WorkspacePathValidationError::NotDirectory(path)
        | WorkspacePathValidationError::NotAccessible { path, .. } => TeamError::WorkspacePathUnavailable(path),
    })
}

fn validate_runtime_workspace_path(workspace: &str) -> Result<String, TeamError> {
    validate_workspace_path_availability(workspace).map_err(|error| match error {
        WorkspacePathValidationError::Empty => TeamError::InvalidRequest("Team workspace is empty".into()),
        WorkspacePathValidationError::DoesNotExist(path)
        | WorkspacePathValidationError::NotDirectory(path)
        | WorkspacePathValidationError::NotAccessible { path, .. } => TeamError::WorkspacePathRuntimeUnavailable(path),
    })
}

fn usable_runtime_workspace(workspace: &str) -> Option<String> {
    validate_runtime_workspace_path(workspace).ok()
}

pub(crate) struct TeamWorkspaceResolver {
    repo: Arc<dyn ITeamRepository>,
    conversation_port: Arc<dyn TeamConversationProvisioningPort>,
}

impl TeamWorkspaceResolver {
    pub(crate) fn new(
        repo: Arc<dyn ITeamRepository>,
        conversation_port: Arc<dyn TeamConversationProvisioningPort>,
    ) -> Self {
        Self {
            repo,
            conversation_port,
        }
    }

    pub(crate) async fn resolve_for_new_agent(&self, row: &TeamRow, team: &Team) -> Result<String, TeamError> {
        if let Some(workspace) = usable_runtime_workspace(row.workspace.trim()) {
            return Ok(workspace);
        }

        if let Some(leader_workspace) = self.resolve_from_leader(team).await? {
            self.write_team_workspace(&row.id, &leader_workspace).await?;
            warn!(
                team_id = %row.id,
                workspace_source = "leader_conversation",
                "team workspace lazy backfilled"
            );
            return Ok(leader_workspace);
        }

        let workspace = self.conversation_port.create_team_temp_workspace(&row.id).await?;
        let workspace = validate_runtime_workspace_path(&workspace)?;
        self.write_team_workspace(&row.id, &workspace).await?;
        self.patch_leader_workspace_best_effort(&row.id, team, &workspace).await;
        warn!(
            team_id = %row.id,
            workspace_source = "team_temp_fallback",
            "team workspace lazy backfilled"
        );
        Ok(workspace)
    }

    async fn resolve_from_leader(&self, team: &Team) -> Result<Option<String>, TeamError> {
        let Some(leader) = team
            .agents
            .iter()
            .find(|agent| Some(&agent.slot_id) == team.lead_agent_id.as_ref())
            .or_else(|| team.agents.iter().find(|agent| agent.role == TeammateRole::Lead))
        else {
            return Ok(None);
        };
        let Some(workspace) = self
            .conversation_port
            .conversation_workspace(&leader.conversation_id)
            .await?
        else {
            return Ok(None);
        };
        Ok(usable_runtime_workspace(workspace.trim()))
    }

    async fn write_team_workspace(&self, team_id: &str, workspace: &str) -> Result<(), TeamError> {
        self.repo
            .update_team(
                team_id,
                &UpdateTeamParams {
                    workspace: Some(workspace.to_owned()),
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }

    async fn patch_leader_workspace_best_effort(&self, team_id: &str, team: &Team, workspace: &str) {
        let Some(leader) = team
            .agents
            .iter()
            .find(|agent| Some(&agent.slot_id) == team.lead_agent_id.as_ref())
            .or_else(|| team.agents.iter().find(|agent| agent.role == TeammateRole::Lead))
        else {
            return;
        };
        if let Err(e) = self
            .conversation_port
            .patch_runtime_config(&leader.conversation_id, serde_json::json!({ "workspace": workspace }))
            .await
        {
            warn!(
                team_id = %team_id,
                conversation_id = %leader.conversation_id,
                error = %e,
                "failed to patch leader workspace after team temp fallback"
            );
        }
    }
}
