use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aionui_api_types::{
    DynamicToolCallOutputContentItem, DynamicToolCallParams, DynamicToolCallPayload, DynamicToolCallResponse,
    DynamicToolResultPayload, DynamicToolSpec, DynamicToolsRegisterRequest, DynamicToolsRegisteredPayload,
    WebSocketMessage,
};
use aionui_realtime::{ConnectionId, MessageRouter, WebSocketManager};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;
use tracing::{info, warn};

pub const DYNAMIC_TOOLS_REGISTER_EVENT: &str = "agent.dynamicToolsRegister";
pub const DYNAMIC_TOOLS_REGISTERED_EVENT: &str = "agent.dynamicToolsRegistered";
pub const DYNAMIC_TOOL_CALL_EVENT: &str = "agent.dynamicToolCall";
pub const DYNAMIC_TOOL_RESULT_EVENT: &str = "agent.dynamicToolResult";
pub const CODEX_DYNAMIC_TOOLS_META_KEY: &str = "codex/dynamic_tools";
pub const CODEX_DYNAMIC_TOOL_CALL_METHOD: &str = "codex/dynamic_tool_call";

const DEFAULT_DYNAMIC_TOOL_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_DYNAMIC_TOOL_SPECS: usize = 64;

#[derive(Clone)]
pub struct DynamicToolRegistry {
    manager: Arc<WebSocketManager>,
    state: Arc<Mutex<RegistryState>>,
    next_generation: Arc<AtomicU64>,
    call_timeout: Duration,
}

#[derive(Default)]
struct RegistryState {
    registrations: HashMap<String, HashMap<ConnectionId, Registration>>,
    conflicted_conversations: HashSet<String>,
    pending: HashMap<PendingKey, PendingCall>,
}

#[derive(Clone)]
struct Registration {
    id: String,
    generation: u64,
    tools: Vec<DynamicToolSpec>,
    thread_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PendingKey {
    registration_id: String,
    thread_id: String,
    call_id: String,
}

struct PendingCall {
    conversation_id: String,
    connection_id: ConnectionId,
    turn_id: String,
    namespace: Option<String>,
    tool: String,
    response_tx: oneshot::Sender<DynamicToolCallResponse>,
}

#[derive(Clone)]
pub struct DynamicToolSession {
    registry: DynamicToolRegistry,
    conversation_id: String,
    connection_id: ConnectionId,
    registration_id: String,
    generation: u64,
    tools: Vec<DynamicToolSpec>,
}

impl fmt::Debug for DynamicToolSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DynamicToolSession")
            .field("conversation_id", &self.conversation_id)
            .field("connection_id", &self.connection_id)
            .field("registration_id", &self.registration_id)
            .field("generation", &self.generation)
            .field("tool_count", &self.tools.len())
            .finish()
    }
}

impl DynamicToolRegistry {
    pub fn new(manager: Arc<WebSocketManager>) -> Self {
        Self::with_timeout(manager, DEFAULT_DYNAMIC_TOOL_TIMEOUT)
    }

    pub fn with_timeout(manager: Arc<WebSocketManager>, call_timeout: Duration) -> Self {
        Self {
            manager,
            state: Arc::new(Mutex::new(RegistryState::default())),
            next_generation: Arc::new(AtomicU64::new(1)),
            call_timeout,
        }
    }

    pub fn session_for(&self, conversation_id: &str) -> Option<DynamicToolSession> {
        let state = self.state.lock().unwrap();
        if state.conflicted_conversations.contains(conversation_id) {
            return None;
        }
        let registrations = state.registrations.get(conversation_id)?;
        if registrations.len() != 1 {
            return None;
        }
        let (&connection_id, registration) = registrations.iter().next()?;
        Some(DynamicToolSession {
            registry: self.clone(),
            conversation_id: conversation_id.to_owned(),
            connection_id,
            registration_id: registration.id.clone(),
            generation: registration.generation,
            tools: registration.tools.clone(),
        })
    }

    fn register(
        &self,
        connection_id: ConnectionId,
        request: DynamicToolsRegisterRequest,
    ) -> DynamicToolsRegisteredPayload {
        let request_id = request.request_id.trim().to_owned();
        let conversation_id = request.conversation_id.trim().to_owned();
        if request_id.is_empty() || conversation_id.is_empty() {
            return registration_failure(request_id, conversation_id, "dynamic_tool_registration_invalid");
        }
        if let Err(code) = validate_tool_specs(&request.tools) {
            return registration_failure(request_id, conversation_id, code);
        }

        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let registration_id = format!("dynamic-tools-{generation}");
        let mut state = self.state.lock().unwrap();

        let was_sole_reregistration = state
            .registrations
            .get(&conversation_id)
            .is_some_and(|owners| owners.len() == 1 && owners.contains_key(&connection_id));
        if was_sole_reregistration {
            state.conflicted_conversations.remove(&conversation_id);
        }

        let previous_registration_id = state
            .registrations
            .get_mut(&conversation_id)
            .and_then(|owners| owners.remove(&connection_id))
            .map(|registration| registration.id);
        if let Some(previous_registration_id) = previous_registration_id {
            drop_pending_for_registration(&mut state, &previous_registration_id);
        }

        let owners = state.registrations.entry(conversation_id.clone()).or_default();
        owners.insert(
            connection_id,
            Registration {
                id: registration_id.clone(),
                generation,
                tools: request.tools,
                thread_id: None,
            },
        );
        let ambiguous = owners.len() != 1;
        if ambiguous {
            state.conflicted_conversations.insert(conversation_id.clone());
        }
        drop(state);

        info!(
            %connection_id,
            %conversation_id,
            %registration_id,
            generation,
            accepted = !ambiguous,
            "dynamic tool registration observed"
        );
        DynamicToolsRegisteredPayload {
            request_id,
            conversation_id,
            accepted: !ambiguous,
            registration_id: Some(registration_id),
            error_code: ambiguous.then(|| "dynamic_tool_owner_ambiguous".to_owned()),
        }
    }

    fn handle_result(&self, connection_id: ConnectionId, result: DynamicToolResultPayload) -> bool {
        let key = PendingKey {
            registration_id: result.registration_id.clone(),
            thread_id: result.thread_id.clone(),
            call_id: result.call_id.clone(),
        };
        let mut state = self.state.lock().unwrap();
        let Some(pending) = state.pending.get(&key) else {
            warn!(
                %connection_id,
                thread_id = %result.thread_id,
                turn_id = %result.turn_id,
                call_id = %result.call_id,
                tool = %result.tool,
                "dynamic tool result rejected for unknown call"
            );
            return false;
        };
        let identity_matches = pending.connection_id == connection_id
            && pending.conversation_id == result.conversation_id
            && pending.turn_id == result.turn_id
            && pending.namespace == result.namespace
            && pending.tool == result.tool;
        if !identity_matches {
            warn!(
                %connection_id,
                thread_id = %result.thread_id,
                turn_id = %result.turn_id,
                call_id = %result.call_id,
                tool = %result.tool,
                "dynamic tool result rejected for identity mismatch"
            );
            return false;
        }
        let pending = state.pending.remove(&key).expect("pending call checked above");
        drop(state);
        let _ = pending.response_tx.send(result.into_response());
        true
    }

    fn disconnect(&self, connection_id: ConnectionId) {
        let mut state = self.state.lock().unwrap();
        let mut emptied = Vec::new();
        let mut removed_registration_ids = Vec::new();
        for (conversation_id, owners) in &mut state.registrations {
            if let Some(registration) = owners.remove(&connection_id) {
                removed_registration_ids.push(registration.id);
            }
            if owners.is_empty() {
                emptied.push(conversation_id.clone());
            }
        }
        for conversation_id in emptied {
            state.registrations.remove(&conversation_id);
            state.conflicted_conversations.remove(&conversation_id);
        }
        for registration_id in &removed_registration_ids {
            drop_pending_for_registration(&mut state, registration_id);
        }
        drop(state);
        if !removed_registration_ids.is_empty() {
            info!(
                %connection_id,
                registrations_removed = removed_registration_ids.len(),
                "dynamic tool registrations cleared on websocket disconnect"
            );
        }
    }

    fn send_registration_ack(&self, connection_id: ConnectionId, payload: DynamicToolsRegisteredPayload) {
        self.manager.send_to(
            connection_id,
            WebSocketMessage::new(
                DYNAMIC_TOOLS_REGISTERED_EVENT,
                serde_json::to_value(payload).unwrap_or_else(|_| {
                    json!({
                        "accepted": false,
                        "errorCode": "dynamic_tool_registration_internal"
                    })
                }),
            ),
        );
    }
}

impl MessageRouter for DynamicToolRegistry {
    fn route(&self, connection_id: ConnectionId, name: &str, data: Value) -> bool {
        match name {
            DYNAMIC_TOOLS_REGISTER_EVENT => {
                let fallback_request_id = data
                    .get("requestId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let fallback_conversation_id = data
                    .get("conversationId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let payload = match serde_json::from_value::<DynamicToolsRegisterRequest>(data) {
                    Ok(request) => self.register(connection_id, request),
                    Err(_) => registration_failure(
                        fallback_request_id,
                        fallback_conversation_id,
                        "dynamic_tool_registration_invalid",
                    ),
                };
                self.send_registration_ack(connection_id, payload);
                true
            }
            DYNAMIC_TOOL_RESULT_EVENT => {
                match serde_json::from_value::<DynamicToolResultPayload>(data) {
                    Ok(result) => {
                        self.handle_result(connection_id, result);
                    }
                    Err(error) => {
                        warn!(%connection_id, error = %error, "malformed dynamic tool result rejected");
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn disconnected(&self, connection_id: ConnectionId) {
        self.disconnect(connection_id);
    }
}

impl DynamicToolSession {
    pub fn metadata(&self) -> Map<String, Value> {
        let mut metadata = Map::new();
        metadata.insert(
            CODEX_DYNAMIC_TOOLS_META_KEY.to_owned(),
            json!({
                "version": 1,
                "tools": self.tools,
            }),
        );
        metadata
    }

    pub fn bind_thread(&self, thread_id: &str) -> bool {
        let thread_id = thread_id.trim();
        if thread_id.is_empty() {
            return false;
        }
        let mut state = self.registry.state.lock().unwrap();
        if state.conflicted_conversations.contains(&self.conversation_id) {
            return false;
        }
        let Some(registration) = state
            .registrations
            .get_mut(&self.conversation_id)
            .and_then(|owners| owners.get_mut(&self.connection_id))
        else {
            return false;
        };
        if registration.id != self.registration_id || registration.generation != self.generation {
            return false;
        }
        registration.thread_id = Some(thread_id.to_owned());
        info!(
            conversation_id = %self.conversation_id,
            registration_id = %self.registration_id,
            thread_id,
            "dynamic tool registration bound to ACP session"
        );
        true
    }

    pub async fn dispatch(&self, params: DynamicToolCallParams) -> DynamicToolCallResponse {
        if !valid_call_identity(&params) || !tool_is_registered(&self.tools, params.namespace.as_deref(), &params.tool)
        {
            return dynamic_tool_unavailable();
        }
        let key = PendingKey {
            registration_id: self.registration_id.clone(),
            thread_id: params.thread_id.clone(),
            call_id: params.call_id.clone(),
        };
        let (response_tx, response_rx) = oneshot::channel();
        {
            let mut state = self.registry.state.lock().unwrap();
            if state.conflicted_conversations.contains(&self.conversation_id) {
                return dynamic_tool_unavailable();
            }
            let Some(registration) = state
                .registrations
                .get(&self.conversation_id)
                .and_then(|owners| owners.get(&self.connection_id))
            else {
                return dynamic_tool_unavailable();
            };
            let registration_is_current = registration.id == self.registration_id
                && registration.generation == self.generation
                && registration.thread_id.as_deref() == Some(params.thread_id.as_str());
            if !registration_is_current || state.pending.contains_key(&key) {
                return dynamic_tool_unavailable();
            }
            state.pending.insert(
                key.clone(),
                PendingCall {
                    conversation_id: self.conversation_id.clone(),
                    connection_id: self.connection_id,
                    turn_id: params.turn_id.clone(),
                    namespace: params.namespace.clone(),
                    tool: params.tool.clone(),
                    response_tx,
                },
            );
        }

        info!(
            conversation_id = %self.conversation_id,
            thread_id = %params.thread_id,
            turn_id = %params.turn_id,
            call_id = %params.call_id,
            tool = %params.tool,
            "dynamic tool call dispatched to websocket owner"
        );
        self.registry.manager.send_to(
            self.connection_id,
            WebSocketMessage::new(
                DYNAMIC_TOOL_CALL_EVENT,
                serde_json::to_value(DynamicToolCallPayload {
                    conversation_id: self.conversation_id.clone(),
                    registration_id: self.registration_id.clone(),
                    thread_id: params.thread_id,
                    turn_id: params.turn_id,
                    call_id: params.call_id,
                    namespace: params.namespace,
                    tool: params.tool,
                    arguments: params.arguments,
                })
                .unwrap_or_else(|_| json!({})),
            ),
        );

        match tokio::time::timeout(self.registry.call_timeout, response_rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) | Err(_) => {
                self.registry.state.lock().unwrap().pending.remove(&key);
                dynamic_tool_unavailable()
            }
        }
    }
}

fn registration_failure(
    request_id: String,
    conversation_id: String,
    error_code: &str,
) -> DynamicToolsRegisteredPayload {
    DynamicToolsRegisteredPayload {
        request_id,
        conversation_id,
        accepted: false,
        registration_id: None,
        error_code: Some(error_code.to_owned()),
    }
}

fn validate_tool_specs(tools: &[DynamicToolSpec]) -> Result<(), &'static str> {
    if tools.is_empty() || tools.len() > MAX_DYNAMIC_TOOL_SPECS {
        return Err("dynamic_tool_registry_invalid");
    }
    let mut identities = HashSet::new();
    for spec in tools {
        match spec {
            DynamicToolSpec::Function { name, .. } => {
                if name.trim().is_empty() || !identities.insert((None, name.trim().to_owned())) {
                    return Err("dynamic_tool_registry_invalid");
                }
            }
            DynamicToolSpec::Namespace { name, tools, .. } => {
                let namespace = name.trim();
                if namespace.is_empty() || tools.is_empty() {
                    return Err("dynamic_tool_registry_invalid");
                }
                for nested in tools {
                    let DynamicToolSpec::Function { name, .. } = nested else {
                        return Err("dynamic_tool_registry_invalid");
                    };
                    if name.trim().is_empty()
                        || !identities.insert((Some(namespace.to_owned()), name.trim().to_owned()))
                    {
                        return Err("dynamic_tool_registry_invalid");
                    }
                }
            }
        }
    }
    Ok(())
}

fn tool_is_registered(tools: &[DynamicToolSpec], namespace: Option<&str>, tool: &str) -> bool {
    match namespace {
        None => tools
            .iter()
            .any(|spec| matches!(spec, DynamicToolSpec::Function { name, .. } if name == tool)),
        Some(namespace) => tools.iter().any(|spec| {
            matches!(
                spec,
                DynamicToolSpec::Namespace { name, tools, .. }
                    if name == namespace
                        && tools.iter().any(|nested| matches!(nested, DynamicToolSpec::Function { name, .. } if name == tool))
            )
        }),
    }
}

fn valid_call_identity(params: &DynamicToolCallParams) -> bool {
    !params.thread_id.trim().is_empty()
        && !params.turn_id.trim().is_empty()
        && !params.call_id.trim().is_empty()
        && !params.tool.trim().is_empty()
        && params
            .namespace
            .as_ref()
            .is_none_or(|namespace| !namespace.trim().is_empty())
}

fn drop_pending_for_registration(state: &mut RegistryState, registration_id: &str) {
    state.pending.retain(|key, _| key.registration_id != registration_id);
}

pub fn dynamic_tool_unavailable() -> DynamicToolCallResponse {
    DynamicToolCallResponse {
        success: false,
        content_items: vec![DynamicToolCallOutputContentItem::InputText {
            text: "Dynamic tool unavailable.".to_owned(),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_realtime::{PER_CONNECTION_BUFFER, WsOutbound};
    use serde_json::json;
    use tokio::sync::mpsc;

    fn test_tools() -> Vec<DynamicToolSpec> {
        vec![DynamicToolSpec::Function {
            name: "list_threads".into(),
            description: "List tasks".into(),
            input_schema: json!({"type": "object"}),
            defer_loading: None,
        }]
    }

    fn add_connection(manager: &Arc<WebSocketManager>) -> (ConnectionId, mpsc::Receiver<WsOutbound>) {
        let (tx, rx) = mpsc::channel(PER_CONNECTION_BUFFER);
        (manager.add_client("token".into(), tx), rx)
    }

    fn register(
        registry: &DynamicToolRegistry,
        connection_id: ConnectionId,
        conversation_id: &str,
    ) -> DynamicToolsRegisteredPayload {
        registry.register(
            connection_id,
            DynamicToolsRegisterRequest {
                request_id: format!("request-{conversation_id}"),
                conversation_id: conversation_id.into(),
                tools: test_tools(),
            },
        )
    }

    fn call_params(thread_id: &str) -> DynamicToolCallParams {
        DynamicToolCallParams {
            thread_id: thread_id.into(),
            turn_id: "turn-1".into(),
            call_id: "call-1".into(),
            namespace: None,
            tool: "list_threads".into(),
            arguments: json!({"scope": "current_project"}),
        }
    }

    fn outbound_json(outbound: WsOutbound) -> Value {
        let WsOutbound::Text(text) = outbound else {
            panic!("expected websocket text");
        };
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn session_metadata_matches_codex_dynamic_tools_extension() {
        let manager = Arc::new(WebSocketManager::new());
        let registry = DynamicToolRegistry::new(manager.clone());
        let (connection_id, _rx) = add_connection(&manager);
        assert!(register(&registry, connection_id, "conversation-1").accepted);

        let session = registry.session_for("conversation-1").unwrap();
        let meta = session.metadata();
        assert_eq!(meta[CODEX_DYNAMIC_TOOLS_META_KEY]["version"], 1);
        assert_eq!(meta[CODEX_DYNAMIC_TOOLS_META_KEY]["tools"][0]["name"], "list_threads");
    }

    #[test]
    fn ambiguous_owner_stays_revoked_after_one_connection_disconnects() {
        let manager = Arc::new(WebSocketManager::new());
        let registry = DynamicToolRegistry::new(manager.clone());
        let (first, _first_rx) = add_connection(&manager);
        let (second, _second_rx) = add_connection(&manager);
        assert!(register(&registry, first, "conversation-1").accepted);
        assert!(!register(&registry, second, "conversation-1").accepted);
        assert!(registry.session_for("conversation-1").is_none());

        registry.disconnect(second);
        assert!(registry.session_for("conversation-1").is_none());
        assert!(register(&registry, first, "conversation-1").accepted);
        assert!(registry.session_for("conversation-1").is_some());
    }

    #[tokio::test]
    async fn call_result_round_trip_preserves_full_identity() {
        let manager = Arc::new(WebSocketManager::new());
        let registry = DynamicToolRegistry::with_timeout(manager.clone(), Duration::from_secs(1));
        let (connection_id, mut rx) = add_connection(&manager);
        let registration = register(&registry, connection_id, "conversation-1");
        let session = registry.session_for("conversation-1").unwrap();
        assert!(session.bind_thread("thread-1"));

        let dispatch = tokio::spawn({
            let session = session.clone();
            async move { session.dispatch(call_params("thread-1")).await }
        });
        let call = outbound_json(rx.recv().await.unwrap());
        assert_eq!(call["name"], DYNAMIC_TOOL_CALL_EVENT);
        assert_eq!(call["data"]["conversationId"], "conversation-1");
        assert_eq!(call["data"]["arguments"]["scope"], "current_project");

        assert!(registry.handle_result(
            connection_id,
            DynamicToolResultPayload {
                conversation_id: "conversation-1".into(),
                registration_id: registration.registration_id.unwrap(),
                thread_id: "thread-1".into(),
                turn_id: "turn-1".into(),
                call_id: "call-1".into(),
                namespace: None,
                tool: "list_threads".into(),
                content_items: vec![DynamicToolCallOutputContentItem::InputText { text: "ready".into() }],
                success: true,
            },
        ));
        assert_eq!(dispatch.await.unwrap().content_items.len(), 1);
    }

    #[tokio::test]
    async fn wrong_connection_and_thread_fail_closed() {
        let manager = Arc::new(WebSocketManager::new());
        let registry = DynamicToolRegistry::with_timeout(manager.clone(), Duration::from_millis(20));
        let (owner, mut owner_rx) = add_connection(&manager);
        let (other, _other_rx) = add_connection(&manager);
        let registration = register(&registry, owner, "conversation-1");
        let session = registry.session_for("conversation-1").unwrap();
        assert!(session.bind_thread("thread-1"));

        assert!(!session.dispatch(call_params("thread-other")).await.success);
        let dispatch = tokio::spawn({
            let session = session.clone();
            async move { session.dispatch(call_params("thread-1")).await }
        });
        let _call = owner_rx.recv().await.unwrap();
        assert!(!registry.handle_result(
            other,
            DynamicToolResultPayload {
                conversation_id: "conversation-1".into(),
                registration_id: registration.registration_id.unwrap(),
                thread_id: "thread-1".into(),
                turn_id: "turn-1".into(),
                call_id: "call-1".into(),
                namespace: None,
                tool: "list_threads".into(),
                content_items: vec![],
                success: true,
            },
        ));
        assert!(!dispatch.await.unwrap().success);
    }

    #[tokio::test]
    async fn disconnect_clears_registration_and_pending_call() {
        let manager = Arc::new(WebSocketManager::new());
        let registry = DynamicToolRegistry::with_timeout(manager.clone(), Duration::from_secs(1));
        let (connection_id, mut rx) = add_connection(&manager);
        register(&registry, connection_id, "conversation-1");
        let session = registry.session_for("conversation-1").unwrap();
        assert!(session.bind_thread("thread-1"));

        let dispatch = tokio::spawn({
            let session = session.clone();
            async move { session.dispatch(call_params("thread-1")).await }
        });
        let _call = rx.recv().await.unwrap();
        registry.disconnect(connection_id);

        assert!(!dispatch.await.unwrap().success);
        assert!(registry.session_for("conversation-1").is_none());
    }
}
