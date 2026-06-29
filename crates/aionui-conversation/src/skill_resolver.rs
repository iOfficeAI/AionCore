//! Abstraction over "what are the auto-inject skill names right now?" so
//! `ConversationService` can compute the initial snapshot without forcing
//! every test setup to stand up a real `SkillPaths` and skill repository.

use std::path::Path;
use std::sync::Arc;

use aionui_db::ISkillRepository;
pub use aionui_extension::ResolvedAgentSkill;
use async_trait::async_trait;
use tracing::warn;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedAgentSkill {
    pub name: String,
    pub body: String,
}

#[async_trait]
pub trait SkillResolver: Send + Sync {
    /// Returns the sorted list of auto-inject builtin skill names currently
    /// available on this installation.
    async fn auto_inject_names(&self) -> Vec<String>;

    /// Resolve each skill name to its on-disk source directory, using the
    /// same search order as `materialize_skills_for_agent`.
    async fn resolve_skills(&self, names: &[String]) -> Vec<ResolvedAgentSkill>;

    /// Load full skill bodies for prompt-protocol agents that request
    /// `[LOAD_SKILL: name]` in their response.
    async fn load_skill_bodies(&self, names: &[String]) -> Vec<LoadedAgentSkill> {
        let resolved = self.resolve_skills(names).await;
        load_resolved_skill_bodies(&resolved).await
    }

    /// Create symlinks pointing at each resolved skill inside the given
    /// workspace's per-backend native skills directories. `rel_dirs` is
    /// the list of relative paths (e.g. `.claude/skills`) to populate.
    /// Returns the number of symlinks successfully created.
    async fn link_workspace_skills(&self, workspace: &Path, rel_dirs: &[&str], skills: &[ResolvedAgentSkill]) -> usize;
}

/// Production adapter backed by `aionui_extension::skill_service`.
pub struct ExtensionSkillResolver {
    paths: Arc<aionui_extension::SkillPaths>,
    skill_repo: Arc<dyn ISkillRepository>,
}

impl ExtensionSkillResolver {
    pub fn new(paths: Arc<aionui_extension::SkillPaths>, skill_repo: Arc<dyn ISkillRepository>) -> Self {
        Self { paths, skill_repo }
    }
}

async fn load_resolved_skill_bodies(skills: &[ResolvedAgentSkill]) -> Vec<LoadedAgentSkill> {
    let mut loaded = Vec::new();
    for skill in skills {
        let skill_file = skill.source_path.join("SKILL.md");
        match tokio::fs::read_to_string(&skill_file).await {
            Ok(content) => loaded.push(LoadedAgentSkill {
                name: skill.name.clone(),
                body: extract_skill_body(&content),
            }),
            Err(e) => {
                warn!(
                    skill = %skill.name,
                    path = %skill_file.display(),
                    error = %e,
                    "Failed to read requested skill body"
                );
            }
        }
    }
    loaded
}

fn extract_skill_body(content: &str) -> String {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return content.to_string();
    }

    let after_open = &trimmed[3..];
    if let Some(close_idx) = after_open.find("---") {
        let after_close = &after_open[close_idx + 3..];
        after_close.trim_start_matches('\n').to_string()
    } else {
        content.to_string()
    }
}

#[async_trait]
impl SkillResolver for ExtensionSkillResolver {
    async fn auto_inject_names(&self) -> Vec<String> {
        match aionui_extension::list_available_skills_with_repo(&self.paths, self.skill_repo.as_ref()).await {
            Ok(items) => {
                let mut names: Vec<String> = items
                    .into_iter()
                    .filter(|item| {
                        item.source == aionui_extension::SkillSource::Builtin
                            && item
                                .relative_location
                                .as_deref()
                                .is_some_and(|location| location.starts_with("auto-inject/"))
                    })
                    .map(|item| item.name)
                    .collect();
                names.sort();
                names
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "auto_inject_names: skill catalog lookup failed, falling back to empty"
                );
                Vec::new()
            }
        }
    }

    async fn resolve_skills(&self, names: &[String]) -> Vec<ResolvedAgentSkill> {
        if names.is_empty() {
            return Vec::new();
        }
        // Conversation_id is validated upstream; we don't use a real one here
        // because this resolver is purely a path-resolution helper.
        match aionui_extension::materialize_skills_for_agent_with_repo(
            &self.paths,
            self.skill_repo.as_ref(),
            "workspace-link",
            names,
        )
        .await
        {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "resolve_skills failed; returning empty list"
                );
                Vec::new()
            }
        }
    }

    async fn link_workspace_skills(&self, workspace: &Path, rel_dirs: &[&str], skills: &[ResolvedAgentSkill]) -> usize {
        if rel_dirs.is_empty() || skills.is_empty() {
            return 0;
        }
        match aionui_extension::link_workspace_skills(workspace, rel_dirs, skills).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    workspace = %workspace.display(),
                    error = %e,
                    "link_workspace_skills failed"
                );
                0
            }
        }
    }
}

#[cfg(test)]
pub struct FixedSkillResolver {
    pub names: Vec<String>,
}

#[cfg(test)]
#[async_trait]
impl SkillResolver for FixedSkillResolver {
    async fn auto_inject_names(&self) -> Vec<String> {
        self.names.clone()
    }

    async fn resolve_skills(&self, _names: &[String]) -> Vec<ResolvedAgentSkill> {
        Vec::new()
    }

    async fn link_workspace_skills(
        &self,
        _workspace: &Path,
        _rel_dirs: &[&str],
        _skills: &[ResolvedAgentSkill],
    ) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::SqliteSkillRepository;

    fn write_skill(dir: &Path, name: &str, description: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nBody"),
        )
        .unwrap();
    }

    #[test]
    fn extract_skill_body_removes_frontmatter() {
        let content = "---\nname: cron\ndescription: Cron\n---\nCron body";
        assert_eq!(extract_skill_body(content), "Cron body");
    }

    #[tokio::test]
    async fn extension_resolver_reads_auto_inject_names_from_skill_catalog() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Arc::new(aionui_extension::SkillPaths {
            data_dir: tmp.path().to_path_buf(),
            user_skills_dir: tmp.path().join("skills"),
            cron_skills_dir: tmp.path().join("cron").join("skills"),
            builtin_skills_dir: tmp.path().join("builtin-skills"),
            builtin_rules_dir: tmp.path().join("builtin-rules"),
            assistant_rules_dir: tmp.path().join("assistant-rules"),
            assistant_skills_dir: tmp.path().join("assistant-skills"),
        });
        write_skill(&paths.builtin_skills_dir, "review", "Top-level builtin");
        write_skill(
            &paths.builtin_skills_dir.join("auto-inject"),
            "auto-cron",
            "Auto-injected builtin",
        );
        write_skill(&paths.cron_skills_dir, "scheduled-task", "Cron source skill");

        let db = aionui_db::init_database_memory().await.unwrap();
        let repo: Arc<dyn ISkillRepository> = Arc::new(SqliteSkillRepository::new(db.pool().clone()));
        aionui_extension::sync_skill_catalog_into_repo(paths.as_ref(), repo.as_ref())
            .await
            .unwrap();

        let resolver = ExtensionSkillResolver::new(paths, repo);

        assert_eq!(resolver.auto_inject_names().await, vec!["auto-cron".to_string()]);
    }
}
