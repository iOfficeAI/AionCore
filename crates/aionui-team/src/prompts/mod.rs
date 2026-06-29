use std::collections::HashMap;

pub use aionui_team_prompts::AvailableAssistant;

use crate::types::{MailboxMessage, MailboxMessageType, TaskStatus, TeamAgent, TeamTask};

fn to_prompt_role(role: crate::types::TeammateRole) -> aionui_team_prompts::TeamPromptRole {
    match role {
        crate::types::TeammateRole::Lead => aionui_team_prompts::TeamPromptRole::Lead,
        crate::types::TeammateRole::Teammate => aionui_team_prompts::TeamPromptRole::Teammate,
    }
}

fn to_prompt_agent(agent: &TeamAgent) -> aionui_team_prompts::TeamPromptAgent {
    aionui_team_prompts::TeamPromptAgent {
        slot_id: agent.slot_id.clone(),
        name: agent.name.clone(),
        role: to_prompt_role(agent.role),
        backend: agent.backend.clone(),
        model: agent.model.clone(),
        status: agent.status.map(|status| status.to_string()),
    }
}

/// Build the leader system prompt.
///
/// Delegates to `aionui-team-prompts`, the canonical Team role prompt crate.
/// A one-line `Team: "<name>"` header is prepended so the leader knows which
/// team it belongs to.
///
/// `available_assistants` is the assistant catalog the leader may use when
/// staffing the team. Callers should only include assistants that are both
/// enabled and team-selectable.
pub fn build_lead_prompt(
    team_name: &str,
    members: &[TeamAgent],
    available_assistants: &[AvailableAssistant],
) -> String {
    let prompt_members: Vec<_> = members.iter().map(to_prompt_agent).collect();
    let renamed: HashMap<String, String> = HashMap::new();

    let body = aionui_team_prompts::build_lead_prompt(&aionui_team_prompts::LeadPromptParams {
        team_name,
        teammates: &prompt_members,
        available_agent_types: &[],
        available_assistants,
        renamed_agents: &renamed,
        team_workspace: None,
    });
    format!("Team: \"{team_name}\"\n\n{body}")
}

pub fn build_teammate_prompt(agent: &TeamAgent, team_name: &str, members: &[TeamAgent]) -> String {
    let prompt_agent = to_prompt_agent(agent);
    let prompt_members: Vec<_> = members.iter().map(to_prompt_agent).collect();
    let leader = prompt_members
        .iter()
        .find(|candidate| candidate.role == aionui_team_prompts::TeamPromptRole::Lead)
        .cloned()
        .unwrap_or_else(|| aionui_team_prompts::TeamPromptAgent {
            slot_id: "lead".to_owned(),
            name: "Lead".to_owned(),
            role: aionui_team_prompts::TeamPromptRole::Lead,
            backend: agent.backend.clone(),
            model: agent.model.clone(),
            status: None,
        });
    let teammates: Vec<_> = prompt_members
        .iter()
        .filter(|candidate| candidate.slot_id != prompt_agent.slot_id)
        .cloned()
        .collect();
    let renamed = HashMap::new();

    aionui_team_prompts::build_teammate_prompt(&aionui_team_prompts::TeammatePromptParams {
        agent: &prompt_agent,
        team_name,
        leader: &leader,
        teammates: &teammates,
        renamed_agents: &renamed,
        team_workspace: None,
    })
}

pub fn build_wake_payload(agent: &TeamAgent, tasks: &[TeamTask], unread_messages: &[MailboxMessage]) -> String {
    let mut payload = String::with_capacity(2048);

    if !unread_messages.is_empty() {
        payload.push_str("## New Messages\n\n");
        for msg in unread_messages {
            let type_label = match msg.msg_type {
                MailboxMessageType::Message => "message",
                MailboxMessageType::IdleNotification => "idle_notification",
                MailboxMessageType::ShutdownRequest => "shutdown_request",
            };
            payload.push_str(&format!(
                "- From `{}` [{}]: {}\n",
                msg.from_agent_id, type_label, msg.content,
            ));
            if let Some(ref summary) = msg.summary {
                payload.push_str(&format!("  Summary: {summary}\n"));
            }
        }
        payload.push('\n');
    } else {
        payload.push_str("## New Messages\n\nNo new messages.\n\n");
    }

    if !tasks.is_empty() {
        payload.push_str("## Current Task Board\n\n");
        payload.push_str("| ID | Subject | Status | Owner | Blocked By |\n");
        payload.push_str("|---|---|---|---|---|\n");
        for task in tasks {
            let status = match task.status {
                TaskStatus::Pending => "pending",
                TaskStatus::InProgress => "in_progress",
                TaskStatus::Completed => "completed",
                TaskStatus::Deleted => "deleted",
            };
            let owner = task.owner.as_deref().unwrap_or("-");
            let blocked = if task.blocked_by.is_empty() {
                "-".to_owned()
            } else {
                task.blocked_by.join(", ")
            };
            let short_id = if task.id.len() > 8 { &task.id[..8] } else { &task.id };
            payload.push_str(&format!(
                "| {short_id}… | {} | {status} | {owner} | {blocked} |\n",
                task.subject,
            ));
        }
        payload.push('\n');
    } else {
        payload.push_str("## Current Task Board\n\nNo tasks on the board.\n\n");
    }

    payload.push_str(&format!(
        "You are **{}** (role: {}). Proceed with your work.\n",
        agent.name, agent.role,
    ));

    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TeammateRole;

    fn make_lead() -> TeamAgent {
        TeamAgent {
            slot_id: "lead-1".into(),
            name: "Lead".into(),
            role: TeammateRole::Lead,
            conversation_id: "conv-1".into(),
            backend: "acp".into(),
            model: "claude".into(),
            assistant_id: None,
            status: None,
            conversation_type: None,
            cli_path: None,
        }
    }

    fn make_teammate(slot_id: &str, name: &str) -> TeamAgent {
        TeamAgent {
            slot_id: slot_id.into(),
            name: name.into(),
            role: TeammateRole::Teammate,
            conversation_id: format!("conv-{slot_id}"),
            backend: "acp".into(),
            model: "claude".into(),
            assistant_id: None,
            status: None,
            conversation_type: None,
            cli_path: None,
        }
    }

    fn make_task(id: &str, subject: &str, status: TaskStatus) -> TeamTask {
        TeamTask {
            id: id.into(),
            team_id: "t1".into(),
            subject: subject.into(),
            description: None,
            status,
            owner: Some("worker-1".into()),
            blocked_by: vec![],
            blocks: vec![],
            metadata: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn make_message(from: &str, content: &str, msg_type: MailboxMessageType) -> MailboxMessage {
        MailboxMessage {
            id: "msg-1".into(),
            team_id: "t1".into(),
            to_agent_id: "lead-1".into(),
            from_agent_id: from.into(),
            msg_type,
            content: content.into(),
            summary: None,
            files: None,
            read: false,
            created_at: 0,
        }
    }

    // -- Lead prompt ----------------------------------------------------------

    fn default_assistants() -> Vec<AvailableAssistant> {
        vec![AvailableAssistant {
            assistant_id: "word-creator".into(),
            name: "Word Creator".into(),
            backend: "claude".into(),
            description: "Drafts Word documents".into(),
            skills: vec!["docx".into(), "formatting".into()],
        }]
    }

    #[test]
    fn lead_prompt_contains_team_name() {
        let assistants = default_assistants();
        let prompt = build_lead_prompt("Alpha", &[], &assistants);
        assert!(prompt.contains("\"Alpha\""));
    }

    #[test]
    fn lead_prompt_contains_member_list() {
        let assistants = default_assistants();
        let members = vec![make_lead(), make_teammate("w1", "Worker1")];
        let prompt = build_lead_prompt("Alpha", &members, &assistants);

        // AionUi bullet format: `- {name} ({backend}, status: {status})`
        assert!(prompt.contains("- Lead (acp, status:"));
        assert!(prompt.contains("- Worker1 (acp, status:"));
    }

    #[test]
    fn lead_prompt_contains_core_sections() {
        let assistants = default_assistants();
        let prompt = build_lead_prompt("Alpha", &[], &assistants);

        // Workflow — 15-step procedure with model listing at step 3
        assert!(prompt.contains("## Workflow"));
        assert!(prompt.contains("FIRST call `team_list_assistants`"));
        assert!(prompt.contains("Wait for explicit confirmation before using team_spawn_agent"));
        assert!(prompt.contains("End your turn after the proposal"));

        // Model Selection Guidelines
        assert!(prompt.contains("## Model Selection Guidelines"));
        assert!(prompt.contains("exact model ID strings"));
        assert!(prompt.contains("omit the model parameter"));

        // Conversation Style — don't pitch proposals up-front
        assert!(prompt.contains("## Conversation Style"));
        assert!(prompt.contains("reply warmly and naturally"));

        // Idle, sequencing, shutdown, important rules
        assert!(prompt.contains("## Teammate Idle State"));
        assert!(prompt.contains("## Sequencing Dependent Work"));
        assert!(prompt.contains("## Shutting Down Teammates"));
        assert!(prompt.contains("team_shutdown_agent"));
        assert!(prompt.contains("## Important Rules"));

        // Team coordination tool list still referenced
        assert!(prompt.contains("team_send_message"));
        assert!(prompt.contains("team_spawn_agent"));
        assert!(prompt.contains("team_members"));
        assert!(prompt.contains("team_task_list"));
        assert!(prompt.contains("team_rename_agent"));
    }

    #[test]
    fn lead_prompt_includes_available_assistants_section() {
        let assistants = default_assistants();
        let prompt = build_lead_prompt("Alpha", &[], &assistants);

        assert!(prompt.contains("## Available Assistants for Spawning"));
        assert!(prompt.contains("- `word-creator` (Word Creator) — Drafts Word documents"));
        assert!(prompt.contains("skills: docx, formatting"));
        assert!(prompt.contains("Pass the assistant's ID as `assistant_id`"));
        assert!(!prompt.contains("backend: claude"));
    }

    #[test]
    fn lead_prompt_omits_available_assistants_section_when_empty() {
        let prompt = build_lead_prompt("Alpha", &[], &[]);
        assert!(!prompt.contains("## Available Assistants for Spawning"));
    }

    #[test]
    fn lead_prompt_no_members_shows_empty_lineup_copy() {
        let assistants = default_assistants();
        let prompt = build_lead_prompt("Solo", &[], &assistants);
        assert!(prompt.contains("(no teammates yet"));
        assert!(prompt.contains("propose the lineup to the user first"));
    }

    #[test]
    fn lead_prompt_has_no_unsubstituted_placeholders() {
        let assistants = default_assistants();
        let members = vec![make_lead(), make_teammate("w1", "Worker1")];
        let prompt = build_lead_prompt("Alpha", &members, &assistants);
        assert!(
            !prompt.contains("${"),
            "unsubstituted template placeholder leaked:\n{prompt}"
        );
        assert!(!prompt.contains("assistant or backend"));
        assert!(!prompt.contains("Available Generic Backends"));
    }

    // -- Teammate prompt ------------------------------------------------------

    #[test]
    fn teammate_prompt_contains_agent_identity() {
        let agent = make_teammate("w1", "Worker1");
        let members = vec![make_lead(), agent.clone()];
        let prompt = build_teammate_prompt(&agent, "Alpha", &members);

        assert!(prompt.contains("## Team Governance"));
        assert!(prompt.contains("Name: Worker1"));
        assert!(prompt.contains("Team: Alpha"));
        assert!(prompt.contains("Leader: Lead"));
    }

    #[test]
    fn teammate_prompt_contains_communication_protocol() {
        let agent = make_teammate("w1", "Worker1");
        let members = vec![make_lead(), agent.clone()];
        let prompt = build_teammate_prompt(&agent, "Alpha", &members);

        assert!(prompt.contains("## Team Coordination Tools"));
        assert!(prompt.contains("You MUST use the `team_*` MCP tools for ALL team coordination."));
        assert!(prompt.contains("team_send_message"));
        assert!(prompt.contains("team_task_update"));
        assert!(prompt.contains("shutdown_request"));
        assert!(prompt.contains("shutdown_approved"));
        assert!(prompt.contains("STOP GENERATING"));
    }

    #[test]
    fn teammate_prompt_contains_team_name() {
        let agent = make_teammate("w1", "W");
        let members = vec![make_lead(), agent.clone()];
        let prompt = build_teammate_prompt(&agent, "Beta Team", &members);
        assert!(prompt.contains("Team: Beta Team"));
    }

    // -- Wake payload ---------------------------------------------------------

    #[test]
    fn wake_payload_with_messages() {
        let agent = make_lead();
        let msgs = vec![make_message("w1", "Task A done", MailboxMessageType::Message)];
        let payload = build_wake_payload(&agent, &[], &msgs);

        assert!(payload.contains("New Messages"));
        assert!(payload.contains("`w1`"));
        assert!(payload.contains("[message]"));
        assert!(payload.contains("Task A done"));
    }

    #[test]
    fn wake_payload_with_idle_notification() {
        let agent = make_lead();
        let mut msg = make_message("w1", "idle", MailboxMessageType::IdleNotification);
        msg.summary = Some("Finished feature X".into());
        let payload = build_wake_payload(&agent, &[], &[msg]);

        assert!(payload.contains("[idle_notification]"));
        assert!(payload.contains("Summary: Finished feature X"));
    }

    #[test]
    fn wake_payload_with_shutdown_request() {
        let agent = make_teammate("w1", "W");
        let msg = make_message("lead-1", "No longer needed", MailboxMessageType::ShutdownRequest);
        let payload = build_wake_payload(&agent, &[], &[msg]);

        assert!(payload.contains("[shutdown_request]"));
        assert!(payload.contains("No longer needed"));
    }

    #[test]
    fn wake_payload_with_tasks() {
        let agent = make_lead();
        let tasks = vec![
            make_task(
                "aaaaaaaa-1234-5678-9abc-def012345678",
                "Implement X",
                TaskStatus::InProgress,
            ),
            make_task("bbbbbbbb-1234-5678-9abc-def012345678", "Test Y", TaskStatus::Pending),
        ];
        let payload = build_wake_payload(&agent, &tasks, &[]);

        assert!(payload.contains("Current Task Board"));
        assert!(payload.contains("Implement X"));
        assert!(payload.contains("in_progress"));
        assert!(payload.contains("Test Y"));
        assert!(payload.contains("pending"));
        assert!(payload.contains("aaaaaaaa…"));
    }

    #[test]
    fn wake_payload_with_task_dependencies() {
        let agent = make_lead();
        let mut task = make_task("cccccccc-1234-5678-9abc-def012345678", "Deploy", TaskStatus::Pending);
        task.blocked_by = vec!["task-a".into(), "task-b".into()];
        let payload = build_wake_payload(&agent, &[task], &[]);

        assert!(payload.contains("task-a, task-b"));
    }

    #[test]
    fn wake_payload_empty() {
        let agent = make_lead();
        let payload = build_wake_payload(&agent, &[], &[]);

        assert!(payload.contains("No new messages"));
        assert!(payload.contains("No tasks on the board"));
        assert!(payload.contains("**Lead**"));
    }

    #[test]
    fn wake_payload_contains_agent_identity() {
        let agent = make_teammate("w1", "Worker1");
        let payload = build_wake_payload(&agent, &[], &[]);

        assert!(payload.contains("**Worker1**"));
        assert!(payload.contains("teammate"));
    }

    #[test]
    fn wake_payload_short_task_id_no_truncation() {
        let agent = make_lead();
        let task = make_task("short", "Short ID Task", TaskStatus::Pending);
        let payload = build_wake_payload(&agent, &[task], &[]);
        assert!(payload.contains("short…"));
    }
}
