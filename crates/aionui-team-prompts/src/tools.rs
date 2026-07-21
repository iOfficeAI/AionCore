pub use aionui_api_types::{
    TEAM_DESCRIBE_ASSISTANT_DESCRIPTION, TEAM_LIST_ASSISTANTS_DESCRIPTION, TEAM_SPAWN_AGENT_DESCRIPTION,
    TeamToolDescriptor, TeamToolPermission, TeamToolRole, team_tool_descriptors,
};

#[derive(Debug, Clone)]
pub struct TeamToolSpec {
    pub name: &'static str,
    pub permission: TeamToolPermission,
    pub description: String,
    pub input_schema: serde_json::Value,
}

pub fn team_tool_specs() -> Vec<TeamToolSpec> {
    team_tool_descriptors()
        .into_iter()
        .map(|descriptor| TeamToolSpec {
            name: aionui_api_types::TeamToolName::parse(&descriptor.name)
                .expect("descriptor must use canonical team tool name")
                .as_str(),
            permission: descriptor.permission,
            description: descriptor.description,
            input_schema: descriptor.input_schema,
        })
        .collect()
}

pub fn visible_team_tool_descriptors(is_lead: bool) -> Vec<TeamToolDescriptor> {
    let role = if is_lead {
        TeamToolRole::Lead
    } else {
        TeamToolRole::Teammate
    };
    aionui_api_types::team_tool_descriptors_for_role(role)
}

pub fn authorize_team_tool(is_lead: bool, tool_name: &str) -> Result<(), String> {
    let Some(spec) = aionui_api_types::team_tool_descriptor(tool_name) else {
        return Err(format!("Unknown tool: {tool_name}"));
    };
    if spec.permission == TeamToolPermission::LeadOnly && !is_lead {
        return Err(format!("Only Lead can use {tool_name}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_lead_tools_list_hides_lead_only_tools() {
        let names: Vec<String> = visible_team_tool_descriptors(false)
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert!(!names.contains(&"team_spawn_agent".to_owned()));
        assert!(!names.contains(&"team_rename_agent".to_owned()));
        assert!(!names.contains(&"team_shutdown_agent".to_owned()));
        assert!(names.contains(&"team_send_message".to_owned()));
    }

    #[test]
    fn authorization_rejects_non_lead_rename() {
        let err = authorize_team_tool(false, "team_rename_agent").unwrap_err();
        assert_eq!(err, "Only Lead can use team_rename_agent");
    }

    #[test]
    fn descriptor_permissions_are_shared() {
        let permissions: Vec<(String, TeamToolPermission)> = team_tool_descriptors()
            .into_iter()
            .map(|tool| (tool.name, tool.permission))
            .collect();
        assert_eq!(
            permissions,
            vec![
                ("team_members".to_owned(), TeamToolPermission::AnyTeamAgent),
                ("team_send_message".to_owned(), TeamToolPermission::AnyTeamAgent),
                ("team_task_create".to_owned(), TeamToolPermission::AnyTeamAgent),
                ("team_task_update".to_owned(), TeamToolPermission::AnyTeamAgent),
                ("team_task_list".to_owned(), TeamToolPermission::AnyTeamAgent),
                ("team_list_assistants".to_owned(), TeamToolPermission::AnyTeamAgent),
                ("team_describe_assistant".to_owned(), TeamToolPermission::AnyTeamAgent),
                ("team_spawn_agent".to_owned(), TeamToolPermission::LeadOnly),
                ("team_rename_agent".to_owned(), TeamToolPermission::LeadOnly),
                ("team_shutdown_agent".to_owned(), TeamToolPermission::LeadOnly),
            ]
        );
    }
}
