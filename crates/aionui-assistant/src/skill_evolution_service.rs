//! Skill Evolution service — WorkMate-native WikiSkill gate objects.
//!
//! Phase 1: store experience summaries + draft SKILL.md proposals.
//! Phase 2: Maintainer/Proposer LLM evolve + apply to Skills Hub / pin.
//! Does not inject experience into inference prompts. Does not vendor community
//! wikiskill CLI.

use std::sync::Arc;

use crate::error::AssistantError;
use aionui_api_types::{
    ApplySkillEvolutionRequest, ApplySkillEvolutionResponse, ApproveSkillEvolutionResponse,
    CreateExperienceArticleRequest, CreateSkillEvolutionProposalRequest, EvolveSkillEvolutionRequest,
    EvolveSkillEvolutionResponse, ExperienceArticleResponse, ReviewSkillEvolutionRequest, SkillEvolutionAction,
    SkillEvolutionExportPayload, SkillEvolutionProposalResponse, SkillEvolutionSkillRefPayload, SkillEvolutionStatus,
    SkillEvolutionTrajectoryOverview,
};
use aionui_common::{generate_prefixed_id, now_ms};
use aionui_db::{
    CreateExperienceArticleParams, CreateSkillEvolutionProposalParams, IConversationRepository,
    IExperienceArticleRepository, ISkillEvolutionProposalRepository, SkillEvolutionProposalRow,
    UpdateSkillEvolutionProposalParams,
};

use crate::skill_evolution_ports::{
    SkillEvolutionApplyPort, SkillEvolutionLlmPort, SkillEvolutionPinPort, SkillEvolutionTrajectoryPort,
    TrajectoryDigest,
};
use crate::skill_evolution_prompts;

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

fn article_row_to_response(row: aionui_db::ExperienceArticleRow) -> ExperienceArticleResponse {
    ExperienceArticleResponse {
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
    }
}

fn extract_first_heading(md: &str) -> Option<String> {
    for line in md.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("# ") {
            let title = rest.trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

struct ProposerDraft {
    title: String,
    target_skill_key: String,
    action: String,
    experience_summary: String,
    draft_diff_summary: Option<String>,
    draft_skill_md: String,
}

fn parse_proposer_json(raw: &str) -> Option<ProposerDraft> {
    let trimmed = raw.trim();
    let json_str = if let Some(start) = trimmed.find('{') {
        let end = trimmed.rfind('}')?;
        &trimmed[start..=end]
    } else {
        return None;
    };
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let title = v.get("title")?.as_str()?.trim().to_string();
    if title.is_empty() {
        return None;
    }
    let target_skill_key = v
        .get("target_skill_key")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| slugify_skill_key(&title));
    let action = v.get("action").and_then(|x| x.as_str()).unwrap_or("create").to_string();
    let experience_summary = v
        .get("experience_summary")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let draft_diff_summary = v
        .get("draft_diff_summary")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let draft_skill_md = v.get("draft_skill_md")?.as_str()?.to_string();
    if draft_skill_md.trim().is_empty() {
        return None;
    }
    Some(ProposerDraft {
        title,
        target_skill_key,
        action,
        experience_summary,
        draft_diff_summary,
        draft_skill_md,
    })
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
    trajectory: Option<Arc<dyn SkillEvolutionTrajectoryPort>>,
    llm: Option<Arc<dyn SkillEvolutionLlmPort>>,
    apply_port: Option<Arc<dyn SkillEvolutionApplyPort>>,
    pin_port: Option<Arc<dyn SkillEvolutionPinPort>>,
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
            trajectory: None,
            llm: None,
            apply_port: None,
            pin_port: None,
        }
    }

    pub fn with_ports(
        mut self,
        trajectory: Option<Arc<dyn SkillEvolutionTrajectoryPort>>,
        llm: Option<Arc<dyn SkillEvolutionLlmPort>>,
        apply_port: Option<Arc<dyn SkillEvolutionApplyPort>>,
        pin_port: Option<Arc<dyn SkillEvolutionPinPort>>,
    ) -> Self {
        self.trajectory = trajectory;
        self.llm = llm;
        self.apply_port = apply_port;
        self.pin_port = pin_port;
        self
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
            "## 被拒提案（经验库保留，勿重复踩坑）\n\n- proposal_id: `{}`\n- title: {}\n- target_skill_key: {}\n\n### 审核意见\n\n{}\n\n### 经验摘要\n\n{}\n\n### 草案片段\n\n```\n{}\n```\n",
            row.id,
            row.title,
            row.target_skill_key.as_deref().unwrap_or("(未指定)"),
            if comment.trim().is_empty() {
                "（未填写具体意见）"
            } else {
                comment.as_str()
            },
            row.experience_summary,
            truncate_chars(&row.draft_skill_md, 1200),
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

    pub async fn apply(
        &self,
        user_id: &str,
        id: &str,
        req: ApplySkillEvolutionRequest,
    ) -> Result<ApplySkillEvolutionResponse, AssistantError> {
        let row = self.require_owned_proposal(user_id, id).await?;
        if row.status != "approved" && row.status != "applied" {
            return Err(AssistantError::BadRequest(
                "仅已通过（或已应用）的提案可写入 Skills Hub；请先审核通过。".into(),
            ));
        }
        let skill_key = row
            .applied_skill_key
            .clone()
            .or(row.target_skill_key.clone())
            .unwrap_or_else(|| "evolved-skill".into());
        let version = row.applied_skill_version.clone().unwrap_or_else(|| "0.1.0".into());

        let mut skills_hub_path = None;
        let mut workspace_skill_path = None;
        let mut skill_ref = None;

        let workspace = if let Some(ws) = req.workspace_root.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(ws.to_string())
        } else if let Some(cid) = row.conversation_id.as_deref() {
            if let Some(traj) = self.trajectory.as_ref() {
                traj.load_digest(user_id, cid).await.ok().and_then(|d| d.workspace)
            } else {
                None
            }
        } else {
            None
        };

        if req.write_to_skills_hub || workspace.is_some() {
            let port = self
                .apply_port
                .as_ref()
                .ok_or_else(|| AssistantError::Internal("技能应用端口未配置：请升级 Core 后重试".into()))?;
            let outcome = port
                .write_skill(
                    user_id,
                    &skill_key,
                    &row.draft_skill_md,
                    workspace.as_deref(),
                    req.write_to_skills_hub,
                )
                .await
                .map_err(|e| match e {
                    AssistantError::BadRequest(msg) => {
                        AssistantError::BadRequest(format!("写入 Skills Hub / 工作区失败：{msg}"))
                    }
                    other => AssistantError::Internal(format!("写入技能失败：{other}")),
                })?;
            skills_hub_path = outcome.skills_hub_path;
            workspace_skill_path = outcome.workspace_skill_path;
        }

        if req.pin_on_assistant
            && let Some(aid) = row.assistant_id.as_deref().filter(|s| !s.is_empty())
            && let Some(pin) = self.pin_port.as_ref()
        {
            pin.pin_skill(user_id, aid, &skill_key, &version).await.map_err(|e| {
                AssistantError::BadRequest(format!(
                    "技能已写入，但 pin 到智能体失败：{e}。可稍后在智能体中心手动绑定。"
                ))
            })?;
            skill_ref = Some(SkillEvolutionSkillRefPayload {
                skill_key: skill_key.clone(),
                version_policy: "pin".into(),
                pinned_version: Some(version.clone()),
                source: Some("skill-evolution".into()),
            });
        }

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
            suggested_path: workspace_skill_path
                .clone()
                .unwrap_or_else(|| format!(".csbu-workmate/skills/{skill_key}/SKILL.md")),
        };
        Ok(ApplySkillEvolutionResponse {
            proposal: row_to_response(updated)?,
            export,
            skills_hub_path,
            workspace_skill_path,
            skill_ref,
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

    /// From-conversation evolve: load trajectory → Maintainer → Proposer → draft proposal.
    pub async fn evolve_from_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
        req: EvolveSkillEvolutionRequest,
    ) -> Result<EvolveSkillEvolutionResponse, AssistantError> {
        let found = self
            .conversations
            .get(user_id, conversation_id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;
        if found.is_none() {
            return Err(AssistantError::NotFound(format!("conversation {conversation_id}")));
        }
        let digest = self.load_trajectory_digest(user_id, conversation_id).await?;
        self.run_evolve(user_id, Some(conversation_id), None, digest, req).await
    }

    /// Re-run evolve on an existing draft/pending proposal (keeps id).
    pub async fn evolve_proposal(
        &self,
        user_id: &str,
        proposal_id: &str,
        req: EvolveSkillEvolutionRequest,
    ) -> Result<EvolveSkillEvolutionResponse, AssistantError> {
        let row = self.require_owned_proposal(user_id, proposal_id).await?;
        if row.status != "draft" && row.status != "pending_review" && row.status != "rejected" {
            return Err(AssistantError::BadRequest(
                "仅草稿、待审核或已拒绝的提案可再次智能提炼".into(),
            ));
        }
        let conversation_id = row
            .conversation_id
            .clone()
            .ok_or_else(|| AssistantError::BadRequest("proposal has no conversation_id".into()))?;
        let mut merged = req;
        if merged.assistant_id.is_none() {
            merged.assistant_id = row.assistant_id.clone();
        }
        if merged.title.is_none() {
            merged.title = Some(row.title.clone());
        }
        if merged.target_skill_key.is_none() {
            merged.target_skill_key = row.target_skill_key.clone();
        }
        let digest = self.load_trajectory_digest(user_id, &conversation_id).await?;
        self.run_evolve(
            user_id,
            Some(conversation_id.as_str()),
            Some(proposal_id),
            digest,
            merged,
        )
        .await
    }

    async fn load_trajectory_digest(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<TrajectoryDigest, AssistantError> {
        let port = self
            .trajectory
            .as_ref()
            .ok_or_else(|| AssistantError::Internal("trajectory port not configured for skill evolution".into()))?;
        let mut digest = port.load_digest(user_id, conversation_id).await?;
        digest.digest_md = redact_secrets(&digest.digest_md);
        Ok(digest)
    }

    async fn run_evolve(
        &self,
        user_id: &str,
        conversation_id: Option<&str>,
        existing_proposal_id: Option<&str>,
        digest: TrajectoryDigest,
        req: EvolveSkillEvolutionRequest,
    ) -> Result<EvolveSkillEvolutionResponse, AssistantError> {
        let llm = self.llm.as_ref().ok_or_else(|| {
            AssistantError::BadRequest("未配置可用模型：请在设置中启用至少一个模型提供商后再使用「智能提炼」".into())
        })?;

        let overview = SkillEvolutionTrajectoryOverview {
            turns: digest.turns,
            steps: digest.steps,
            tools: digest.tools,
            errors: digest.errors,
            record_count: digest.record_count,
            digest_md: digest.digest_md.clone(),
            conversation_name: digest.conversation_name.clone(),
            workspace: digest.workspace.clone(),
        };

        let conv_label = conversation_id.unwrap_or("n/a");
        let maintainer_user = skill_evolution_prompts::maintainer_user(&digest.digest_md, conv_label);
        let (pattern_raw, model_used) = match llm
            .complete(
                user_id,
                skill_evolution_prompts::MAINTAINER_SYSTEM,
                &maintainer_user,
                req.model.as_deref(),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                return Err(AssistantError::BadRequest(format!(
                    "智能提炼失败（Maintainer）：{e}。请确认已配置可用模型。"
                )));
            }
        };
        let pattern_body = redact_secrets(pattern_raw.trim());
        if pattern_body.is_empty() {
            return Err(AssistantError::BadRequest(
                "Maintainer 返回空内容，请重试或更换模型".into(),
            ));
        }

        let pattern_title =
            extract_first_heading(&pattern_body).unwrap_or_else(|| format!("会话经验模式 · {conv_label}"));
        let article_id = generate_prefixed_id("ea");
        let source_json = serde_json::to_string(&conversation_id.map(|c| vec![c.to_string()]).unwrap_or_default())
            .unwrap_or_else(|_| "[]".into());
        let pattern_article = self
            .experience
            .create(&CreateExperienceArticleParams {
                id: &article_id,
                owner_user_id: user_id,
                assistant_id: req.assistant_id.as_deref(),
                team_id: None,
                kind: "pattern",
                title: &pattern_title,
                body_md: &pattern_body,
                source_conversation_ids: &source_json,
                tags: r#"["skill-evolution","pattern","maintainer"]"#,
                status: "active",
            })
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;

        let impact_id = generate_prefixed_id("ea");
        let impact_body = format!(
            "## 技能影响笔记\n\n- 会话: `{conv_label}`\n- 模型: `{model_used}`\n- 关联 pattern: `{}`\n\n说明：经验库仅用于技能进化，**不会**注入日常对话。\n",
            pattern_article.id
        );
        let impact_article = self
            .experience
            .create(&CreateExperienceArticleParams {
                id: &impact_id,
                owner_user_id: user_id,
                assistant_id: req.assistant_id.as_deref(),
                team_id: None,
                kind: "skill_impact",
                title: &format!("影响笔记 · {pattern_title}"),
                body_md: &impact_body,
                source_conversation_ids: &source_json,
                tags: r#"["skill-evolution","skill_impact"]"#,
                status: "active",
            })
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;

        let prior_notes = self
            .load_prior_evolution_notes(user_id, req.assistant_id.as_deref(), conversation_id)
            .await;
        let proposer_user = skill_evolution_prompts::proposer_user(
            &pattern_body,
            &digest.digest_md,
            req.title.as_deref(),
            req.target_skill_key.as_deref(),
            prior_notes.as_deref(),
        );
        let (proposer_raw, model_used2) = match llm
            .complete(
                user_id,
                skill_evolution_prompts::PROPOSER_SYSTEM,
                &proposer_user,
                req.model.as_deref(),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                return Err(AssistantError::BadRequest(format!(
                    "智能提炼失败（Proposer）：{e}。经验库 pattern 已保存，可稍后重试。"
                )));
            }
        };
        let model_used = if model_used2 != model_used {
            format!("{model_used}+{model_used2}")
        } else {
            model_used
        };

        let parsed = parse_proposer_json(&proposer_raw).unwrap_or_else(|| {
            let title = req.title.clone().unwrap_or_else(|| pattern_title.clone());
            let key = req
                .target_skill_key
                .clone()
                .unwrap_or_else(|| slugify_skill_key(&title));
            ProposerDraft {
                title: title.clone(),
                target_skill_key: key.clone(),
                action: "create".into(),
                experience_summary: truncate_chars(&pattern_body, 1200),
                draft_diff_summary: Some("LLM 未返回合法 JSON，已回退为 stub 草案".into()),
                draft_skill_md: stub_skill_md(&key, &title, &truncate_chars(&pattern_body, 800), conversation_id),
            }
        });

        let action = match req.action {
            Some(SkillEvolutionAction::Patch) => "patch",
            Some(SkillEvolutionAction::Create) => "create",
            None if parsed.action == "patch" => "patch",
            None => "create",
        };
        let title = req
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(parsed.title.trim())
            .to_string();
        let skill_key = req
            .target_skill_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(parsed.target_skill_key.trim())
            .to_string();
        let skill_key = if skill_key.is_empty() {
            slugify_skill_key(&title)
        } else {
            skill_key
        };
        let summary = redact_secrets(parsed.experience_summary.trim());
        let draft = redact_secrets(parsed.draft_skill_md.trim());
        let draft = if draft.is_empty() {
            stub_skill_md(&skill_key, &title, &summary, conversation_id)
        } else {
            draft
        };
        let diff = parsed.draft_diff_summary.as_deref().map(redact_secrets);
        let article_ids = vec![pattern_article.id.clone(), impact_article.id.clone()];
        let article_ids_json = serde_json::to_string(&article_ids).unwrap_or_else(|_| "[]".into());
        let status = if req.submit { "pending_review" } else { "draft" };

        let proposal = if let Some(pid) = existing_proposal_id {
            self.proposals
                .update(
                    pid,
                    &UpdateSkillEvolutionProposalParams {
                        status: Some(status),
                        title: Some(title.as_str()),
                        experience_summary: Some(summary.as_str()),
                        experience_article_ids: Some(article_ids_json.as_str()),
                        draft_skill_md: Some(draft.as_str()),
                        draft_diff_summary: diff.as_deref(),
                        target_skill_key: Some(skill_key.as_str()),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| AssistantError::Internal(e.to_string()))?
                .ok_or_else(|| AssistantError::NotFound(pid.to_owned()))?
        } else {
            let id = generate_prefixed_id("sep");
            self.proposals
                .create(&CreateSkillEvolutionProposalParams {
                    id: &id,
                    owner_user_id: user_id,
                    assistant_id: req.assistant_id.as_deref(),
                    conversation_id,
                    status,
                    title: title.as_str(),
                    experience_summary: summary.as_str(),
                    experience_article_ids: article_ids_json.as_str(),
                    action,
                    target_skill_key: Some(skill_key.as_str()),
                    draft_skill_md: draft.as_str(),
                    draft_diff_summary: diff.as_deref(),
                })
                .await
                .map_err(|e| AssistantError::Internal(e.to_string()))?
        };

        let experience_articles = vec![
            article_row_to_response(pattern_article),
            article_row_to_response(impact_article),
        ];

        Ok(EvolveSkillEvolutionResponse {
            proposal: row_to_response(proposal)?,
            experience_articles,
            trajectory_overview: overview,
            model_used: Some(model_used),
        })
    }

    /// Load durable rejected_note / skill_impact articles for this conversation
    /// (and assistant when set) so re-evolve does not repeat failed proposals.
    async fn load_prior_evolution_notes(
        &self,
        user_id: &str,
        assistant_id: Option<&str>,
        conversation_id: Option<&str>,
    ) -> Option<String> {
        let rows = self
            .experience
            .list_for_owner(user_id, assistant_id, 80)
            .await
            .unwrap_or_default();
        let mut chunks: Vec<String> = Vec::new();
        for row in rows {
            if row.kind != "rejected_note" && row.kind != "skill_impact" {
                continue;
            }
            if let Some(cid) = conversation_id {
                let sources = parse_json_string_array(&row.source_conversation_ids);
                if !sources.is_empty() && !sources.iter().any(|s| s == cid) {
                    continue;
                }
            }
            let body = truncate_chars(&row.body_md, 1800);
            chunks.push(format!("### [{}] {}\n\n{}\n", row.kind, row.title, body));
            if chunks.len() >= 6 {
                break;
            }
        }
        if chunks.is_empty() {
            None
        } else {
            Some(chunks.join("\n"))
        }
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
