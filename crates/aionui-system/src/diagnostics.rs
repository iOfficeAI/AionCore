use std::collections::BTreeSet;
use std::sync::Arc;

use aionui_api_types::{
    FeedbackDiagnosticsContextResponse, FeedbackDiagnosticsPrivacyResponse, FeedbackDiagnosticsProfileResponse,
    FeedbackDiagnosticsQuery, FeedbackDiagnosticsResponse,
};
use aionui_db::{
    FeedbackDiagnosticsDbContext, FeedbackDiagnosticsProfile, FeedbackDiagnosticsRequest,
    IFeedbackDiagnosticsRepository,
};

use crate::error::SystemError;

#[derive(Clone)]
pub struct FeedbackDiagnosticsService {
    repo: Arc<dyn IFeedbackDiagnosticsRepository>,
}

impl FeedbackDiagnosticsService {
    pub fn new(repo: Arc<dyn IFeedbackDiagnosticsRepository>) -> Self {
        Self { repo }
    }

    pub async fn collect(
        &self,
        user_id: &str,
        query: FeedbackDiagnosticsQuery,
    ) -> Result<FeedbackDiagnosticsResponse, SystemError> {
        let context = resolve_context(&query);
        let profiles = resolve_profiles(&query, context.conversation_id.as_deref());
        let db_request = FeedbackDiagnosticsRequest {
            user_id: user_id.to_owned(),
            profiles: profiles.clone(),
            context: FeedbackDiagnosticsDbContext {
                conversation_id: context.conversation_id.clone(),
                provider_id: context.provider_id.clone(),
                agent_id: context.agent_id.clone(),
                team_id: context.team_id.clone(),
                mcp_server_id: context.mcp_server_id.clone(),
            },
        };

        let result = self.repo.collect_feedback_diagnostics(&db_request).await?;
        let profile_names = profiles.iter().map(|profile| profile.as_name().to_owned()).collect();

        Ok(FeedbackDiagnosticsResponse {
            schema_version: result.schema_version,
            context: FeedbackDiagnosticsContextResponse {
                route_at_open: query.route_at_open,
                route_at_submit: query.route_at_submit,
                selected_module: query.selected_module,
                conversation_id: context.conversation_id,
                provider_id: context.provider_id,
                agent_id: context.agent_id,
                team_id: context.team_id,
                mcp_server_id: context.mcp_server_id,
                selected_profiles: profile_names,
            },
            profiles: result
                .profiles
                .into_iter()
                .map(|profile| FeedbackDiagnosticsProfileResponse {
                    name: profile.name,
                    mode: profile.mode,
                    data: profile.data,
                    warnings: profile.warnings,
                })
                .collect(),
            privacy: FeedbackDiagnosticsPrivacyResponse {
                redaction: "aionCore returns selected metadata, ids, titles, status fields, counts, endpoint hosts, selected configuration values, and MCP original_json keeps connection structure while credential values are redacted; raw error and tool-call diagnostic content may be included; non-error message content and prompts are summarized or redacted."
                    .to_owned(),
                raw_content_included: true,
                api_keys_included: false,
            },
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ResolvedContext {
    conversation_id: Option<String>,
    provider_id: Option<String>,
    agent_id: Option<String>,
    team_id: Option<String>,
    mcp_server_id: Option<String>,
}

fn resolve_context(query: &FeedbackDiagnosticsQuery) -> ResolvedContext {
    let route_conversation_id = query
        .route_at_submit
        .as_deref()
        .and_then(extract_conversation_id)
        .or_else(|| query.route_at_open.as_deref().and_then(extract_conversation_id));

    ResolvedContext {
        conversation_id: query.conversation_id.clone().or(route_conversation_id),
        provider_id: query.provider_id.clone(),
        agent_id: query.agent_id.clone(),
        team_id: query.team_id.clone(),
        mcp_server_id: query.mcp_server_id.clone(),
    }
}

fn resolve_profiles(
    query: &FeedbackDiagnosticsQuery,
    conversation_id: Option<&str>,
) -> Vec<FeedbackDiagnosticsProfile> {
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();

    if let Some(route) = query.route_at_submit.as_deref() {
        push_route_profiles(&mut selected, &mut seen, route, conversation_id);
    }
    if let Some(route) = query.route_at_open.as_deref() {
        push_route_profiles(&mut selected, &mut seen, route, conversation_id);
    }
    if let Some(module) = query.selected_module.as_deref() {
        push_module_profiles(&mut selected, &mut seen, module);
    }
    if let Some(profiles) = query.profiles.as_deref() {
        for profile in profiles.split(',').filter_map(|value| parse_profile(value.trim())) {
            push_profile(&mut selected, &mut seen, profile);
        }
    }
    push_profile(&mut selected, &mut seen, FeedbackDiagnosticsProfile::GlobalSummary);

    selected
}

fn push_route_profiles(
    selected: &mut Vec<FeedbackDiagnosticsProfile>,
    seen: &mut BTreeSet<FeedbackDiagnosticsProfile>,
    route: &str,
    conversation_id: Option<&str>,
) {
    let route = route.to_ascii_lowercase();

    if conversation_id.is_some()
        || route.contains("conversation")
        || route.contains("chat")
        || route.contains("session")
    {
        push_profile(selected, seen, FeedbackDiagnosticsProfile::ConversationSession);
        push_profile(selected, seen, FeedbackDiagnosticsProfile::ModelAuth);
        push_profile(selected, seen, FeedbackDiagnosticsProfile::McpTools);
    }
    if route.contains("team") || route.contains("collaboration") {
        push_profile(selected, seen, FeedbackDiagnosticsProfile::AgentTeam);
    }
    if route.contains("mcp") || route.contains("tool") {
        push_profile(selected, seen, FeedbackDiagnosticsProfile::McpTools);
    }
    if route.contains("provider") || route.contains("model") || route.contains("settings") || route.contains("agent") {
        push_profile(selected, seen, FeedbackDiagnosticsProfile::ModelAuth);
    }
}

fn push_module_profiles(
    selected: &mut Vec<FeedbackDiagnosticsProfile>,
    seen: &mut BTreeSet<FeedbackDiagnosticsProfile>,
    module: &str,
) {
    match module.to_ascii_lowercase().as_str() {
        "conversation-session"
        | "dialogue-session"
        | "assistant-preset"
        | "channel"
        | "search-history"
        | "workspace-preview"
        | "display-desktop"
        | "对话与会话" => {
            push_profile(selected, seen, FeedbackDiagnosticsProfile::ConversationSession);
            push_profile(selected, seen, FeedbackDiagnosticsProfile::ModelAuth);
            push_profile(selected, seen, FeedbackDiagnosticsProfile::McpTools);
        }
        "agent-detection" => {
            push_profile(selected, seen, FeedbackDiagnosticsProfile::ModelAuth);
            push_profile(selected, seen, FeedbackDiagnosticsProfile::McpTools);
            push_profile(selected, seen, FeedbackDiagnosticsProfile::ConversationSession);
        }
        "agent-team" | "team-collaboration" | "团队协作" => {
            push_profile(selected, seen, FeedbackDiagnosticsProfile::AgentTeam);
            push_profile(selected, seen, FeedbackDiagnosticsProfile::ConversationSession);
        }
        "mcp-tools" | "mcp" => {
            push_profile(selected, seen, FeedbackDiagnosticsProfile::McpTools);
        }
        "model-auth" => {
            push_profile(selected, seen, FeedbackDiagnosticsProfile::ModelAuth);
        }
        "skills-plugin" => {
            push_profile(selected, seen, FeedbackDiagnosticsProfile::McpTools);
            push_profile(selected, seen, FeedbackDiagnosticsProfile::ConversationSession);
        }
        "webui-remote" => {
            push_profile(selected, seen, FeedbackDiagnosticsProfile::ModelAuth);
            push_profile(selected, seen, FeedbackDiagnosticsProfile::McpTools);
        }
        "scheduled-task" => {
            push_profile(selected, seen, FeedbackDiagnosticsProfile::ConversationSession);
            push_profile(selected, seen, FeedbackDiagnosticsProfile::AgentTeam);
        }
        "system-settings" | "settings" | "系统设置" => {
            push_profile(selected, seen, FeedbackDiagnosticsProfile::ModelAuth);
            push_profile(selected, seen, FeedbackDiagnosticsProfile::McpTools);
        }
        _ => {}
    }
}

fn push_profile(
    selected: &mut Vec<FeedbackDiagnosticsProfile>,
    seen: &mut BTreeSet<FeedbackDiagnosticsProfile>,
    profile: FeedbackDiagnosticsProfile,
) {
    if seen.insert(profile.clone()) {
        selected.push(profile);
    }
}

fn parse_profile(value: &str) -> Option<FeedbackDiagnosticsProfile> {
    match value {
        "conversation-session" => Some(FeedbackDiagnosticsProfile::ConversationSession),
        "model-auth" => Some(FeedbackDiagnosticsProfile::ModelAuth),
        "agent-team" => Some(FeedbackDiagnosticsProfile::AgentTeam),
        "mcp-tools" => Some(FeedbackDiagnosticsProfile::McpTools),
        "global-summary" => Some(FeedbackDiagnosticsProfile::GlobalSummary),
        _ => None,
    }
}

fn extract_conversation_id(route: &str) -> Option<String> {
    extract_query_value(route, "conversationId")
        .or_else(|| extract_query_value(route, "conversation_id"))
        .or_else(|| extract_segment_after(route, "conversations"))
        .or_else(|| extract_segment_after(route, "conversation"))
}

fn extract_query_value(route: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let start = route.find(&needle)? + needle.len();
    let value = route[start..]
        .split(['&', '#'])
        .next()
        .unwrap_or_default()
        .trim_matches('/');
    non_reserved_id(value)
}

fn extract_segment_after(route: &str, marker: &str) -> Option<String> {
    let normalized = route.replace(['#', '?', '&', '='], "/");
    let mut segments = normalized.split('/').filter(|segment| !segment.is_empty());
    while let Some(segment) = segments.next() {
        if segment == marker
            && let Some(value) = segments.next()
        {
            return non_reserved_id(value);
        }
    }
    None
}

fn non_reserved_id(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || matches!(value, "new" | "clone" | "active-count" | "search") {
        return None;
    }
    Some(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_context_takes_submit_route_before_open_route() {
        let query = FeedbackDiagnosticsQuery {
            route_at_open: Some("#/conversations/conv-open".to_owned()),
            route_at_submit: Some("#/conversations/conv-submit".to_owned()),
            ..FeedbackDiagnosticsQuery::default()
        };

        let context = resolve_context(&query);

        assert_eq!(context.conversation_id.as_deref(), Some("conv-submit"));
    }

    #[test]
    fn explicit_context_overrides_route_extraction() {
        let query = FeedbackDiagnosticsQuery {
            route_at_submit: Some("#/conversations/conv-route".to_owned()),
            conversation_id: Some("conv-explicit".to_owned()),
            ..FeedbackDiagnosticsQuery::default()
        };

        let context = resolve_context(&query);

        assert_eq!(context.conversation_id.as_deref(), Some("conv-explicit"));
    }

    #[test]
    fn profile_resolution_unions_route_module_and_explicit_profiles() {
        let query = FeedbackDiagnosticsQuery {
            route_at_submit: Some("#/conversations/conv-1".to_owned()),
            selected_module: Some("system-settings".to_owned()),
            profiles: Some("agent-team".to_owned()),
            ..FeedbackDiagnosticsQuery::default()
        };

        let profiles = resolve_profiles(&query, Some("conv-1"));

        assert_eq!(profiles[0], FeedbackDiagnosticsProfile::ConversationSession);
        assert!(profiles.contains(&FeedbackDiagnosticsProfile::ModelAuth));
        assert!(profiles.contains(&FeedbackDiagnosticsProfile::McpTools));
        assert!(profiles.contains(&FeedbackDiagnosticsProfile::AgentTeam));
        assert!(profiles.contains(&FeedbackDiagnosticsProfile::GlobalSummary));
    }

    #[test]
    fn known_feedback_modules_resolve_to_diagnostic_profiles() {
        let cases = [
            (
                "agent-detection",
                vec![
                    FeedbackDiagnosticsProfile::ModelAuth,
                    FeedbackDiagnosticsProfile::McpTools,
                    FeedbackDiagnosticsProfile::ConversationSession,
                ],
            ),
            (
                "assistant-preset",
                vec![
                    FeedbackDiagnosticsProfile::ConversationSession,
                    FeedbackDiagnosticsProfile::ModelAuth,
                    FeedbackDiagnosticsProfile::McpTools,
                ],
            ),
            ("model-auth", vec![FeedbackDiagnosticsProfile::ModelAuth]),
            ("mcp-tools", vec![FeedbackDiagnosticsProfile::McpTools]),
            (
                "skills-plugin",
                vec![
                    FeedbackDiagnosticsProfile::McpTools,
                    FeedbackDiagnosticsProfile::ConversationSession,
                ],
            ),
            ("channel", vec![FeedbackDiagnosticsProfile::ConversationSession]),
            ("search-history", vec![FeedbackDiagnosticsProfile::ConversationSession]),
            (
                "workspace-preview",
                vec![FeedbackDiagnosticsProfile::ConversationSession],
            ),
            (
                "webui-remote",
                vec![
                    FeedbackDiagnosticsProfile::ModelAuth,
                    FeedbackDiagnosticsProfile::McpTools,
                ],
            ),
            (
                "scheduled-task",
                vec![
                    FeedbackDiagnosticsProfile::ConversationSession,
                    FeedbackDiagnosticsProfile::AgentTeam,
                ],
            ),
            (
                "agent-team",
                vec![
                    FeedbackDiagnosticsProfile::AgentTeam,
                    FeedbackDiagnosticsProfile::ConversationSession,
                ],
            ),
            ("display-desktop", vec![FeedbackDiagnosticsProfile::ConversationSession]),
            (
                "system-settings",
                vec![
                    FeedbackDiagnosticsProfile::ModelAuth,
                    FeedbackDiagnosticsProfile::McpTools,
                ],
            ),
            ("other", vec![]),
        ];

        for (module, expected_profiles) in cases {
            let query = FeedbackDiagnosticsQuery {
                selected_module: Some(module.to_owned()),
                ..FeedbackDiagnosticsQuery::default()
            };

            let profiles = resolve_profiles(&query, None);

            for profile in expected_profiles {
                assert!(
                    profiles.contains(&profile),
                    "{module} should include {}",
                    profile.as_name()
                );
            }
            assert!(
                profiles.contains(&FeedbackDiagnosticsProfile::GlobalSummary),
                "{module} should always include global-summary"
            );
        }
    }
}
