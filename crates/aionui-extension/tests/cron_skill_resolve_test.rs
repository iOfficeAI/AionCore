use aionui_db::{ISkillRepository, IUserRepository, SqliteSkillRepository, SqliteUserRepository, UpsertSkillParams};
use aionui_extension::{resolve_skill_paths, skill_service};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("aionui-extension-{label}-{}-{nanos}", std::process::id()))
}

fn write_cron_skill(base: &std::path::Path, dir_name: &str, skill_name: &str) -> std::path::PathBuf {
    let skill_dir = base.join("cron").join("skills").join(dir_name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {skill_name}\ndescription: Saved cron skill\n---\nUse the saved steps."),
    )
    .unwrap();
    skill_dir
}

async fn make_skill_repo_with_users(usernames: &[&str]) -> (Arc<dyn ISkillRepository>, Vec<String>) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let user_repo = SqliteUserRepository::new(db.pool().clone());
    let mut ids = Vec::new();
    for username in usernames {
        ids.push(user_repo.create_user(username, "hash").await.unwrap().id);
    }
    (Arc::new(SqliteSkillRepository::new(db.pool().clone())), ids)
}

#[tokio::test]
async fn resolve_skill_paths_includes_cron_skills_dir() {
    let base = unique_temp_dir("cron-paths");
    std::fs::create_dir_all(&base).unwrap();

    let paths = resolve_skill_paths(&base, &base);
    assert_eq!(paths.cron_skills_dir, base.join("cron").join("skills"));

    std::fs::remove_dir_all(&base).unwrap();
}

#[tokio::test]
async fn materialize_does_not_probe_cron_dirs_without_a_repo_row() {
    let base = unique_temp_dir("cron-materialize-unauthorized");
    write_cron_skill(&base, "cron-job-123", "cron-job-123");

    let paths = resolve_skill_paths(&base, &base);
    // Per-job cron skill content must not resolve by name alone: without an
    // owned repo row there is no proof the requester owns the job.
    let resolved = skill_service::materialize_skills_for_agent(&paths, "conv-1", &["cron-job-123".to_owned()])
        .await
        .unwrap();
    assert!(resolved.is_empty(), "cron dir must not be probed: {resolved:?}");

    std::fs::remove_dir_all(&base).unwrap();
}

#[tokio::test]
async fn cron_skill_resolves_only_for_owning_user() {
    let base = unique_temp_dir("cron-materialize-owner");
    let skill_dir = write_cron_skill(&base, "cron-job-123", "number-analysis");
    let paths = resolve_skill_paths(&base, &base);
    let (repo, user_ids) = make_skill_repo_with_users(&["user-a", "user-b"]).await;
    let (user_a, user_b) = (user_ids[0].as_str(), user_ids[1].as_str());

    repo.upsert_for_user(
        user_a,
        UpsertSkillParams {
            name: "number-analysis",
            description: Some("Saved cron skill"),
            path: skill_dir.to_str().unwrap(),
            source: "cron",
            enabled: true,
        },
    )
    .await
    .unwrap();

    // The owning user resolves the skill through their repo row.
    let resolved = skill_service::materialize_skills_for_agent_with_repo_for_user(
        &paths,
        repo.as_ref(),
        user_a,
        "conv-a",
        &["number-analysis".to_owned()],
    )
    .await
    .unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].name, "number-analysis");
    assert_eq!(resolved[0].source_path, skill_dir);

    // Another user has no visible row and no disk fallback: no content leak.
    let leaked = skill_service::materialize_skills_for_agent_with_repo_for_user(
        &paths,
        repo.as_ref(),
        user_b,
        "conv-b",
        &["number-analysis".to_owned()],
    )
    .await
    .unwrap();
    assert!(
        leaked.is_empty(),
        "user-b must not resolve user-a's cron skill: {leaked:?}"
    );

    std::fs::remove_dir_all(&base).unwrap();
}
