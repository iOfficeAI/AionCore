//! Abstraction over "what are the auto-inject skill names right now?" so
//! `ConversationService` can compute the initial snapshot without forcing
//! every test setup to stand up a real `SkillPaths` and skill repository.

use std::sync::Arc;

use aionui_db::ISkillRepository;
pub use aionui_extension::ResolvedAgentSkill;
// Frontmatter stripping lives in `aionui-extension` so this channel and the
// `aioncore skills show` command return byte-identical bodies. A local copy is
// how the two would quietly drift.
use aionui_extension::skill_service::extract_skill_body;
use async_trait::async_trait;
use tracing::warn;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedAgentSkill {
    pub name: String,
    pub body: String,
    /// Absolute skill root.
    ///
    /// Load-bearing, not decoration: skill bodies reference their own files
    /// relatively (`references/workflows.md`, `scripts/init_skill.py`), and
    /// without a stated root the agent resolves those against its CWD -- the
    /// workspace. That either fails silently or, worse, reads an unrelated
    /// same-named user file.
    pub source_path: std::path::PathBuf,
}

#[async_trait]
pub trait SkillResolver: Send + Sync {
    /// Returns the sorted list of auto-inject builtin skill names currently
    /// available on this installation.
    async fn auto_inject_names(&self) -> Vec<String>;

    /// Resolve each skill name to its on-disk source directory, using the
    /// same search order as `materialize_skills_for_agent`.
    async fn resolve_skills(&self, names: &[String]) -> Vec<ResolvedAgentSkill>;

    /// Resolve each skill name for a specific Core user.
    async fn resolve_skills_for_user(&self, _user_id: &str, names: &[String]) -> Vec<ResolvedAgentSkill> {
        self.resolve_skills(names).await
    }

    /// Load full skill bodies for prompt-protocol agents that request
    /// `[LOAD_SKILL: name]` in their response.
    async fn load_skill_bodies(&self, names: &[String]) -> Vec<LoadedAgentSkill> {
        let resolved = self.resolve_skills(names).await;
        load_resolved_skill_bodies(&resolved).await
    }

    /// Load full skill bodies for prompt-protocol agents under one Core user.
    async fn load_skill_bodies_for_user(&self, user_id: &str, names: &[String]) -> Vec<LoadedAgentSkill> {
        let resolved = self.resolve_skills_for_user(user_id, names).await;
        load_resolved_skill_bodies(&resolved).await
    }

    /// Rebuild this conversation's skill VIEW directory under AionUi's own data
    /// dir, so the agent CLI can be pointed at it instead of at the user's
    /// workspace. Returns the number of links created.
    ///
    /// Called unconditionally, not only for vendors that consume it: the view is
    /// AionUi's own tree, and building it for every conversation means a delivery
    /// mode flipped in the registry needs no per-conversation backfill.
    ///
    /// Defaults to a no-op so the many test stubs in this workspace do not each
    /// need a body. The default cannot mask a PRODUCTION regression: the real
    /// implementation is covered by
    /// `extension_resolver_syncs_and_removes_a_real_view_directory` below, plus
    /// the service-level tests that assert this method was actually called.
    async fn sync_skill_view(&self, _user_id: &str, _conversation_id: &str, _skills: &[ResolvedAgentSkill]) -> usize {
        0
    }

    /// Drop this conversation's skill view directory. No-op by default, for the
    /// same reason as [`Self::sync_skill_view`].
    async fn remove_skill_view(&self, _user_id: &str, _conversation_id: &str) {}
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
                source_path: skill.source_path.clone(),
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
        self.resolve_skills_for_user("system_default_user", names).await
    }

    async fn resolve_skills_for_user(&self, user_id: &str, names: &[String]) -> Vec<ResolvedAgentSkill> {
        if names.is_empty() {
            return Vec::new();
        }
        // Conversation_id is validated upstream; we don't use a real one here
        // because this resolver is purely a path-resolution helper.
        match aionui_extension::materialize_skills_for_agent_with_repo_for_user(
            &self.paths,
            self.skill_repo.as_ref(),
            user_id,
            "skill-resolve",
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

    async fn sync_skill_view(&self, user_id: &str, conversation_id: &str, skills: &[ResolvedAgentSkill]) -> usize {
        match aionui_extension::skill_view::rebuild_view(&self.paths.data_dir, user_id, conversation_id, skills).await {
            Ok(n) => n,
            Err(e) => {
                // `error`, not `warn`: the view is layer 1's only channel, so an
                // unwritable one means native delivery is unavailable for this
                // session. Not fatal -- layer 2's dual channel still covers it,
                // so the conversation must still start.
                tracing::error!(
                    user_id = %user_id,
                    conversation_id = %conversation_id,
                    error = %e,
                    "sync_skill_view failed; native skill delivery unavailable for this session"
                );
                0
            }
        }
    }

    async fn remove_skill_view(&self, user_id: &str, conversation_id: &str) {
        if let Err(e) = aionui_extension::skill_view::remove_view(&self.paths.data_dir, user_id, conversation_id).await
        {
            tracing::warn!(
                user_id = %user_id,
                conversation_id = %conversation_id,
                error = %e,
                "remove_skill_view failed; the orphan view will be reaped at next startup"
            );
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::{SqliteSkillRepository, UpsertSkillParams};
    use std::path::Path;

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

    /// `sync_skill_view` / `remove_skill_view` default to no-ops on the trait so
    /// the workspace's many test stubs need no bodies. This test is what keeps
    /// that default from masking a production regression: it drives the REAL
    /// implementation and asserts the view directory actually appears on disk
    /// under `{data_dir}/session-skills/{user}/{conversation}/`.
    #[tokio::test]
    async fn extension_resolver_syncs_and_removes_a_real_view_directory() {
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
        let sources = tmp.path().join("sources");
        write_skill(&sources, "cron", "Schedule stuff");

        let db = aionui_db::init_database_memory().await.unwrap();
        let repo: Arc<dyn ISkillRepository> = Arc::new(SqliteSkillRepository::new(db.pool().clone()));
        let resolver = ExtensionSkillResolver::new(paths.clone(), repo);

        let resolved = vec![ResolvedAgentSkill {
            name: "cron".to_owned(),
            source_path: sources.join("cron"),
        }];
        assert_eq!(resolver.sync_skill_view("user_a", "conv_1", &resolved).await, 1);

        let view = aionui_extension::skill_view::view_dir(&paths.data_dir, "user_a", "conv_1").unwrap();
        assert!(view.join(".claude-plugin").join("plugin.json").is_file());
        assert!(view.join("skills").join("cron").join("SKILL.md").is_file());

        resolver.remove_skill_view("user_a", "conv_1").await;
        assert!(!view.exists());
        // The link is gone; the real source must not be.
        assert!(sources.join("cron").join("SKILL.md").is_file());
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

    #[tokio::test]
    async fn extension_resolver_resolves_user_scoped_skill_rows() {
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
        let user_a_skill = tmp.path().join("user-a-skill");
        let user_b_skill = tmp.path().join("user-b-skill");
        write_skill(&user_a_skill, "shared", "User A skill");
        write_skill(&user_b_skill, "shared", "User B skill");

        let db = aionui_db::init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES ('user_b', 'local', 'user_b', 'hash', 'active', 0, 1, 1)",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let repo = Arc::new(SqliteSkillRepository::new(db.pool().clone()));
        let user_a_skill_path = user_a_skill.join("shared").to_string_lossy().into_owned();
        let user_b_skill_path = user_b_skill.join("shared").to_string_lossy().into_owned();
        repo.upsert_for_user(
            "system_default_user",
            UpsertSkillParams {
                name: "shared",
                description: Some("User A skill"),
                path: &user_a_skill_path,
                source: "user",
                enabled: true,
            },
        )
        .await
        .unwrap();
        repo.upsert_for_user(
            "user_b",
            UpsertSkillParams {
                name: "shared",
                description: Some("User B skill"),
                path: &user_b_skill_path,
                source: "user",
                enabled: true,
            },
        )
        .await
        .unwrap();

        let resolver = ExtensionSkillResolver::new(paths, repo);
        let resolved = resolver.resolve_skills_for_user("user_b", &["shared".to_owned()]).await;

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].source_path, user_b_skill.join("shared"));
    }
}
