//! HTTP integration tests for the built-in skills migration surface:
//! `/api/skills/builtin-skill`, `/api/skills`, and the symlink-contract
//! `/api/skills/materialize-for-agent` (POST).
//!
//! Covers the spec's §9.2 scenarios end-to-end through
//! `aionui_app::create_router_with_states` against an in-memory DB.

mod common;

use std::sync::Arc;

use aionui_app::{ModuleStates, build_module_states, create_router_with_states};
use aionui_db::init_database_memory;
use aionui_extension::{ExternalPathsManager, SkillPaths, SkillRouterState};
use axum::http::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

use common::{body_json, get_with_token, json_with_token, setup_and_login};

// ---------------------------------------------------------------------------
// Fixture — build router with embedded-corpus paths rooted at a temp dir
// ---------------------------------------------------------------------------

struct Fixture {
    app: axum::Router,
    token: String,
    csrf: String,
    data_dir: std::path::PathBuf,
    _tmp: TempDir,
}

/// Build an app whose skill state points at a freshly materialized
/// builtin-skills tree rooted at a temp `data_dir`. `write_skill` can
/// still seed user skills under `{data_dir}/skills/`.
async fn fixture_embedded() -> Fixture {
    // Ensure no env override interferes.
    // SAFETY: tests in this file may mutate this env var across async
    // tasks on the same process. Rust 2024 marks `remove_var` as unsafe
    // for exactly that reason. The var is only read at router-state
    // construction time, and each test calls `fixture_embedded` once at
    // the top, so the mutation is race-free in practice.
    unsafe {
        std::env::remove_var("AIONUI_BUILTIN_SKILLS_PATH");
    }

    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().to_path_buf();

    // Materialize the embedded corpus onto the temp data dir so the
    // per-test router can read it just like production would.
    aionui_extension::materialize_if_needed(&data_dir, aionui_extension::builtin_skills_corpus(), "test-fixture")
        .await
        .expect("failed to materialize embedded builtin skills for test fixture");

    let db = init_database_memory().await.unwrap();
    let services = aionui_app::AppServices::from_config(db, &aionui_app::AppConfig::default())
        .await
        .unwrap();
    let (mut states, _): (ModuleStates, _) = build_module_states(&services).await.expect("build module states");

    // Replace the skill state with a deterministic one rooted at tmp.
    // `build_module_states` builds a state pointing at `~/.aionui/`,
    // which is fine for production but unsuitable here.
    let skill_paths = SkillPaths {
        data_dir: data_dir.clone(),
        user_skills_dir: data_dir.join("skills"),
        cron_skills_dir: data_dir.join("cron").join("skills"),
        builtin_skills_dir: data_dir.join("builtin-skills"),
        builtin_rules_dir: data_dir.join("builtin-rules"),
        assistant_rules_dir: data_dir.join("assistant-rules"),
        assistant_skills_dir: data_dir.join("assistant-skills"),
    };
    let ext_paths_mgr = Arc::new(ExternalPathsManager::with_file(data_dir.join("paths.json")).await);
    let skill_repo = Arc::new(aionui_db::SqliteSkillRepository::new(services.database.pool().clone()));
    states.skill = SkillRouterState {
        skill_paths,
        skill_repo: skill_repo.clone(),
        external_paths_manager: ext_paths_mgr,
        assistant_dispatcher: states.skill.assistant_dispatcher.clone(),
    };
    aionui_extension::sync_skill_catalog_into_repo(&states.skill.skill_paths, skill_repo.as_ref())
        .await
        .expect("sync embedded builtin skill catalog");

    let mut app = create_router_with_states(&services, states);
    let (token, csrf) = setup_and_login(&mut app, &services, "builtin-e2e", "StrongP@ss1").await;

    Fixture {
        app,
        token,
        csrf,
        data_dir,
        _tmp: tmp,
    }
}

// ===========================================================================
// GET /api/skills — embedded corpus
// ===========================================================================

#[tokio::test]
async fn unified_skill_list_includes_auto_inject_entries_from_embedded_corpus() {
    let fx = fixture_embedded().await;

    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/skills", &fx.token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    let arr = json["data"].as_array().unwrap();
    let auto_items: Vec<&Value> = arr
        .iter()
        .filter(|item| {
            item["source"] == "builtin"
                && item["relative_location"]
                    .as_str()
                    .is_some_and(|location| location.starts_with("auto-inject/"))
        })
        .collect();
    assert!(
        auto_items.len() >= 3,
        "expected ≥3 auto-inject entries, got {}",
        auto_items.len()
    );
    let names: Vec<&str> = auto_items.iter().filter_map(|item| item["name"].as_str()).collect();
    assert!(
        !names.contains(&"aionui-skills"),
        "aionui-skills should not be shipped as an auto-inject builtin skill: {names:?}",
    );
    for item in auto_items {
        assert!(item["name"].is_string());
        assert!(item["description"].is_string());
        let loc = item["relative_location"].as_str().unwrap();
        assert!(loc.starts_with("auto-inject/"), "location={loc}");
        assert!(loc.ends_with("/SKILL.md"));
    }
}

// ===========================================================================
// POST /api/skills/builtin-skill
// ===========================================================================

#[tokio::test]
async fn builtin_skill_read_auto_inject_returns_frontmatter_content() {
    let fx = fixture_embedded().await;

    let resp = fx
        .app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/skills/builtin-skill",
            json!({"file_name": "auto-inject/cron/SKILL.md"}),
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    let content = json["data"].as_str().unwrap();
    assert!(content.trim_start().starts_with("---"), "content={content}");
}

#[tokio::test]
async fn builtin_skill_read_opt_in_returns_frontmatter_content() {
    let fx = fixture_embedded().await;

    // mermaid is a well-known opt-in skill in the corpus.
    let resp = fx
        .app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/skills/builtin-skill",
            json!({"file_name": "mermaid/SKILL.md"}),
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let content = json["data"].as_str().unwrap();
    assert!(!content.is_empty(), "mermaid SKILL.md is empty");
}

#[tokio::test]
async fn builtin_skill_missing_file_returns_empty_string() {
    let fx = fixture_embedded().await;

    let resp = fx
        .app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/skills/builtin-skill",
            json!({"file_name": "unknown/SKILL.md"}),
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"], "");
}

#[tokio::test]
async fn builtin_skill_rejects_traversal() {
    let fx = fixture_embedded().await;

    for bad in ["../etc/passwd", "/etc/passwd", "auto-inject/../../escape", ""] {
        let resp = fx
            .app
            .clone()
            .oneshot(json_with_token(
                "POST",
                "/api/skills/builtin-skill",
                json!({"file_name": bad}),
                &fx.token,
                &fx.csrf,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "file_name={bad:?} should be rejected",
        );
    }
}

// ===========================================================================
// GET /api/skills — merged list with relative_location for builtin
// ===========================================================================

#[tokio::test]
async fn list_skills_builtin_entries_carry_relative_location() {
    let fx = fixture_embedded().await;

    // Seed one user skill so the merge is non-trivial.
    let source_dir = fx.data_dir.join("import-source").join("my-custom");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(
        source_dir.join("SKILL.md"),
        "---\nname: my-custom\ndescription: Custom skill for test\n---\nBody",
    )
    .unwrap();
    let resp = fx
        .app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/skills/import",
            json!({ "skill_path": source_dir.to_str().unwrap() }),
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = fx
        .app
        .clone()
        .oneshot(get_with_token("/api/skills", &fx.token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let arr = json["data"].as_array().unwrap();

    let mut saw_builtin = false;
    let mut saw_custom = false;
    for item in arr {
        match item["source"].as_str().unwrap() {
            "builtin" => {
                saw_builtin = true;
                let rel = item["relative_location"].as_str().unwrap();
                let normalized_rel = rel.replace('\\', "/");
                assert!(normalized_rel.ends_with("/SKILL.md"));
                let loc = item["location"].as_str().unwrap();
                assert!(
                    loc.contains("builtin-skills"),
                    "builtin location should live under builtin-skills dir: {loc}"
                );
                // The builtin-skills tree is materialized at startup, so
                // SKILL.md must already exist on disk.
                assert!(
                    std::path::Path::new(loc).exists(),
                    "builtin skill file missing on disk: {loc}"
                );
            }
            "custom" => {
                saw_custom = true;
                assert!(item.get("relative_location").is_none());
                assert_eq!(item["name"], "my-custom");
            }
            "cron" => {
                assert!(item.get("relative_location").is_none());
                let loc = item["location"].as_str().unwrap();
                let normalized_loc = loc.replace('\\', "/");
                assert!(
                    normalized_loc.ends_with("/SKILL.md"),
                    "cron skill location should point at SKILL.md: {loc}"
                );
            }
            other => panic!("unexpected source: {other}"),
        }
    }
    assert!(saw_builtin, "expected at least one builtin entry");
    assert!(saw_custom, "expected the seeded custom entry");
}

// ===========================================================================
// POST /api/skills/materialize-for-agent
// ===========================================================================

#[tokio::test]
async fn materialize_for_agent_returns_source_path_for_auto_inject_skill() {
    // Post-snapshot contract: `materialize-for-agent` resolves each
    // requested name to its on-disk source directory without copying.
    // The frontend symlinks `source_path` into the CLI's native skills
    // dir. `cron` lives under `auto-inject/cron/` in the builtin tree.
    let fx = fixture_embedded().await;

    let resp = fx
        .app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/skills/materialize-for-agent",
            json!({
                "conversation_id": "conv-happy",
                "skills": ["cron"],
            }),
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json: Value = body_json(resp).await;
    let skills = json["data"]["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["name"], "cron");
    let source_path = skills[0]["source_path"].as_str().unwrap();
    let path = std::path::Path::new(source_path);
    assert!(path.is_absolute(), "source_path must be absolute: {source_path}");
    assert!(path.is_dir(), "source_path must exist: {source_path}");
    assert!(
        path.join("SKILL.md").exists(),
        "source_path must contain SKILL.md at {source_path}",
    );
    // It must live under the builtin tree, not under a
    // per-conversation copy dir.
    assert!(
        source_path.contains("builtin-skills"),
        "expected auto-inject source under builtin-skills, got {source_path}",
    );
}

#[tokio::test]
async fn materialize_for_agent_returns_source_path_for_opt_in_skill() {
    let fx = fixture_embedded().await;

    let resp = fx
        .app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/skills/materialize-for-agent",
            json!({
                "conversation_id": "conv-opt",
                "enabled_skills": ["mermaid"],
            }),
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json: Value = body_json(resp).await;
    let skills = json["data"]["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["name"], "mermaid");
    let source_path = skills[0]["source_path"].as_str().unwrap();
    assert!(
        std::path::Path::new(source_path).join("SKILL.md").exists(),
        "mermaid source_path must exist: {source_path}",
    );
}

#[tokio::test]
async fn materialize_for_agent_silently_skips_unknown_skill() {
    let fx = fixture_embedded().await;

    let resp = fx
        .app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/skills/materialize-for-agent",
            json!({
                "conversation_id": "conv-bogus",
                "enabled_skills": ["this-does-not-exist"],
            }),
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json: Value = body_json(resp).await;
    let skills = json["data"]["skills"].as_array().unwrap();
    // Unknown skill is silently dropped.
    assert!(skills.is_empty(), "unknown skills must be silently omitted");
}

#[tokio::test]
async fn materialize_for_agent_does_not_touch_data_dir() {
    // Symlink-contract guardrail: the backend no longer writes anywhere
    // under {data_dir}/agent-skills/ or {data_dir}/conversations/ for
    // materialize-for-agent — it only reads the source tree.
    let fx = fixture_embedded().await;

    fx.app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/skills/materialize-for-agent",
            json!({"conversation_id": "conv-noop", "enabled_skills": ["cron"]}),
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();

    assert!(!fx.data_dir.join("agent-skills").exists());
    assert!(!fx.data_dir.join("conversations").join("conv-noop").exists());
}

#[tokio::test]
async fn materialize_for_agent_returns_sorted_list() {
    let fx = fixture_embedded().await;

    let resp = fx
        .app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/skills/materialize-for-agent",
            json!({
                "conversation_id": "conv-sorted",
                "skills": ["mermaid", "cron"],
            }),
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json: Value = body_json(resp).await;
    let skills = json["data"]["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 2);
    assert_eq!(skills[0]["name"], "cron");
    assert_eq!(skills[1]["name"], "mermaid");
}

#[tokio::test]
async fn materialize_for_agent_rejects_empty_conversation_id() {
    let fx = fixture_embedded().await;

    let resp = fx
        .app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/skills/materialize-for-agent",
            json!({"conversation_id": "", "enabled_skills": []}),
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn materialize_for_agent_rejects_traversal_in_conversation_id() {
    let fx = fixture_embedded().await;

    let resp = fx
        .app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/skills/materialize-for-agent",
            json!({"conversation_id": "../evil", "enabled_skills": []}),
            &fx.token,
            &fx.csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// DELETE /api/skills/materialize-for-agent/:conversation_id removed — the
// symlink contract has nothing to clean up on the backend side.
// ===========================================================================
