//! Route-level audit write points (M8 E12-4).
//!
//! One middleware, mounted inside `auth_middleware` on every authenticated
//! route group (and on the runtime session-message routes), records an
//! `audit_events` row for each **successful** (2xx) state-changing request
//! that matches [`AUDIT_RULES`]: configuration writes (MCP, providers,
//! settings, agents, assistants, skills, cron, channels), approval decisions
//! (agent approvals, task approvals, tool-call approval cards), skill
//! import / install / publish, cross-session message delivery, and device
//! follow-ups. Reads and unmatched routes pass through untouched.
//!
//! The JSON request body (up to [`AUDIT_BODY_MAX_BYTES`]) is kept as the
//! event diff; the ledger redacts secrets before insert. Non-JSON or larger
//! bodies are recorded by size only and are streamed through unbuffered.
//! Device pairing / revoke are recorded by the device routes themselves.
use axum::{
    body::Body,
    extract::{OriginalUri, Request, State},
    http::{HeaderMap, Method, header},
    middleware::Next,
    response::Response,
};
use serde_json::{Value, json};

use aionui_auth::{
    AgentPrincipal, CurrentUser, DevicePrincipal, RUNTIME_CONVERSATION_ID_HEADER, RUNTIME_USER_ID_HEADER,
};
use aionui_db::{AuditActorKind, AuditSink, AuditVia, NewAuditEvent};

/// Largest JSON body buffered for the diff.
pub const AUDIT_BODY_MAX_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub struct RouteAuditState {
    pub sink: AuditSink,
    /// `Desktop` in local identity mode, `WebUi` otherwise.
    pub user_via: AuditVia,
}

/// Who may trigger a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Callers {
    Any,
    /// Only paired-device requests (the same route from the desktop is an
    /// ordinary message, not a governed action).
    DeviceOnly,
}

#[derive(Debug, Clone, Copy)]
struct AuditRule {
    method: &'static str,
    template: &'static str,
    action: &'static str,
    target_kind: &'static str,
    /// Path params joined with `/` to form the target id. Empty: no target.
    target_params: &'static [&'static str],
    /// Path param carrying an approval id (`with_approval`).
    approval_param: Option<&'static str>,
    callers: Callers,
}

const fn rule(
    method: &'static str,
    template: &'static str,
    action: &'static str,
    target_kind: &'static str,
    target_params: &'static [&'static str],
) -> AuditRule {
    AuditRule {
        method,
        template,
        action,
        target_kind,
        target_params,
        approval_param: None,
        callers: Callers::Any,
    }
}

const fn approval(
    template: &'static str,
    action: &'static str,
    target_kind: &'static str,
    target_params: &'static [&'static str],
    approval_param: &'static str,
) -> AuditRule {
    AuditRule {
        method: "POST",
        template,
        action,
        target_kind,
        target_params,
        approval_param: Some(approval_param),
        callers: Callers::Any,
    }
}

const ID: &[&str] = &["id"];
const NONE: &[&str] = &[];

/// Audited routes. Read-only POSTs (test-connection, detect, read, scan, …)
/// are deliberately absent.
const AUDIT_RULES: &[AuditRule] = &[
    // MCP servers
    rule("POST", "/api/mcp/servers", "config.mcp.create", "mcp_server", NONE),
    rule(
        "POST",
        "/api/mcp/servers/import",
        "config.mcp.import",
        "mcp_server",
        NONE,
    ),
    rule("PUT", "/api/mcp/servers/{id}", "config.mcp.update", "mcp_server", ID),
    rule("DELETE", "/api/mcp/servers/{id}", "config.mcp.delete", "mcp_server", ID),
    rule(
        "POST",
        "/api/mcp/servers/{id}/toggle",
        "config.mcp.toggle",
        "mcp_server",
        ID,
    ),
    rule(
        "POST",
        "/api/mcp/oauth/login",
        "config.mcp.oauth_login",
        "mcp_server",
        NONE,
    ),
    rule(
        "POST",
        "/api/mcp/oauth/logout",
        "config.mcp.oauth_logout",
        "mcp_server",
        NONE,
    ),
    // Model providers
    rule("POST", "/api/providers", "config.provider.create", "provider", NONE),
    rule("PUT", "/api/providers/{id}", "config.provider.update", "provider", ID),
    rule(
        "DELETE",
        "/api/providers/{id}",
        "config.provider.delete",
        "provider",
        ID,
    ),
    rule(
        "POST",
        "/api/providers/{id}/models",
        "config.provider.models",
        "provider",
        ID,
    ),
    // Settings
    rule("PATCH", "/api/settings", "config.settings.update", "settings", NONE),
    rule(
        "PUT",
        "/api/settings/client",
        "config.settings.client",
        "settings",
        NONE,
    ),
    // Agents
    rule("POST", "/api/agents/custom", "config.agent.create", "agent", NONE),
    rule("PUT", "/api/agents/custom/{id}", "config.agent.update", "agent", ID),
    rule("DELETE", "/api/agents/custom/{id}", "config.agent.delete", "agent", ID),
    rule("PATCH", "/api/agents/{id}/enabled", "config.agent.enabled", "agent", ID),
    rule(
        "PUT",
        "/api/agents/{id}/overrides",
        "config.agent.overrides",
        "agent",
        ID,
    ),
    rule(
        "POST",
        "/api/agents/codex/account/login",
        "config.agent.account_login",
        "agent",
        NONE,
    ),
    rule(
        "POST",
        "/api/agents/codex/account/logout",
        "config.agent.account_logout",
        "agent",
        NONE,
    ),
    rule(
        "POST",
        "/api/remote-agents",
        "config.remote_agent.create",
        "remote_agent",
        NONE,
    ),
    rule(
        "PUT",
        "/api/remote-agents/{id}",
        "config.remote_agent.update",
        "remote_agent",
        ID,
    ),
    rule(
        "DELETE",
        "/api/remote-agents/{id}",
        "config.remote_agent.delete",
        "remote_agent",
        ID,
    ),
    // Assistants / agent center
    rule("POST", "/api/assistants", "config.assistant.create", "assistant", NONE),
    rule(
        "POST",
        "/api/assistants/import",
        "config.assistant.import",
        "assistant",
        NONE,
    ),
    rule(
        "PUT",
        "/api/assistants/{id}",
        "config.assistant.update",
        "assistant",
        ID,
    ),
    rule(
        "DELETE",
        "/api/assistants/{id}",
        "config.assistant.delete",
        "assistant",
        ID,
    ),
    rule(
        "PATCH",
        "/api/assistants/{id}/state",
        "config.assistant.state",
        "assistant",
        ID,
    ),
    rule(
        "POST",
        "/api/agent-center/agents",
        "config.agent_center.create",
        "agent_center_agent",
        NONE,
    ),
    rule(
        "PUT",
        "/api/agent-center/agents/{id}",
        "config.agent_center.update",
        "agent_center_agent",
        ID,
    ),
    rule(
        "POST",
        "/api/agent-center/agents/{id}/publish",
        "agent_center.publish",
        "agent_center_agent",
        ID,
    ),
    rule(
        "POST",
        "/api/agent-center/agents/{id}/unpublish",
        "agent_center.unpublish",
        "agent_center_agent",
        ID,
    ),
    // Skills: import / install / publish and config
    rule("POST", "/api/skills/import", "skill.import", "skill", NONE),
    rule(
        "POST",
        "/api/skills/sources/import",
        "skill.import_source",
        "skill",
        NONE,
    ),
    rule(
        "POST",
        "/api/skills/id/{skill_id}/refresh",
        "skill.refresh",
        "skill",
        &["skill_id"],
    ),
    rule(
        "POST",
        "/api/skills/id/{skill_id}/review",
        "skill.review",
        "skill",
        &["skill_id"],
    ),
    rule(
        "PUT",
        "/api/skills/id/{skill_id}/visibility",
        "skill.publish",
        "skill",
        &["skill_id"],
    ),
    rule(
        "POST",
        "/api/skills/id/{skill_id}/copy-to-private",
        "skill.copy_private",
        "skill",
        &["skill_id"],
    ),
    rule(
        "POST",
        "/api/skills/id/{skill_id}/versions/{version_no}/restore",
        "skill.restore",
        "skill",
        &["skill_id"],
    ),
    rule(
        "DELETE",
        "/api/skills/{name}",
        "config.skill.delete",
        "skill",
        &["name"],
    ),
    rule(
        "POST",
        "/api/skills/assistant-rule/write",
        "config.skill.assistant_rule_write",
        "assistant",
        NONE,
    ),
    rule(
        "DELETE",
        "/api/skills/assistant-rule/{id}",
        "config.skill.assistant_rule_delete",
        "assistant",
        ID,
    ),
    rule(
        "POST",
        "/api/skills/assistant-skill/write",
        "config.skill.assistant_skill_write",
        "assistant",
        NONE,
    ),
    rule(
        "DELETE",
        "/api/skills/assistant-skill/{id}",
        "config.skill.assistant_skill_delete",
        "assistant",
        ID,
    ),
    rule(
        "POST",
        "/api/skills/external-paths",
        "config.skill.external_path_add",
        "skill_path",
        NONE,
    ),
    rule(
        "DELETE",
        "/api/skills/external-paths",
        "config.skill.external_path_remove",
        "skill_path",
        NONE,
    ),
    rule(
        "POST",
        "/api/skills/market/enable",
        "config.skill.market_enable",
        "settings",
        NONE,
    ),
    rule(
        "POST",
        "/api/skills/market/disable",
        "config.skill.market_disable",
        "settings",
        NONE,
    ),
    rule(
        "POST",
        "/api/skill-registry/installations",
        "skill.install",
        "skill",
        NONE,
    ),
    rule(
        "PUT",
        "/api/skill-registry/installations/{namespace}/{slug}",
        "skill.install_update",
        "skill",
        &["namespace", "slug"],
    ),
    approval(
        "/api/skill-evolution/proposals/{id}/approve",
        "approval.skill_proposal.approve",
        "skill_proposal",
        ID,
        "id",
    ),
    approval(
        "/api/skill-evolution/proposals/{id}/reject",
        "approval.skill_proposal.reject",
        "skill_proposal",
        ID,
        "id",
    ),
    rule(
        "POST",
        "/api/skill-evolution/proposals/{id}/apply",
        "skill.evolution_apply",
        "skill_proposal",
        ID,
    ),
    rule(
        "POST",
        "/api/skill-evolution/proposals/{id}/rollback",
        "skill.evolution_rollback",
        "skill_proposal",
        ID,
    ),
    rule(
        "PUT",
        "/api/skill-evolution/settings",
        "config.skill_evolution.settings",
        "settings",
        NONE,
    ),
    // Scheduled tasks
    // M8 E6-7: manual read-only GitHub Issue import into the backlog.
    rule(
        "POST",
        "/api/task-sessions/import/github",
        "task.import.github",
        "task_board",
        NONE,
    ),
    rule("POST", "/api/cron/jobs", "config.cron.create", "cron_job", NONE),
    rule("PUT", "/api/cron/jobs/{id}", "config.cron.update", "cron_job", ID),
    rule("DELETE", "/api/cron/jobs/{id}", "config.cron.delete", "cron_job", ID),
    rule(
        "POST",
        "/api/cron/jobs/{id}/skill",
        "config.cron.skill_set",
        "cron_job",
        ID,
    ),
    rule(
        "DELETE",
        "/api/cron/jobs/{id}/skill",
        "config.cron.skill_clear",
        "cron_job",
        ID,
    ),
    // IM channels
    rule(
        "PUT",
        "/api/channel/settings/{platform}/assistant",
        "config.channel.assistant",
        "channel",
        &["platform"],
    ),
    rule(
        "PUT",
        "/api/channel/settings/{platform}/default-model",
        "config.channel.default_model",
        "channel",
        &["platform"],
    ),
    rule(
        "POST",
        "/api/channel/settings/sync",
        "config.channel.sync",
        "channel",
        NONE,
    ),
    rule(
        "POST",
        "/api/channel/plugins/enable",
        "config.channel.plugin_enable",
        "channel",
        NONE,
    ),
    rule(
        "POST",
        "/api/channel/plugins/disable",
        "config.channel.plugin_disable",
        "channel",
        NONE,
    ),
    rule(
        "POST",
        "/api/channel/pairings/approve",
        "channel.pairing_approve",
        "channel_user",
        NONE,
    ),
    rule(
        "POST",
        "/api/channel/pairings/reject",
        "channel.pairing_reject",
        "channel_user",
        NONE,
    ),
    rule(
        "POST",
        "/api/channel/users/revoke",
        "channel.user_revoke",
        "channel_user",
        NONE,
    ),
    // Approval decisions
    approval(
        "/api/agent-approvals/{id}/approve",
        "approval.agent.approve",
        "agent_approval",
        ID,
        "id",
    ),
    approval(
        "/api/agent-approvals/{id}/deny",
        "approval.agent.deny",
        "agent_approval",
        ID,
        "id",
    ),
    approval(
        "/api/task-sessions/{id}/approvals/{approval_id}/decision",
        "approval.task.decide",
        "task_session",
        ID,
        "approval_id",
    ),
    // D12-8: only tool calls that raised an approval card are audited, and
    // this is the card's decision endpoint.
    approval(
        "/api/conversations/{id}/confirmations/{callId}/confirm",
        "approval.tool_call.confirm",
        "conversation",
        ID,
        "callId",
    ),
    // Cross-session delivery (runtime token, agent caller)
    rule(
        "POST",
        "/api/runtime/session-messages/send",
        "session_message.deliver",
        "conversation",
        NONE,
    ),
    // Phone follow-ups (desktop sends are ordinary chat, not audited)
    AuditRule {
        method: "POST",
        template: "/api/conversations/{id}/messages",
        action: "conversation.follow_up",
        target_kind: "conversation",
        target_params: ID,
        approval_param: None,
        callers: Callers::DeviceOnly,
    },
];

/// Match `path` against a `{param}` template; returns the params on a match.
fn match_template<'a>(template: &'static str, path: &'a str) -> Option<Vec<(&'static str, &'a str)>> {
    let path = path.trim_end_matches('/');
    let mut params = Vec::new();
    let mut want = template.split('/');
    let mut got = path.split('/');
    loop {
        match (want.next(), got.next()) {
            (None, None) => return Some(params),
            (Some(w), Some(g)) => {
                if let Some(name) = w.strip_prefix('{').and_then(|w| w.strip_suffix('}')) {
                    if g.is_empty() {
                        return None;
                    }
                    params.push((name, g));
                } else if w != g {
                    return None;
                }
            }
            _ => return None,
        }
    }
}

struct Matched<'a> {
    rule: &'static AuditRule,
    params: Vec<(&'static str, &'a str)>,
}

fn find_rule<'a>(method: &Method, path: &'a str) -> Option<Matched<'a>> {
    AUDIT_RULES.iter().find_map(|rule| {
        if rule.method != method.as_str() {
            return None;
        }
        match_template(rule.template, path).map(|params| Matched { rule, params })
    })
}

fn param<'a>(params: &[(&'static str, &'a str)], name: &str) -> Option<&'a str> {
    params.iter().find(|(key, _)| *key == name).map(|(_, value)| *value)
}

/// Who made the request, as seen after authentication.
struct Caller {
    user_id: Option<String>,
    actor_kind: AuditActorKind,
    actor_id: Option<String>,
    via: AuditVia,
    is_device: bool,
}

fn caller_of(request: &Request, user_via: AuditVia) -> Caller {
    let ext = request.extensions();
    let user_id = ext.get::<CurrentUser>().map(|user| user.id.clone());
    if let Some(device) = ext.get::<DevicePrincipal>() {
        return Caller {
            user_id,
            actor_kind: AuditActorKind::Device,
            actor_id: Some(device.device_id.clone()),
            via: AuditVia::Mobile,
            is_device: true,
        };
    }
    if let Some(agent) = ext.get::<AgentPrincipal>() {
        return Caller {
            user_id,
            actor_kind: AuditActorKind::Agent,
            actor_id: Some(agent.conversation_id.clone()),
            via: AuditVia::Helper,
            is_device: false,
        };
    }
    if user_id.is_none() {
        // Runtime-token routes authenticate in the handler; a 2xx response
        // means the headers were verified.
        let header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        let runtime_user = header(RUNTIME_USER_ID_HEADER);
        let runtime_conversation = header(RUNTIME_CONVERSATION_ID_HEADER);
        if runtime_user.is_some() || runtime_conversation.is_some() {
            return Caller {
                user_id: runtime_user,
                actor_kind: AuditActorKind::Agent,
                actor_id: runtime_conversation,
                via: AuditVia::Helper,
                is_device: false,
            };
        }
    }
    Caller {
        actor_id: user_id.clone(),
        user_id,
        actor_kind: AuditActorKind::User,
        via: user_via,
        is_device: false,
    }
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().to_ascii_lowercase().starts_with("application/json"))
}

fn content_length(headers: &HeaderMap) -> Option<usize> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

pub async fn route_audit_middleware(State(state): State<RouteAuditState>, request: Request, next: Next) -> Response {
    let method = request.method().clone();
    if matches!(method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(request).await;
    }
    let path = request
        .extensions()
        .get::<OriginalUri>()
        .map_or_else(|| request.uri().path().to_owned(), |uri| uri.path().to_owned());
    let Some(matched) = find_rule(&method, &path) else {
        return next.run(request).await;
    };
    let rule = matched.rule;
    let caller = caller_of(&request, state.user_via);
    if rule.callers == Callers::DeviceOnly && !caller.is_device {
        return next.run(request).await;
    }
    let target_id = if rule.target_params.is_empty() {
        None
    } else {
        let parts: Option<Vec<&str>> = rule
            .target_params
            .iter()
            .map(|name| param(&matched.params, name))
            .collect();
        parts.map(|parts| parts.join("/"))
    };
    let approval_id = rule
        .approval_param
        .and_then(|name| param(&matched.params, name))
        .map(str::to_owned);

    // Buffer small JSON bodies with a declared length for the diff; stream
    // everything else (chunked, large, or non-JSON) untouched.
    let declared_len = content_length(request.headers());
    let buffer_len = declared_len.filter(|len| *len <= AUDIT_BODY_MAX_BYTES && is_json(request.headers()));
    let (request, body_value) = if let Some(len) = buffer_len {
        let (parts, body) = request.into_parts();
        match axum::body::to_bytes(body, len).await {
            Ok(bytes) => {
                let value =
                    serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|_| json!({ "request_bytes": bytes.len() }));
                (Request::from_parts(parts, Body::from(bytes)), Some(value))
            }
            Err(_) => {
                // The body is consumed (client aborted or sent more than it
                // declared); refuse rather than forward a truncated one.
                return axum::response::IntoResponse::into_response((
                    axum::http::StatusCode::BAD_REQUEST,
                    "invalid request body",
                ));
            }
        }
    } else {
        (request, None)
    };

    let response = next.run(request).await;
    if !response.status().is_success() {
        return response;
    }

    let mut diff = json!({ "method": method.as_str(), "path": path });
    match body_value {
        Some(value) => diff["request"] = value,
        None => {
            if let Some(len) = declared_len {
                diff["request_bytes"] = json!(len);
            }
        }
    }
    if let Some(call_id) = rule
        .approval_param
        .filter(|name| *name == "callId")
        .and_then(|name| param(&matched.params, name))
    {
        diff["call_id"] = json!(call_id);
    }
    let mut event = NewAuditEvent::new(rule.action, caller.actor_kind, caller.via)
        .with_summary(format!("{} {}", method.as_str(), rule.template))
        .with_diff(diff);
    if let Some(user_id) = &caller.user_id {
        event = event.with_user(user_id.clone());
    }
    if let Some(actor_id) = &caller.actor_id {
        event = event.with_actor(actor_id.clone());
    }
    if let Some(target_id) = target_id {
        event = event.with_target(rule.target_kind, target_id);
    }
    if let Some(approval_id) = approval_id {
        let approver = (caller.actor_kind == AuditActorKind::User || caller.is_device)
            .then(|| caller.user_id.clone())
            .flatten();
        event = event.with_approval(approval_id, approver);
    }
    state.sink.record(event).await;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_match_params_and_reject_other_shapes() {
        assert_eq!(
            match_template("/api/providers/{id}", "/api/providers/p1"),
            Some(vec![("id", "p1")])
        );
        assert_eq!(
            match_template("/api/providers/{id}", "/api/providers/p1/"),
            Some(vec![("id", "p1")])
        );
        assert!(match_template("/api/providers/{id}", "/api/providers").is_none());
        assert!(match_template("/api/providers/{id}", "/api/providers/p1/models").is_none());
        assert!(match_template("/api/providers", "/api/providers2").is_none());
    }

    #[test]
    fn read_only_posts_and_reads_are_not_audited() {
        assert!(find_rule(&Method::POST, "/api/mcp/test-connection").is_none());
        assert!(find_rule(&Method::POST, "/api/providers/fetch-models").is_none());
        assert!(find_rule(&Method::POST, "/api/skills/assistant-rule/read").is_none());
        assert!(find_rule(&Method::GET, "/api/providers").is_none());
        assert_eq!(
            find_rule(&Method::POST, "/api/task-sessions/import/github")
                .unwrap()
                .rule
                .action,
            "task.import.github"
        );
        // `import` is a literal segment, not an `{id}`.
        assert_eq!(
            find_rule(&Method::POST, "/api/mcp/servers/import").unwrap().rule.action,
            "config.mcp.import"
        );
    }

    #[test]
    fn every_rule_is_well_formed_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for rule in AUDIT_RULES {
            assert!(aionui_db::audit::is_valid_action(rule.action), "{}", rule.action);
            assert!(seen.insert((rule.method, rule.template)), "duplicate {}", rule.template);
            for name in rule.target_params.iter().chain(rule.approval_param.iter()) {
                assert!(
                    rule.template.contains(&format!("{{{name}}}")),
                    "{}: {name}",
                    rule.template
                );
            }
        }
    }

    mod http {
        use std::sync::Arc;

        use axum::{
            Router,
            body::Body,
            http::{Request as HttpRequest, StatusCode},
            middleware::{from_fn, from_fn_with_state},
            routing::{post, put},
        };
        use tower::ServiceExt;

        use super::super::*;
        use aionui_db::{
            AuditEventFilter, AuditEventRow, AuditOrder, IAuditEventRepository, SqliteAuditEventRepository,
            init_database_memory,
        };

        /// Stands in for `auth_middleware`: the `x-test-caller` header picks
        /// the principal it would have inserted.
        async fn fake_auth(mut request: Request, next: Next) -> Response {
            let caller = request
                .headers()
                .get("x-test-caller")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("user")
                .to_owned();
            request.extensions_mut().insert(CurrentUser::local_default());
            match caller.as_str() {
                "device" => {
                    request.extensions_mut().insert(DevicePrincipal {
                        device_id: "dev-1".into(),
                        scopes: vec!["mobile_default".into()],
                    });
                }
                "agent" => {
                    request.extensions_mut().insert(AgentPrincipal {
                        conversation_id: "conv-9".into(),
                    });
                }
                _ => {}
            }
            next.run(request).await
        }

        async fn app() -> (Router, Arc<dyn IAuditEventRepository>, aionui_db::Database) {
            let db = init_database_memory().await.unwrap();
            let repo = Arc::new(SqliteAuditEventRepository::new(db.pool().clone())) as Arc<dyn IAuditEventRepository>;
            let state = RouteAuditState {
                sink: AuditSink::new(repo.clone()),
                user_via: AuditVia::Desktop,
            };
            let ok = || async { "ok" };
            let router = Router::new()
                .route("/api/providers/{id}", put(ok))
                .route("/api/providers/fetch-models", post(ok))
                .route("/api/agent-approvals/{id}/approve", post(ok))
                .route("/api/conversations/{id}/messages", post(ok))
                .route("/api/conversations/{id}/confirmations/{callId}/confirm", post(ok))
                .route("/api/mcp/servers", post(|| async { (StatusCode::BAD_REQUEST, "no") }))
                .route_layer(from_fn_with_state(state, route_audit_middleware))
                .route_layer(from_fn(fake_auth));
            (router, repo, db)
        }

        async fn send(
            app: &Router,
            method: &str,
            uri: &str,
            caller: &str,
            body: Option<serde_json::Value>,
        ) -> StatusCode {
            let mut req = HttpRequest::builder()
                .method(method)
                .uri(uri)
                .header("x-test-caller", caller);
            let body = match body {
                Some(body) => {
                    let body = body.to_string();
                    req = req
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::CONTENT_LENGTH, body.len());
                    Body::from(body)
                }
                None => Body::empty(),
            };
            app.clone().oneshot(req.body(body).unwrap()).await.unwrap().status()
        }

        async fn rows(repo: &Arc<dyn IAuditEventRepository>) -> Vec<AuditEventRow> {
            repo.list(&AuditEventFilter {
                order: AuditOrder::OldestFirst,
                ..Default::default()
            })
            .await
            .unwrap()
        }

        #[tokio::test]
        async fn successful_writes_are_recorded_with_redacted_body() {
            let (app, repo, _db) = app().await;
            let body = json!({ "name": "OpenAI", "api_key": "sk-secret-value" });
            assert_eq!(
                send(&app, "PUT", "/api/providers/p1", "user", Some(body)).await,
                StatusCode::OK
            );
            let rows = rows(&repo).await;
            assert_eq!(rows.len(), 1);
            let row = &rows[0];
            assert_eq!(row.action, "config.provider.update");
            assert_eq!(row.actor_kind, "user");
            assert_eq!(row.via, "desktop");
            assert_eq!(row.user_id.as_deref(), Some("system_default_user"));
            assert_eq!(row.target_kind.as_deref(), Some("provider"));
            assert_eq!(row.target_id.as_deref(), Some("p1"));
            let diff = row.diff.as_deref().unwrap();
            assert!(diff.contains("OpenAI"), "{diff}");
            assert!(!diff.contains("sk-secret-value"), "{diff}");
        }

        #[tokio::test]
        async fn failures_reads_and_read_only_posts_are_not_recorded() {
            let (app, repo, _db) = app().await;
            assert_eq!(
                send(&app, "POST", "/api/mcp/servers", "user", Some(json!({}))).await,
                StatusCode::BAD_REQUEST
            );
            assert_eq!(
                send(&app, "POST", "/api/providers/fetch-models", "user", Some(json!({}))).await,
                StatusCode::OK
            );
            assert_eq!(
                send(
                    &app,
                    "POST",
                    "/api/conversations/c1/messages",
                    "user",
                    Some(json!({"content": "hi"}))
                )
                .await,
                StatusCode::OK
            );
            assert!(rows(&repo).await.is_empty());
        }

        #[tokio::test]
        async fn device_agent_and_approval_callers_are_attributed() {
            let (app, repo, _db) = app().await;
            send(
                &app,
                "POST",
                "/api/conversations/c1/messages",
                "device",
                Some(json!({"content": "hi"})),
            )
            .await;
            send(&app, "POST", "/api/agent-approvals/ap-1/approve", "device", None).await;
            send(&app, "PUT", "/api/providers/p2", "agent", Some(json!({}))).await;
            send(
                &app,
                "POST",
                "/api/conversations/c1/confirmations/call-7/confirm",
                "user",
                Some(json!({"decision": "allow"})),
            )
            .await;
            let rows = rows(&repo).await;
            let summary: Vec<(&str, &str, &str, Option<&str>)> = rows
                .iter()
                .map(|r| {
                    (
                        r.action.as_str(),
                        r.actor_kind.as_str(),
                        r.via.as_str(),
                        r.actor_id.as_deref(),
                    )
                })
                .collect();
            assert_eq!(
                summary,
                [
                    ("conversation.follow_up", "device", "mobile", Some("dev-1")),
                    ("approval.agent.approve", "device", "mobile", Some("dev-1")),
                    ("config.provider.update", "agent", "helper", Some("conv-9")),
                    (
                        "approval.tool_call.confirm",
                        "user",
                        "desktop",
                        Some("system_default_user")
                    ),
                ]
            );
            assert_eq!(rows[1].approval_id.as_deref(), Some("ap-1"));
            assert_eq!(rows[1].approver_id.as_deref(), Some("system_default_user"));
            assert_eq!(rows[2].approval_id, None);
            assert_eq!(rows[3].approval_id.as_deref(), Some("call-7"));
            assert!(rows[3].diff.as_deref().unwrap().contains("call-7"));
        }
    }
}
