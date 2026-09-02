//! Business logic for the runtime skills domain. No axum here.
//!
//! Every read starts from the conversation's own `extra.skills` snapshot. That
//! filter is the security boundary of this crate, not a convenience: without it
//! a conversation-scoped runtime token could read any skill on the installation,
//! including another assistant's and — on a multi-user Core — another user's.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_api_types::{
    RuntimeSkillFileResponse, RuntimeSkillListItem, RuntimeSkillListResponse, RuntimeSkillShowResponse,
};
use aionui_db::{IConversationRepository, ISkillRepository};
use aionui_extension::SkillPaths;
use tracing::{info, warn};

use crate::error::SkillRuntimeError;

pub struct SkillRuntimeService {
    conversation_repo: Arc<dyn IConversationRepository>,
    skill_paths: Arc<SkillPaths>,
    skill_repo: Arc<dyn ISkillRepository>,
}

impl SkillRuntimeService {
    pub fn new(
        conversation_repo: Arc<dyn IConversationRepository>,
        skill_paths: Arc<SkillPaths>,
        skill_repo: Arc<dyn ISkillRepository>,
    ) -> Self {
        Self {
            conversation_repo,
            skill_paths,
            skill_repo,
        }
    }

    /// The allow-list: names in this conversation's `extra.skills` snapshot.
    async fn enabled_skill_names(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<String>, SkillRuntimeError> {
        let row = self
            .conversation_repo
            .get(user_id, conversation_id)
            .await?
            .ok_or(SkillRuntimeError::ConversationNotFound)?;
        let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or(serde_json::Value::Null);
        Ok(extra
            .get("skills")
            .and_then(|value| serde_json::from_value::<Vec<String>>(value.clone()).ok())
            .unwrap_or_default())
    }

    /// `skills list` — only what this conversation enabled.
    ///
    /// Descriptions are read from the same on-disk `SKILL.md` that `show` reads,
    /// NOT from the DB catalog. That keeps the two commands consistent by
    /// construction: a catalog read would make `list` depend on whether a startup
    /// sync has happened, so a freshly imported skill could be invisible to
    /// `list` while `show` served it happily.
    pub async fn list(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<RuntimeSkillListResponse, SkillRuntimeError> {
        let enabled = self.enabled_skill_names(user_id, conversation_id).await?;
        if enabled.is_empty() {
            return Ok(RuntimeSkillListResponse { skills: Vec::new() });
        }

        let resolved = aionui_extension::materialize_skills_for_agent_with_repo_for_user(
            &self.skill_paths,
            self.skill_repo.as_ref(),
            user_id,
            conversation_id,
            &enabled,
        )
        .await
        .map_err(|e| SkillRuntimeError::ReadFailed { reason: e.to_string() })?;

        let mut skills: Vec<RuntimeSkillListItem> = Vec::with_capacity(resolved.len());
        for skill in resolved {
            // An unreadable or malformed SKILL.md must not cost the agent the
            // whole listing -- it would lose access to every OTHER skill too.
            let description = match tokio::fs::read_to_string(skill.source_path.join("SKILL.md")).await {
                Ok(content) => aionui_extension::skill_service::parse_frontmatter_fields(&content)
                    .map(|(_, description)| description)
                    .unwrap_or_default(),
                Err(e) => {
                    warn!(
                        conversation_id = %conversation_id,
                        skill = %skill.name,
                        error = %e,
                        "runtime skill listing: SKILL.md unreadable; listing it without a description"
                    );
                    String::new()
                }
            };
            skills.push(RuntimeSkillListItem {
                name: skill.name,
                description,
            });
        }
        // Sorted so repeated calls in one conversation do not reshuffle, which
        // would make the agent's own context churn for no reason.
        skills.sort_by(|a, b| a.name.cmp(&b.name));

        info!(
            conversation_id = %conversation_id,
            channel = "skills_cli",
            command = "list",
            skills = skills.len(),
            "runtime skill request served"
        );
        Ok(RuntimeSkillListResponse { skills })
    }

    /// Resolve `name` to its source dir, but ONLY if this conversation enabled it.
    async fn resolve_enabled(
        &self,
        user_id: &str,
        conversation_id: &str,
        name: &str,
    ) -> Result<PathBuf, SkillRuntimeError> {
        let enabled = self.enabled_skill_names(user_id, conversation_id).await?;
        if !enabled.iter().any(|candidate| candidate == name) {
            warn!(
                conversation_id = %conversation_id,
                channel = "skills_cli",
                skill = %name,
                "runtime skill request refused: not in this conversation's snapshot"
            );
            return Err(SkillRuntimeError::SkillNotEnabled { name: name.to_owned() });
        }

        // Same resolution the view directory uses, and scoped to this user, so a
        // same-named skill owned by another user cannot resolve here.
        let resolved = aionui_extension::materialize_skills_for_agent_with_repo_for_user(
            &self.skill_paths,
            self.skill_repo.as_ref(),
            user_id,
            conversation_id,
            std::slice::from_ref(&name.to_owned()),
        )
        .await
        .map_err(|e| SkillRuntimeError::ReadFailed { reason: e.to_string() })?;

        resolved
            .into_iter()
            .next()
            .map(|skill| skill.source_path)
            .ok_or_else(|| SkillRuntimeError::SkillNotFound { name: name.to_owned() })
    }

    /// `skills show <name>` — body plus the absolute root.
    pub async fn show(
        &self,
        user_id: &str,
        conversation_id: &str,
        name: &str,
    ) -> Result<RuntimeSkillShowResponse, SkillRuntimeError> {
        let root = self.resolve_enabled(user_id, conversation_id, name).await?;
        let content = tokio::fs::read_to_string(root.join("SKILL.md"))
            .await
            .map_err(|e| SkillRuntimeError::ReadFailed { reason: e.to_string() })?;

        info!(
            conversation_id = %conversation_id,
            channel = "skills_cli",
            command = "show",
            skill = %name,
            "runtime skill request served"
        );
        Ok(RuntimeSkillShowResponse {
            name: name.to_owned(),
            // Shared with the `[LOAD_SKILL]` channel so both return identical
            // bodies; a local copy is how the two would drift.
            body: aionui_extension::skill_service::extract_skill_body(&content),
            path: root.to_string_lossy().into_owned(),
        })
    }

    /// `skills cat <name>/<relative-path>` — a supplementary file.
    pub async fn read_file(
        &self,
        user_id: &str,
        conversation_id: &str,
        name: &str,
        rel_path: &str,
    ) -> Result<RuntimeSkillFileResponse, SkillRuntimeError> {
        let root = self.resolve_enabled(user_id, conversation_id, name).await?;
        let target = resolve_inside(&root, rel_path).inspect_err(|_| {
            // The REQUESTED relative shape only. Logging the resolved absolute
            // path would put the escape target in the log, which is exactly the
            // thing an attacker wanted to learn.
            warn!(
                conversation_id = %conversation_id,
                channel = "skills_cli",
                skill = %name,
                requested = %rel_path,
                "runtime skill file request refused: path escapes the skill directory"
            );
        })?;

        let content = tokio::fs::read_to_string(&target)
            .await
            .map_err(|e| SkillRuntimeError::ReadFailed { reason: e.to_string() })?;

        info!(
            conversation_id = %conversation_id,
            channel = "skills_cli",
            command = "cat",
            skill = %name,
            "runtime skill request served"
        );
        Ok(RuntimeSkillFileResponse {
            name: name.to_owned(),
            path: rel_path.to_owned(),
            content,
        })
    }
}

/// Traversal guard.
///
/// Rejects absolute paths and `..` components up front, then canonicalizes and
/// re-checks containment. The second step is the one that matters: a SYMLINK
/// inside the skill directory pointing outward is traversal by another name, and
/// no amount of lexical checking catches it.
///
/// The root is canonicalized too — the skill directory is itself reached through
/// a symlink in some layouts, so comparing a canonical target against a
/// non-canonical root would reject every legitimate read.
fn resolve_inside(root: &Path, rel: &str) -> Result<PathBuf, SkillRuntimeError> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err(SkillRuntimeError::InvalidPath {
            reason: "path must not be empty".to_owned(),
        });
    }
    let candidate = Path::new(rel);
    if candidate.is_absolute()
        || candidate.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_) | std::path::Component::RootDir
            )
        })
    {
        return Err(SkillRuntimeError::InvalidPath {
            reason: "path must be relative and must not contain '..'".to_owned(),
        });
    }

    let canonical_root = root.canonicalize().map_err(|e| SkillRuntimeError::ReadFailed {
        reason: format!("skill directory unreadable: {e}"),
    })?;
    let canonical_target =
        canonical_root
            .join(candidate)
            .canonicalize()
            .map_err(|e| SkillRuntimeError::InvalidPath {
                reason: format!("path does not resolve inside the skill: {e}"),
            })?;
    if !canonical_target.starts_with(&canonical_root) {
        return Err(SkillRuntimeError::InvalidPath {
            reason: "path resolves outside the skill directory".to_owned(),
        });
    }
    Ok(canonical_target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("cron");
        std::fs::create_dir_all(root.join("references")).unwrap();
        std::fs::write(root.join("SKILL.md"), "---\nname: cron\n---\nBody").unwrap();
        std::fs::write(root.join("references").join("notes.md"), "REFTOKEN").unwrap();
        (tmp, root)
    }

    #[test]
    fn a_plain_relative_file_resolves() {
        let (_tmp, root) = skill_dir();
        let target = resolve_inside(&root, "references/notes.md").unwrap();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "REFTOKEN");
    }

    #[test]
    fn every_traversal_shape_is_refused() {
        let (_tmp, root) = skill_dir();
        for bad in [
            "../../../.ssh/id_rsa",
            "references/../../escape.md",
            "/etc/passwd",
            "references/../../../etc/passwd",
            "..",
            "",
            "   ",
        ] {
            assert!(
                matches!(resolve_inside(&root, bad), Err(SkillRuntimeError::InvalidPath { .. })),
                "path {bad:?} must be refused as InvalidPath"
            );
        }
    }

    /// The case lexical checking cannot catch: nothing about `escape.md` looks
    /// suspicious, yet it leaves the skill directory.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_skill_directory_is_refused() {
        let (tmp, root) = skill_dir();
        let outside = tmp.path().join("outside-secret.md");
        std::fs::write(&outside, "OUTSIDE").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape.md")).unwrap();

        assert!(
            matches!(
                resolve_inside(&root, "escape.md"),
                Err(SkillRuntimeError::InvalidPath { .. })
            ),
            "a symlink whose canonical target leaves the skill dir must be refused"
        );
    }

    /// A skill directory reached THROUGH a symlink must still be readable -- the
    /// view directory is built exactly that way, so canonicalizing only the
    /// target would reject every legitimate read.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_skill_root_still_reads() {
        let (tmp, real_root) = skill_dir();
        let linked_root = tmp.path().join("linked-cron");
        std::os::unix::fs::symlink(&real_root, &linked_root).unwrap();

        let target = resolve_inside(&linked_root, "references/notes.md").unwrap();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "REFTOKEN");
    }
}
