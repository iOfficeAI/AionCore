use std::sync::{Arc, Mutex};

use aionui_auth::CurrentUser;
use aionui_db::{SqliteApprovalRepository, init_database_memory};
use aionui_development::{
    ApprovalError, ApprovalOption, ApprovalRequestInput, ApprovalResolver, ApprovalRouterState, ApprovalService,
    ApprovalSource, ResolveApprovalContext, approval_routes,
};
use async_trait::async_trait;
use axum::Extension;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

#[derive(Default)]
struct RecordingResolver {
    calls: Mutex<Vec<(String, String, Value, bool)>>,
    fail: bool,
}

#[async_trait]
impl ApprovalResolver for RecordingResolver {
    async fn resolve(
        &self,
        conversation_id: &str,
        call_id: &str,
        value: Value,
        always_allow: bool,
    ) -> Result<(), String> {
        if self.fail {
            return Err("agent stopped".into());
        }
        self.calls
            .lock()
            .unwrap()
            .push((conversation_id.into(), call_id.into(), value, always_allow));
        Ok(())
    }
}

async fn setup(resolver: Arc<RecordingResolver>) -> (ApprovalService, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO conversations \
         (id, user_id, name, type, extra, pinned, created_at, updated_at) \
         VALUES ('conversation-1', 'system_default_user', 'Approval test', 'acp', '{}', 0, 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let service = ApprovalService::new(Arc::new(SqliteApprovalRepository::new(db.pool().clone())), resolver);
    (service, db)
}

fn request() -> ApprovalRequestInput {
    ApprovalRequestInput {
        requester_user_id: "system_default_user".into(),
        project_id: None,
        run_id: None,
        task_id: None,
        conversation_id: "conversation-1".into(),
        agent_id: Some("claude-code".into()),
        call_id: "call-1".into(),
        action_type: "execute".into(),
        command: Some("TOKEN=super-secret cargo test".into()),
        working_directory: Some("/tmp/project".into()),
        risk_level: "high".into(),
        options: vec![
            ApprovalOption {
                label: "Allow once".into(),
                value: json!("allow_once"),
                params: None,
            },
            ApprovalOption {
                label: "Always allow".into(),
                value: json!("allow_always"),
                params: Some(json!({"always_allow": true})),
            },
            ApprovalOption {
                label: "Reject".into(),
                value: json!("reject"),
                params: Some(json!({"decision": "reject"})),
            },
        ],
        source: Some(ApprovalSource {
            channel: "telegram".into(),
            user_id: "telegram-user-1".into(),
            chat_id: "-1003977604085".into(),
            thread_id: Some(5),
        }),
    }
}

#[tokio::test]
async fn create_is_idempotent_short_lived_and_redacts_secrets() {
    let (service, _db) = setup(Arc::new(RecordingResolver::default())).await;
    let first = service.create(request()).await.unwrap();
    let second = service.create(request()).await.unwrap();

    assert_eq!(first.id, second.id);
    assert!(first.id.len() <= 20);
    assert!(!first.command.as_deref().unwrap().contains("super-secret"));
    assert_eq!(first.status, "pending");
    assert!(first.expires_at > first.created_at);
}

#[tokio::test]
async fn telegram_resolution_requires_matching_user_chat_and_thread() {
    let resolver = Arc::new(RecordingResolver::default());
    let (service, _db) = setup(resolver.clone()).await;
    let approval = service.create(request()).await.unwrap();

    let wrong_thread = service
        .resolve(
            &approval.id,
            0,
            ResolveApprovalContext::Channel {
                user_id: "system_default_user".into(),
                channel: "telegram".into(),
                source_user_id: "telegram-user-1".into(),
                chat_id: "-1003977604085".into(),
                thread_id: Some(7),
                is_admin: false,
            },
        )
        .await;
    assert!(matches!(wrong_thread, Err(ApprovalError::Forbidden(_))));

    let row = service
        .resolve(
            &approval.id,
            1,
            ResolveApprovalContext::Channel {
                user_id: "system_default_user".into(),
                channel: "telegram".into(),
                source_user_id: "telegram-user-1".into(),
                chat_id: "-1003977604085".into(),
                thread_id: Some(5),
                is_admin: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(row.status, "approved");
    assert_eq!(resolver.calls.lock().unwrap().len(), 1);
    assert!(resolver.calls.lock().unwrap()[0].3);
}

#[tokio::test]
async fn telegram_topic_admin_can_resolve_for_requester_but_other_members_cannot() {
    let resolver = Arc::new(RecordingResolver::default());
    let (service, _db) = setup(resolver.clone()).await;
    let approval = service.create(request()).await.unwrap();

    let member = service
        .resolve(
            &approval.id,
            0,
            ResolveApprovalContext::Channel {
                user_id: "system_default_user".into(),
                channel: "telegram".into(),
                source_user_id: "different-member".into(),
                chat_id: "-1003977604085".into(),
                thread_id: Some(5),
                is_admin: false,
            },
        )
        .await;
    assert!(matches!(member, Err(ApprovalError::Forbidden(_))));

    let resolved = service
        .resolve(
            &approval.id,
            0,
            ResolveApprovalContext::Channel {
                user_id: "system_default_user".into(),
                channel: "telegram".into(),
                source_user_id: "topic-admin".into(),
                chat_id: "-1003977604085".into(),
                thread_id: Some(5),
                is_admin: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(resolved.status, "approved");
    assert_eq!(resolver.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn web_owner_can_reject_but_cannot_consume_twice() {
    let resolver = Arc::new(RecordingResolver::default());
    let (service, _db) = setup(resolver.clone()).await;
    let approval = service.create(request()).await.unwrap();

    let row = service
        .resolve(
            &approval.id,
            2,
            ResolveApprovalContext::Web {
                user_id: "system_default_user".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(row.status, "rejected");

    let duplicate = service
        .resolve(
            &approval.id,
            0,
            ResolveApprovalContext::Web {
                user_id: "system_default_user".into(),
            },
        )
        .await;
    assert!(matches!(duplicate, Err(ApprovalError::Conflict(_))));
    assert_eq!(resolver.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn resolver_failure_cancels_consumed_approval() {
    let resolver = Arc::new(RecordingResolver {
        fail: true,
        ..Default::default()
    });
    let (service, _db) = setup(resolver).await;
    let approval = service.create(request()).await.unwrap();

    assert!(matches!(
        service
            .resolve(
                &approval.id,
                0,
                ResolveApprovalContext::Web {
                    user_id: "system_default_user".into(),
                },
            )
            .await,
        Err(ApprovalError::Resolver(_))
    ));
    assert_eq!(
        service.get("system_default_user", &approval.id).await.unwrap().status,
        "cancelled"
    );
}

#[tokio::test]
async fn authenticated_routes_list_and_resolve_owned_approvals() {
    let resolver = Arc::new(RecordingResolver::default());
    let (service, _db) = setup(resolver.clone()).await;
    let approval = service.create(request()).await.unwrap();
    let app = approval_routes(ApprovalRouterState {
        service: Arc::new(service),
    })
    .layer(Extension(CurrentUser {
        id: "system_default_user".into(),
        username: "system_default_user".into(),
    }));

    let response = app
        .clone()
        .oneshot(Request::builder().uri("/api/approvals").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 1);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/approvals/{}/resolve", approval.id))
                .header("content-type", "application/json")
                .body(Body::from(json!({"option_index": 0}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(resolver.calls.lock().unwrap().len(), 1);
}
