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

pub const SOLO_TEAM_GUIDE_BACKENDS: &[&str] = &["claude", "codex", "gemini", "aionrs", "codebuddy"];

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

### How to proceed when Team is requested or approved (STRICT - follow every step, do NOT skip)
1. FIRST call `aion_list_models` to check available models for each agent type you plan to use.
2. Explain in one sentence why the Team setup helps this task.
3. Present a team configuration table: role name, responsibility, agent type, and recommended model (from aion_list_models results) for each member. Example format:
   | Role | Responsibility | Type | Model |
   | Leader | Coordinate and review | {leader_cell} | (default) |
   | Developer | Implement features | {agent_type} | (model from list) |
   | Tester | Write and run tests | {agent_type} | (model from list) |
4. **Output the table as a normal text message and END YOUR TURN.** Do NOT call `aion_create_team` or any other tool (including ask_user) in this turn. Wait for the user to reply in their next message with explicit confirmation (e.g. \"ok\", \"go ahead\", \"confirm\") before proceeding.
5. After user confirms -> call `aion_create_team`. The summary MUST include both the goal and the confirmed team configuration. (The system automatically sets the correct agent type - you do NOT need to pass agentType.)
6. After `aion_create_team` returns -> end this solo turn and hand off to the created Team conversation. Do NOT call `team_*` tools from this solo Guide MCP session.
7. User declines or wants changes -> adjust or proceed solo. Do not mention Team again unless the user asks.

### Tool constraint
Before team creation: use **only** `aion_create_team` and `aion_list_models`. After `aion_create_team` succeeds: do not call any `team_*` tools in this solo turn. Team tools are only for normal Team runtime after the Team page accepts the user's first Team message and an active `TeamRun` exists.";

pub fn is_solo_team_guide_backend(backend: &str) -> bool {
    SOLO_TEAM_GUIDE_BACKENDS.contains(&backend)
}

pub fn build_solo_team_guide_prompt(backend: &str) -> String {
    build_solo_team_guide_prompt_with_label(backend, None)
}

pub fn build_solo_team_guide_prompt_with_label(backend: &str, leader_label: Option<&str>) -> String {
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
    fn guide_prompt_hands_off_after_create_team() {
        let prompt = build_solo_team_guide_prompt("claude");
        assert!(prompt.contains("aion_create_team"));
        assert!(prompt.contains("aion_list_models"));
        assert!(prompt.contains("hand off to the created Team conversation"));
        assert!(!prompt.contains("Immediately"));
        assert!(!prompt.contains(
            "use team tools (`team_spawn_agent`, `team_send_message`, `team_members`, `team_task_create`, etc.) to manage your team"
        ));
    }

    #[test]
    fn guide_prompt_supports_preset_leader_label() {
        let prompt = build_solo_team_guide_prompt_with_label("gemini", Some("Word Creator"));
        assert!(prompt.contains("| Leader | Coordinate and review | Word Creator (gemini) | (default) |"));
        assert!(prompt.contains("| Developer | Implement features | gemini | (model from list) |"));
    }

    #[test]
    fn empty_backend_falls_back_to_claude() {
        let prompt = build_solo_team_guide_prompt("");
        assert!(prompt.contains("| Leader | Coordinate and review | claude | (default) |"));
    }
}
