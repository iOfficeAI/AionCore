//! Team Guide Prompt (Layer 1) — injected into solo ACP agents so they know
//! when/how to propose a multi-agent Team. Reproduces AionUi
//! `src/process/team/prompts/teamGuidePrompt.ts` byte-for-byte.
//!
//! Hard constraint (aionui-audit §8 #5, team-prompts.md §5): the template text
//! is treated as raw material — it must not be translated, rewritten, or
//! reordered. Only the interpolated slots (`backend`, `leader_label`) may vary.

const EXPLICIT_TEAM_REQUEST_CRITERIA: &str = "\
- The user explicitly asks to create a Team
- The user explicitly asks for multiple agents, teammates, or parallel workers
- The user says they want to pull in a Team before starting";

const EXTREME_COMPLEXITY_CRITERIA: &str = "\
- The task is so large, risky, or specialized that one agent is unlikely to complete it well alone
- The work needs substantial parallel role separation that cannot be reasonably handled in a normal solo workflow
- This bar is very high: if you can handle the task yourself, stay solo";

const STAY_SOLO_CRITERIA: &str = "\
- Greetings, casual conversation, or general questions
- Single-point tasks: one question, one file, one fix, one translation, one explanation
- Normal coding, writing, research, or analysis tasks that one agent can handle with some effort
- Any task you can reasonably complete yourself, even if it takes multiple turns";

const SOLO_DEFAULT_RULE: &str = "Handle the task yourself in the current chat by default. Do NOT proactively recommend Team just because the work spans multiple files, takes multiple rounds, or would benefit from specialization.";

/// Full Team Guide prompt template with `{leader_cell}`
/// placeholders. Exported for cross-crate snapshot tests and the Wave 5
/// capability injector; prefer [`build_team_guide_prompt`] for runtime use.
pub const TEAM_GUIDE_PROMPT_TEMPLATE: &str = "## Team Mode

You can create a multi-agent Team for the user.

### Default behavior
{solo_default_rule}

### Only bring up Team in either of these cases
1. The user explicitly wants a Team or multiple agents:
{explicit_team_request_criteria}
2. The task is exceptionally complex and you genuinely believe one agent is unlikely to handle it well alone:
{extreme_complexity_criteria}

### Otherwise stay solo and do not mention Team
{stay_solo_criteria}

If case 2 applies, ask at most once whether the user wants to bring in a Team. Keep it brief and optional. If the user says no, ignores it, or prefers solo help, continue solo and do not mention Team again.

### How to proceed when Team is requested or approved (STRICT — follow every step, do NOT skip)
1. FIRST call `aion_list_models` to check available models for each assistant you plan to use.
2. Explain in one sentence why the Team setup helps this task.
3. Present a team configuration table: role name, responsibility, recommended assistant, and recommended model (from aion_list_models results) for each member. Example format:
   | Role | Responsibility | Assistant | Model |
   | Leader | Coordinate and review | {leader_cell} | (default) |
   | Developer | Implement features | Suitable assistant | (model from list) |
   | Tester | Write and run tests | Suitable assistant | (model from list) |
4. **Output the table as a normal text message and END YOUR TURN.** Do NOT call `aion_create_team` or any other tool (including ask_user) in this turn. Wait for the user to reply in their next message with explicit confirmation (e.g. \"ok\", \"go ahead\", \"确认\") before proceeding.
5. After user confirms → call `aion_create_team`. The summary MUST include both the goal and the confirmed team configuration. (The system automatically derives the correct backend from a chosen assistant — you do NOT need to pass agentType when using assistant identities.)
6. After `aion_create_team` returns → you ARE now the team Leader. The system navigates to the team page automatically. **Immediately** use `team_spawn_agent` to create each teammate from the confirmed configuration table. Then use `team_send_message` to assign initial tasks to each spawned teammate. Do NOT end your turn until all teammates are spawned and tasked.
7. User declines or wants changes → adjust or proceed solo. Do not mention Team again unless the user asks.

### Tool constraint
Before team creation: use **only** `aion_create_team` and `aion_list_models`. After `aion_create_team` succeeds: use team tools (`team_spawn_agent`, `team_send_message`, `team_members`, `team_task_create`, etc.) to manage your team.";

/// Build the Team Guide prompt for a solo agent.
///
/// * `backend` — assistant runtime backend key (`"claude"`, `"gemini"`, `"codex"`, …). Empty
///   string falls back to `"claude"`, matching AionUi `opts.backend || 'claude'`.
/// * `leader_label` — optional display name for a preset assistant (e.g.
///   `"Word Creator"`). When present it renders as `"{label} ({backend})"`,
///   mirroring AionUi's preset-label formatting branch. Whitespace-only labels
///   are treated as absent.
pub fn build_team_guide_prompt(backend: &str, leader_label: Option<&str>) -> String {
    let leader_backend = if backend.is_empty() { "claude" } else { backend };
    let raw_label = leader_label.map(str::trim).filter(|s| !s.is_empty());
    let leader_cell = match raw_label {
        Some(label) => format!("{label} ({leader_backend})"),
        None => format!("Current assistant ({leader_backend})"),
    };

    TEAM_GUIDE_PROMPT_TEMPLATE
        .replace("{solo_default_rule}", SOLO_DEFAULT_RULE)
        .replace("{explicit_team_request_criteria}", EXPLICIT_TEAM_REQUEST_CRITERIA)
        .replace("{extreme_complexity_criteria}", EXTREME_COMPLEXITY_CRITERIA)
        .replace("{stay_solo_criteria}", STAY_SOLO_CRITERIA)
        .replace("{leader_cell}", &leader_cell)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_guide_prompt_plain_backend_matches_snapshot() {
        let prompt = build_team_guide_prompt("claude", None);
        let expected = "## Team Mode\n\
\n\
You can create a multi-agent Team for the user.\n\
\n\
### Default behavior\n\
Handle the task yourself in the current chat by default. Do NOT proactively recommend Team just because the work spans multiple files, takes multiple rounds, or would benefit from specialization.\n\
\n\
### Only bring up Team in either of these cases\n\
1. The user explicitly wants a Team or multiple agents:\n\
- The user explicitly asks to create a Team\n\
- The user explicitly asks for multiple agents, teammates, or parallel workers\n\
- The user says they want to pull in a Team before starting\n\
2. The task is exceptionally complex and you genuinely believe one agent is unlikely to handle it well alone:\n\
- The task is so large, risky, or specialized that one agent is unlikely to complete it well alone\n\
- The work needs substantial parallel role separation that cannot be reasonably handled in a normal solo workflow\n\
- This bar is very high: if you can handle the task yourself, stay solo\n\
\n\
### Otherwise stay solo and do not mention Team\n\
- Greetings, casual conversation, or general questions\n\
- Single-point tasks: one question, one file, one fix, one translation, one explanation\n\
- Normal coding, writing, research, or analysis tasks that one agent can handle with some effort\n\
- Any task you can reasonably complete yourself, even if it takes multiple turns\n\
\n\
If case 2 applies, ask at most once whether the user wants to bring in a Team. Keep it brief and optional. If the user says no, ignores it, or prefers solo help, continue solo and do not mention Team again.\n\
\n\
### How to proceed when Team is requested or approved (STRICT — follow every step, do NOT skip)\n\
1. FIRST call `aion_list_models` to check available models for each assistant you plan to use.\n\
2. Explain in one sentence why the Team setup helps this task.\n\
3. Present a team configuration table: role name, responsibility, recommended assistant, and recommended model (from aion_list_models results) for each member. Example format:\n   \
| Role | Responsibility | Assistant | Model |\n   \
| Leader | Coordinate and review | Current assistant (claude) | (default) |\n   \
| Developer | Implement features | Suitable assistant | (model from list) |\n   \
| Tester | Write and run tests | Suitable assistant | (model from list) |\n\
4. **Output the table as a normal text message and END YOUR TURN.** Do NOT call `aion_create_team` or any other tool (including ask_user) in this turn. Wait for the user to reply in their next message with explicit confirmation (e.g. \"ok\", \"go ahead\", \"确认\") before proceeding.\n\
5. After user confirms → call `aion_create_team`. The summary MUST include both the goal and the confirmed team configuration. (The system automatically derives the correct backend from a chosen assistant — you do NOT need to pass agentType when using assistant identities.)\n\
6. After `aion_create_team` returns → you ARE now the team Leader. The system navigates to the team page automatically. **Immediately** use `team_spawn_agent` to create each teammate from the confirmed configuration table. Then use `team_send_message` to assign initial tasks to each spawned teammate. Do NOT end your turn until all teammates are spawned and tasked.\n\
7. User declines or wants changes → adjust or proceed solo. Do not mention Team again unless the user asks.\n\
\n\
### Tool constraint\n\
Before team creation: use **only** `aion_create_team` and `aion_list_models`. After `aion_create_team` succeeds: use team tools (`team_spawn_agent`, `team_send_message`, `team_members`, `team_task_create`, etc.) to manage your team.";
        assert_eq!(prompt, expected);
    }

    #[test]
    fn team_guide_prompt_with_preset_leader_label() {
        let prompt = build_team_guide_prompt("gemini", Some("Word Creator"));
        assert!(prompt.contains("| Leader | Coordinate and review | Word Creator (gemini) | (default) |"));
        assert!(prompt.contains("| Developer | Implement features | Suitable assistant | (model from list) |"));
        assert!(prompt.contains("| Tester | Write and run tests | Suitable assistant | (model from list) |"));
        assert!(!prompt.contains("{leader_cell}"));
    }

    #[test]
    fn team_guide_prompt_empty_backend_falls_back_to_claude() {
        let prompt = build_team_guide_prompt("", None);
        assert!(prompt.contains("| Leader | Coordinate and review | Current assistant (claude) | (default) |"));
    }

    #[test]
    fn team_guide_prompt_whitespace_label_treated_as_absent() {
        let prompt = build_team_guide_prompt("codex", Some("   "));
        assert!(prompt.contains("| Leader | Coordinate and review | Current assistant (codex) | (default) |"));
    }

    #[test]
    fn team_guide_prompt_without_leader_label_uses_assistant_first_leader_cell() {
        let prompt = build_team_guide_prompt("gemini", None);
        assert!(prompt.contains("| Leader | Coordinate and review | Current assistant (gemini) | (default) |"));
        assert!(!prompt.contains("| Leader | Coordinate and review | gemini | (default) |"));
    }
}
