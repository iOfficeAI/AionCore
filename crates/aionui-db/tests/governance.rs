//! M8 E12-3: roles, run permissions and launch policies (migration 080).
use std::sync::Arc;

use aionui_common::AgentType;
use aionui_db::governance::{
    BOOTSTRAP_OWNER_USER_ID, evaluate_policies, is_full_auto_mode, is_no_sandbox_mode, normalize_policy_value,
};
use aionui_db::{
    AuditEventFilter, AuditOrder, AuditSink, GovernanceError, GovernanceService, IAuditEventRepository,
    IGovernanceRepository, LaunchFacts, LaunchSurface, PolicyKey, PolicySet, SqliteAuditEventRepository,
    SqliteGovernanceRepository, UserRole, init_database_memory,
};
use serde_json::json;

async fn service(roles_active: bool) -> (GovernanceService, Arc<dyn IAuditEventRepository>) {
    let db = init_database_memory().await.unwrap();
    let audit: Arc<dyn IAuditEventRepository> = Arc::new(SqliteAuditEventRepository::new(db.pool().clone()));
    let repo = Arc::new(SqliteGovernanceRepository::new(db.pool().clone()));
    (
        GovernanceService::new(repo, Some(AuditSink::new(audit.clone())), roles_active),
        audit,
    )
}

async fn actions(audit: &Arc<dyn IAuditEventRepository>) -> Vec<String> {
    audit
        .list(&AuditEventFilter {
            order: AuditOrder::OldestFirst,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.action)
        .collect()
}

#[tokio::test]
async fn migration_seeds_the_bootstrap_owner_and_no_policies() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteGovernanceRepository::new(db.pool().clone());
    let owner = repo.get_role(BOOTSTRAP_OWNER_USER_ID).await.unwrap();
    // The seed row only exists when the system user does; either way the
    // bootstrap account resolves to owner (see role_of).
    if let Some(owner) = owner {
        assert_eq!(owner.role, "owner");
    }
    assert!(repo.list_policies().await.unwrap().is_empty());
    assert_eq!(PolicySet::from_rows(&[]), PolicySet::default());
}

#[tokio::test]
async fn desktop_mode_is_always_owner() {
    let (svc, _) = service(false).await;
    assert_eq!(svc.role_of("anyone").await.unwrap(), UserRole::Owner);
}

#[tokio::test]
async fn webui_defaults_bootstrap_owner_and_member() {
    let (svc, _) = service(true).await;
    assert_eq!(svc.role_of(BOOTSTRAP_OWNER_USER_ID).await.unwrap(), UserRole::Owner);
    assert_eq!(svc.role_of("u2").await.unwrap(), UserRole::Member);
}

#[tokio::test]
async fn last_owner_cannot_be_demoted() {
    let (svc, audit) = service(true).await;
    let err = svc
        .set_role(BOOTSTRAP_OWNER_USER_ID, BOOTSTRAP_OWNER_USER_ID, UserRole::Member, None)
        .await
        .unwrap_err();
    assert!(matches!(err, GovernanceError::Conflict(_)));

    svc.set_role(BOOTSTRAP_OWNER_USER_ID, "u2", UserRole::Owner, None)
        .await
        .unwrap();
    assert_eq!(svc.role_of("u2").await.unwrap(), UserRole::Owner);
    svc.set_role("u2", BOOTSTRAP_OWNER_USER_ID, UserRole::Admin, None)
        .await
        .unwrap();
    assert_eq!(svc.role_of(BOOTSTRAP_OWNER_USER_ID).await.unwrap(), UserRole::Admin);
    let err = svc.set_role("u2", "u2", UserRole::Member, None).await.unwrap_err();
    assert!(matches!(err, GovernanceError::Conflict(_)));
    assert_eq!(actions(&audit).await, vec!["role.update", "role.update"]);
}

#[tokio::test]
async fn policy_update_is_validated_cached_and_audited() {
    let (svc, audit) = service(false).await;
    assert!(matches!(
        svc.set_policy("u1", PolicyKey::NoYolo, &json!("yes"), None).await,
        Err(GovernanceError::Invalid(_))
    ));
    let set = svc
        .set_policy("u1", PolicyKey::NoYolo, &json!(true), None)
        .await
        .unwrap();
    assert!(set.no_yolo);
    let set = svc
        .set_policy(
            "u1",
            PolicyKey::BannedBackends,
            &json!([" Codex ", "codex", "qwen"]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(set.banned_backends, vec!["codex", "qwen"]);
    assert!(svc.policies().await.unwrap().no_yolo);
    let rows = audit
        .list(&AuditEventFilter {
            order: AuditOrder::OldestFirst,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].action, "policy.update");
    assert_eq!(rows[0].target_id.as_deref(), Some("no_yolo"));
    let diff: serde_json::Value = serde_json::from_str(rows[0].diff.as_deref().unwrap()).unwrap();
    assert_eq!(diff, json!({ "before": false, "after": true }));
}

#[tokio::test]
async fn check_launch_denies_and_audits() {
    let (svc, audit) = service(false).await;
    let mut facts = LaunchFacts::new(LaunchSurface::ConversationCreate);
    facts.target = Some(("conversation", "c1"));
    facts.agent_type = Some(AgentType::Acp);
    facts.backend = Some("claude");
    facts.mode = Some("bypassPermissions");
    // Everything off by default.
    svc.check_launch("u1", &facts).await.unwrap();

    svc.set_policy("u1", PolicyKey::NoYolo, &json!(true), None)
        .await
        .unwrap();
    let denial = svc.check_launch("u1", &facts).await.unwrap_err();
    assert_eq!(denial.key, "no_yolo");
    facts.mode = Some("default");
    svc.check_launch("u1", &facts).await.unwrap();

    let rows = audit
        .list(&AuditEventFilter {
            action: Some("policy.deny".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].target_id.as_deref(), Some("c1"));
}

#[tokio::test]
async fn run_permissions_apply_only_with_roles() {
    let (desktop, _) = service(false).await;
    desktop
        .set_agent_run_permission("u1", "agent_type", "Codex", Some(&[UserRole::Admin]), None)
        .await
        .unwrap();
    let mut facts = LaunchFacts::new(LaunchSurface::ConversationCreate);
    facts.agent_type = Some(AgentType::Acp);
    facts.backend = Some("codex");
    desktop.check_launch("u1", &facts).await.unwrap();

    let (webui, _) = service(true).await;
    let row = webui
        .set_agent_run_permission(
            BOOTSTRAP_OWNER_USER_ID,
            "agent_type",
            "Codex",
            Some(&[UserRole::Owner, UserRole::Admin]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(row.subject_id, "codex");
    assert_eq!(
        webui.check_launch("u2", &facts).await.unwrap_err().key,
        "agent_run_permission"
    );
    webui.check_launch(BOOTSTRAP_OWNER_USER_ID, &facts).await.unwrap();
    webui
        .set_agent_run_permission(BOOTSTRAP_OWNER_USER_ID, "agent_type", "codex", None, None)
        .await
        .unwrap();
    webui.check_launch("u2", &facts).await.unwrap();
    assert!(
        webui
            .set_agent_run_permission(BOOTSTRAP_OWNER_USER_ID, "robot", "x", None, None)
            .await
            .is_err()
    );
}

#[test]
fn evaluate_covers_each_policy() {
    let mut policy = PolicySet {
        banned_backends: vec!["codex".into()],
        ..Default::default()
    };
    let mut facts = LaunchFacts::new(LaunchSurface::ConversationCreate);
    facts.agent_type = Some(AgentType::Acp);
    facts.backend = Some("Codex");
    assert_eq!(evaluate_policies(&policy, &facts).unwrap().key, "banned_backends");
    facts.backend = Some("claude");
    assert!(evaluate_policies(&policy, &facts).is_none());

    policy.force_sandbox = true;
    facts.mode = Some("yoloNoSandbox");
    assert_eq!(evaluate_policies(&policy, &facts).unwrap().key, "force_sandbox");
    facts.mode = Some("yolo");
    assert!(evaluate_policies(&policy, &facts).is_none());
    facts.mode = None;
    facts.in_place_workspace = true;
    assert_eq!(evaluate_policies(&policy, &facts).unwrap().key, "force_sandbox");
    facts.in_place_workspace = false;

    policy.mcp_strict_only = true;
    // Not a session launch: skipped.
    assert!(evaluate_policies(&policy, &facts).is_none());
    facts.mcp_strict = Some(false);
    assert_eq!(evaluate_policies(&policy, &facts).unwrap().key, "mcp_strict_only");
    facts.mcp_strict = Some(true);
    assert!(evaluate_policies(&policy, &facts).is_none());
    facts.backend = Some("qwen");
    assert_eq!(evaluate_policies(&policy, &facts).unwrap().key, "mcp_strict_only");
    facts.agent_type = Some(AgentType::Aionrs);
    facts.backend = None;
    assert!(evaluate_policies(&policy, &facts).is_none());
}

#[test]
fn full_auto_and_sandbox_mode_detection() {
    for mode in [
        "yolo",
        "yoloNoSandbox",
        "bypassPermissions",
        "full-access",
        "agent-full-access",
    ] {
        assert!(is_full_auto_mode(mode, None, None), "{mode}");
    }
    assert!(!is_full_auto_mode("default", Some(AgentType::Acp), Some("hermes")));
    assert!(!is_full_auto_mode("plan", Some(AgentType::Acp), Some("claude")));
    assert!(!is_full_auto_mode("", None, None));
    assert!(is_no_sandbox_mode("full-access"));
    assert!(!is_no_sandbox_mode("yolo"));
}

#[test]
fn policy_values_are_normalised() {
    assert_eq!(
        normalize_policy_value(PolicyKey::ForceSandbox, &json!(false)),
        Ok(json!(false))
    );
    assert!(normalize_policy_value(PolicyKey::BannedBackends, &json!("codex")).is_err());
    assert!(normalize_policy_value(PolicyKey::BannedBackends, &json!([""])).is_err());
    let many: Vec<String> = (0..65).map(|i| format!("b{i}")).collect();
    assert!(normalize_policy_value(PolicyKey::BannedBackends, &json!(many)).is_err());
    assert_eq!(PolicyKey::parse("no_yolo"), Some(PolicyKey::NoYolo));
    assert_eq!(PolicyKey::parse("nope"), None);
}

// M8 E12: Autopilot full-auto (`autopilot_yolo`) carries no forced mode, so
// `no_yolo` checks the explicit confirmation flag.
#[tokio::test]
async fn no_yolo_refuses_confirmed_full_auto_without_a_mode() {
    let (svc, audit) = service(false).await;
    let mut facts = LaunchFacts::new(LaunchSurface::Cron);
    facts.target = Some(("cron_job", "j1"));
    facts.full_auto = true;
    // Off by default.
    svc.check_launch("u1", &facts).await.unwrap();

    svc.set_policy("u1", PolicyKey::NoYolo, &json!(true), None)
        .await
        .unwrap();
    let denial = svc.check_launch("u1", &facts).await.unwrap_err();
    assert_eq!(denial.key, "no_yolo");
    assert!(denial.message.contains("Autopilot"), "{}", denial.message);
    facts.full_auto = false;
    svc.check_launch("u1", &facts).await.unwrap();
    assert_eq!(actions(&audit).await.iter().filter(|a| *a == "policy.deny").count(), 1);
}

#[tokio::test]
async fn catalog_filter_hides_restricted_assistants_for_members() {
    let (webui, _) = service(true).await;
    webui
        .set_agent_run_permission(
            BOOTSTRAP_OWNER_USER_ID,
            "assistant",
            "asst-secret",
            Some(&[UserRole::Owner, UserRole::Admin]),
            None,
        )
        .await
        .unwrap();
    assert!(webui.catalog_filter_active("u2").await.unwrap());
    assert!(!webui.catalog_filter_active(BOOTSTRAP_OWNER_USER_ID).await.unwrap());
    assert!(!webui.may_see_assistant("u2", "asst-secret", None, None).await.unwrap());
    assert!(
        webui
            .may_see_assistant(BOOTSTRAP_OWNER_USER_ID, "asst-secret", None, None)
            .await
            .unwrap()
    );
    assert!(
        webui.may_see_assistant("u2", "asst-open", None, None).await.unwrap(),
        "unlisted assistants stay visible"
    );
}

#[tokio::test]
async fn catalog_filter_respects_agent_type_rows() {
    let (webui, _) = service(true).await;
    webui
        .set_agent_run_permission(
            BOOTSTRAP_OWNER_USER_ID,
            "agent_type",
            "codex",
            Some(&[UserRole::Owner]),
            None,
        )
        .await
        .unwrap();
    assert!(
        !webui
            .may_see_assistant("u2", "asst-1", Some(AgentType::Acp), Some("codex"))
            .await
            .unwrap()
    );
    assert!(
        webui
            .may_see_assistant("u2", "asst-1", Some(AgentType::Acp), Some("claude"))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn catalog_filter_inactive_on_desktop() {
    let (desktop, _) = service(false).await;
    desktop
        .set_agent_run_permission("u1", "assistant", "x", Some(&[UserRole::Admin]), None)
        .await
        .unwrap();
    assert!(!desktop.catalog_filter_active("u1").await.unwrap());
    assert!(desktop.may_see_assistant("u1", "x", None, None).await.unwrap());
}
