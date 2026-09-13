//! Shared first-message prefix injection for ACP agents.
//!
//! Takes the conversation's first-message content and produces a new content
//! string that may include an `[Assistant Rules]` block with preset context
//! and a skills index. The shape depends on whether the agent's native CLI
//! can read skills from the workspace directly.

use std::sync::Arc;

use aionui_api_types::SkillDeliveryMode;

use crate::capability::skill_manager::{AcpSkillManager, prepare_first_message_with_skills_index};

/// Configuration for the first-message injector.
pub struct InjectionConfig<'a> {
    /// Core user that owns the conversation and its user-scoped skills.
    pub user_id: &'a str,
    /// Preset context (assistant-level system prompt injection).
    pub preset_context: Option<&'a str>,
    /// Resolved skill names (snapshot from `conversation.extra.skills`).
    pub skills: &'a [String],
    /// How this vendor receives skills.
    ///
    /// This replaced a `native_skill_support: bool` derived from
    /// `native_skills_dirs.is_some()`. That signal conflated two different
    /// things — "declares a workspace skills directory" and "can discover
    /// skills natively" — so a vendor with a declared directory but no working
    /// discovery got LIGHT injection and therefore no skills at all.
    pub delivery_mode: SkillDeliveryMode,
}

/// Produce the content string to send as the first prompt.
///
/// Two states, from three modes:
///  * `Argv` / `Protocol` — LIGHT: only `preset_context`. The CLI owns skill
///    discovery, and injecting an index would duplicate the name+description
///    that a plugin-registering CLI already adds always-on.
///  * `Injected` — the skills index plus the dual-channel instructions.
pub async fn inject_first_message_prefix(
    content: &str,
    manager: &Arc<AcpSkillManager>,
    config: InjectionConfig<'_>,
) -> String {
    if is_light_mode(&config.delivery_mode) {
        return match config.preset_context {
            Some(ctx) if !ctx.is_empty() => {
                format!("[Assistant Rules]\n{ctx}\n[/Assistant Rules]\n\n{content}")
            }
            _ => content.to_string(),
        };
    }

    let skills = manager.discover_by_names_for_user(config.user_id, config.skills).await;
    let has_context = config.preset_context.is_some_and(|s| !s.is_empty());
    if skills.is_empty() && !has_context {
        return content.to_string();
    }
    prepare_first_message_with_skills_index(content, &skills, config.preset_context)
}

/// The same block [`inject_first_message_prefix`] would prepend, WITHOUT the
/// user's content appended — `None` when there is nothing to inject.
///
/// For backends that carry the prefix separately from the turn's message rather
/// than concatenating it once: agy re-invokes its CLI per turn, so its rules
/// belong on the first invocation only. Sharing this function with the
/// concatenating path is what keeps the two from drifting into different wording.
pub async fn compose_injected_prefix(manager: &Arc<AcpSkillManager>, config: InjectionConfig<'_>) -> Option<String> {
    // A sentinel the caller can split on: `prepare_first_message_with_skills_index`
    // owns the block's exact layout, so re-deriving it here would be a second
    // copy of that layout.
    const SENTINEL: &str = "\u{0}AIONUI_CONTENT\u{0}";
    let composed = inject_first_message_prefix(SENTINEL, manager, config).await;
    match composed.strip_suffix(SENTINEL) {
        // Nothing was prepended: the content came back untouched.
        None => None,
        Some(prefix) if prefix.trim().is_empty() => None,
        Some(prefix) => Some(prefix.trim_end().to_owned()),
    }
}

fn is_light_mode(mode: &SkillDeliveryMode) -> bool {
    matches!(mode, SkillDeliveryMode::Argv | SkillDeliveryMode::Protocol)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_extension::{BUILTIN_SKILLS_ENV_VAR, resolve_skill_paths};
    use tempfile::TempDir;

    fn test_mgr(base: &std::path::Path) -> Arc<AcpSkillManager> {
        let paths = Arc::new(resolve_skill_paths(base, base));
        AcpSkillManager::new(paths)
    }

    /// Point the embedded corpus at an empty dir so tests don't pick up
    /// real auto-inject builtin skills.
    struct EmptyBuiltinGuard;
    impl EmptyBuiltinGuard {
        fn new(empty_path: &std::path::Path) -> Self {
            unsafe {
                std::env::set_var(BUILTIN_SKILLS_ENV_VAR, empty_path);
            }
            Self
        }
    }
    impl Drop for EmptyBuiltinGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(BUILTIN_SKILLS_ENV_VAR);
            }
        }
    }

    fn skill_corpus(base: &std::path::Path) {
        let auto = base.join("auto-inject");
        std::fs::create_dir_all(auto.join("cron")).unwrap();
        std::fs::write(
            auto.join("cron").join("SKILL.md"),
            "---\nname: cron\ndescription: Schedule stuff\n---\nBody.",
        )
        .unwrap();
    }

    /// BOTH layer-1 modes must be light. `Protocol` used to fall to the heavy
    /// branch because the old signal was a single bool: codex would then have
    /// received a duplicate index on top of what its CLI already injects.
    #[tokio::test]
    async fn protocol_mode_is_light_just_like_argv() {
        let tmp = TempDir::new().unwrap();
        skill_corpus(tmp.path());
        let _guard = EmptyBuiltinGuard::new(tmp.path());
        let mgr = test_mgr(tmp.path());

        let out = inject_first_message_prefix(
            "Do stuff",
            &mgr,
            InjectionConfig {
                user_id: "system_default_user",
                preset_context: Some("Custom rule"),
                skills: &["cron".to_owned()],
                delivery_mode: SkillDeliveryMode::Protocol,
            },
        )
        .await;

        assert!(out.contains("Custom rule"), "preset context still ships in light mode");
        assert!(
            !out.contains("Available Skills"),
            "protocol delivery must not also inject an index: {out}"
        );
    }

    /// The `injected` block must advertise both channels — this is the wiring
    /// half of the truncation-is-safe argument.
    #[tokio::test]
    async fn injected_mode_carries_the_dual_channel_instructions() {
        let tmp = TempDir::new().unwrap();
        skill_corpus(tmp.path());
        let _guard = EmptyBuiltinGuard::new(tmp.path());
        let mgr = test_mgr(tmp.path());

        let out = inject_first_message_prefix(
            "Hello",
            &mgr,
            InjectionConfig {
                user_id: "system_default_user",
                preset_context: None,
                skills: &["cron".to_owned()],
                delivery_mode: SkillDeliveryMode::Injected,
            },
        )
        .await;
        assert!(out.contains("Available Skills"));
        assert!(out.contains("skills show"));
        assert!(out.contains("[LOAD_SKILL:"));
    }

    /// `compose_injected_prefix` must produce the block WITHOUT the caller's
    /// content, and must not leak the sentinel it splits on.
    #[tokio::test]
    async fn compose_injected_prefix_returns_the_block_without_content() {
        let tmp = TempDir::new().unwrap();
        skill_corpus(tmp.path());
        let _guard = EmptyBuiltinGuard::new(tmp.path());
        let mgr = test_mgr(tmp.path());

        let prefix = compose_injected_prefix(
            &mgr,
            InjectionConfig {
                user_id: "system_default_user",
                preset_context: Some("Rule 1."),
                skills: &["cron".to_owned()],
                delivery_mode: SkillDeliveryMode::Injected,
            },
        )
        .await
        .expect("a conversation with rules and skills must produce a prefix");

        assert!(prefix.contains("[Assistant Rules]"));
        assert!(prefix.contains("Rule 1."));
        assert!(prefix.contains("Available Skills"));
        assert!(
            prefix.ends_with("[/Assistant Rules]"),
            "trailing blank lines trimmed: {prefix:?}"
        );
        assert!(!prefix.contains('\u{0}'), "the split sentinel must never escape");
    }

    /// Nothing to inject must be `None`, not an empty string: the caller uses it
    /// to decide whether to touch the prompt at all.
    #[tokio::test]
    async fn compose_injected_prefix_is_none_when_there_is_nothing_to_inject() {
        let tmp = TempDir::new().unwrap();
        let _guard = EmptyBuiltinGuard::new(tmp.path());
        let mgr = test_mgr(tmp.path());

        assert!(
            compose_injected_prefix(
                &mgr,
                InjectionConfig {
                    user_id: "system_default_user",
                    preset_context: None,
                    skills: &[],
                    delivery_mode: SkillDeliveryMode::Injected,
                },
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn argv_mode_is_light_and_only_carries_preset_context() {
        let tmp = TempDir::new().unwrap();
        let mgr = test_mgr(tmp.path());

        let out = inject_first_message_prefix(
            "Hello",
            &mgr,
            InjectionConfig {
                user_id: "system_default_user",
                preset_context: Some("Be concise."),
                skills: &[],
                delivery_mode: SkillDeliveryMode::Argv,
            },
        )
        .await;

        assert!(out.contains("[Assistant Rules]"));
        assert!(out.contains("Be concise."));
        assert!(out.ends_with("Hello"));
    }

    #[tokio::test]
    async fn light_mode_with_no_context_passes_through() {
        let tmp = TempDir::new().unwrap();
        let mgr = test_mgr(tmp.path());

        let out = inject_first_message_prefix(
            "Hello",
            &mgr,
            InjectionConfig {
                user_id: "system_default_user",
                preset_context: None,
                skills: &[],
                delivery_mode: SkillDeliveryMode::Argv,
            },
        )
        .await;
        assert_eq!(out, "Hello");
    }

    #[tokio::test]
    async fn injected_mode_with_no_skills_and_no_context_passes_through() {
        let tmp = TempDir::new().unwrap();
        let _guard = EmptyBuiltinGuard::new(tmp.path());
        let mgr = test_mgr(tmp.path());

        let out = inject_first_message_prefix(
            "Hello",
            &mgr,
            InjectionConfig {
                user_id: "system_default_user",
                preset_context: None,
                skills: &[],
                delivery_mode: SkillDeliveryMode::Injected,
            },
        )
        .await;
        assert_eq!(out, "Hello");
    }

    #[tokio::test]
    async fn injected_mode_with_preset_context_and_no_skills() {
        let tmp = TempDir::new().unwrap();
        let _guard = EmptyBuiltinGuard::new(tmp.path());
        let mgr = test_mgr(tmp.path());

        let out = inject_first_message_prefix(
            "Go.",
            &mgr,
            InjectionConfig {
                user_id: "system_default_user",
                preset_context: Some("Rule 1."),
                skills: &[],
                delivery_mode: SkillDeliveryMode::Injected,
            },
        )
        .await;

        assert!(out.contains("[Assistant Rules]"));
        assert!(out.contains("Rule 1."));
        assert!(out.ends_with("Go."));
    }

    #[tokio::test]
    async fn injected_mode_with_resolved_skills_injects_the_index() {
        // Set up a builtin skills dir with two skills; pass only one in `skills`.
        let tmp = TempDir::new().unwrap();
        let auto = tmp.path().join("auto-inject");
        std::fs::create_dir_all(auto.join("cron")).unwrap();
        std::fs::write(
            auto.join("cron").join("SKILL.md"),
            "---\nname: cron\ndescription: Schedule stuff\n---\nBody.",
        )
        .unwrap();
        std::fs::create_dir_all(auto.join("pdf")).unwrap();
        std::fs::write(
            auto.join("pdf").join("SKILL.md"),
            "---\nname: pdf\ndescription: Render PDFs\n---\nBody.",
        )
        .unwrap();
        let _guard = EmptyBuiltinGuard::new(tmp.path());
        let mgr = test_mgr(tmp.path());

        let out = inject_first_message_prefix(
            "Hello",
            &mgr,
            InjectionConfig {
                user_id: "system_default_user",
                preset_context: None,
                skills: &["cron".to_owned()],
                delivery_mode: SkillDeliveryMode::Injected,
            },
        )
        .await;
        assert!(out.contains("cron"));
        assert!(!out.contains("pdf"));
        assert!(out.ends_with("Hello"));
    }

    #[tokio::test]
    async fn a_layer_one_vendor_stays_light_even_with_skills() {
        let tmp = TempDir::new().unwrap();
        let _guard = EmptyBuiltinGuard::new(tmp.path());
        let mgr = test_mgr(tmp.path());

        let out = inject_first_message_prefix(
            "Do stuff",
            &mgr,
            InjectionConfig {
                user_id: "system_default_user",
                preset_context: Some("Custom rule"),
                skills: &["cron".to_owned()],
                delivery_mode: SkillDeliveryMode::Argv,
            },
        )
        .await;

        assert!(out.contains("[Assistant Rules]"));
        assert!(out.contains("Custom rule"));
        assert!(!out.contains("Available Skills"));
        assert!(out.ends_with("Do stuff"));
    }
}
