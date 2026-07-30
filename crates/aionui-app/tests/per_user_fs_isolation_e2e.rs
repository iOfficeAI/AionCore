//! End-to-end: user data on disk is physically isolated per Core User, and
//! AionUi → AionPro adoption moves the default user's files to the adopter.
//!
//! Boots the real AppServices (AionPro mode) so the assertions run against the
//! production filesystem layout, not hand-built paths.

use std::path::Path;

use aionui_extension::skill_service::{
    import_skill_with_repo_for_user, materialize_skills_for_agent_with_repo_for_user,
};
use tower::ServiceExt;

fn write_skill(dir: &Path, name: &str, body: &str) {
    let skill_dir = dir.join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {body}\n---\n{body}\n"),
    )
    .unwrap();
}

async fn aionpro_services(data_dir: &Path) -> aionui_app::AppServices {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        identity_mode: aionui_app::IdentityMode::AionPro,
        bootstrap_secret: Some("bootstrap-secret".to_string()),
        data_dir: data_dir.to_path_buf(),
        work_dir: data_dir.to_path_buf(),
        ..Default::default()
    };
    aionui_app::AppServices::from_config(db, &config).await.unwrap()
}

async fn seed_user(services: &aionui_app::AppServices, username: &str) -> String {
    use aionui_db::IUserRepository;
    aionui_db::SqliteUserRepository::new(services.database.pool().clone())
        .create_user(username, "hash")
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn same_named_skill_is_physically_isolated_per_user() {
    let data = tempfile::tempdir().unwrap();
    let services = aionpro_services(data.path()).await;
    let paths = services.skill_paths.as_ref();
    let repo = services.skill_repo.as_ref();

    let user_a = seed_user(&services, "alice").await;
    let user_b = seed_user(&services, "bob").await;

    // Same-named skill, different bodies, imported by each user via the real
    // import path.
    let src = data.path().join("src");
    write_skill(&src.join("a"), "shared", "body-a");
    write_skill(&src.join("b"), "shared", "body-b");
    import_skill_with_repo_for_user(paths, repo, &user_a, &src.join("a").join("shared"))
        .await
        .unwrap();
    import_skill_with_repo_for_user(paths, repo, &user_b, &src.join("b").join("shared"))
        .await
        .unwrap();

    // Real on-disk tree is type-first per user, roots distinct.
    let dir_a = aionui_common::user_dir_name(&user_a).unwrap();
    let dir_b = aionui_common::user_dir_name(&user_b).unwrap();
    let root_a = paths.user_skills_dir.join("users").join(&dir_a).join("shared");
    let root_b = paths.user_skills_dir.join("users").join(&dir_b).join("shared");
    assert!(
        root_a.join("SKILL.md").exists(),
        "user A skill dir missing: {}",
        root_a.display()
    );
    assert!(
        root_b.join("SKILL.md").exists(),
        "user B skill dir missing: {}",
        root_b.display()
    );
    assert_ne!(root_a, root_b);
    assert!(
        std::fs::read_to_string(root_a.join("SKILL.md"))
            .unwrap()
            .contains("body-a")
    );
    assert!(
        std::fs::read_to_string(root_b.join("SKILL.md"))
            .unwrap()
            .contains("body-b")
    );

    // Materialization is isolated: A resolves its own body, never B's.
    let resolved_a =
        materialize_skills_for_agent_with_repo_for_user(paths, repo, &user_a, "conv-a", &["shared".into()])
            .await
            .unwrap();
    assert_eq!(resolved_a.len(), 1);
    assert_eq!(resolved_a[0].source_path, root_a);

    services.database.close().await;
}

#[tokio::test]
async fn provisioning_first_aionpro_user_adopts_default_user_files_on_disk() {
    let data = tempfile::tempdir().unwrap();
    let services = aionpro_services(data.path()).await;
    let (app, _runtime) = aionui_app::create_router_with_runtime(&services).await.unwrap();

    // A default-user skill sitting under the new layout, owned (in DB) by the
    // local default user, about to be adopted.
    let paths = services.skill_paths.as_ref();
    let repo = services.skill_repo.as_ref();
    let src = data.path().join("src");
    write_skill(&src, "legacy", "default-body");
    import_skill_with_repo_for_user(paths, repo, "system_default_user", &src.join("legacy"))
        .await
        .unwrap();
    let default_root = paths
        .user_skills_dir
        .join("users")
        .join("system_default_user")
        .join("legacy");
    assert!(default_root.exists());

    // Provision the first external (AionPro) user via the bootstrap-secret API
    // — this triggers DB adoption + the filesystem move.
    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("PUT")
                .uri("/api/auth/internal/external-users/pro-1")
                .header("content-type", "application/json")
                .header("x-aioncore-bootstrap-secret", "bootstrap-secret")
                .body(axum::body::Body::from(r#"{"user_type":"aionpro","username":"Pro"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let adopter = json["data"]["user_id"].as_str().unwrap();
    let adopter_dir = aionui_common::user_dir_name(adopter).unwrap();

    // Files physically moved to the adopter's root; default root emptied; the
    // skill row's path rewritten.
    let adopter_root = paths.user_skills_dir.join("users").join(&adopter_dir).join("legacy");
    assert!(
        adopter_root.join("SKILL.md").exists(),
        "adopted skill missing at {}",
        adopter_root.display()
    );
    assert!(
        !default_root.exists(),
        "default user root should be emptied after adoption"
    );
    let row = repo.find_by_name_for_user(adopter, "legacy").await.unwrap().unwrap();
    assert_eq!(row.path, adopter_root.to_string_lossy());

    services.database.close().await;
}
