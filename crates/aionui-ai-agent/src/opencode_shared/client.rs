//! Thin typed wrapper over the `opencode serve` V2 HTTP API (verified against
//! opencode 1.18.30 `/doc`): session create/get, durable-input prompt,
//! interrupt, model/agent switch, pending permission/question lists + replies,
//! history, and the replayable session SSE stream.
//!
//! Everything speaks the `/api/*` namespace — the legacy `/session*` routes
//! are only kept alive by opencode for SDK compat and their `GET /event` bus
//! is a no-op in this build (confirmed live: zero events even attached during
//! a turn), so the shared backend must not depend on them.

use std::sync::OnceLock;

use serde_json::{Value, json};

use super::pool::{ServerInfo, auth_header, shared_client};

/// Dedicated client for the long-lived SSE GET: unlike the pooled
/// `shared_client()` (30s overall timeout), an idle event stream must never be
/// timed out. reqwest applies no timeout unless one is set.
static SSE_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

#[derive(Clone)]
pub struct OpencodeClient {
    http: reqwest::Client,
    base_url: String,
    auth: Option<String>,
}

/// One parsed SSE record. opencode frames every event as a single
/// `data: {json}` line (no `event:` field — confirmed live capture); the
/// durable envelope carries `{id, type, durable:{seq}, data}`.
#[derive(Debug, Clone)]
pub struct StreamEvent {
    pub seq: Option<u64>,
    pub event_type: String,
    pub data: Value,
}

pub fn sse_data_to_event(v: Value) -> Option<StreamEvent> {
    let event_type = v.get("type")?.as_str()?.to_string();
    let seq = v.get("durable").and_then(|d| d.get("seq")).and_then(|s| s.as_u64());
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    Some(StreamEvent { seq, event_type, data })
}

impl OpencodeClient {
    pub fn new(info: &ServerInfo) -> Self {
        Self {
            http: shared_client(),
            base_url: info.base_url.clone(),
            auth: auth_header(info),
        }
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut r = self.http.request(method, format!("{}{}", self.base_url, path));
        if let Some(auth) = &self.auth {
            r = r.header(reqwest::header::AUTHORIZATION, auth);
        }
        r
    }

    async fn send_json(&self, rb: reqwest::RequestBuilder, ctx: &str) -> Result<Value, super::HttpError> {
        let resp = rb
            .send()
            .await
            .map_err(|e| super::HttpError::Transport(format!("{ctx}: {e}")))?;
        let status = resp.status();
        let body: Value = if status == reqwest::StatusCode::NO_CONTENT {
            Value::Null
        } else {
            resp.json()
                .await
                .map_err(|e| super::HttpError::Transport(format!("{ctx}: bad json: {e}")))?
        };
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(super::HttpError::NotFound(ctx.to_string()));
        }
        if !status.is_success() {
            return Err(super::HttpError::Api {
                status: status.as_u16(),
                ctx: ctx.to_string(),
                body,
            });
        }
        Ok(body)
    }

    /// `POST /api/session` → (session_id, project_id). `directory` is the
    /// session's working directory (the API's per-session workspace pin).
    pub async fn create_session(&self, directory: &str) -> Result<(String, String), super::HttpError> {
        let body = json!({ "location": { "directory": directory } });
        let v = self
            .send_json(
                self.req(reqwest::Method::POST, "/api/session").json(&body),
                "POST /api/session",
            )
            .await?;
        let id = v
            .pointer("/data/id")
            .and_then(|s| s.as_str())
            .ok_or_else(|| super::HttpError::Transport("session create: no data.id".into()))?
            .to_string();
        let project = v
            .pointer("/data/projectID")
            .and_then(|s| s.as_str())
            .unwrap_or("global")
            .to_string();
        Ok((id, project))
    }

    /// `GET /api/session/{id}` — existence probe for resume.
    pub async fn get_session(&self, id: &str) -> Result<Value, super::HttpError> {
        self.send_json(
            self.req(reqwest::Method::GET, &format!("/api/session/{id}")),
            "GET /api/session/{id}",
        )
        .await
    }

    /// `POST /api/session/{id}/prompt` — durably admits (and, with
    /// `delivery:"steer"`, schedules) the agent loop. Returns the input's
    /// (session, message id, admittedSeq).
    pub async fn prompt(&self, session_id: &str, text: &str) -> Result<PromptAdmit, super::HttpError> {
        let body = json!({
            "prompt": { "text": text },
            "delivery": "steer",
        });
        let v = self
            .send_json(
                self.req(reqwest::Method::POST, &format!("/api/session/{session_id}/prompt"))
                    .json(&body),
                "POST prompt",
            )
            .await?;
        Ok(PromptAdmit {
            id: v
                .pointer("/data/id")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string(),
            admitted_seq: v.pointer("/data/admittedSeq").and_then(|s| s.as_u64()),
        })
    }

    /// `POST /api/session/{id}/interrupt` — abort the running turn (204).
    pub async fn interrupt(&self, session_id: &str) -> Result<(), super::HttpError> {
        self.send_json(
            self.req(reqwest::Method::POST, &format!("/api/session/{session_id}/interrupt")),
            "POST interrupt",
        )
        .await
        .map(|_| ())
    }

    /// `POST /api/session/{id}/model` — switch the session model
    /// (`{"model":{"id","providerID"}}`, 204).
    pub async fn set_model(&self, session_id: &str, model: &str) -> Result<(), super::HttpError> {
        let (provider, id) = split_model(model);
        let body = json!({ "model": { "id": id, "providerID": provider } });
        self.send_json(
            self.req(reqwest::Method::POST, &format!("/api/session/{session_id}/model"))
                .json(&body),
            "POST model",
        )
        .await
        .map(|_| ())
    }

    /// `GET /api/session/active` → true when `session_id` currently drains a
    /// foreground execution (the turn-in-flight ground truth; `/wait` is a
    /// 503 stub in 1.18.30, so the backend polls this to close a turn).
    pub async fn session_active(&self, session_id: &str) -> Result<bool, super::HttpError> {
        let v = self
            .send_json(
                self.req(reqwest::Method::GET, "/api/session/active"),
                "GET /api/session/active",
            )
            .await?;
        Ok(v.get("data").and_then(|d| d.get(session_id)).is_some())
    }

    /// `GET /api/session/{id}/permission?limit=…` pending permission list.
    pub async fn pending_permissions(&self, session_id: &str) -> Vec<Value> {
        match self
            .send_json(
                self.req(reqwest::Method::GET, &format!("/api/session/{session_id}/permission")),
                "GET permissions",
            )
            .await
        {
            Ok(v) => data_array(v),
            Err(_) => Vec::new(),
        }
    }

    /// `POST /api/session/{id}/permission/{request_id}/reply`.
    pub async fn reply_permission(
        &self,
        session_id: &str,
        request_id: &str,
        reply: &str,
    ) -> Result<(), super::HttpError> {
        let body = json!({ "reply": reply });
        self.send_json(
            self.req(
                reqwest::Method::POST,
                &format!("/api/session/{session_id}/permission/{request_id}/reply"),
            )
            .json(&body),
            "POST permission reply",
        )
        .await
        .map(|_| ())
    }

    /// `GET /api/session/{id}/question` pending question list.
    pub async fn pending_questions(&self, session_id: &str) -> Vec<Value> {
        match self
            .send_json(
                self.req(reqwest::Method::GET, &format!("/api/session/{session_id}/question")),
                "GET questions",
            )
            .await
        {
            Ok(v) => data_array(v),
            Err(_) => Vec::new(),
        }
    }

    /// `POST /api/session/{id}/question/{request_id}/reply` —
    /// `answers` must be an ARRAY of string arrays (one per question, see
    /// QuestionV2.Answer; a flat string array 400s).
    pub async fn reply_question(
        &self,
        session_id: &str,
        request_id: &str,
        answers: Vec<Vec<String>>,
    ) -> Result<(), super::HttpError> {
        let body = json!({ "answers": answers });
        self.send_json(
            self.req(
                reqwest::Method::POST,
                &format!("/api/session/{session_id}/question/{request_id}/reply"),
            )
            .json(&body),
            "POST question reply",
        )
        .await
        .map(|_| ())
    }

    /// `POST /api/session/{id}/question/{request_id}/reject`.
    pub async fn reject_question(&self, session_id: &str, request_id: &str) -> Result<(), super::HttpError> {
        self.send_json(
            self.req(
                reqwest::Method::POST,
                &format!("/api/session/{session_id}/question/{request_id}/reject"),
            ),
            "POST question reject",
        )
        .await
        .map(|_| ())
    }

    /// `GET /api/session/{id}/history?after=<exclusive seq>` — pages are
    /// hard-capped at 100 (limit>100 → 400, confirmed live), so page until
    /// `hasMore` is false; returns (events newest-last, last durable seq seen).
    pub async fn history_tail(&self, session_id: &str) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        let mut after: i64 = 0; // exclusive; durable seqs start at 1
        loop {
            let path = format!("/api/session/{session_id}/history?limit=100&after={after}");
            let Ok(v) = self
                .send_json(self.req(reqwest::Method::GET, &path), "GET history")
                .await
            else {
                break;
            };
            let page = data_array(v.clone());
            let has_more = v.get("hasMore").and_then(|h| h.as_bool()).unwrap_or(false);
            let mut max = after;
            for ev in &page {
                if let Some(seq) = ev.get("durable").and_then(|d| d.get("seq")).and_then(|s| s.as_u64()) {
                    max = max.max(seq as i64);
                }
                if let Some(se) = sse_data_to_event(ev.clone()) {
                    out.push(se);
                }
            }
            if !has_more || page.is_empty() {
                break;
            }
            after = max;
        }
        out
    }

    /// `GET /api/session/{id}/event?after=<seq>` — durable + transient SSE.
    /// Resumable by durable seq (replay verified across server restarts).
    /// Returns a stream of raw JSON records; the caller parses via
    /// [`sse_data_to_event`].
    pub async fn event_stream(
        &self,
        session_id: &str,
        after: Option<u64>,
    ) -> Result<reqwest::Response, super::HttpError> {
        let path = match after {
            Some(a) => format!("/api/session/{session_id}/event?after={a}"),
            None => format!("/api/session/{session_id}/event"),
        };
        // No client timeout on the stream: the pooled 30s client would kill
        // an idle SSE, so connect with a dedicated no-timeout client.
        let stream_client = SSE_CLIENT.get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("sse reqwest client")
        });
        let mut req2 = stream_client
            .get(format!("{}{}", self.base_url, path))
            .header(reqwest::header::ACCEPT, "text/event-stream");
        if let Some(auth) = &self.auth {
            req2 = req2.header(reqwest::header::AUTHORIZATION, auth);
        }
        let resp = req2
            .send()
            .await
            .map_err(|e| super::HttpError::Transport(format!("sse connect: {e}")))?;
        if !resp.status().is_success() {
            return Err(super::HttpError::Api {
                status: resp.status().as_u16(),
                ctx: "GET event".into(),
                body: Value::Null,
            });
        }
        Ok(resp)
    }
}

pub struct PromptAdmit {
    pub id: String,
    pub admitted_seq: Option<u64>,
}

/// `provider/model` → (providerID, modelID); no slash keeps the whole string
/// as modelID (the server then resolves its configured default provider).
pub fn split_model(model: &str) -> (String, String) {
    match model.split_once('/') {
        Some((p, m)) if !p.is_empty() && !m.is_empty() => (p.to_string(), m.to_string()),
        _ => (String::new(), model.to_string()),
    }
}

fn data_array(v: Value) -> Vec<Value> {
    match v.get("data") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_model_provider_slash() {
        assert_eq!(
            split_model("opencode/big-pickle"),
            ("opencode".into(), "big-pickle".into())
        );
        assert_eq!(
            split_model("gzbit/qwen3.8-flash-next"),
            ("gzbit".into(), "qwen3.8-flash-next".into())
        );
        // No slash → whole string is the model id, provider left blank so the
        // server falls back to its configured default.
        assert_eq!(split_model("solo"), (String::new(), "solo".into()));
        // Edge: leading slash must not invent an empty provider name…
        assert_eq!(split_model("/x"), (String::new(), "/x".into()));
        // …nor trailing slash an empty model.
        assert_eq!(split_model("p/"), (String::new(), "p/".into()));
        // Multi-slash model ids keep the FIRST slash as the boundary.
        assert_eq!(
            split_model("openrouter/meta-llama/llama-3"),
            ("openrouter".into(), "meta-llama/llama-3".into())
        );
    }

    #[test]
    fn sse_event_parses_durable_envelope() {
        let v: Value = serde_json::from_str(
            r#"{"id":"evt1","type":"session.next.text.delta","durable":{"aggregateID":"ses_x","seq":12},"data":{"textID":"t1","delta":"hi"}}"#,
        )
        .unwrap();
        let ev = sse_data_to_event(v).unwrap();
        assert_eq!(ev.event_type, "session.next.text.delta");
        assert_eq!(ev.seq, Some(12));
        assert_eq!(ev.data["delta"], "hi");
    }

    #[test]
    fn sse_event_without_durable_is_transient() {
        let v: Value = serde_json::from_str(r#"{"type":"session.idle","data":{"sessionID":"ses_x"}}"#).unwrap();
        let ev = sse_data_to_event(v).unwrap();
        assert_eq!(ev.event_type, "session.idle");
        assert_eq!(ev.seq, None);
    }

    #[test]
    fn sse_event_missing_type_is_none() {
        let v: Value = serde_json::from_str(r#"{"data":{}}"#).unwrap();
        assert!(sse_data_to_event(v).is_none());
    }
}
