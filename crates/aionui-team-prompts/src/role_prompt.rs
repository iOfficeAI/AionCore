use std::collections::HashMap;
use std::fmt::Write;

use crate::governance::with_team_governance;

pub const LEAD_PROMPT_TEMPLATE: &str = include_str!("prompt_templates/lead.txt");

const PLACEHOLDER_TEAMMATE_LIST: &str = "${teammateList}";
const PLACEHOLDER_AVAILABLE_TYPES_SECTION: &str = "${availableTypesSection}";
const PLACEHOLDER_AVAILABLE_ASSISTANTS_SECTION: &str = "${availableAssistantsSection}";
const PLACEHOLDER_WORKSPACE_SECTION: &str = "${workspaceSection}";
const PLACEHOLDER_PRESET_FORMATTING_STEP_RULE: &str = "${presetFormattingStepRule}";
const PLACEHOLDER_PRESET_FORMATTING_IMPORTANT_RULE: &str = "${presetFormattingImportantRule}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamPromptRole {
    Lead,
    Teammate,
}

impl std::fmt::Display for TeamPromptRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TeamPromptRole::Lead => f.write_str("lead"),
            TeamPromptRole::Teammate => f.write_str("teammate"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TeamPromptAgent {
    pub slot_id: String,
    pub name: String,
    pub role: TeamPromptRole,
    pub backend: String,
    pub model: String,
    pub status: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AvailableAgentType {
    pub agent_type: String,
    pub display_name: String,
}

#[derive(Debug, Clone)]
pub struct AvailableAssistant {
    pub custom_agent_id: String,
    pub name: String,
    pub backend: String,
    pub description: Option<String>,
    pub skills: Vec<String>,
}

pub struct LeadPromptParams<'a> {
    pub team_name: &'a str,
    pub teammates: &'a [TeamPromptAgent],
    pub available_agent_types: &'a [AvailableAgentType],
    pub available_assistants: &'a [AvailableAssistant],
    pub renamed_agents: &'a HashMap<String, String>,
    pub team_workspace: Option<&'a str>,
}

pub struct TeammatePromptParams<'a> {
    pub agent: &'a TeamPromptAgent,
    pub team_name: &'a str,
    pub leader: &'a TeamPromptAgent,
    pub teammates: &'a [TeamPromptAgent],
    pub renamed_agents: &'a HashMap<String, String>,
    pub team_workspace: Option<&'a str>,
}

pub fn build_lead_prompt(params: &LeadPromptParams<'_>) -> String {
    let role_prompt = build_lead_role_prompt(params);
    with_team_governance(&role_prompt)
}

pub fn build_teammate_prompt(params: &TeammatePromptParams<'_>) -> String {
    let role_prompt = build_teammate_role_prompt(params);
    with_team_governance(&role_prompt)
}

fn build_lead_role_prompt(params: &LeadPromptParams<'_>) -> String {
    let teammate_list = render_teammate_list(params.teammates, params.renamed_agents);
    let available_types_section = render_available_types_section(params.available_agent_types);
    let available_assistants_section = render_available_assistants_section(params.available_assistants);
    let workspace_section = render_workspace_section(params.team_workspace);

    let preset_formatting_step_rule = "";
    let preset_formatting_important_rule = "";

    LEAD_PROMPT_TEMPLATE
        .replace(PLACEHOLDER_TEAMMATE_LIST, &teammate_list)
        .replace(PLACEHOLDER_AVAILABLE_TYPES_SECTION, &available_types_section)
        .replace(PLACEHOLDER_AVAILABLE_ASSISTANTS_SECTION, &available_assistants_section)
        .replace(PLACEHOLDER_WORKSPACE_SECTION, &workspace_section)
        .replace(PLACEHOLDER_PRESET_FORMATTING_STEP_RULE, preset_formatting_step_rule)
        .replace(
            PLACEHOLDER_PRESET_FORMATTING_IMPORTANT_RULE,
            preset_formatting_important_rule,
        )
}

fn render_teammate_list(teammates: &[TeamPromptAgent], renamed_agents: &HashMap<String, String>) -> String {
    if teammates.is_empty() {
        return "(no teammates yet — propose the lineup to the user first, then use \
                team_spawn_agent only after they confirm or explicitly ask you to create \
                teammates immediately)"
            .to_owned();
    }

    let mut out = String::with_capacity(teammates.len() * 64);
    for (idx, teammate) in teammates.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let status = teammate.status.as_deref().unwrap_or("unknown");
        let _ = write!(out, "- {} ({}, status: {})", teammate.name, teammate.backend, status);
        if let Some(former) = renamed_agents.get(&teammate.slot_id) {
            let _ = write!(out, " [formerly: {former}]");
        }
    }
    out
}

fn render_available_types_section(agent_types: &[AvailableAgentType]) -> String {
    if agent_types.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n## Available Agent Types for Spawning\n");
    for (idx, agent_type) in agent_types.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let _ = write!(out, "- `{}` — {}", agent_type.agent_type, agent_type.display_name);
    }
    out.push_str("\n\nUse `team_list_models` to query available models for each agent type before spawning.");
    out
}

fn render_available_assistants_section(assistants: &[AvailableAssistant]) -> String {
    if assistants.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n## Available Preset Assistants for Spawning\n");
    out.push_str(
        "These are user-configured assistants with pre-loaded rules and skills for specific \
         domains (writing, research, PPT building, etc.). When a task matches a preset's \
         specialty, prefer spawning the preset over a generic CLI agent — you get its domain \
         expertise automatically.\n\n",
    );
    for (idx, assistant) in assistants.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let desc = assistant
            .description
            .as_deref()
            .filter(|description| !description.is_empty())
            .map(|description| format!(" — {description}"))
            .unwrap_or_default();
        let skills = if assistant.skills.is_empty() {
            String::new()
        } else {
            format!("\n   skills: {}", assistant.skills.join(", "))
        };
        let _ = write!(
            out,
            "- `{}` ({}, backend: {}){}{}",
            assistant.custom_agent_id, assistant.name, assistant.backend, desc, skills,
        );
    }
    out.push_str(
        "\n\n### How to pick a preset\n\
         1. Scan the one-line descriptions and skills above. If one clearly matches the user's \
         domain (e.g. \"quarterly Word report\" → `word-creator`), spawn it directly with \
         `team_spawn_agent`.\n\
         2. If two or more presets seem relevant, call `team_describe_assistant` on each \
         candidate to see its full description, skills, and example tasks, then choose the best \
         fit.\n\
         3. If no preset matches the task, fall back to a generic CLI agent from the \
         \"Available Agent Types\" section.\n\n\
         Pass the preset's ID as `custom_agent_id` to `team_spawn_agent`. The `agent_type` is \
         derived from the preset's backend and does not need to be specified.",
    );
    out
}

fn render_workspace_section(team_workspace: Option<&str>) -> String {
    match team_workspace {
        Some(workspace) => format!(
            "\n\n## Team Workspace\nYour working directory `{workspace}` IS the shared team workspace.\n\
             All teammates work in this directory for project-related operations."
        ),
        None => String::new(),
    }
}

const TEAMMATE_PROMPT_TEMPLATE: &str = r#"# You are a Team Member

## Your Identity
Name: {{AGENT_NAME}}, Role: {{ROLE_DESC}}

## Conversation Style
- If the user greets you, starts a new chat, or asks what you can do without assigning concrete work yet, reply warmly and naturally
- Briefly introduce yourself and your role on the team, then invite the user to share what they need
- Do NOT open with task board details, idle/waiting status, or coordination mechanics unless they are directly relevant

## Your Team
Team: {{TEAM_NAME}}
Leader: {{LEADER_NAME}}
Teammates: {{TEAMMATES}}{{WORKSPACE}}

## Team Coordination Tools
You MUST use the `team_*` MCP tools for ALL team coordination.
Your platform may provide similarly named built-in tools (e.g. SendMessage,
TaskCreate, TaskUpdate). Do NOT use those — they belong to a different
system and will break team coordination. Always use the `team_*` versions.

Use `team_task_list` and `team_members` to check current team state.

## How to Work
1. Read your unread messages to understand your assignment
2. If you have a clear task assignment in the messages AND no prerequisite is blocking it, start working on it immediately
3. Use team_task_update to mark your task as "in_progress" when you start
4. Do the actual work (read files, write code, search, etc.)
5. When done, use team_task_update to mark the task "completed"
6. Use team_send_message to report results to the leader

## Standing By (CRITICAL — read carefully)
"Standing by" or "waiting" means **end your current turn**, not generate idle text in a live LLM stream. The system holds you in an idle state and re-wakes you the instant new mailbox messages arrive — there is nothing you need to do meanwhile.

You are in a "standing by" situation when ANY of these is true:
- Your task board is empty and no concrete task was assigned in the messages
- The leader asked you to wait for a prerequisite (e.g. "hold until reviewer-1 finishes")
- You finished your current task and have nothing else assigned

**The correct way to stand by:**
1. (Optional) Send ONE short acknowledgement via `team_send_message` to the leader, e.g. `"Acknowledged, standing by until reviewer-1 finishes"` or `"Ready, no task yet — standing by"`
2. **STOP GENERATING.** Do NOT continue producing text like "I am waiting...", "still standing by...", reasoning loops, or repeated status updates. End your turn and return control.

**Why this matters:** if you keep your turn open while "waiting", your underlying LLM request stays open and will hit the provider's hard request timeout (often 300 seconds) — the system will then mark you as failed. Ending the turn is the correct, lossless way to wait. The mailbox + wake mechanism guarantees you will be re-activated the moment work is ready for you.

## Bug Fix Priority
When fixing bugs: **locate the problem → fix the problem → types/code style last**.
Do NOT prioritize type errors or code style issues unless they affect runtime behavior.

## Shutdown Requests
If you receive a message with type `shutdown_request`, the leader is asking you to shut down.
- To agree: use `team_send_message` to send exactly `shutdown_approved` to the leader.
- To refuse: use `team_send_message` to send `shutdown_rejected: <your reason>` to the leader.

## Important Rules
- Focus on your assigned tasks — don't go beyond what was asked
- Report back to the leader when you finish, including a summary of what you did
- If you get stuck, send a message to the leader asking for guidance
- You can communicate with other teammates directly if needed
- Use your native tools (Read, Write, Bash, etc.) for implementation work"#;

fn build_teammate_role_prompt(params: &TeammatePromptParams<'_>) -> String {
    let teammates_section = if params.teammates.is_empty() {
        "(none)".to_string()
    } else {
        params
            .teammates
            .iter()
            .map(|teammate| match params.renamed_agents.get(&teammate.slot_id) {
                Some(original) => format!("{} [formerly: {}]", teammate.name, original),
                None => teammate.name.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };

    let workspace_section = match params.team_workspace {
        Some(workspace) => format!(
            "\n\n## Workspaces\n\
- **Team workspace**: `{workspace}` — all project work (code, files, tests) happens here.\n\
- **Your working directory**: your private space for personal memory, notes, and experience logs. Not for project files.\n\n\
Always use the team workspace path for any project-related operations."
        ),
        None => String::new(),
    };

    TEAMMATE_PROMPT_TEMPLATE
        .replace("{{AGENT_NAME}}", &params.agent.name)
        .replace("{{ROLE_DESC}}", &role_description(&params.agent.backend))
        .replace("{{TEAM_NAME}}", params.team_name)
        .replace("{{LEADER_NAME}}", &params.leader.name)
        .replace("{{TEAMMATES}}", &teammates_section)
        .replace("{{WORKSPACE}}", &workspace_section)
}

fn role_description(agent_type: &str) -> String {
    match agent_type.to_lowercase().as_str() {
        "claude" => "general-purpose AI assistant".to_string(),
        "gemini" => "Google Gemini AI assistant".to_string(),
        "codex" => "code generation specialist".to_string(),
        "qwen" => "Qwen AI assistant".to_string(),
        other => format!("{other} AI assistant"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt_agent(slot_id: &str, name: &str, role: TeamPromptRole) -> TeamPromptAgent {
        TeamPromptAgent {
            slot_id: slot_id.to_owned(),
            name: name.to_owned(),
            role,
            backend: "claude".to_owned(),
            model: "sonnet".to_owned(),
            status: None,
        }
    }

    #[test]
    fn lead_prompt_prepends_governance_and_fills_sections() {
        let renamed = HashMap::new();
        let teammate = prompt_agent("worker-1", "Worker", TeamPromptRole::Teammate);
        let agent_types = vec![AvailableAgentType {
            agent_type: "claude".to_owned(),
            display_name: "Claude".to_owned(),
        }];
        let prompt = build_lead_prompt(&LeadPromptParams {
            team_name: "Alpha",
            teammates: &[teammate],
            available_agent_types: &agent_types,
            available_assistants: &[],
            renamed_agents: &renamed,
            team_workspace: None,
        });

        assert!(prompt.starts_with("## Team Governance"));
        assert!(prompt.contains("- Worker (claude, status: unknown)"));
        assert!(prompt.contains("## Available Agent Types for Spawning"));
        assert!(!prompt.contains("${"));
    }

    #[test]
    fn teammate_prompt_contains_canonical_coordination_rules() {
        let leader = prompt_agent("lead-1", "Lead", TeamPromptRole::Lead);
        let worker = prompt_agent("worker-1", "Worker", TeamPromptRole::Teammate);
        let prompt = build_teammate_prompt(&TeammatePromptParams {
            agent: &worker,
            team_name: "Alpha",
            leader: &leader,
            teammates: &[],
            renamed_agents: &HashMap::new(),
            team_workspace: None,
        });

        assert!(prompt.contains("## Team Governance"));
        assert!(prompt.contains("You MUST use the `team_*` MCP tools for ALL team coordination."));
        assert!(prompt.contains("Use team_send_message to report results to the leader"));
        assert!(prompt.contains("STOP GENERATING"));
    }
}
