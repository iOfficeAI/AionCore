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

/// Full Team Guide prompt template with `{leader_cell}` / `{agent_type}`
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
1. FIRST call `aion_list_models` to check available models for each agent type you plan to use.
2. Explain in one sentence why the Team setup helps this task.
3. Present a team configuration table: role name, responsibility, agent type, and recommended model (from aion_list_models results) for each member. Example format:
   | Role | Responsibility | Type | Model |
   | Leader | Coordinate and review | {leader_cell} | (default) |
   | Developer | Implement features | {agent_type} | (model from list) |
   | Tester | Write and run tests | {agent_type} | (model from list) |
4. **Output the table as a normal text message and END YOUR TURN.** Do NOT call `aion_create_team` or any other tool (including ask_user) in this turn. Wait for the user to reply in their next message with explicit confirmation (e.g. \"ok\", \"go ahead\", \"确认\") before proceeding.
5. After user confirms → call `aion_create_team`. The summary MUST include both the goal and the confirmed team configuration. (The system automatically sets the correct agent type — you do NOT need to pass agentType.)
6. After `aion_create_team` returns → the Team has been created and the current conversation has been bound as Leader. **Do NOT call `team_spawn_agent`, `team_send_message`, or any other `team_*` tool in this solo turn.** Output only one brief user-facing handoff in the user's language. It should mean: the Team is ready, send the next message, and I will continue from there. Then END YOUR TURN. Do not mention the Team page, solo turn, `team_*` tools, `TeamRun`, or internal tool state in the user-facing handoff.
7. User declines or wants changes → adjust or proceed solo. Do not mention Team again unless the user asks.

### Tool constraint
Before team creation: use **only** `aion_create_team` and `aion_list_models`. After `aion_create_team` succeeds: do not call any `team_*` tools in this solo turn. Team tools are only for normal Team runtime after the Team page accepts the user's first Team message and an active `TeamRun` exists.";

/// Build the Team Guide prompt for a solo agent.
///
/// * `backend` — agent backend key (`"claude"`, `"gemini"`, `"codex"`, …). Empty
///   string falls back to `"claude"`, matching AionUi `opts.backend || 'claude'`.
/// * `leader_label` — optional display name for a preset assistant (e.g.
///   `"Word Creator"`). When present it renders as `"{label} ({backend})"`,
///   mirroring the `rawLabel ? "${rawLabel} (${agentType})" : agentType` branch
///   in `teamGuidePrompt.ts`. Whitespace-only labels are treated as absent.
pub fn build_team_guide_prompt(backend: &str, leader_label: Option<&str>) -> String {
    let agent_type = if backend.is_empty() { "claude" } else { backend };
    let raw_label = leader_label.map(str::trim).filter(|s| !s.is_empty());
    let leader_cell = match raw_label {
        Some(label) => format!("{label} ({agent_type})"),
        None => agent_type.to_owned(),
    };

    TEAM_GUIDE_PROMPT_TEMPLATE
        .replace("{solo_default_rule}", SOLO_DEFAULT_RULE)
        .replace("{explicit_team_request_criteria}", EXPLICIT_TEAM_REQUEST_CRITERIA)
        .replace("{extreme_complexity_criteria}", EXTREME_COMPLEXITY_CRITERIA)
        .replace("{stay_solo_criteria}", STAY_SOLO_CRITERIA)
        .replace("{leader_cell}", &leader_cell)
        .replace("{agent_type}", agent_type)
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
1. FIRST call `aion_list_models` to check available models for each agent type you plan to use.\n\
2. Explain in one sentence why the Team setup helps this task.\n\
3. Present a team configuration table: role name, responsibility, agent type, and recommended model (from aion_list_models results) for each member. Example format:\n   \
| Role | Responsibility | Type | Model |\n   \
| Leader | Coordinate and review | claude | (default) |\n   \
| Developer | Implement features | claude | (model from list) |\n   \
| Tester | Write and run tests | claude | (model from list) |\n\
4. **Output the table as a normal text message and END YOUR TURN.** Do NOT call `aion_create_team` or any other tool (including ask_user) in this turn. Wait for the user to reply in their next message with explicit confirmation (e.g. \"ok\", \"go ahead\", \"确认\") before proceeding.\n\
5. After user confirms → call `aion_create_team`. The summary MUST include both the goal and the confirmed team configuration. (The system automatically sets the correct agent type — you do NOT need to pass agentType.)\n\
6. After `aion_create_team` returns → the Team has been created and the current conversation has been bound as Leader. **Do NOT call `team_spawn_agent`, `team_send_message`, or any other `team_*` tool in this solo turn.** Output only one brief user-facing handoff in the user's language. It should mean: the Team is ready, send the next message, and I will continue from there. Then END YOUR TURN. Do not mention the Team page, solo turn, `team_*` tools, `TeamRun`, or internal tool state in the user-facing handoff.\n\
7. User declines or wants changes → adjust or proceed solo. Do not mention Team again unless the user asks.\n\
\n\
### Tool constraint\n\
Before team creation: use **only** `aion_create_team` and `aion_list_models`. After `aion_create_team` succeeds: do not call any `team_*` tools in this solo turn. Team tools are only for normal Team runtime after the Team page accepts the user's first Team message and an active `TeamRun` exists.";
        assert_eq!(prompt, expected);
    }

    #[test]
    fn team_guide_prompt_hands_off_after_create_team() {
        let prompt = build_team_guide_prompt("claude", None);

        assert!(prompt.contains(
            "After `aion_create_team` returns → the Team has been created and the current conversation has been bound as Leader."
        ));
        assert!(prompt.contains(
            "Do NOT call `team_spawn_agent`, `team_send_message`, or any other `team_*` tool in this solo turn."
        ));
        assert!(prompt.contains(
            "Output only one brief user-facing handoff in the user's language. It should mean: the Team is ready, send the next message, and I will continue from there."
        ));
        assert!(prompt.contains(
            "Do not mention the Team page, solo turn, `team_*` tools, `TeamRun`, or internal tool state in the user-facing handoff."
        ));
        assert!(
            prompt.contains("After `aion_create_team` succeeds: do not call any `team_*` tools in this solo turn.")
        );
        assert!(
            !prompt.contains("Your team tools (team_spawn_agent, team_send_message, etc.) are now active."),
            "prompt must not claim Team tools are active immediately after creation"
        );
        assert!(
            !prompt.contains("Immediately proceed to spawn teammates as planned"),
            "prompt must not ask the solo agent to spawn teammates in the same solo turn"
        );
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
    }
}
