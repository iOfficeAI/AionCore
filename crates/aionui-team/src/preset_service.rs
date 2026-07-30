use std::sync::Arc;

use aionui_api_types::{
    CreateTeamPresetRequest, TeamPresetListResponse, TeamPresetMember, TeamPresetResponse, UpdateTeamPresetRequest,
};
use aionui_common::{generate_id, now_ms};
use aionui_db::models::{TeamPresetMemberRow, TeamPresetRow};
use aionui_db::{ITeamRepository, UpdateTeamPresetParams};

use crate::error::TeamError;

/// Domain service for persisted expert-team presets.
///
/// Presets are user-owned templates describing a team roster (leader + members)
/// and descriptive metadata. The service validates ownership on every
/// mutating read and maps between the typed API DTOs and the JSON-in-SQLite
/// repository rows.
pub struct TeamPresetService {
    repo: Arc<dyn ITeamRepository>,
}

impl TeamPresetService {
    pub fn new(repo: Arc<dyn ITeamRepository>) -> Self {
        Self { repo }
    }

    /// Create a new preset owned by `user_id`.
    pub async fn create_preset(
        &self,
        user_id: &str,
        req: CreateTeamPresetRequest,
    ) -> Result<TeamPresetResponse, TeamError> {
        validate_preset_request(&req)?;

        let id = generate_id();
        let now = now_ms();
        let row = TeamPresetRow {
            id: id.clone(),
            user_id: user_id.to_owned(),
            name: req.name,
            icon: req.icon,
            category: req.category,
            description: req.description,
            expertise_tags: serde_json::to_string(&req.expertise_tags)?,
            example_prompts: serde_json::to_string(&req.example_prompts)?,
            leader: serde_json::to_string(&member_to_row(&req.leader))?,
            members: serde_json::to_string(&req.members.iter().map(member_to_row).collect::<Vec<_>>())?,
            version: 1,
            created_at: now,
            updated_at: now,
        };

        self.repo.create_team_preset(&row).await?;
        row_to_response(row)
    }

    /// List presets for the authenticated user, newest first.
    pub async fn list_presets(&self, user_id: &str) -> Result<TeamPresetListResponse, TeamError> {
        let rows = self.repo.list_team_presets_by_user(user_id).await?;
        let mut responses = Vec::with_capacity(rows.len());
        for row in rows {
            responses.push(row_to_response(row)?);
        }
        Ok(responses)
    }

    /// Get a single preset if owned by `user_id`.
    pub async fn get_preset(&self, user_id: &str, preset_id: &str) -> Result<TeamPresetResponse, TeamError> {
        let row = self.load_owned_preset(user_id, preset_id).await?;
        row_to_response(row)
    }

    /// Update a preset owned by `user_id`.
    pub async fn update_preset(
        &self,
        user_id: &str,
        preset_id: &str,
        req: UpdateTeamPresetRequest,
    ) -> Result<TeamPresetResponse, TeamError> {
        let existing = self.load_owned_preset(user_id, preset_id).await?;
        let existing_leader = parse_leader(&existing)?;
        let existing_members = parse_members(&existing)?;
        let existing_tags = parse_tags(&existing)?;
        let existing_prompts = parse_prompts(&existing)?;

        if req.leader.is_some() || req.members.is_some() {
            let leader = req.leader.as_ref().unwrap_or(&existing_leader);
            let members = req.members.as_ref().unwrap_or(&existing_members);
            let combined = CreateTeamPresetRequest {
                name: req.name.clone().unwrap_or_else(|| existing.name.clone()),
                icon: req.icon.clone().or_else(|| existing.icon.clone()),
                category: req.category.clone().or_else(|| existing.category.clone()),
                description: req.description.clone().unwrap_or_else(|| existing.description.clone()),
                expertise_tags: req.expertise_tags.clone().unwrap_or(existing_tags.clone()),
                example_prompts: req.example_prompts.clone().unwrap_or(existing_prompts.clone()),
                leader: leader.clone(),
                members: members.clone(),
            };
            validate_preset_request(&combined)?;
        }

        let mut params = UpdateTeamPresetParams::default();
        if let Some(name) = req.name {
            params.name = Some(name);
        }
        if let Some(icon) = req.icon {
            params.icon = Some(icon);
        }
        if let Some(category) = req.category {
            params.category = Some(category);
        }
        if let Some(description) = req.description {
            params.description = Some(description);
        }
        if let Some(tags) = req.expertise_tags {
            params.expertise_tags = Some(serde_json::to_string(&tags)?);
        }
        if let Some(prompts) = req.example_prompts {
            params.example_prompts = Some(serde_json::to_string(&prompts)?);
        }
        if let Some(leader) = req.leader {
            params.leader = Some(serde_json::to_string(&member_to_row(&leader))?);
        }
        if let Some(members) = req.members {
            params.members = Some(serde_json::to_string(
                &members.iter().map(member_to_row).collect::<Vec<_>>(),
            )?);
        }

        self.repo.update_team_preset(preset_id, &params).await?;
        let updated = self.load_owned_preset(user_id, preset_id).await?;
        row_to_response(updated)
    }

    /// Delete a preset owned by `user_id`.
    pub async fn delete_preset(&self, user_id: &str, preset_id: &str) -> Result<(), TeamError> {
        // Ownership check.
        let _row = self.load_owned_preset(user_id, preset_id).await?;
        self.repo.delete_team_preset(preset_id).await?;
        Ok(())
    }

    async fn load_owned_preset(&self, user_id: &str, preset_id: &str) -> Result<TeamPresetRow, TeamError> {
        let row = self
            .repo
            .get_team_preset(preset_id)
            .await?
            .ok_or_else(|| TeamError::PresetNotFound(preset_id.into()))?;
        if row.user_id != user_id {
            return Err(TeamError::Forbidden(format!(
                "team preset {preset_id} is not owned by current user"
            )));
        }
        Ok(row)
    }
}

fn member_to_row(member: &TeamPresetMember) -> TeamPresetMemberRow {
    TeamPresetMemberRow {
        assistant_backend: member.assistant_backend.clone(),
        assistant_id: member.assistant_id.clone(),
        model: member.model.clone(),
        assistant_name: member.assistant_name.clone(),
        role: member.role.clone(),
        order: member.order,
    }
}

fn row_to_member(member: TeamPresetMemberRow) -> TeamPresetMember {
    TeamPresetMember {
        assistant_backend: member.assistant_backend,
        assistant_id: member.assistant_id,
        model: member.model,
        assistant_name: member.assistant_name,
        role: member.role,
        order: member.order,
    }
}

fn parse_leader(row: &TeamPresetRow) -> Result<TeamPresetMember, TeamError> {
    let db: TeamPresetMemberRow = serde_json::from_str(&row.leader)?;
    Ok(row_to_member(db))
}

fn parse_members(row: &TeamPresetRow) -> Result<Vec<TeamPresetMember>, TeamError> {
    let db: Vec<TeamPresetMemberRow> = serde_json::from_str(&row.members)?;
    Ok(db.into_iter().map(row_to_member).collect())
}

fn parse_tags(row: &TeamPresetRow) -> Result<Vec<String>, TeamError> {
    Ok(serde_json::from_str(&row.expertise_tags)?)
}

fn parse_prompts(row: &TeamPresetRow) -> Result<Vec<String>, TeamError> {
    Ok(serde_json::from_str(&row.example_prompts)?)
}

fn row_to_response(row: TeamPresetRow) -> Result<TeamPresetResponse, TeamError> {
    let expertise_tags = parse_tags(&row)?;
    let example_prompts = parse_prompts(&row)?;
    let leader = parse_leader(&row)?;
    let members = parse_members(&row)?;
    Ok(TeamPresetResponse {
        id: row.id,
        user_id: row.user_id,
        name: row.name,
        icon: row.icon,
        category: row.category,
        description: row.description,
        expertise_tags,
        example_prompts,
        leader,
        members,
        version: row.version,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn validate_preset_request(req: &CreateTeamPresetRequest) -> Result<(), TeamError> {
    if req.name.trim().is_empty() {
        return Err(TeamError::InvalidRequest("preset name is required".into()));
    }
    if req.leader.assistant_backend.trim().is_empty() {
        return Err(TeamError::InvalidRequest(
            "preset leader assistant_backend is required".into(),
        ));
    }
    if req.leader.assistant_name.trim().is_empty() {
        return Err(TeamError::InvalidRequest(
            "preset leader assistant_name is required".into(),
        ));
    }
    if req.leader.role.trim().is_empty() {
        return Err(TeamError::InvalidRequest("preset leader role is required".into()));
    }

    let leader_in_members = req.members.iter().any(|member| {
        member.assistant_backend == req.leader.assistant_backend
            && member.assistant_id == req.leader.assistant_id
            && member.role == req.leader.role
    });
    if !leader_in_members {
        return Err(TeamError::InvalidRequest(
            "preset leader must be included in members".into(),
        ));
    }

    let mut orders: Vec<i64> = req.members.iter().map(|member| member.order).collect();
    orders.sort();
    for (index, order) in orders.iter().enumerate() {
        if *order != index as i64 {
            return Err(TeamError::InvalidRequest(
                "preset member orders must be contiguous starting at 0".into(),
            ));
        }
    }

    Ok(())
}
