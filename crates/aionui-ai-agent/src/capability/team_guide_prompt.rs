//! Solo-agent Team Guide prompt (Layer 1) — teaches a solo ACP agent when and
//! how to propose a multi-agent Team to the user.
//!
//! This is a local copy of the prompt owned by `aionui_team::prompts::team_guide`
//! (see `crates/aionui-team/src/prompts/team_guide.rs`). We duplicate rather
//! than import because `aionui-team` depends on `aionui-ai-agent`, so the
//! reverse direction would create a dependency cycle. Future work may sink
//! this prompt into `aionui-common` and have both crates re-export; until
//! then, **keep this file byte-for-byte in sync with the team-side template**
//! — it ships to the LLM and must match AionUi's
//! `src/process/team/prompts/teamGuidePrompt.ts` exactly (aionui-audit §8 #5).
//!
//! Unlike the team-side helper, this module only exposes the solo branch
//! (`leader_label = None`). The preset-assistant label branch does not apply
//! to Wave 5 solo-agent injection.

use aionui_common::constants::TEAM_CAPABLE_BACKENDS;

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

const TEAM_GUIDE_PROMPT_TEMPLATE: &str = "## Team Mode

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
6. After `aion_create_team` returns → you ARE now the team Leader. The system navigates to the team page automatically. First call `team_list_assistants` if you need the real assistant catalog for the confirmed lineup, and only use returned assistant_id values with `team_spawn_agent`. Then use `team_send_message` to assign initial tasks to each spawned teammate. Do NOT end your turn until all teammates are spawned and tasked.
7. User declines or wants changes → adjust or proceed solo. Do not mention Team again unless the user asks.

### Tool constraint
Before team creation: use **only** `aion_create_team` and `aion_list_models`. After `aion_create_team` succeeds: use team tools (`team_spawn_agent`, `team_send_message`, `team_members`, `team_task_create`, etc.) to manage your team.";

/// Return `true` iff the given backend is a known team-capable backend. An
/// empty or unrecognized backend returns `false`; solo agents with unknown
/// backends do not receive the Team Guide prompt.
pub(crate) fn is_solo_team_guide_backend(backend: &str) -> bool {
    TEAM_CAPABLE_BACKENDS.contains(&backend)
}

/// Build the Team Guide prompt for a solo agent with the given backend label.
/// An empty `backend` falls back to `"claude"`, matching AionUi's
/// `opts.backend || 'claude'` and the team-side helper.
pub(crate) fn build_solo_team_guide_prompt(backend: &str) -> String {
    let leader_backend = if backend.is_empty() { "claude" } else { backend };
    let leader_cell = format!("Current assistant ({leader_backend})");

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
    fn is_solo_team_guide_backend_allows_known_vendors() {
        assert!(is_solo_team_guide_backend("claude"));
        assert!(is_solo_team_guide_backend("codex"));
        assert!(is_solo_team_guide_backend("gemini"));
        assert!(is_solo_team_guide_backend("aionrs"));
    }

    #[test]
    fn is_solo_team_guide_backend_rejects_unknown_and_empty() {
        assert!(!is_solo_team_guide_backend(""));
        assert!(!is_solo_team_guide_backend("qwen"));
        assert!(!is_solo_team_guide_backend("Claude")); // case-sensitive
    }

    #[test]
    fn build_solo_team_guide_prompt_resolves_all_placeholders() {
        let prompt = build_solo_team_guide_prompt("claude");
        assert!(!prompt.contains("{solo_default_rule}"));
        assert!(!prompt.contains("{explicit_team_request_criteria}"));
        assert!(!prompt.contains("{extreme_complexity_criteria}"));
        assert!(!prompt.contains("{stay_solo_criteria}"));
        assert!(!prompt.contains("{leader_cell}"));
        assert!(!prompt.contains("{agent_type}"));
    }

    #[test]
    fn build_solo_team_guide_prompt_uses_assistant_table_copy() {
        let prompt = build_solo_team_guide_prompt("gemini");
        assert!(prompt.contains("| Role | Responsibility | Assistant | Model |"));
        assert!(prompt.contains("| Leader | Coordinate and review | Current assistant (gemini) | (default) |"));
        assert!(prompt.contains("| Developer | Implement features | Suitable assistant | (model from list) |"));
        assert!(prompt.contains("| Tester | Write and run tests | Suitable assistant | (model from list) |"));
        assert!(!prompt.contains("| Role | Responsibility | Type | Model |"));
        assert!(!prompt.contains("| Leader | Coordinate and review | gemini | (default) |"));
    }

    #[test]
    fn build_solo_team_guide_prompt_empty_backend_falls_back_to_claude() {
        let prompt = build_solo_team_guide_prompt("");
        assert!(prompt.contains("| Leader | Coordinate and review | Current assistant (claude) | (default) |"));
    }

    #[test]
    fn snapshot_matches_team_crate_verbatim() {
        // Byte-for-byte equality with the canonical prompt in
        // `aionui-team/src/prompts/team_guide.rs`. If this test fails, one
        // side has drifted — update **both** files (and the AionUi TS
        // source in lockstep) rather than patching this assertion.
        let prompt = build_solo_team_guide_prompt("claude");
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
6. After `aion_create_team` returns → you ARE now the team Leader. The system navigates to the team page automatically. First call `team_list_assistants` if you need the real assistant catalog for the confirmed lineup, and only use returned assistant_id values with `team_spawn_agent`. Then use `team_send_message` to assign initial tasks to each spawned teammate. Do NOT end your turn until all teammates are spawned and tasked.\n\
7. User declines or wants changes → adjust or proceed solo. Do not mention Team again unless the user asks.\n\
\n\
### Tool constraint\n\
Before team creation: use **only** `aion_create_team` and `aion_list_models`. After `aion_create_team` succeeds: use team tools (`team_spawn_agent`, `team_send_message`, `team_members`, `team_task_create`, etc.) to manage your team.";
        assert_eq!(prompt, expected);
    }
}
