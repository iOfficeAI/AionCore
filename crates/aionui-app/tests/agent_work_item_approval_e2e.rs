//! M8 Q9 (D6-7): an agent may only *propose* a work item. The proposal is
//! parked behind the existing agent action approval (card + single-use
//! approval id); nothing is persisted, shown on the board or able to trigger
//! E7-4 rules before the user approves, and the approved item always lands
//! in `backlog`, unassigned.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use aionui_ai_agent::TEAM_RUNTIME_TOKEN_SESSION_GENERATION;

const CONVERSATION_ID: &str = "conv-q9-agent";

struct Harness {
    app: axum::Router,
    services: aionui_app::AppServices,
    user_id: String,
    token: String,
}

async fn harness() -> Harness {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        identity_mode: aionui_app::IdentityMode::AionPro,
        bootstrap_secret: Some("bootstrap-secret".to_string()),
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let app = aionui_app::create_router(&services).await.expect("build router");
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/auth/internal/external-users/q9-user")
                .header("content-type", "application/json")
                .header("x-aioncore-bootstrap-secret", "bootstrap-secret")
                .body(Body::from(r#"{"user_type":"aionpro","username":"Q9 User"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let user_id = body_json(resp).await["data"]["user_id"].as_str().unwrap().to_owned();
    let token = services
        .runtime_token_service
        .issue(
            user_id.as_str(),
            CONVERSATION_ID,
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            aionui_ai_agent::helper_scopes::conversation_helper_scopes(None),
        )
        .token;
    Harness {
        app,
        services,
        user_id,
        token,
    }
}

async fn body_json(resp: axum::response::Response) -> Value {
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&body).unwrap_or(Value::Null)
}

impl Harness {
    fn agent_request(&self, method: &str, path: &str, body: &Value, approval_id: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("x-aionui-user-id", &self.user_id)
            .header("x-aionui-conversation-id", CONVERSATION_ID)
            .header("x-aionui-runtime-token", &self.token);
        if let Some(id) = approval_id {
            builder = builder.header("x-aionui-approval-id", id);
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, Value) {
        let resp = self.app.clone().oneshot(request).await.unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    /// First call parks the request; returns the approval id.
    async fn request_approval(&self, body: &Value) -> String {
        let (status, json) = self
            .send(self.agent_request("POST", "/api/task-sessions", body, None))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(json["code"], "AGENT_ACTION_APPROVAL_REQUIRED", "{json}");
        assert_eq!(json["details"]["kind"], "work_item", "{json}");
        json["details"]["approval_id"].as_str().unwrap().to_owned()
    }

    async fn decide_and_replay(&self, body: &Value, approve: bool) -> (StatusCode, Value) {
        let approval_id = self.request_approval(body).await;
        // Still parked: nothing exists yet.
        assert_eq!(self.work_item_count().await, 0, "nothing persists before approval");
        self.services
            .agent_approvals
            .resolve(&approval_id, &self.user_id, approve)
            .expect("pending approval");
        self.send(self.agent_request("POST", "/api/task-sessions", body, Some(&approval_id)))
            .await
    }

    async fn work_item_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM task_sessions")
            .fetch_one(self.services.database.pool())
            .await
            .unwrap()
    }
}

fn proposal() -> Value {
    json!({
        "title": "Add retry to export",
        "objective": "Retry transient export failures",
        "acceptance_criteria": ["Retries up to 3 times"],
        "mode": "agent",
        "agent_type": "claude"
    })
}

fn drain_board_events(
    rx: &mut tokio::sync::broadcast::Receiver<aionui_api_types::WebSocketMessage<Value>>,
) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if msg.name == aionui_api_types::EVENT_TASK_BOARD_UPDATED {
            out.push(msg.data);
        }
    }
    out
}

#[tokio::test]
async fn approved_agent_proposal_lands_in_backlog_unassigned_with_origin_comment() {
    let h = harness().await;
    let mut rx = h.services.event_bus.subscribe();

    let approval_id = h.request_approval(&proposal()).await;
    assert_eq!(h.work_item_count().await, 0);
    assert!(drain_board_events(&mut rx).is_empty(), "no board event before approval");

    // Replaying before the user decides stays parked.
    let (status, json) = h
        .send(h.agent_request("POST", "/api/task-sessions", &proposal(), Some(&approval_id)))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(json["code"], "AGENT_ACTION_APPROVAL_PENDING");
    assert_eq!(h.work_item_count().await, 0);

    h.services
        .agent_approvals
        .resolve(&approval_id, &h.user_id, true)
        .expect("pending approval");
    let (status, json) = h
        .send(h.agent_request("POST", "/api/task-sessions", &proposal(), Some(&approval_id)))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let id = json["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(json["data"]["status"], "draft");

    let row: (String, Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT board_status, assignee_kind, assignee_id, conversation_id FROM task_sessions WHERE id = ?",
    )
    .bind(&id)
    .fetch_one(h.services.database.pool())
    .await
    .unwrap();
    assert_eq!(row, ("backlog".to_owned(), None, None, None));

    let comments: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT author_kind, author_id, body FROM task_comments WHERE task_session_id = ? ORDER BY created_at",
    )
    .bind(&id)
    .fetch_all(h.services.database.pool())
    .await
    .unwrap();
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert_eq!(comments[0].0, "system");
    assert_eq!(comments[0].1, "agent-approval");
    assert!(comments[0].2.contains(CONVERSATION_ID), "{}", comments[0].2);
    assert!(comments[0].2.contains("Backlog"), "{}", comments[0].2);

    let events = drain_board_events(&mut rx);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["task_session_id"], id);
    assert_eq!(events[0]["change"], "board");
    assert_eq!(events[0]["item"]["board_status"], "backlog");
    assert!(events[0]["item"]["assignee"].is_null(), "{}", events[0]);

    // The approval is single use.
    let (status, json) = h
        .send(h.agent_request("POST", "/api/task-sessions", &proposal(), Some(&approval_id)))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["code"], "AGENT_ACTION_APPROVAL_INVALID");
    assert_eq!(h.work_item_count().await, 1);

    h.services.database.close().await;
}

#[tokio::test]
async fn denied_agent_proposal_creates_nothing() {
    let h = harness().await;
    let mut rx = h.services.event_bus.subscribe();
    let (status, json) = h.decide_and_replay(&proposal(), false).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["code"], "AGENT_ACTION_APPROVAL_DENIED");
    assert_eq!(h.work_item_count().await, 0);
    assert!(drain_board_events(&mut rx).is_empty());
    h.services.database.close().await;
}

#[tokio::test]
async fn agent_cannot_create_beyond_backlog_even_when_approved() {
    let h = harness().await;
    let cases = [
        ("board_status", json!("todo")),
        ("board_status", json!("in_progress")),
        ("assignee", json!({ "kind": "agent", "id": "claude" })),
        ("assignee", json!({ "kind": "squad", "id": "sq1" })),
        ("priority", json!("high")),
        ("labels", json!(["x"])),
        ("conversation_id", json!(CONVERSATION_ID)),
        ("agent_session_id", json!("s1")),
        ("status", json!("ready")),
    ];
    for (field, value) in cases {
        let mut body = proposal();
        body[field] = value;
        let (status, json) = h.decide_and_replay(&body, true).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{field}: {json}");
        assert_eq!(json["code"], "agent_work_item_backlog_only", "{field}: {json}");
        assert_eq!(json["details"]["field"], field, "{field}: {json}");
        assert_eq!(h.work_item_count().await, 0, "{field}");
    }
    // Explicit draft is fine.
    let mut body = proposal();
    body["status"] = json!("draft");
    let (status, json) = h.decide_and_replay(&body, true).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    h.services.database.close().await;
}

#[tokio::test]
async fn agent_cannot_move_assign_review_or_comment_on_work_items() {
    let h = harness().await;
    let (status, json) = h.decide_and_replay(&proposal(), true).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let id = json["data"]["id"].as_str().unwrap().to_owned();

    let attempts = [
        (
            "PATCH",
            format!("/api/task-sessions/{id}/board"),
            json!({ "board_status": "todo" }),
        ),
        (
            "PATCH",
            format!("/api/task-sessions/{id}/board"),
            json!({ "assignee": { "kind": "agent", "id": "claude" } }),
        ),
        (
            "POST",
            format!("/api/task-sessions/{id}/review"),
            json!({ "decision": "approve" }),
        ),
        (
            "POST",
            format!("/api/task-sessions/{id}/comments"),
            json!({ "body": "hi" }),
        ),
        (
            "PATCH",
            format!("/api/task-sessions/{id}"),
            json!({ "title": "renamed" }),
        ),
    ];
    for (method, path, body) in attempts {
        let (status, json) = h.send(h.agent_request(method, &path, &body, None)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {json}");
        assert_eq!(json["code"], "AGENT_SCOPE_DENIED", "{method} {path}: {json}");
    }
    let row: (String, Option<String>, String) =
        sqlx::query_as("SELECT board_status, assignee_kind, title FROM task_sessions WHERE id = ?")
            .bind(&id)
            .fetch_one(h.services.database.pool())
            .await
            .unwrap();
    assert_eq!(row, ("backlog".to_owned(), None, "Add retry to export".to_owned()));
    h.services.database.close().await;
}

/// M8 E6-7: GitHub Issue import is a user-only, manual action.
#[tokio::test]
async fn agent_cannot_import_github_issues() {
    let h = harness().await;
    let (status, json) = h
        .send(h.agent_request(
            "POST",
            "/api/task-sessions/import/github",
            &json!({ "repo": "acme/widgets" }),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["code"], "AGENT_SCOPE_DENIED", "{json}");
    assert_eq!(h.work_item_count().await, 0);
    h.services.database.close().await;
}
