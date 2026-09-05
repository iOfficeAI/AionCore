//! Ports for Skill Evolution Phase 2 (trajectory / LLM / apply / pin).
//!
//! Implementations live in `aionui-app` so this crate stays free of conversation
//! and provider wiring. Experience articles MUST NOT be injected into normal
//! Inference Agent chat prompts — these ports are evolve/apply only.

use async_trait::async_trait;

use crate::error::AssistantError;

#[derive(Debug, Clone, Default)]
pub struct TrajectoryDigest {
    pub turns: u64,
    pub steps: u64,
    pub tools: u64,
    pub errors: u64,
    pub record_count: usize,
    pub digest_md: String,
    pub conversation_name: Option<String>,
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SkillWriteOutcome {
    pub skills_hub_path: Option<String>,
    pub workspace_skill_path: Option<String>,
    pub skill_key: String,
}

#[async_trait]
pub trait SkillEvolutionTrajectoryPort: Send + Sync {
    async fn load_digest(&self, user_id: &str, conversation_id: &str) -> Result<TrajectoryDigest, AssistantError>;
}

#[async_trait]
pub trait SkillEvolutionLlmPort: Send + Sync {
    /// Returns `(completion_text, model_id_used)`.
    async fn complete(
        &self,
        user_id: &str,
        system: &str,
        user: &str,
        model_hint: Option<&str>,
    ) -> Result<(String, String), AssistantError>;
}

#[async_trait]
pub trait SkillEvolutionApplyPort: Send + Sync {
    async fn write_skill(
        &self,
        user_id: &str,
        skill_key: &str,
        skill_md: &str,
        workspace_root: Option<&str>,
        write_to_skills_hub: bool,
    ) -> Result<SkillWriteOutcome, AssistantError>;
}

#[async_trait]
pub trait SkillEvolutionPinPort: Send + Sync {
    async fn pin_skill(
        &self,
        user_id: &str,
        assistant_id: &str,
        skill_key: &str,
        version: &str,
    ) -> Result<(), AssistantError>;
}
