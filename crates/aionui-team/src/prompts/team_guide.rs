//! Solo Team Guide prompt wrapper.
//!
//! The canonical template lives in `aionui-team-prompts` so ACP, Aionrs, and
//! Team-side prompt tests share one source of truth.

pub const TEAM_GUIDE_PROMPT_TEMPLATE: &str = aionui_team_prompts::guide::TEAM_GUIDE_PROMPT_TEMPLATE;

pub fn build_team_guide_prompt(backend: &str, leader_label: Option<&str>) -> String {
    aionui_team_prompts::guide::build_solo_team_guide_prompt_with_label(backend, leader_label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_guide_prompt_hands_off_after_create_team() {
        let prompt = build_team_guide_prompt("claude", None);

        assert!(prompt.contains("aion_create_team"));
        assert!(prompt.contains("aion_list_models"));
        assert!(prompt.contains("hand off to the created Team conversation"));
        assert!(prompt.contains("Do NOT call `team_*` tools from this solo Guide MCP session."));
        assert!(!prompt.contains("Immediately"));
        assert!(!prompt.contains(
            "use team tools (`team_spawn_agent`, `team_send_message`, `team_members`, `team_task_create`, etc.) to manage your team"
        ));
    }

    #[test]
    fn team_guide_prompt_with_preset_leader_label() {
        let prompt = build_team_guide_prompt("gemini", Some("Word Creator"));
        assert!(prompt.contains("| Leader | Coordinate and review | Word Creator (gemini) | (default) |"));
        assert!(prompt.contains("| Developer | Implement features | gemini | (model from list) |"));
        assert!(prompt.contains("| Tester | Write and run tests | gemini | (model from list) |"));
        assert!(!prompt.contains("{leader_cell}"));
        assert!(!prompt.contains("{agent_type}"));
    }

    #[test]
    fn team_guide_prompt_empty_backend_falls_back_to_claude() {
        let prompt = build_team_guide_prompt("", None);
        assert!(prompt.contains("| Leader | Coordinate and review | claude | (default) |"));
    }

    #[test]
    fn team_guide_prompt_whitespace_label_treated_as_absent() {
        let prompt = build_team_guide_prompt("codex", Some("   "));
        assert!(prompt.contains("| Leader | Coordinate and review | codex | (default) |"));
        assert!(!prompt.contains("()"));
    }
}
