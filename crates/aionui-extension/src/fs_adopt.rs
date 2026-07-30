//! Filesystem side of AionUi → AionPro adoption.
//!
//! DB adoption (`adopt_system_default_data`) re-owns the local default user's
//! rows to the first external (AionPro) user. Because per-user data now lives
//! under `X/users/{user_dir}/`, the on-disk files must move with the DB
//! ownership: `X/users/system_default_user/*` → `X/users/{adopter_dir}/*`,
//! and any stored absolute `path` column (`skills.path`) rewritten to match.
//!
//! Same-volume `rename` is a metadata operation (O(entries), not O(bytes));
//! a cross-volume move falls back to copy-then-remove. Best-effort: a failure
//! is logged and skipped, never fatal to provisioning.

use std::path::{Path, PathBuf};

use aionui_db::ISkillRepository;
use tracing::{info, warn};

use crate::skill_service::SkillPaths;

const DEFAULT_USER_ID: &str = "system_default_user";

/// Business directories (under a base root) that hold per-user data as
/// `X/users/{user_dir}/…`. Skills live under `data_dir/skills`; assistant
/// data under `data_dir/{assistant-rules,assistant-skills,assistant-avatars}`.
fn per_user_base_dirs(paths: &SkillPaths) -> Vec<PathBuf> {
    vec![
        paths.user_skills_dir.clone(),
        paths.assistant_rules_dir.clone(),
        paths.assistant_skills_dir.clone(),
        paths.data_dir.join("assistant-avatars"),
    ]
}

/// Move `system_default_user`'s on-disk files to `adopter`'s per-user roots and
/// rewrite `skills.path`. Idempotent: entries already present at the
/// destination are left as-is. Returns the number of top-level entries moved.
pub async fn adopt_user_filesystem(
    paths: &SkillPaths,
    skill_repo: &dyn ISkillRepository,
    adopter_user_id: &str,
) -> u64 {
    let Ok(adopter_dir) = aionui_common::user_dir_name(adopter_user_id) else {
        warn!(adopter = %adopter_user_id, "fs adoption skipped: adopter id is not filename-safe");
        return 0;
    };
    if adopter_dir == DEFAULT_USER_ID {
        return 0; // adopting into the default user itself is a no-op
    }

    let mut moved = 0u64;
    for base in per_user_base_dirs(paths) {
        let src = base.join("users").join(DEFAULT_USER_ID);
        let dst = base.join("users").join(&adopter_dir);
        moved += move_dir_contents(&src, &dst);
    }

    rewrite_adopted_skill_paths(paths, skill_repo, adopter_user_id, &adopter_dir).await;

    if moved > 0 {
        info!(adopter = %adopter_user_id, entries = moved, "adopted system_default_user filesystem into external user");
    }
    moved
}

/// Move every top-level entry from `src` into `dst`. Missing `src` → 0.
fn move_dir_contents(src: &Path, dst: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(src) else {
        return 0;
    };
    if let Err(err) = std::fs::create_dir_all(dst) {
        warn!(dst = %dst.display(), error = %err, "fs adoption: create destination failed");
        return 0;
    }

    let mut moved = 0u64;
    for entry in entries.flatten() {
        let from = entry.path();
        let Some(name) = from.file_name() else { continue };
        let to = dst.join(name);
        if to.exists() {
            continue; // idempotent: destination already has it
        }
        match std::fs::rename(&from, &to) {
            Ok(()) => moved += 1,
            Err(err) if is_cross_device(&err) => {
                if copy_then_remove(&from, &to) {
                    moved += 1;
                }
            }
            Err(err) => {
                warn!(from = %from.display(), to = %to.display(), error = %err, "fs adoption: move failed");
            }
        }
    }
    moved
}

fn is_cross_device(err: &std::io::Error) -> bool {
    // EXDEV = 18 on unix; also surfaced as CrossesDevices on newer std.
    err.raw_os_error() == Some(18)
}

fn copy_then_remove(from: &Path, to: &Path) -> bool {
    let copied = if from.is_dir() {
        copy_dir_recursive(from, to).is_ok()
    } else {
        std::fs::copy(from, to).map(|_| ()).is_ok()
    };
    if !copied {
        warn!(from = %from.display(), to = %to.display(), "fs adoption: cross-device copy failed");
        return false;
    }
    let removed = if from.is_dir() {
        std::fs::remove_dir_all(from)
    } else {
        std::fs::remove_file(from)
    };
    if let Err(err) = removed {
        warn!(from = %from.display(), error = %err, "fs adoption: source cleanup after copy failed");
    }
    true
}

fn copy_dir_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let child_from = entry.path();
        let child_to = to.join(entry.file_name());
        if child_from.is_dir() {
            copy_dir_recursive(&child_from, &child_to)?;
        } else {
            std::fs::copy(&child_from, &child_to)?;
        }
    }
    Ok(())
}

/// Rewrite `skills.path` for rows now owned by the adopter whose stored path
/// still points at the old `users/system_default_user/` location.
async fn rewrite_adopted_skill_paths(
    paths: &SkillPaths,
    skill_repo: &dyn ISkillRepository,
    adopter_user_id: &str,
    adopter_dir: &str,
) {
    let old_root = paths.user_skills_dir.join("users").join(DEFAULT_USER_ID);
    let new_root = paths.user_skills_dir.join("users").join(adopter_dir);

    let rows = match skill_repo.list_for_user(adopter_user_id).await {
        Ok(rows) => rows,
        Err(err) => {
            warn!(error = %err, "fs adoption: list adopter skills failed");
            return;
        }
    };
    for row in rows {
        if row.user_id.as_deref() != Some(adopter_user_id) {
            continue; // skip global rows surfaced by the user-scoped list
        }
        let Ok(rel) = Path::new(&row.path).strip_prefix(&old_root) else {
            continue;
        };
        let new_path = new_root.join(rel);
        let params = aionui_db::UpsertSkillParams {
            name: &row.name,
            description: row.description.as_deref(),
            path: &new_path.to_string_lossy(),
            source: &row.source,
            enabled: row.enabled,
        };
        if let Err(err) = skill_repo.upsert_for_user(adopter_user_id, params).await {
            warn!(skill = %row.name, error = %err, "fs adoption: skill path rewrite failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::{SqliteSkillRepository, UpsertSkillParams, init_database_memory};
    use tempfile::TempDir;

    fn paths_at(root: &Path) -> SkillPaths {
        SkillPaths {
            data_dir: root.to_path_buf(),
            user_skills_dir: root.join("skills"),
            cron_skills_dir: root.join("cron").join("skills"),
            builtin_skills_dir: root.join("builtin-skills"),
            builtin_rules_dir: root.join("builtin-rules"),
            assistant_rules_dir: root.join("assistant-rules"),
            assistant_skills_dir: root.join("assistant-skills"),
        }
    }

    async fn seed_user(db: &aionui_db::Database) -> String {
        use aionui_db::IUserRepository;
        aionui_db::SqliteUserRepository::new(db.pool().clone())
            .create_user("adopter", "hash")
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn adoption_moves_files_and_rewrites_skill_path() {
        let tmp = TempDir::new().unwrap();
        let paths = paths_at(tmp.path());
        let db = init_database_memory().await.unwrap();
        let adopter = seed_user(&db).await;
        let adopter = adopter.as_str();
        let adopter_dir = aionui_common::user_dir_name(adopter).unwrap();
        let adopter_dir = adopter_dir.as_str();

        // Seed a default-user skill dir + assistant rule file on disk.
        let old_skill = paths.user_skills_dir.join("users").join(DEFAULT_USER_ID).join("mine");
        std::fs::create_dir_all(&old_skill).unwrap();
        std::fs::write(old_skill.join("SKILL.md"), b"body").unwrap();
        let old_rule_dir = paths.assistant_rules_dir.join("users").join(DEFAULT_USER_ID);
        std::fs::create_dir_all(&old_rule_dir).unwrap();
        std::fs::write(old_rule_dir.join("a.zh-CN.md"), b"rule").unwrap();

        let repo = SqliteSkillRepository::new(db.pool().clone());
        // Skill row owned by adopter (post-DB-adoption) but path at old location.
        repo.upsert_for_user(
            adopter,
            UpsertSkillParams {
                name: "mine",
                description: None,
                path: &old_skill.to_string_lossy(),
                source: "user",
                enabled: true,
            },
        )
        .await
        .unwrap();

        let moved = adopt_user_filesystem(&paths, &repo, adopter).await;
        assert!(moved >= 2, "skill dir + rule file must move, moved={moved}");

        // Files physically at adopter's roots, gone from default.
        let new_skill = paths.user_skills_dir.join("users").join(adopter_dir).join("mine");
        assert!(new_skill.join("SKILL.md").exists());
        assert!(!old_skill.exists());
        assert!(
            paths
                .assistant_rules_dir
                .join("users")
                .join(adopter_dir)
                .join("a.zh-CN.md")
                .exists()
        );

        // skills.path rewritten to the new location.
        let row = repo.find_by_name_for_user(adopter, "mine").await.unwrap().unwrap();
        assert_eq!(row.path, new_skill.to_string_lossy());

        // Idempotent re-run moves nothing.
        assert_eq!(adopt_user_filesystem(&paths, &repo, adopter).await, 0);
    }
}
