//! Adapters wiring Skill Evolution ports to ConversationService / providers / skills.

use std::path::PathBuf;
use std::sync::Arc;

use aionui_api_types::{
    AgentCenterMetaPatch, AgentSkillRef, SkillVersionPolicy, TrajectoryQuery, UpdateAgentCenterRequest,
    UpdateAssistantRequest,
};
use aionui_assistant::{
    AgentCenterService, AssistantError, SkillEvolutionApplyPort, SkillEvolutionLlmPort, SkillEvolutionPinPort,
    SkillEvolutionTrajectoryPort, SkillWriteOutcome, TrajectoryDigest,
};
use aionui_conversation::ConversationService;
use aionui_db::ISkillRepository;
use aionui_extension::{SkillPaths, import_skill_with_repo_for_user};
use aionui_system::ProviderService;
use async_trait::async_trait;
use serde_json::json;

pub struct ConversationTrajectoryAdapter {
    pub conversations: ConversationService,
}

#[async_trait]
impl SkillEvolutionTrajectoryPort for ConversationTrajectoryAdapter {
    async fn load_digest(&self, user_id: &str, conversation_id: &str) -> Result<TrajectoryDigest, AssistantError> {
        let conv = self
            .conversations
            .get(user_id, conversation_id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;
        let projection = self
            .conversations
            .derive_trajectory(
                user_id,
                conversation_id,
                TrajectoryQuery {
                    limit: Some(80),
                    ..TrajectoryQuery::default()
                },
            )
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;

        let mut lines = Vec::new();
        lines.push(format!(
            "# Trajectory overview\n- turns: {}\n- steps: {}\n- tools: {}\n- errors: {}\n",
            projection.overview.turns, projection.overview.steps, projection.overview.tools, projection.overview.errors
        ));
        for rec in &projection.records {
            let input = rec.input_preview.as_deref().unwrap_or("");
            let output = rec.output_preview.as_deref().unwrap_or("");
            lines.push(format!(
                "## [{}] {} ({})\n- summary: {}\n- input: {}\n- output: {}\n",
                rec.category,
                rec.title,
                rec.status,
                truncate(rec.summary.as_str(), 400),
                truncate(input, 300),
                truncate(output, 400),
            ));
        }

        let workspace = conv
            .extra
            .get("workspace")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Ok(TrajectoryDigest {
            turns: projection.overview.turns,
            steps: projection.overview.steps,
            tools: projection.overview.tools,
            errors: projection.overview.errors,
            record_count: projection.records.len(),
            digest_md: lines.join("\n"),
            conversation_name: Some(conv.name),
            workspace,
        })
    }
}

pub struct ProviderLlmAdapter {
    pub providers: ProviderService,
    pub http: reqwest::Client,
}

#[async_trait]
impl SkillEvolutionLlmPort for ProviderLlmAdapter {
    async fn complete(
        &self,
        user_id: &str,
        system: &str,
        user: &str,
        model_hint: Option<&str>,
    ) -> Result<(String, String), AssistantError> {
        let providers = self
            .providers
            .list(user_id)
            .await
            .map_err(|e| AssistantError::BadRequest(format!("无法读取模型提供商：{e}")))?;

        let mut chosen = None;
        for p in providers.into_iter().filter(|p| p.enabled) {
            if p.api_key.trim().is_empty() || p.base_url.trim().is_empty() {
                continue;
            }
            let model = pick_model(&p, model_hint);
            if let Some(model) = model {
                chosen = Some((p, model));
                break;
            }
        }
        let (provider, model) = chosen.ok_or_else(|| {
            AssistantError::BadRequest("未找到可用模型：请在设置中启用提供商并配置 API Key / 模型列表".into())
        })?;

        let url = chat_completions_url(&provider.base_url, provider.is_full_url);
        let body = json!({
            "model": model,
            "temperature": 0.2,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ]
        });
        let response = self
            .http
            .post(&url)
            .bearer_auth(provider.api_key.trim())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AssistantError::BadRequest(format!("模型请求失败：{e}")))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| AssistantError::BadRequest(format!("读取模型响应失败：{e}")))?;
        if !status.is_success() {
            return Err(AssistantError::BadRequest(format!(
                "模型返回 HTTP {status}: {}",
                truncate(&text, 400)
            )));
        }
        let content = extract_chat_content(&text)
            .ok_or_else(|| AssistantError::BadRequest(format!("无法解析模型响应：{}", truncate(&text, 400))))?;
        Ok((content, model))
    }
}

pub struct SkillHubApplyAdapter {
    pub skill_paths: SkillPaths,
    pub skill_repo: Arc<dyn ISkillRepository>,
}

#[async_trait]
impl SkillEvolutionApplyPort for SkillHubApplyAdapter {
    async fn write_skill(
        &self,
        user_id: &str,
        skill_key: &str,
        skill_md: &str,
        workspace_root: Option<&str>,
        write_to_skills_hub: bool,
    ) -> Result<SkillWriteOutcome, AssistantError> {
        let key = sanitize_skill_key(skill_key);
        if key.is_empty() {
            return Err(AssistantError::BadRequest("invalid skill_key".into()));
        }

        let mut skills_hub_path = None;
        if write_to_skills_hub {
            let tmp = tempfile::tempdir().map_err(|e| AssistantError::Internal(format!("tempdir: {e}")))?;
            let skill_dir = tmp.path().join(&key);
            tokio::fs::create_dir_all(&skill_dir)
                .await
                .map_err(|e| AssistantError::Internal(e.to_string()))?;
            let skill_file = skill_dir.join("SKILL.md");
            tokio::fs::write(&skill_file, skill_md)
                .await
                .map_err(|e| AssistantError::Internal(e.to_string()))?;
            let imported =
                import_skill_with_repo_for_user(&self.skill_paths, self.skill_repo.as_ref(), user_id, &skill_dir)
                    .await
                    .map_err(|e| AssistantError::Internal(format!("import skill failed: {e}")))?;
            skills_hub_path = Some(format!("skills/{}/SKILL.md", imported.name));
        }

        let mut workspace_skill_path = None;
        if let Some(ws) = workspace_root.map(str::trim).filter(|s| !s.is_empty()) {
            let root = PathBuf::from(ws);
            let target_dir = root.join(".csbu-workmate").join("skills").join(&key);
            tokio::fs::create_dir_all(&target_dir)
                .await
                .map_err(|e| AssistantError::Internal(format!("create workspace skill dir: {e}")))?;
            let target = target_dir.join("SKILL.md");
            tokio::fs::write(&target, skill_md)
                .await
                .map_err(|e| AssistantError::Internal(format!("write workspace SKILL.md: {e}")))?;
            workspace_skill_path = Some(target.display().to_string());
        }

        Ok(SkillWriteOutcome {
            skills_hub_path,
            workspace_skill_path,
            skill_key: key,
        })
    }
}

pub struct AgentCenterPinAdapter {
    pub agent_center: Arc<AgentCenterService>,
}

#[async_trait]
impl SkillEvolutionPinPort for AgentCenterPinAdapter {
    async fn pin_skill(
        &self,
        user_id: &str,
        assistant_id: &str,
        skill_key: &str,
        version: &str,
    ) -> Result<(), AssistantError> {
        let detail = self
            .agent_center
            .get_detail_for_user(user_id, assistant_id, None)
            .await?;
        let mut refs = detail.meta.skill_refs;
        if let Some(existing) = refs.iter_mut().find(|r| r.skill_key == skill_key) {
            existing.version_policy = SkillVersionPolicy::Pin;
            existing.pinned_version = Some(version.to_string());
            existing.source = Some("skill-evolution".into());
        } else {
            refs.push(AgentSkillRef {
                skill_key: skill_key.to_string(),
                source: Some("skill-evolution".into()),
                version_policy: SkillVersionPolicy::Pin,
                pinned_version: Some(version.to_string()),
            });
        }
        let req = UpdateAgentCenterRequest {
            assistant: UpdateAssistantRequest::default(),
            meta: AgentCenterMetaPatch {
                skill_refs: Some(refs),
                ..AgentCenterMetaPatch::default()
            },
        };
        let _ = self.agent_center.update_for_user(user_id, assistant_id, req).await?;
        Ok(())
    }
}

fn pick_model(provider: &aionui_api_types::ProviderResponse, hint: Option<&str>) -> Option<String> {
    let is_enabled = |model: &str| {
        provider
            .model_enabled
            .as_ref()
            .and_then(|m| m.get(model))
            .copied()
            .unwrap_or(true)
    };
    if let Some(h) = hint.map(str::trim).filter(|s| !s.is_empty())
        && provider.models.iter().any(|m| m == h)
        && is_enabled(h)
    {
        return Some(h.to_string());
    }
    provider.models.iter().find(|m| is_enabled(m)).cloned()
}

fn chat_completions_url(base_url: &str, is_full_url: bool) -> String {
    let base = base_url.trim_end_matches('/');
    if is_full_url || base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    }
}

fn extract_chat_content(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if let Some(content) = v.pointer("/choices/0/message/content").and_then(|c| c.as_str()) {
        return Some(content.to_string());
    }
    // Some providers return content as array of parts
    if let Some(arr) = v.pointer("/choices/0/message/content").and_then(|c| c.as_array()) {
        let mut out = String::new();
        for part in arr {
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                out.push_str(t);
            } else if let Some(t) = part.as_str() {
                out.push_str(t);
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

fn sanitize_skill_key(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if (ch == '-' || ch == '_') && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}
