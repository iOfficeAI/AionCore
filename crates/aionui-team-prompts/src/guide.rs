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

pub fn is_solo_team_guide_backend(backend: &str) -> bool {
    SOLO_TEAM_GUIDE_BACKENDS.contains(&backend)
}

pub fn build_solo_team_guide_prompt(backend: &str) -> String {
    build_solo_team_guide_prompt_with_label(backend, None)
}

pub fn build_solo_team_guide_prompt_with_label(backend: &str, leader_label: Option<&str>) -> String {
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
    fn guide_prompt_uses_team_tools_after_create_team() {
        let prompt = build_solo_team_guide_prompt("claude");
        assert!(prompt.contains("aion_create_team"));
        assert!(prompt.contains("aion_list_models"));
        assert!(prompt.contains("only use returned assistant_id values with `team_spawn_agent`"));
        assert!(prompt.contains(
            "use team tools (`team_spawn_agent`, `team_send_message`, `team_members`, `team_task_create`, etc.) to manage your team"
        ));
        assert!(!prompt.contains("Immediately"));
        assert!(!prompt.contains("hand off to the created Team conversation"));
    }

    #[test]
    fn guide_prompt_supports_preset_leader_label() {
        let prompt = build_solo_team_guide_prompt_with_label("gemini", Some("Word Creator"));
        assert!(prompt.contains("| Leader | Coordinate and review | Word Creator (gemini) | (default) |"));
        assert!(prompt.contains("| Developer | Implement features | Suitable assistant | (model from list) |"));
        assert!(prompt.contains("| Tester | Write and run tests | Suitable assistant | (model from list) |"));
    }

    #[test]
    fn empty_backend_falls_back_to_claude() {
        let prompt = build_solo_team_guide_prompt("");
        assert!(prompt.contains("| Leader | Coordinate and review | Current assistant (claude) | (default) |"));
    }

    #[test]
    fn whitespace_label_treated_as_absent() {
        let prompt = build_solo_team_guide_prompt_with_label("codex", Some("   "));
        assert!(prompt.contains("| Leader | Coordinate and review | Current assistant (codex) | (default) |"));
        assert!(!prompt.contains("()"));
    }
}
