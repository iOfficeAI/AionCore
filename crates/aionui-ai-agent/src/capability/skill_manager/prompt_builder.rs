use super::{SkillDefinition, SkillIndex};

/// Per-skill description budget, in CHARS.
///
/// The injection block was measured at roughly 1545 characters with a SINGLE
/// 687-character description accounting for 47% of it. 200 keeps every compliant
/// description intact (the well-behaved builtins sit at 133-142) and cuts only
/// the genuinely oversized ones.
///
/// Truncating is safe only BECAUSE channel A exists: an agent that sees a cut
/// description and suspects the skill is relevant can fetch the full text with
/// `skills show`. Without that escape hatch, truncation would degrade the
/// agent's ability to decide when a skill applies.
const DESCRIPTION_CHAR_BUDGET: usize = 200;

fn truncate_description(description: &str) -> String {
    // chars(), not bytes: a byte slice would split a multi-byte codepoint and
    // produce invalid UTF-8 for any non-ASCII description.
    if description.chars().count() <= DESCRIPTION_CHAR_BUDGET {
        return description.to_owned();
    }
    let mut out: String = description.chars().take(DESCRIPTION_CHAR_BUDGET).collect();
    out.push('…');
    out
}

/// Build the skills index block injected for `injected`-mode agents.
///
/// Two channels are offered and the AGENT chooses. We deliberately do not try to
/// predict whether it can execute commands: permission mode (plan / read-only)
/// is agent-side runtime state that no CLI capability query exposes. Channel B
/// requires no vendor capability at all, which is what makes it a true fallback,
/// and channel A is listed first because it is a normal tool call rather than
/// text-matching plus an extra turn.
pub fn build_skills_index_text(skills: &[SkillIndex]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    // Sorted: the upstream discovery returns from a HashMap, so without this the
    // same conversation could be injected a differently-ordered block on each
    // open -- churning the agent's context for no reason and defeating any
    // prefix caching.
    let mut ordered: Vec<&SkillIndex> = skills.iter().collect();
    ordered.sort_by(|a, b| a.name.cmp(&b.name));

    let mut lines = Vec::with_capacity(ordered.len() + 5);
    lines.push("## Available Skills".to_string());
    lines.push(String::new());
    for skill in ordered {
        lines.push(format!(
            "- **{}**: {}",
            skill.name,
            truncate_description(&skill.description)
        ));
    }
    lines.push(String::new());
    // These command lines are the CONTRACT, not prose: both subcommands read their
    // arguments as a JSON object on STDIN and reject positional arguments outright
    // (`aionui-app/src/cli.rs` declares them as argument-less variants). An earlier
    // wording taught `skills show <name>`, which cost live agents three to five
    // failed tool calls each before they guessed the real shape -- and `--help` did
    // not mention stdin either, so the obvious self-service path was a dead end.
    // `skills_cli_commands_in_the_index_are_parseable` in `aionui-app` pins the two
    // sides together so they cannot drift apart again.
    lines.push(
        "To get a skill's full content, prefer running \
         `printf '%s' '{\"name\":\"<name>\"}' | \"$AIONUI_HELPER_BIN\" skills show` — it also \
         returns the skill's absolute directory, and \
         `printf '%s' '{\"path\":\"<name>/<relative-path>\"}' | \"$AIONUI_HELPER_BIN\" skills cat` \
         reads its supplementary files. Both read their arguments as a JSON object on stdin and \
         take no positional arguments; run `\"$AIONUI_HELPER_BIN\" skills capabilities` for the \
         full contract."
            .to_string(),
    );
    lines.push(
        "If you cannot execute commands, output `[LOAD_SKILL: <name>]` in your response instead \
         and the content will be provided on the next turn."
            .to_string(),
    );

    lines.join("\n")
}

/// Build system instructions text with full skill content (for Gemini).
pub fn build_system_instructions(base_instructions: &str, skills: &[SkillDefinition]) -> String {
    if skills.is_empty() {
        return base_instructions.to_string();
    }

    let mut parts = vec![base_instructions.to_string()];

    for skill in skills {
        if let Some(body) = &skill.body {
            parts.push(format!("\n## Skill: {}\n\n{}", skill.name, body));
        }
    }

    parts.join("\n")
}

/// Prepare the first message with skills index prefix (for ACP/Codex).
///
/// Prepends `[Assistant Rules]` block with skill index to the user content.
pub fn prepare_first_message_with_skills_index(
    content: &str,
    skills: &[SkillIndex],
    preset_context: Option<&str>,
) -> String {
    let mut parts = Vec::new();

    let index_text = build_skills_index_text(skills);
    let has_rules = !index_text.is_empty() || preset_context.is_some();

    if has_rules {
        parts.push("[Assistant Rules]".to_string());

        if let Some(ctx) = preset_context
            && !ctx.is_empty()
        {
            parts.push(ctx.to_string());
        }

        if !index_text.is_empty() {
            parts.push(index_text);
        }

        parts.push("[/Assistant Rules]".to_string());
        parts.push(String::new());
    }

    parts.push(content.to_string());
    parts.join("\n")
}

/// Build system instructions with skills index only (for Gemini index-only mode).
///
/// Unlike [`build_system_instructions`] which injects full skill bodies,
/// this variant injects only the skill index (name + description) and
/// the `[LOAD_SKILL]` protocol, allowing the agent to request full content on demand.
pub fn build_system_instructions_with_skills_index(base_instructions: &str, skills: &[SkillIndex]) -> String {
    let index_text = build_skills_index_text(skills);
    if index_text.is_empty() {
        return base_instructions.to_string();
    }

    format!("{base_instructions}\n\n{index_text}")
}

/// Prepare the first message with full skill content (for Gemini).
///
/// Prepends `[Assistant Rules]` block with complete skill bodies.
pub fn prepare_first_message(content: &str, skills: &[SkillDefinition], preset_context: Option<&str>) -> String {
    let mut parts = Vec::new();
    let has_rules = !skills.is_empty() || preset_context.is_some();

    if has_rules {
        parts.push("[Assistant Rules]".to_string());

        if let Some(ctx) = preset_context
            && !ctx.is_empty()
        {
            parts.push(ctx.to_string());
        }

        for skill in skills {
            if let Some(body) = &skill.body {
                parts.push(format!("## Skill: {}\n\n{}", skill.name, body));
            }
        }

        parts.push("[/Assistant Rules]".to_string());
        parts.push(String::new());
    }

    parts.push(content.to_string());
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // Skills index text
    // -----------------------------------------------------------------------

    #[test]
    fn build_skills_index_text_empty() {
        assert!(build_skills_index_text(&[]).is_empty());
    }

    #[test]
    fn build_skills_index_text_with_skills() {
        let skills = vec![
            SkillIndex {
                name: "review".into(),
                description: "Code review".into(),
            },
            SkillIndex {
                name: "debug".into(),
                description: "Debugging helper".into(),
            },
        ];
        let text = build_skills_index_text(&skills);
        assert!(text.contains("## Available Skills"));
        assert!(text.contains("- **review**: Code review"));
        assert!(text.contains("- **debug**: Debugging helper"));
    }

    /// 200 CHARS, not bytes: a byte slice would cut a Chinese description
    /// mid-codepoint and emit invalid UTF-8.
    #[test]
    fn a_long_description_is_truncated_at_200_chars_on_a_char_boundary() {
        let text = build_skills_index_text(&[SkillIndex {
            name: "verbose".into(),
            description: "重".repeat(400),
        }]);
        let line = text.lines().find(|line| line.contains("verbose")).unwrap();
        let rendered = line.split_once(": ").unwrap().1;
        assert_eq!(rendered.chars().count(), 201, "200 chars plus the ellipsis");
        assert!(rendered.ends_with('…'));
    }

    #[test]
    fn a_compliant_description_is_kept_verbatim() {
        let text = build_skills_index_text(&[SkillIndex {
            name: "cron".into(),
            description: "Scheduled task management.".into(),
        }]);
        assert!(text.contains("- **cron**: Scheduled task management."));
        assert!(!text.contains('…'));
    }

    /// A description exactly at the budget must NOT gain an ellipsis -- an
    /// off-by-one here would mark compliant skills as truncated.
    #[test]
    fn a_description_exactly_at_the_budget_is_not_truncated() {
        let text = build_skills_index_text(&[SkillIndex {
            name: "edge".into(),
            description: "x".repeat(200),
        }]);
        assert!(!text.contains('…'), "200 is within budget, not over it");
    }

    /// Truncation is only SAFE because channel A exists, so the block must
    /// advertise both channels -- with the command first, since it is a normal
    /// tool call rather than text-matching plus an extra turn.
    #[test]
    fn the_index_block_advertises_both_channels_with_the_command_first() {
        let text = build_skills_index_text(&[SkillIndex {
            name: "cron".into(),
            description: "d".into(),
        }]);
        let command_at = text.find("skills show").expect("channel A must be advertised");
        let protocol_at = text.find("[LOAD_SKILL:").expect("channel B must stay as the fallback");
        assert!(command_at < protocol_at, "channel A is the preferred path");
        assert!(text.contains("$AIONUI_HELPER_BIN"));
        assert!(text.contains("skills cat"), "supplementary files need their own hint");
    }

    /// Discovery upstream returns from a HashMap, so an unsorted block would vary
    /// between opens of the SAME conversation -- churning context and defeating
    /// prefix caching.
    #[test]
    fn the_index_is_ordered_regardless_of_input_order() {
        let forward = build_skills_index_text(&[
            SkillIndex {
                name: "alpha".into(),
                description: "a".into(),
            },
            SkillIndex {
                name: "zeta".into(),
                description: "z".into(),
            },
        ]);
        let reversed = build_skills_index_text(&[
            SkillIndex {
                name: "zeta".into(),
                description: "z".into(),
            },
            SkillIndex {
                name: "alpha".into(),
                description: "a".into(),
            },
        ]);
        assert_eq!(forward, reversed);
        assert!(forward.find("alpha").unwrap() < forward.find("zeta").unwrap());
    }

    // -----------------------------------------------------------------------
    // First message preparation
    // -----------------------------------------------------------------------

    #[test]
    fn prepare_first_message_with_index_no_skills() {
        let result = prepare_first_message_with_skills_index("Hello", &[], None);
        assert_eq!(result, "Hello");
    }

    #[test]
    fn prepare_first_message_with_index_and_context() {
        let skills = vec![SkillIndex {
            name: "test".into(),
            description: "Testing".into(),
        }];
        let result = prepare_first_message_with_skills_index("Hello", &skills, Some("Be concise."));
        assert!(result.contains("[Assistant Rules]"));
        assert!(result.contains("Be concise."));
        assert!(result.contains("- **test**: Testing"));
        assert!(result.contains("[/Assistant Rules]"));
        assert!(result.ends_with("Hello"));
    }

    #[test]
    fn prepare_first_message_with_full_skills() {
        let skills = vec![SkillDefinition {
            name: "review".into(),
            description: "Review".into(),
            location: PathBuf::new(),
            source: aionui_extension::SkillSource::Custom,
            relative_location: None,
            body: Some("Full review instructions here.".into()),
        }];
        let result = prepare_first_message("Hello", &skills, None);
        assert!(result.contains("[Assistant Rules]"));
        assert!(result.contains("## Skill: review"));
        assert!(result.contains("Full review instructions here."));
        assert!(result.contains("[/Assistant Rules]"));
        assert!(result.ends_with("Hello"));
    }

    #[test]
    fn prepare_first_message_no_skills_no_context() {
        let result = prepare_first_message("Hello", &[], None);
        assert_eq!(result, "Hello");
    }

    #[test]
    fn prepare_first_message_context_only() {
        let result = prepare_first_message_with_skills_index("Hello", &[], Some("Rules here."));
        assert!(result.contains("[Assistant Rules]"));
        assert!(result.contains("Rules here."));
        assert!(result.ends_with("Hello"));
    }

    // -----------------------------------------------------------------------
    // System instructions builder
    // -----------------------------------------------------------------------

    #[test]
    fn build_system_instructions_no_skills() {
        let result = build_system_instructions("Base prompt", &[]);
        assert_eq!(result, "Base prompt");
    }

    #[test]
    fn build_system_instructions_with_skills() {
        let skills = vec![SkillDefinition {
            name: "helper".into(),
            description: "A helper".into(),
            location: PathBuf::new(),
            source: aionui_extension::SkillSource::Custom,
            relative_location: None,
            body: Some("Helper body content.".into()),
        }];
        let result = build_system_instructions("Base prompt", &skills);
        assert!(result.starts_with("Base prompt"));
        assert!(result.contains("## Skill: helper"));
        assert!(result.contains("Helper body content."));
    }

    #[test]
    fn build_system_instructions_with_skills_index_no_skills() {
        let result = build_system_instructions_with_skills_index("Base prompt", &[]);
        assert_eq!(result, "Base prompt");
    }

    #[test]
    fn build_system_instructions_with_skills_index_includes_index() {
        let skills = vec![SkillIndex {
            name: "helper".into(),
            description: "A helper skill".into(),
        }];
        let result = build_system_instructions_with_skills_index("Base prompt", &skills);
        assert!(result.starts_with("Base prompt"));
        assert!(result.contains("## Available Skills"));
        assert!(result.contains("- **helper**: A helper skill"));
        // The placeholder text changed from `skill-name` to `<name>` when the
        // block gained its second channel. Asserting the PROTOCOL MARKER instead
        // of the exact placeholder keeps the meaningful part of the check --
        // that this builder still carries a load instruction -- without pinning
        // wording that channel A's arrival legitimately rewrote.
        assert!(result.contains("[LOAD_SKILL:"));
        assert!(result.contains("skills show"), "both channels travel with the index");
    }

    #[test]
    fn build_system_instructions_skips_unloaded_skills() {
        let skills = vec![SkillDefinition {
            name: "unloaded".into(),
            description: "Not loaded".into(),
            location: PathBuf::new(),
            source: aionui_extension::SkillSource::Custom,
            relative_location: None,
            body: None,
        }];
        let result = build_system_instructions("Base", &skills);
        assert_eq!(result, "Base");
    }
}
