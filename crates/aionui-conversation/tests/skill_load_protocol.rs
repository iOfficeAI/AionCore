//! Channel B (`[LOAD_SKILL: name]`) end-to-end through the middleware.
//!
//! The scenario here is the one that made P2 worth fixing on its own, ahead of
//! any directory allow-listing: it is not "the referenced file is missing" but
//! "the workspace happens to hold a file with the SAME relative name". Without a
//! stated skill root, the agent resolves `references/workflows.md` against its
//! CWD and reads unrelated user content while believing it read the skill's.

use std::path::PathBuf;

use aionui_conversation::response_middleware::{ISkillLoadService, MessageMiddleware};
use aionui_conversation::skill_resolver::LoadedAgentSkill;

struct FixedLoader {
    root: PathBuf,
}

#[async_trait::async_trait]
impl ISkillLoadService for FixedLoader {
    async fn load_skill_bodies(&self, names: &[String]) -> Vec<LoadedAgentSkill> {
        names
            .iter()
            .map(|name| LoadedAgentSkill {
                name: name.clone(),
                // The real skill-creator body references its own files relatively.
                body: "See references/workflows.md and run scripts/init_skill.py".to_owned(),
                source_path: self.root.clone(),
            })
            .collect()
    }
}

#[tokio::test]
async fn a_same_named_workspace_file_does_not_shadow_the_skill_reference() {
    let tmp = tempfile::TempDir::new().unwrap();

    // The workspace holds a DECOY at the exact relative path the body references.
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("references")).unwrap();
    std::fs::write(workspace.join("references").join("workflows.md"), "USER-SECRET").unwrap();

    let skill_root = tmp.path().join("sources").join("skill-creator");
    std::fs::create_dir_all(skill_root.join("references")).unwrap();
    std::fs::write(skill_root.join("references").join("workflows.md"), "SKILL-CONTENT-9931").unwrap();

    let result = MessageMiddleware::new_with_skill_loader(Some(Box::new(FixedLoader {
        root: skill_root.clone(),
    })))
    .process("Need [LOAD_SKILL: skill-creator]", "user_a", "conv_1")
    .await;

    let injected = &result.system_responses[0];
    assert!(
        injected.contains(&skill_root.display().to_string()),
        "the injected text must anchor relative references to the skill root: {injected}"
    );
    assert!(
        !injected.contains(&workspace.display().to_string()),
        "nothing may point the agent at the workspace copy: {injected}"
    );
    // The body's own relative references still travel verbatim -- the fix adds a
    // root, it does not rewrite the skill.
    assert!(injected.contains("references/workflows.md"));
    assert!(injected.contains("scripts/init_skill.py"));
}

/// Channel B must work with no HTTP endpoint and no command execution: that is
/// the whole reason it stays as the fallback leg. This exercise touches only the
/// middleware, so a green result here means an agent in a read-only or
/// plan-permission mode can still load a skill.
#[tokio::test]
async fn channel_b_needs_no_command_execution_or_endpoint() {
    let tmp = tempfile::TempDir::new().unwrap();
    let skill_root = tmp.path().join("cron");
    std::fs::create_dir_all(&skill_root).unwrap();

    let result = MessageMiddleware::new_with_skill_loader(Some(Box::new(FixedLoader {
        root: skill_root.clone(),
    })))
    .process(
        "<think>this needs [LOAD_SKILL: cron]</think>Working on it.",
        "user_a",
        "conv_1",
    )
    .await;

    assert_eq!(result.system_responses.len(), 1);
    assert!(result.system_responses[0].contains("[Skill: cron]"));
    assert_eq!(result.message, "Working on it.");
}
