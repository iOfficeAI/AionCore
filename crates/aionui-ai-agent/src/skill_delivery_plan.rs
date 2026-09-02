//! Turn a vendor's `skill_delivery` declaration plus this conversation's
//! resolved skills into concrete launch arguments / protocol parameters.
//!
//! Pure and syscall-free on purpose: the substitution rules are the part most
//! likely to go subtly wrong (wrong root handed to the wrong vendor, an
//! allow-list that silently collapses to one entry), so they are unit-testable
//! in isolation and the backends only ever see already-substituted strings.

use aionui_api_types::{SkillDelivery, SkillDeliveryMode};
pub use aionui_session::SkillDirSpec;

/// The plugin root — `.claude-plugin/plugin.json` lives directly under it.
const PLACEHOLDER_VIEW_DIR: &str = "{skill_view_dir}";
/// The skills root — `{name}/SKILL.md` lives directly under it.
const PLACEHOLDER_VIEW_SKILLS_DIR: &str = "{skill_view_skills_dir}";
/// One skill's REAL source directory. Expanded once per enabled skill.
const PLACEHOLDER_SKILL_DIR: &str = "{skill_dir}";

pub struct SkillDeliveryPlanInput {
    /// `None` = the column was NULL or unparseable, which means `injected`.
    pub delivery: Option<SkillDelivery>,
    pub view_dir: Option<String>,
    pub view_skills_dir: Option<String>,
    /// This conversation's resolved skills, as REAL source directories.
    pub skill_dirs: Vec<SkillDirSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDeliveryPlan {
    pub mode: SkillDeliveryMode,
    /// Appended to `SessionConfig.extra_args`; already substituted.
    pub extra_args: Vec<String>,
    /// `Some` only in protocol mode: the skills root to send to the CLI.
    pub protocol_skills_root: Option<String>,
    /// Placeholders we did not recognize, kept verbatim in `extra_args`.
    /// Surfaced so the caller can `warn` once instead of dropping a flag.
    pub unknown_placeholders: Vec<String>,
}

/// "Deliver nothing", which is what the safe default mode with an empty snapshot
/// produces. Used by tests that do not exercise skill delivery.
impl Default for SkillDeliveryPlan {
    fn default() -> Self {
        Self {
            mode: SkillDeliveryMode::Injected,
            extra_args: Vec::new(),
            protocol_skills_root: None,
            unknown_placeholders: Vec::new(),
        }
    }
}

pub fn plan_skill_delivery(input: SkillDeliveryPlanInput) -> SkillDeliveryPlan {
    let delivery = input.delivery.unwrap_or_else(SkillDelivery::injected_default);
    let mode = delivery.mode.clone();

    // No skills means nothing to deliver. Registering an empty plugin root would
    // still cost an always-on token line for zero skills.
    if input.skill_dirs.is_empty() {
        return SkillDeliveryPlan {
            mode,
            extra_args: Vec::new(),
            protocol_skills_root: None,
            unknown_placeholders: Vec::new(),
        };
    }

    let mut unknown = Vec::new();
    let mut extra_args = Vec::new();

    if mode == SkillDeliveryMode::Argv {
        // Skipped entirely when the view path is unavailable (a rejected id):
        // half-substituted args would point the CLI at a literal
        // `{skill_view_dir}` directory, which is worse than no flag.
        if input.view_dir.is_some() || input.view_skills_dir.is_some() {
            for arg in &delivery.args {
                extra_args.push(substitute_scalar(
                    arg,
                    input.view_dir.as_deref(),
                    input.view_skills_dir.as_deref(),
                    &mut unknown,
                ));
            }
        }
    }

    // Allow-listing is orthogonal to mode and does not depend on the view: it
    // targets the real source dirs, so it survives a missing view directory.
    if !delivery.allow_dir_args.is_empty() {
        for skill in &input.skill_dirs {
            for arg in &delivery.allow_dir_args {
                extra_args.push(if arg.contains(PLACEHOLDER_SKILL_DIR) {
                    arg.replace(PLACEHOLDER_SKILL_DIR, &skill.path)
                } else {
                    substitute_scalar(
                        arg,
                        input.view_dir.as_deref(),
                        input.view_skills_dir.as_deref(),
                        &mut unknown,
                    )
                });
            }
        }
    }

    let protocol_skills_root = match mode {
        SkillDeliveryMode::Protocol => input.view_skills_dir.clone(),
        _ => None,
    };

    unknown.sort();
    unknown.dedup();
    SkillDeliveryPlan {
        mode,
        extra_args,
        protocol_skills_root,
        unknown_placeholders: unknown,
    }
}

fn substitute_scalar(
    arg: &str,
    view_dir: Option<&str>,
    view_skills_dir: Option<&str>,
    unknown: &mut Vec<String>,
) -> String {
    let mut out = arg.to_owned();
    // Longest first: `{skill_view_dir}` is not a prefix of
    // `{skill_view_skills_dir}`, but keeping the order explicit documents that
    // the two are distinct roots rather than interchangeable.
    if let Some(view_skills_dir) = view_skills_dir {
        out = out.replace(PLACEHOLDER_VIEW_SKILLS_DIR, view_skills_dir);
    }
    if let Some(view_dir) = view_dir {
        out = out.replace(PLACEHOLDER_VIEW_DIR, view_dir);
    }
    // A leftover `{...}` is a placeholder from a newer registry. Keep it
    // verbatim rather than dropping the flag (which would silently change the
    // spawn) and let the caller warn with the actual value.
    if out.starts_with('{') && out.ends_with('}') {
        unknown.push(out.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::{SkillDelivery, SkillDeliveryMode};

    fn dirs() -> Vec<SkillDirSpec> {
        vec![
            SkillDirSpec {
                name: "cron".into(),
                path: "/src/cron".into(),
            },
            SkillDirSpec {
                name: "pdf".into(),
                path: "/src/pdf".into(),
            },
        ]
    }

    fn input(delivery: SkillDelivery) -> SkillDeliveryPlanInput {
        SkillDeliveryPlanInput {
            delivery: Some(delivery),
            view_dir: Some("/data/session-skills/u/c".into()),
            view_skills_dir: Some("/data/session-skills/u/c/skills".into()),
            skill_dirs: dirs(),
        }
    }

    #[test]
    fn argv_mode_substitutes_the_view_dir_and_expands_allow_dir_per_skill() {
        let plan = plan_skill_delivery(input(SkillDelivery {
            mode: SkillDeliveryMode::Argv,
            args: vec!["--plugin-dir".into(), "{skill_view_dir}".into()],
            allow_dir_args: vec!["--add-dir".into(), "{skill_dir}".into()],
            method: None,
        }));

        assert_eq!(plan.mode, SkillDeliveryMode::Argv);
        assert_eq!(
            plan.extra_args,
            vec![
                "--plugin-dir",
                "/data/session-skills/u/c",
                // One expansion PER SKILL, and the REAL source dir rather than
                // the view: a CLI that resolves symlinks to their canonical path
                // would not match a view-directory entry.
                "--add-dir",
                "/src/cron",
                "--add-dir",
                "/src/pdf",
            ]
        );
        assert!(plan.protocol_skills_root.is_none());
    }

    #[test]
    fn protocol_mode_exposes_the_skills_root_not_the_plugin_root() {
        let plan = plan_skill_delivery(input(SkillDelivery {
            mode: SkillDeliveryMode::Protocol,
            args: Vec::new(),
            allow_dir_args: Vec::new(),
            method: Some("skills/extraRoots/set".into()),
        }));
        assert_eq!(
            plan.protocol_skills_root.as_deref(),
            Some("/data/session-skills/u/c/skills"),
            "extraRoots expects the skills root, not the plugin root"
        );
        assert!(plan.extra_args.is_empty());
    }

    /// Allow-listing is orthogonal to mode, so `injected` still emits it.
    #[test]
    fn injected_mode_still_emits_allow_dir_args_but_not_argv_args() {
        let plan = plan_skill_delivery(input(SkillDelivery {
            mode: SkillDeliveryMode::Injected,
            args: vec!["--plugin-dir".into(), "{skill_view_dir}".into()],
            allow_dir_args: vec!["--add-dir".into(), "{skill_dir}".into()],
            method: None,
        }));
        assert_eq!(
            plan.extra_args,
            vec!["--add-dir", "/src/cron", "--add-dir", "/src/pdf"],
            "`args` belongs to argv mode only; allow_dir_args applies to every mode"
        );
    }

    /// A leftover placeholder means a newer registry wrote it. Dropping the flag
    /// would silently change the spawn, so it is kept verbatim and reported.
    #[test]
    fn an_unrecognized_placeholder_is_kept_verbatim_and_does_not_clear_the_list() {
        let plan = plan_skill_delivery(input(SkillDelivery {
            mode: SkillDeliveryMode::Argv,
            args: vec!["--flag".into(), "{unknown_placeholder}".into()],
            allow_dir_args: Vec::new(),
            method: None,
        }));
        assert_eq!(plan.extra_args, vec!["--flag", "{unknown_placeholder}"]);
        assert_eq!(plan.unknown_placeholders, vec!["{unknown_placeholder}"]);
    }

    #[test]
    fn no_skills_means_no_delivery_at_all() {
        let plan = plan_skill_delivery(SkillDeliveryPlanInput {
            delivery: Some(SkillDelivery {
                mode: SkillDeliveryMode::Argv,
                args: vec!["--plugin-dir".into(), "{skill_view_dir}".into()],
                allow_dir_args: vec!["--add-dir".into(), "{skill_dir}".into()],
                method: None,
            }),
            view_dir: Some("/data/session-skills/u/c".into()),
            view_skills_dir: Some("/data/session-skills/u/c/skills".into()),
            skill_dirs: Vec::new(),
        });
        assert!(
            plan.extra_args.is_empty(),
            "an empty snapshot must not register an empty plugin root, which would \
             still cost an always-on token line"
        );
        assert!(plan.protocol_skills_root.is_none());
    }

    #[test]
    fn a_null_column_plans_as_injected_with_nothing_to_add() {
        let plan = plan_skill_delivery(SkillDeliveryPlanInput {
            delivery: None,
            view_dir: Some("/data/session-skills/u/c".into()),
            view_skills_dir: Some("/data/session-skills/u/c/skills".into()),
            skill_dirs: dirs(),
        });
        assert_eq!(plan.mode, SkillDeliveryMode::Injected);
        assert!(plan.extra_args.is_empty());
    }

    /// The view path is unavailable when the id failed validation. An argv-mode
    /// vendor must then contribute no plugin flag rather than a half-substituted
    /// one that would point the CLI at a literal `{skill_view_dir}` directory.
    #[test]
    fn a_missing_view_dir_drops_the_argv_args_but_keeps_allow_listing() {
        let plan = plan_skill_delivery(SkillDeliveryPlanInput {
            delivery: Some(SkillDelivery {
                mode: SkillDeliveryMode::Argv,
                args: vec!["--plugin-dir".into(), "{skill_view_dir}".into()],
                allow_dir_args: vec!["--add-dir".into(), "{skill_dir}".into()],
                method: None,
            }),
            view_dir: None,
            view_skills_dir: None,
            skill_dirs: dirs(),
        });
        assert_eq!(
            plan.extra_args,
            vec!["--add-dir", "/src/cron", "--add-dir", "/src/pdf"],
            "allow-listing does not depend on the view directory"
        );
        assert!(plan.protocol_skills_root.is_none());
    }
}
