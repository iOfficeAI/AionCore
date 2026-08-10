//! End-to-end reproduction of the AionUi → AionPro adoption path for
//! assistant rules, run against the REAL adoption code (no mocks):
//! real migrations, real `ensure_external_user`, real
//! `adopt_system_default_data` (one-shot stamp), real
//! `fs_adopt::adopt_user_filesystem`, real `AssistantService` reads.
//!
//! Two on-disk layouts are exercised:
//!  - `users/system_default_user/…` (post-user-scope writes)
//!  - the LEGACY root layout `assistant-rules/<id>.<locale>.md` (pre-user-scope
//!    installs upgraded in place, where no disk migration ever moved the files)

use std::path::Path;
use std::sync::Arc;

use aionui_assistant::service::AssistantServiceDeps;
use aionui_assistant::{AssistantService, BuiltinAssistantRegistry};
use aionui_db::{
    Database, ExternalUserProjection, IAssistantDefinitionRepository, IAssistantOverlayRepository,
    IAssistantOverrideRepository, IAssistantPreferenceRepository, IAssistantRepository, IProviderRepository,
    IUserRepository, SqliteAssistantDefinitionRepository, SqliteAssistantOverlayRepository,
    SqliteAssistantOverrideRepository, SqliteAssistantPreferenceRepository, SqliteAssistantRepository,
    SqliteProviderRepository, SqliteSkillRepository, SqliteUserRepository, UserType, init_database_memory,
};
use aionui_extension::{SkillPaths, fs_adopt};
use tempfile::TempDir;

const DEFAULT_USER_ID: &str = "system_default_user";
const ASSISTANT_ID: &str = "my-helper";
const RULE_BODY: &str = "act as my helper";

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

fn assistant_service(db: &Database, data_dir: &Path) -> AssistantService {
    let pool = db.pool().clone();
    let deps = AssistantServiceDeps {
        definition_repo: Arc::new(SqliteAssistantDefinitionRepository::new(pool.clone()))
            as Arc<dyn IAssistantDefinitionRepository>,
        state_repo: Arc::new(SqliteAssistantOverlayRepository::new(pool.clone()))
            as Arc<dyn IAssistantOverlayRepository>,
        preference_repo: Arc::new(SqliteAssistantPreferenceRepository::new(pool.clone()))
            as Arc<dyn IAssistantPreferenceRepository>,
        repo: Arc::new(SqliteAssistantRepository::new(pool.clone())) as Arc<dyn IAssistantRepository>,
        override_repo: Arc::new(SqliteAssistantOverrideRepository::new(pool.clone()))
            as Arc<dyn IAssistantOverrideRepository>,
        provider_repo: Arc::new(SqliteProviderRepository::new(pool.clone())) as Arc<dyn IProviderRepository>,
        builtin: Arc::new(BuiltinAssistantRegistry::load_embedded()),
        agent_catalog: None,
    };
    AssistantService::new(pool, deps, data_dir.to_path_buf())
}

/// Runs the REAL adoption sequence exactly as `ensure_external_user` in
/// aionui-auth does: provision external user → DB adoption (stamps the
/// one-shot marker) → filesystem adoption. Returns the adopter's user id.
async fn run_real_adoption(db: &Database, paths: &SkillPaths) -> String {
    let user_repo = SqliteUserRepository::new(db.pool().clone());
    let adopter = user_repo
        .ensure_external_user(UserType::Aionpro, "ext-user-1", ExternalUserProjection::default())
        .await
        .expect("provision external user");

    let moved_rows = user_repo
        .adopt_system_default_data(&adopter.id)
        .await
        .expect("db adoption");
    assert!(
        user_repo
            .is_default_data_adopter(&adopter.id)
            .await
            .expect("marker query"),
        "adopter must be stamped as the one-shot adopter (moved_rows={moved_rows})"
    );

    let skill_repo = SqliteSkillRepository::new(db.pool().clone());
    fs_adopt::adopt_user_filesystem(paths, &skill_repo, &adopter.id).await;
    adopter.id
}

/// Layout used by post-user-scope versions: the rule file lives under
/// `assistant-rules/users/system_default_user/`.
#[tokio::test]
async fn adopted_user_reads_rule_written_under_default_user_dir() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_at(tmp.path());
    let db = init_database_memory().await.unwrap();
    let service = assistant_service(&db, tmp.path());

    let rules_dir = paths.assistant_rules_dir.join("users").join(DEFAULT_USER_ID);
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(rules_dir.join(format!("{ASSISTANT_ID}.zh-CN.md")), RULE_BODY).unwrap();

    // Sanity: the local default user sees the rule before adoption.
    let before = service
        .read_rule_for_user(DEFAULT_USER_ID, ASSISTANT_ID, Some("zh-CN"))
        .await
        .unwrap();
    assert_eq!(before, RULE_BODY, "default user must see the rule pre-adoption");

    let adopter = run_real_adoption(&db, &paths).await;

    let after = service
        .read_rule_for_user(&adopter, ASSISTANT_ID, Some("zh-CN"))
        .await
        .unwrap();
    assert_eq!(
        after, RULE_BODY,
        "adopter must see the adopted rule (users/system_default_user layout)"
    );
}

/// Layout left behind by pre-user-scope installs upgraded in place: the rule
/// file sits at the LEGACY root `assistant-rules/<id>.<locale>.md` with no
/// `users/` segment. The default user still reads it through the legacy
/// fallback — the question is whether the adopter does after adoption.
#[tokio::test]
async fn adopted_user_reads_rule_left_at_legacy_root() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_at(tmp.path());
    let db = init_database_memory().await.unwrap();
    let service = assistant_service(&db, tmp.path());

    std::fs::create_dir_all(&paths.assistant_rules_dir).unwrap();
    std::fs::write(
        paths.assistant_rules_dir.join(format!("{ASSISTANT_ID}.zh-CN.md")),
        RULE_BODY,
    )
    .unwrap();

    // Sanity: the local default user sees the legacy-root rule before adoption.
    let before = service
        .read_rule_for_user(DEFAULT_USER_ID, ASSISTANT_ID, Some("zh-CN"))
        .await
        .unwrap();
    assert_eq!(
        before, RULE_BODY,
        "default user must see the legacy-root rule pre-adoption"
    );

    let adopter = run_real_adoption(&db, &paths).await;

    let after = service
        .read_rule_for_user(&adopter, ASSISTANT_ID, Some("zh-CN"))
        .await
        .unwrap();
    assert_eq!(
        after, RULE_BODY,
        "adopter must see the adopted rule (legacy root layout, pre-user-scope upgrade)"
    );
}

/// Users who already adopted on a build WITHOUT the legacy-root move are left
/// with: stamp set, `users/system_default_user` swept, but the legacy root
/// files still in place — and their rules unreadable. The auth layer re-runs
/// the filesystem adoption on every login of the stamped adopter
/// (best-effort catch-up for partial moves), so the next login on a fixed
/// build must heal them without any manual intervention.
#[tokio::test]
async fn already_adopted_user_heals_on_next_login_rerun() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_at(tmp.path());
    let db = init_database_memory().await.unwrap();
    let service = assistant_service(&db, tmp.path());

    // State as left behind by the old build: adoption already happened (stamp
    // set, DB rows moved) while the legacy-root rule file was NOT moved.
    let user_repo = SqliteUserRepository::new(db.pool().clone());
    let adopter = user_repo
        .ensure_external_user(UserType::Aionpro, "ext-user-1", ExternalUserProjection::default())
        .await
        .expect("provision external user");
    user_repo
        .adopt_system_default_data(&adopter.id)
        .await
        .expect("db adoption");
    assert!(user_repo.is_default_data_adopter(&adopter.id).await.unwrap());

    std::fs::create_dir_all(&paths.assistant_rules_dir).unwrap();
    std::fs::write(
        paths.assistant_rules_dir.join(format!("{ASSISTANT_ID}.zh-CN.md")),
        RULE_BODY,
    )
    .unwrap();

    // Broken state: the adopter cannot read the rule.
    let broken = service
        .read_rule_for_user(&adopter.id, ASSISTANT_ID, Some("zh-CN"))
        .await
        .unwrap();
    assert_eq!(broken, "", "pre-fix state: adopter must NOT see the legacy-root rule");

    // Next login of the stamped adopter re-runs the filesystem adoption.
    let skill_repo = SqliteSkillRepository::new(db.pool().clone());
    fs_adopt::adopt_user_filesystem(&paths, &skill_repo, &adopter.id).await;

    let healed = service
        .read_rule_for_user(&adopter.id, ASSISTANT_ID, Some("zh-CN"))
        .await
        .unwrap();
    assert_eq!(
        healed, RULE_BODY,
        "catch-up rerun on next login must heal the adopted rules"
    );
}
