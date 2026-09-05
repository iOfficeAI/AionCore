//! Skill Evolution service — WorkMate-native WikiSkill gate objects.
//!
//! Stores experience summaries + draft SKILL.md proposals. Does not inject
//! experience into inference prompts. Does not vendor community wikiskill CLI.

use std::sync::Arc;

use crate::error::AssistantError;
use aionui_api_types::{
    ApproveSkillEvolutionResponse, CreateExperienceArticleRequest, CreateSkillEvolutionProposalRequest,
    ExperienceArticleResponse, ReviewSkillEvolutionRequest, SkillEvolutionAction, SkillEvolutionExportPayload,
    SkillEvolutionProposalResponse, SkillEvolutionStatus,
};
use aionui_common::{generate_prefixed_id, now_ms};
use aionui_db::{
    CreateExperienceArticleParams, CreateSkillEvolutionProposalParams, IConversationRepository,
    IExperienceArticleRepository, ISkillEvolutionProposalRepository, SkillEvolutionProposalRow,
    UpdateSkillEvolutionProposalParams,
};

fn redact_secrets(input: &str) -> String {
    let mut out = input.to_string();
    // Lightweight MVP redaction (no regex dependency): mask common secret prefixes.
    for needle in ["sk-", "SK-", "Bearer ", "bearer ", "api_key=", "api-key=", "API_KEY="] {
        if let Some(idx) = out.find(needle) {
            let start = idx;
            let rest = &out[start + needle.len()..];
            let end_rel = rest
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',')
                .unwrap_or(rest.len().min(64));
            let end = start + needle.len() + end_rel;
            out.replace_range(start..end, "[REDACTED]");
        }
    }
    if out.contains("PRIVATE KEY-----") {
        out = "[REDACTED_PRIVATE_KEY]".to_string();
    }
    out
}

fn parse_status(raw: &str) -> Result<SkillEvolutionStatus, AssistantError> {
    match raw {
        "draft" => Ok(SkillEvolutionStatus::Draft),
        "pending_review" => Ok(SkillEvolutionStatus::PendingReview),
        "approved" => Ok(SkillEvolutionStatus::Approved),
        "rejected" => Ok(SkillEvolutionStatus::Rejected),
        "applied" => Ok(SkillEvolutionStatus::Applied),
        "rolled_back" => Ok(SkillEvolutionStatus::RolledBack),
        other => Err(AssistantError::Internal(format!("unknown proposal status: {other}"))),
    }
}

fn parse_action(raw: &str) -> Result<SkillEvolutionAction, AssistantError> {
    match raw {
        "create" => Ok(SkillEvolutionAction::Create),
        "patch" => Ok(SkillEvolutionAction::Patch),
        other => Err(AssistantError::Internal(format!("unknown proposal action: {other}"))),
    }
}

fn parse_json_string_array(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

fn row_to_response(row: SkillEvolutionProposalRow) -> Result<SkillEvolutionProposalResponse, AssistantError> {
    Ok(SkillEvolutionProposalResponse {
        id: row.id,
        assistant_id: row.assistant_id,
        conversation_id: row.conversation_id,
        status: parse_status(&row.status)?,
        title: row.title,
        experience_summary: row.experience_summary,
        experience_article_ids: parse_json_string_array(&row.experience_article_ids),
        action: parse_action(&row.action)?,
        target_skill_key: row.target_skill_key,
        draft_skill_md: row.draft_skill_md,
        draft_diff_summary: row.draft_diff_summary,
        reviewer_user_id: row.reviewer_user_id,
        review_comment: row.review_comment,
        reviewed_at: row.reviewed_at,
        applied_skill_key: row.applied_skill_key,
        applied_skill_version: row.applied_skill_version,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn slugify_skill_key(title: &str) -> String {
    let mut out = String::new();
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if (ch == '-' || ch == '_' || ch.is_whitespace()) && !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "evolved-skill".to_string()
    } else {
        format!("evolved-{trimmed}")
    }
}

fn stub_skill_md(skill_key: &str, title: &str, summary: &str, conversation_id: Option<&str>) -> String {
    let conv = conversation_id.unwrap_or("n/a");
    format!(
        "---\nname: {skill_key}\ndescription: {title}\nversion: 0.1.0\nsource: workmate-skill-evolution\n---\n\n# {title}\n\n> 由 CSBU WorkMate「技能进化」从会话经验提炼的草案。请人工审核后再发布并 pin。\n\n## 经验摘要\n\n{summary}\n\n## 来源\n\n- conversation_id: `{conv}`\n\n## 使用指引\n\n1. 确认本技能适用范围与边界。\n2. 在智能体中心试跑验证。\n3. 发布时 pin 技能版本。\n"
    )
}

pub struct SkillEvolutionService {
    proposals: Arc<dyn ISkillEvolutionProposalRepository>,
    experience: Arc<dyn IExperienceArticleRepository>,
    conversations: Arc<dyn IConversationRepository>,
}

impl SkillEvolutionService {
    pub fn new(
        proposals: Arc<dyn ISkillEvolutionProposalRepository>,
        experience: Arc<dyn IExperienceArticleRepository>,
        conversations: Arc<dyn IConversationRepository>,
    ) -> Self {
        Self {
            proposals,
            experience,
            conversations,
        }
    }

    pub async fn create_proposal(
        &self,
        user_id: &str,
        req: CreateSkillEvolutionProposalRequest,
    ) -> Result<SkillEvolutionProposalResponse, AssistantError> {
        if req.title.trim().is_empty() {
            return Err(AssistantError::BadRequest("title is required".into()));
        }

        if let Some(ref cid) = req.conversation_id {
            let found = self
                .conversations
                .get(user_id, cid)
                .await
                .map_err(|e| AssistantError::Internal(e.to_string()))?;
            if found.is_none() {
                return Err(AssistantError::NotFound(format!("conversation {cid}")));
            }
        }

        let action = req.action.unwrap_or(SkillEvolutionAction::Create);
        let action_str = match action {
            SkillEvolutionAction::Create => "create",
            SkillEvolutionAction::Patch => "patch",
        };
        if matches!(action, SkillEvolutionAction::Patch)
            && req.target_skill_key.as_ref().is_none_or(|s| s.trim().is_empty())
        {
            return Err(AssistantError::BadRequest(
                "target_skill_key is required for patch action".into(),
            ));
        }

        let summary = redact_secrets(req.experience_summary.as_deref().unwrap_or("").trim());
        let skill_key = req
            .target_skill_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| slugify_skill_key(&req.title));

        let draft = if let Some(md) = req.draft_skill_md.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            redact_secrets(md)
        } else if req.auto_stub {
            stub_skill_md(
                &skill_key,
                &req.title,
                if summary.is_empty() {
                    "（待补充经验摘要）"
                } else {
                    &summary
                },
                req.conversation_id.as_deref(),
            )
        } else {
            return Err(AssistantError::BadRequest(
                "draft_skill_md is required when auto_stub is false".into(),
            ));
        };

        let status = if req.submit { "pending_review" } else { "draft" };
        let id = generate_prefixed_id("sep");
        let row = self
            .proposals
            .create(&CreateSkillEvolutionProposalParams {
                id: &id,
                owner_user_id: user_id,
                assistant_id: req.assistant_id.as_deref(),
                conversation_id: req.conversation_id.as_deref(),
                status,
                title: req.title.trim(),
                experience_summary: &summary,
                experience_article_ids: "[]",
                action: action_str,
                target_skill_key: Some(skill_key.as_str()),
                draft_skill_md: &draft,
                draft_diff_summary: req.draft_diff_summary.as_deref(),
            })
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;

        row_to_response(row)
    }

    pub async fn list_proposals(
        &self,
        user_id: &str,
        status: Option<&str>,
        assistant_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SkillEvolutionProposalResponse>, AssistantError> {
        let rows = self
            .proposals
            .list_for_owner(user_id, status, assistant_id, limit)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;
        rows.into_iter().map(row_to_response).collect()
    }

    pub async fn get_proposal(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<SkillEvolutionProposalResponse, AssistantError> {
        let row = self.require_owned_proposal(user_id, id).await?;
        row_to_response(row)
    }

    pub async fn submit(&self, user_id: &str, id: &str) -> Result<SkillEvolutionProposalResponse, AssistantError> {
        let row = self.require_owned_proposal(user_id, id).await?;
        if row.status != "draft" {
            return Err(AssistantError::BadRequest(
                "only draft proposals can be submitted".into(),
            ));
        }
        let updated = self
            .proposals
            .update(
                id,
                &UpdateSkillEvolutionProposalParams {
                    status: Some("pending_review"),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(id.to_owned()))?;
        row_to_response(updated)
    }

    pub async fn approve(
        &self,
        user_id: &str,
        id: &str,
        req: ReviewSkillEvolutionRequest,
    ) -> Result<ApproveSkillEvolutionResponse, AssistantError> {
        let row = self.require_owned_proposal(user_id, id).await?;
        if row.status != "pending_review" && row.status != "draft" {
            return Err(AssistantError::BadRequest(
                "only draft/pending_review proposals can be approved".into(),
            ));
        }
        let skill_key = req
            .applied_skill_key
            .as_deref()
            .or(row.target_skill_key.as_deref())
            .filter(|s| !s.is_empty())
            .unwrap_or("evolved-skill")
            .to_string();
        let version = req.applied_skill_version.clone().unwrap_or_else(|| "0.1.0".to_string());
        let now = now_ms();
        let updated = self
            .proposals
            .update(
                id,
                &UpdateSkillEvolutionProposalParams {
                    status: Some("approved"),
                    reviewer_user_id: Some(user_id),
                    review_comment: req.comment.as_deref(),
                    reviewed_at: Some(now),
                    applied_skill_key: Some(skill_key.as_str()),
                    applied_skill_version: Some(version.as_str()),
                    previous_skill_md: row.previous_skill_md.as_deref(),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(id.to_owned()))?;

        let export = SkillEvolutionExportPayload {
            skill_key: skill_key.clone(),
            skill_md: updated.draft_skill_md.clone(),
            suggested_path: format!(".csbu-workmate/skills/{skill_key}/SKILL.md"),
        };
        Ok(ApproveSkillEvolutionResponse {
            proposal: row_to_response(updated)?,
            export,
        })
    }

    pub async fn reject(
        &self,
        user_id: &str,
        id: &str,
        req: ReviewSkillEvolutionRequest,
    ) -> Result<SkillEvolutionProposalResponse, AssistantError> {
        let row = self.require_owned_proposal(user_id, id).await?;
        if row.status != "pending_review" && row.status != "draft" {
            return Err(AssistantError::BadRequest(
                "only draft/pending_review proposals can be rejected".into(),
            ));
        }
        let now = now_ms();
        let comment = req.comment.clone().unwrap_or_default();
        let article_id = generate_prefixed_id("ea");
        let body = format!(
            "## 被拒提案\n\n- proposal_id: `{}`\n- title: {}\n\n### 审核意见\n\n{}\n\n### 经验摘要\n\n{}\n",
            row.id, row.title, comment, row.experience_summary
        );
        let article = self
            .experience
            .create(&CreateExperienceArticleParams {
                id: &article_id,
                owner_user_id: user_id,
                assistant_id: row.assistant_id.as_deref(),
                team_id: None,
                kind: "rejected_note",
                title: &format!("拒绝：{}", row.title),
                body_md: &body,
                source_conversation_ids: &serde_json::to_string(
                    &row.conversation_id.iter().cloned().collect::<Vec<_>>(),
                )
                .unwrap_or_else(|_| "[]".into()),
                tags: r#"["skill-evolution","rejected"]"#,
                status: "active",
            })
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;

        let mut article_ids = parse_json_string_array(&row.experience_article_ids);
        article_ids.push(article.id);
        let article_ids_json = serde_json::to_string(&article_ids).unwrap_or_else(|_| "[]".into());

        let updated = self
            .proposals
            .update(
                id,
                &UpdateSkillEvolutionProposalParams {
                    status: Some("rejected"),
                    reviewer_user_id: Some(user_id),
                    review_comment: req.comment.as_deref(),
                    reviewed_at: Some(now),
                    experience_article_ids: Some(article_ids_json.as_str()),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(id.to_owned()))?;
        row_to_response(updated)
    }

    pub async fn apply(&self, user_id: &str, id: &str) -> Result<ApproveSkillEvolutionResponse, AssistantError> {
        let row = self.require_owned_proposal(user_id, id).await?;
        if row.status != "approved" && row.status != "applied" {
            return Err(AssistantError::BadRequest(
                "only approved proposals can be applied".into(),
            ));
        }
        let skill_key = row
            .applied_skill_key
            .clone()
            .or(row.target_skill_key.clone())
            .unwrap_or_else(|| "evolved-skill".into());
        let version = row.applied_skill_version.clone().unwrap_or_else(|| "0.1.0".into());
        let updated = self
            .proposals
            .update(
                id,
                &UpdateSkillEvolutionProposalParams {
                    status: Some("applied"),
                    applied_skill_key: Some(skill_key.as_str()),
                    applied_skill_version: Some(version.as_str()),
                    previous_skill_md: row.previous_skill_md.as_deref(),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(id.to_owned()))?;
        let export = SkillEvolutionExportPayload {
            skill_key: skill_key.clone(),
            skill_md: updated.draft_skill_md.clone(),
            suggested_path: format!(".csbu-workmate/skills/{skill_key}/SKILL.md"),
        };
        Ok(ApproveSkillEvolutionResponse {
            proposal: row_to_response(updated)?,
            export,
        })
    }

    pub async fn rollback(
        &self,
        user_id: &str,
        id: &str,
        req: ReviewSkillEvolutionRequest,
    ) -> Result<SkillEvolutionProposalResponse, AssistantError> {
        let row = self.require_owned_proposal(user_id, id).await?;
        if row.status != "applied" && row.status != "approved" {
            return Err(AssistantError::BadRequest(
                "only approved/applied proposals can be rolled back".into(),
            ));
        }
        let now = now_ms();
        let updated = self
            .proposals
            .update(
                id,
                &UpdateSkillEvolutionProposalParams {
                    status: Some("rolled_back"),
                    reviewer_user_id: Some(user_id),
                    review_comment: req.comment.as_deref(),
                    reviewed_at: Some(now),
                    // Keep applied_* for audit; status conveys rollback.
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(id.to_owned()))?;
        row_to_response(updated)
    }

    pub async fn list_experience(
        &self,
        user_id: &str,
        assistant_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ExperienceArticleResponse>, AssistantError> {
        let rows = self
            .experience
            .list_for_owner(user_id, assistant_id, limit)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|row| ExperienceArticleResponse {
                id: row.id,
                assistant_id: row.assistant_id,
                kind: row.kind,
                title: row.title,
                body_md: row.body_md,
                source_conversation_ids: parse_json_string_array(&row.source_conversation_ids),
                tags: parse_json_string_array(&row.tags),
                status: row.status,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
            .collect())
    }

    pub async fn create_experience(
        &self,
        user_id: &str,
        req: CreateExperienceArticleRequest,
    ) -> Result<ExperienceArticleResponse, AssistantError> {
        if req.title.trim().is_empty() {
            return Err(AssistantError::BadRequest("title is required".into()));
        }
        let id = generate_prefixed_id("ea");
        let kind = req.kind.as_deref().unwrap_or("general");
        let body = redact_secrets(req.body_md.as_deref().unwrap_or(""));
        let source = serde_json::to_string(req.source_conversation_ids.as_deref().unwrap_or(&[]))
            .unwrap_or_else(|_| "[]".into());
        let tags = serde_json::to_string(req.tags.as_deref().unwrap_or(&[])).unwrap_or_else(|_| "[]".into());
        let row = self
            .experience
            .create(&CreateExperienceArticleParams {
                id: &id,
                owner_user_id: user_id,
                assistant_id: req.assistant_id.as_deref(),
                team_id: None,
                kind,
                title: req.title.trim(),
                body_md: &body,
                source_conversation_ids: &source,
                tags: &tags,
                status: "active",
            })
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;
        Ok(ExperienceArticleResponse {
            id: row.id,
            assistant_id: row.assistant_id,
            kind: row.kind,
            title: row.title,
            body_md: row.body_md,
            source_conversation_ids: parse_json_string_array(&row.source_conversation_ids),
            tags: parse_json_string_array(&row.tags),
            status: row.status,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }

    async fn require_owned_proposal(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<SkillEvolutionProposalRow, AssistantError> {
        let row = self
            .proposals
            .get(id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(id.to_owned()))?;
        if row.owner_user_id != user_id {
            return Err(AssistantError::Forbidden("proposal not owned by current user".into()));
        }
        Ok(row)
    }
}
