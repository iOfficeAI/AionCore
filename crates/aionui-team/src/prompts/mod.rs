use std::collections::{HashMap, HashSet};

use aionui_api_types::TeamToolTransport;

mod wake_summary;

pub use aionui_team_prompts::AvailableAssistant;

use crate::types::{MailboxMessage, MailboxMessageType, TeamAgent, TeamTask};

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
    agent: &TeamAgent,
    team_name: &str,
    members: &[TeamAgent],
    available_assistants: &[AvailableAssistant],
) -> String {
    build_lead_prompt_for_transport(agent, team_name, members, available_assistants, TeamToolTransport::Mcp)
}

pub fn build_lead_prompt_for_transport(
    agent: &TeamAgent,
    team_name: &str,
    members: &[TeamAgent],
    available_assistants: &[AvailableAssistant],
    tool_transport: TeamToolTransport,
) -> String {
    let prompt_agent = to_prompt_agent(agent);
    let prompt_members: Vec<_> = members.iter().map(to_prompt_agent).collect();
    let renamed: HashMap<String, String> = HashMap::new();

    let body = aionui_team_prompts::build_lead_prompt(&aionui_team_prompts::LeadPromptParams {
        agent: &prompt_agent,
        team_name,
        teammates: &prompt_members,
        available_agent_types: &[],
        available_assistants,
        renamed_agents: &renamed,
        team_workspace: None,
        tool_transport,
    });
    format!("Team: \"{team_name}\"\n\n{body}")
}

pub fn build_teammate_prompt(agent: &TeamAgent, team_name: &str, members: &[TeamAgent]) -> String {
    build_teammate_prompt_for_transport(agent, team_name, members, TeamToolTransport::Mcp)
}

pub fn build_teammate_prompt_for_transport(
    agent: &TeamAgent,
    team_name: &str,
    members: &[TeamAgent],
    tool_transport: TeamToolTransport,
) -> String {
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
        tool_transport,
    })
}

pub fn build_wake_payload(
    agent: &TeamAgent,
    tasks: &[TeamTask],
    unread_messages: &[MailboxMessage],
    current_slot_ids: &HashSet<String>,
) -> String {
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

    payload.push_str(&wake_summary::render_task_board_summary(agent, tasks, current_slot_ids));

    payload.push_str(&format!(
        "You are **{}** (role: {}). Proceed with your work.\n",
        agent.name, agent.role,
    ));

    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{TaskStatus, TeammateRole};

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

    fn roster(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
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
        let prompt = build_lead_prompt(&make_lead(), "Alpha", &[], &assistants);
        assert!(prompt.contains("\"Alpha\""));
    }

    #[test]
    fn lead_prompt_uses_tool_for_member_list() {
        let assistants = default_assistants();
        let members = vec![make_lead(), make_teammate("w1", "Worker1")];
        let prompt = build_lead_prompt(&make_lead(), "Alpha", &members, &assistants);

        assert!(!prompt.contains("- Lead (acp, status:"));
        assert!(!prompt.contains("- Worker1 (acp, status:"));
        assert!(prompt.contains("team_members"));
    }

    #[test]
    fn lead_prompt_contains_core_sections() {
        let assistants = default_assistants();
        let prompt = build_lead_prompt(&make_lead(), "Alpha", &[], &assistants);

        // Workflow uses tools for dynamic state.
        assert!(prompt.contains("## Workflow"));
        assert!(prompt.contains("FIRST call `team_members`"));
        assert!(prompt.contains("call `team_list_assistants`"));
        assert!(prompt.contains("Wait for explicit confirmation before using team_spawn_agent"));
        assert!(prompt.contains("End your turn after the proposal"));

        // Assistant selection, not model selection.
        assert!(prompt.contains("## Assistant Selection Guidelines"));
        assert!(!prompt.contains("team_list_models"));
        assert!(prompt.contains("Do not pass a model to `team_spawn_agent`"));

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
    fn lead_prompt_omits_dynamic_available_assistants_section() {
        let assistants = default_assistants();
        let prompt = build_lead_prompt(&make_lead(), "Alpha", &[], &assistants);

        assert!(!prompt.contains("## Available Assistants for Spawning"));
        assert!(!prompt.contains("- `word-creator` (Word Creator) — Drafts Word documents"));
        assert!(prompt.contains("team_list_assistants"));
    }

    #[test]
    fn lead_prompt_omits_available_assistants_section_when_empty() {
        let prompt = build_lead_prompt(&make_lead(), "Alpha", &[], &[]);
        assert!(!prompt.contains("## Available Assistants for Spawning"));
    }

    #[test]
    fn lead_prompt_does_not_include_dynamic_member_snapshot() {
        let assistants = default_assistants();
        let prompt = build_lead_prompt(&make_lead(), "Solo", &[], &assistants);
        assert!(!prompt.contains("## Your Teammates"));
        assert!(!prompt.contains("(no teammates yet"));
        assert!(prompt.to_lowercase().contains("first team turn"));
        assert!(prompt.contains("team_members"));
    }

    #[test]
    fn lead_prompt_has_no_unsubstituted_placeholders() {
        let assistants = default_assistants();
        let members = vec![make_lead(), make_teammate("w1", "Worker1")];
        let prompt = build_lead_prompt(&make_lead(), "Alpha", &members, &assistants);
        assert!(
            !prompt.contains("${"),
            "unsubstituted template placeholder leaked:\n{prompt}"
        );
        assert!(!prompt.contains("assistant or backend"));
        assert!(!prompt.contains("Available Generic Backends"));
        assert!(!prompt.contains("## Your Teammates"));
        assert!(!prompt.contains("## Available Assistants for Spawning"));
        assert!(prompt.contains("Name: Lead"));
        assert!(prompt.contains("Slot ID: lead-1"));
        assert!(prompt.contains("Role: lead"));
    }

    // -- Teammate prompt ------------------------------------------------------

    #[test]
    fn teammate_prompt_contains_agent_identity() {
        let agent = make_teammate("w1", "Worker1");
        let members = vec![make_lead(), agent.clone()];
        let prompt = build_teammate_prompt(&agent, "Alpha", &members);

        assert!(prompt.contains("## Team Governance"));
        assert!(prompt.contains("Name: Worker1"));
        assert!(prompt.contains("Slot ID: w1"));
        assert!(prompt.contains("Team: Alpha"));
        assert!(prompt.contains("Leader: Lead"));
        assert!(!prompt.contains("Teammates:"));
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
        let payload = build_wake_payload(&agent, &[], &msgs, &roster(&["lead-1", "w1"]));

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
        let payload = build_wake_payload(&agent, &[], &[msg], &roster(&["lead-1", "w1"]));

        assert!(payload.contains("[idle_notification]"));
        assert!(payload.contains("Summary: Finished feature X"));
    }

    #[test]
    fn wake_payload_with_shutdown_request() {
        let agent = make_teammate("w1", "W");
        let msg = make_message("lead-1", "No longer needed", MailboxMessageType::ShutdownRequest);
        let payload = build_wake_payload(&agent, &[], &[msg], &roster(&["lead-1", "w1"]));

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
        let payload = build_wake_payload(&agent, &tasks, &[], &roster(&["lead-1", "worker-1"]));

        assert!(payload.contains("Current Task Board Summary"));
        assert!(payload.contains("Showing 2 of 2 tasks."));
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
        let payload = build_wake_payload(&agent, &[task], &[], &roster(&["lead-1", "worker-1"]));

        assert!(payload.contains("task-a…"));
        assert!(payload.contains("task-b…"));
        assert!(!payload.contains("task-a, task-b"));
    }

    #[test]
    fn wake_payload_empty() {
        let agent = make_lead();
        let payload = build_wake_payload(&agent, &[], &[], &roster(&["lead-1"]));

        assert!(payload.contains("No new messages"));
        assert!(payload.contains("No tasks on the board"));
        assert!(payload.contains("**Lead**"));
    }

    #[test]
    fn wake_payload_contains_agent_identity() {
        let agent = make_teammate("w1", "Worker1");
        let payload = build_wake_payload(&agent, &[], &[], &roster(&["w1"]));

        assert!(payload.contains("**Worker1**"));
        assert!(payload.contains("teammate"));
    }

    #[test]
    fn wake_payload_short_task_id_no_truncation() {
        let agent = make_lead();
        let task = make_task("short", "Short ID Task", TaskStatus::Pending);
        let payload = build_wake_payload(&agent, &[task], &[], &roster(&["lead-1", "worker-1"]));
        assert!(payload.contains("short…"));
    }
}
