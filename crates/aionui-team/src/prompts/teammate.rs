//! Teammate prompt template + wake payload builder.
//!
//! Template text copied verbatim from AionUi `src/process/team/prompts/teammatePrompt.ts`
//! (aionui-audit §8 #5: prompt text must be reused as-is, no translation, no rewriting).
//!
//! Placeholders enclosed in `{{...}}` are filled by `build_teammate_prompt`:
//! - `{{AGENT_NAME}}`, `{{ROLE_DESC}}`, `{{LEADER_NAME}}`, `{{TEAMMATES}}`, `{{WORKSPACE}}`

use std::collections::HashMap;

use crate::types::{MailboxMessage, MailboxMessageType, TaskStatus, TeamAgent, TeamTask, TeammateRole};

// ---------------------------------------------------------------------------
// TeammatePromptParams — signature frozen by phase1/interface-contracts.md §5
// ---------------------------------------------------------------------------

pub struct TeammatePromptParams<'a> {
    pub agent: &'a TeamAgent,
    pub team_name: &'a str,
    pub leader: &'a TeamAgent,
    pub teammates: &'a [TeamAgent],
    pub renamed_agents: &'a HashMap<String, String>,
    pub team_workspace: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// Template constant (verbatim from teammatePrompt.ts)
// ---------------------------------------------------------------------------

/// Full Teammate system prompt template.
///
/// Tokens `{{...}}` are substituted by [`build_teammate_prompt`]. The rest of
/// the text MUST NOT be modified (aionui-audit §8 #5).
pub const TEAMMATE_PROMPT_TEMPLATE: &str = r#"# You are a Team Member

## Your Identity
Name: {{AGENT_NAME}}, Role: {{ROLE_DESC}}

## Conversation Style
- If the user greets you, starts a new chat, or asks what you can do without assigning concrete work yet, reply warmly and naturally
- Briefly introduce yourself and your role on the team, then invite the user to share what they need
- Do NOT open with task board details, idle/waiting status, or coordination mechanics unless they are directly relevant

## Your Team
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

// ---------------------------------------------------------------------------
// Role description (mirror of teammatePrompt.ts `roleDescription`)
// ---------------------------------------------------------------------------

fn role_description(agent_type: &str) -> String {
    match agent_type.to_lowercase().as_str() {
        "claude" => "general-purpose AI assistant".to_string(),
        "gemini" => "Google Gemini AI assistant".to_string(),
        "codex" => "code generation specialist".to_string(),
        "qwen" => "Qwen AI assistant".to_string(),
        other => format!("{other} AI assistant"),
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Build the full Teammate system prompt by filling [`TEAMMATE_PROMPT_TEMPLATE`].
pub fn build_teammate_prompt(params: &TeammatePromptParams<'_>) -> String {
    let teammates_section = if params.teammates.is_empty() {
        "(none)".to_string()
    } else {
        params
            .teammates
            .iter()
            .map(|t| match params.renamed_agents.get(&t.slot_id) {
                Some(original) => format!("{} [formerly: {}]", t.name, original),
                None => t.name.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };

    let workspace_section = match params.team_workspace {
        Some(ws) => format!(
            "\n\n## Workspaces\n\
- **Team workspace**: `{ws}` — all project work (code, files, tests) happens here.\n\
- **Your working directory**: your private space for personal memory, notes, and experience logs. Not for project files.\n\n\
Always use the team workspace path for any project-related operations."
        ),
        None => String::new(),
    };

    TEAMMATE_PROMPT_TEMPLATE
        .replace("{{AGENT_NAME}}", &params.agent.name)
        .replace("{{ROLE_DESC}}", &role_description(&params.agent.backend))
        .replace("{{LEADER_NAME}}", &params.leader.name)
        .replace("{{TEAMMATES}}", &teammates_section)
        .replace("{{WORKSPACE}}", &workspace_section)
}

// ---------------------------------------------------------------------------
// Wake payload — frozen signature from phase1/interface-contracts.md §5
// ---------------------------------------------------------------------------

/// Build the payload sent as the first `send_message` content when waking an
/// agent. Combines mailbox messages and current task board into markdown.
///
/// AionUi's `TeammateManager.wake` uses `formatMessages(...)` alone for the
/// subsequent-wake case. The phase1 contract extends this to also surface the
/// task board so the agent can see outstanding work without calling
/// `team_task_list` first.
pub fn build_wake_payload(
    agent: &TeamAgent,
    tasks: &[TeamTask],
    unread_messages: &[MailboxMessage],
    sender_name_lookup: &HashMap<String, String>,
) -> String {
    let mut out = String::with_capacity(1024);

    // -- Unread Messages ------------------------------------------------------
    out.push_str("## Unread Messages\n");
    if unread_messages.is_empty() {
        out.push_str("No unread messages.\n");
    } else {
        out.push_str(&format_messages(unread_messages, sender_name_lookup));
        out.push('\n');
    }

    // -- Task Board -----------------------------------------------------------
    out.push('\n');
    out.push_str("## Task Board\n");
    if tasks.is_empty() {
        out.push_str("No tasks on the board.\n");
    } else {
        out.push_str("| ID | Subject | Status | Owner | Blocked By |\n");
        out.push_str("|---|---|---|---|---|\n");
        for t in tasks {
            let short_id = if t.id.len() > 8 { &t.id[..8] } else { &t.id };
            let owner = t.owner.as_deref().unwrap_or("-");
            let blocked = if t.blocked_by.is_empty() {
                "-".to_string()
            } else {
                t.blocked_by.join(", ")
            };
            out.push_str(&format!(
                "| {short_id}… | {} | {} | {} | {} |\n",
                t.subject,
                task_status_label(t.status),
                owner,
                blocked,
            ));
        }
    }

    // -- Identity footer ------------------------------------------------------
    out.push('\n');
    out.push_str(&format!(
        "You are **{}** (role: {}). Proceed with your work.\n",
        agent.name,
        match agent.role {
            TeammateRole::Lead => "lead",
            TeammateRole::Teammate => "teammate",
        },
    ));

    out
}

/// Mirror of AionUi `formatHelpers.ts :: formatMessages`.
/// Sender "user" renders as `[From User]`; known slot_id resolves to agent
/// name via `sender_name_lookup`; unknown slot_id falls back to the raw id.
fn format_messages(messages: &[MailboxMessage], sender_name_lookup: &HashMap<String, String>) -> String {
    messages
        .iter()
        .map(|m| {
            let sender = if m.from_agent_id == "user" {
                "User".to_string()
            } else {
                sender_name_lookup
                    .get(&m.from_agent_id)
                    .cloned()
                    .unwrap_or_else(|| m.from_agent_id.clone())
            };
            let type_tag = match m.msg_type {
                MailboxMessageType::Message => "",
                MailboxMessageType::IdleNotification => " [idle_notification]",
                MailboxMessageType::ShutdownRequest => " [shutdown_request]",
            };
            let summary_line = m
                .summary
                .as_deref()
                .map(|s| format!("\nSummary: {s}"))
                .unwrap_or_default();
            format!("[From {sender}{type_tag}] {content}{summary_line}", content = m.content,)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn task_status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Completed => "completed",
        TaskStatus::Deleted => "deleted",
    }
}

// ---------------------------------------------------------------------------
// Tests — snapshot-style (string-contains) per D5c task spec
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MailboxMessageType, TaskStatus, TeammateRole};

    fn make_agent(slot_id: &str, name: &str, role: TeammateRole, backend: &str) -> TeamAgent {
        TeamAgent {
            slot_id: slot_id.into(),
            name: name.into(),
            role,
            conversation_id: format!("conv-{slot_id}"),
            backend: backend.into(),
            model: "default".into(),
            assistant_id: None,
            status: None,
            conversation_type: None,
            cli_path: None,
        }
    }

    fn make_task(id: &str, subject: &str, status: TaskStatus, owner: Option<&str>) -> TeamTask {
        TeamTask {
            id: id.into(),
            team_id: "t1".into(),
            subject: subject.into(),
            description: None,
            status,
            owner: owner.map(String::from),
            blocked_by: vec![],
            blocks: vec![],
            metadata: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn make_msg(id: &str, from: &str, to: &str, msg_type: MailboxMessageType, content: &str) -> MailboxMessage {
        MailboxMessage {
            id: id.into(),
            team_id: "t1".into(),
            to_agent_id: to.into(),
            from_agent_id: from.into(),
            msg_type,
            content: content.into(),
            summary: None,
            files: None,
            read: false,
            created_at: 0,
        }
    }

    // Test 1: teammate prompt with minimal params (no renamed, no workspace)
    #[test]
    fn teammate_prompt_minimal_params() {
        let lead = make_agent("lead-1", "Captain", TeammateRole::Lead, "claude");
        let agent = make_agent("w1", "Worker1", TeammateRole::Teammate, "gemini");
        let renamed = HashMap::new();
        let params = TeammatePromptParams {
            agent: &agent,
            team_name: "Alpha",
            leader: &lead,
            teammates: &[],
            renamed_agents: &renamed,
            team_workspace: None,
        };
        let out = build_teammate_prompt(&params);

        assert!(out.contains("# You are a Team Member"));
        assert!(out.contains("Name: Worker1, Role: Google Gemini AI assistant"));
        assert!(out.contains("Leader: Captain"));
        assert!(out.contains("Teammates: (none)"));
        assert!(!out.contains("## Workspaces"));
        assert!(out.contains("## Standing By (CRITICAL"));
        assert!(out.contains("shutdown_approved"));
        assert!(out.contains("shutdown_rejected:"));
    }

    // Test 2: teammate prompt with renamed teammates and workspace
    #[test]
    fn teammate_prompt_with_renamed_and_workspace() {
        let lead = make_agent("lead-1", "Captain", TeammateRole::Lead, "claude");
        let agent = make_agent("w1", "Worker1", TeammateRole::Teammate, "claude");
        let mate_a = make_agent("w2", "Alice", TeammateRole::Teammate, "claude");
        let mate_b = make_agent("w3", "Bob", TeammateRole::Teammate, "codex");
        let mut renamed = HashMap::new();
        renamed.insert("w3".into(), "Robert".into());
        let params = TeammatePromptParams {
            agent: &agent,
            team_name: "Alpha",
            leader: &lead,
            teammates: &[mate_a, mate_b],
            renamed_agents: &renamed,
            team_workspace: Some("/workspace/team-alpha"),
        };
        let out = build_teammate_prompt(&params);

        assert!(out.contains("Teammates: Alice, Bob [formerly: Robert]"));
        assert!(out.contains("## Workspaces"));
        assert!(out.contains("`/workspace/team-alpha`"));
        assert!(out.contains("Role: general-purpose AI assistant"));
    }

    // Test 3: wake payload with empty mailbox and no tasks
    #[test]
    fn wake_payload_empty_mailbox_no_tasks() {
        let agent = make_agent("lead-1", "Captain", TeammateRole::Lead, "claude");
        let lookup = HashMap::new();
        let out = build_wake_payload(&agent, &[], &[], &lookup);

        assert!(out.contains("## Unread Messages"));
        assert!(out.contains("No unread messages."));
        assert!(out.contains("## Task Board"));
        assert!(out.contains("No tasks on the board."));
        assert!(out.contains("You are **Captain** (role: lead)"));
    }

    // Test 4: wake payload with tasks and messages (mixed types)
    #[test]
    fn wake_payload_with_tasks_and_messages() {
        let agent = make_agent("w1", "Worker1", TeammateRole::Teammate, "claude");
        let mut lookup = HashMap::new();
        lookup.insert("lead-1".into(), "Captain".into());
        lookup.insert("w2".into(), "Alice".into());

        let msgs = vec![
            make_msg(
                "m1",
                "lead-1",
                "w1",
                MailboxMessageType::Message,
                "Please implement feature X",
            ),
            make_msg("m2", "user", "w1", MailboxMessageType::Message, "Direct user request"),
            make_msg("m3", "w2", "w1", MailboxMessageType::IdleNotification, "done with task"),
        ];

        let tasks = vec![
            make_task(
                "aaaaaaaa-1234-5678-9abc-def012345678",
                "Implement X",
                TaskStatus::InProgress,
                Some("w1"),
            ),
            make_task(
                "bbbbbbbb-1234-5678-9abc-def012345678",
                "Review Y",
                TaskStatus::Pending,
                None,
            ),
        ];

        let out = build_wake_payload(&agent, &tasks, &msgs, &lookup);

        // Messages section
        assert!(out.contains("[From Captain] Please implement feature X"));
        assert!(out.contains("[From User] Direct user request"));
        assert!(out.contains("[From Alice [idle_notification]] done with task"));

        // Task board section
        assert!(out.contains("aaaaaaaa…"));
        assert!(out.contains("Implement X"));
        assert!(out.contains("in_progress"));
        assert!(out.contains("Review Y"));
        assert!(out.contains("pending"));

        // Identity footer
        assert!(out.contains("You are **Worker1** (role: teammate)"));
    }
}
