use std::sync::Arc;

use aionui_api_types::{
    EnsureNodeRuntimeResponse, RuntimeFailureKind, RuntimeResourceKind, RuntimeStatusPayload, RuntimeStatusPhase,
    RuntimeStatusScope, WebSocketMessage,
};
use aionui_realtime::EventBroadcaster;
use aionui_runtime::{
    NodeRuntimeFailureKind, NodeRuntimeProgress, NodeRuntimeProgressPhase, SharedNodeRuntimeProgressReporter,
    ensure_node_runtime_with_reporter,
};

use crate::error::SystemError;

#[derive(Clone)]
pub struct RuntimePrepareService {
    broadcaster: Arc<dyn EventBroadcaster>,
}

impl RuntimePrepareService {
    pub fn new(broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self { broadcaster }
    }

    pub async fn ensure_node_runtime(
        &self,
        scope: RuntimeStatusScope,
    ) -> Result<EnsureNodeRuntimeResponse, SystemError> {
        self.ensure_node_runtime_with_user(None, scope).await
    }

    pub async fn ensure_node_runtime_for_user(
        &self,
        user_id: &str,
        scope: RuntimeStatusScope,
    ) -> Result<EnsureNodeRuntimeResponse, SystemError> {
        self.ensure_node_runtime_with_user(Some(user_id.to_owned()), scope)
            .await
    }

    async fn ensure_node_runtime_with_user(
        &self,
        user_id: Option<String>,
        scope: RuntimeStatusScope,
    ) -> Result<EnsureNodeRuntimeResponse, SystemError> {
        let reporter = self.node_runtime_reporter(user_id, scope);
        ensure_node_runtime_with_reporter(Some(reporter.as_ref()))
            .await
            .map_err(|error| SystemError::BadRequest(error.to_string()))?;
        Ok(EnsureNodeRuntimeResponse { ready: true })
    }

    fn node_runtime_reporter(
        &self,
        user_id: Option<String>,
        scope: RuntimeStatusScope,
    ) -> SharedNodeRuntimeProgressReporter {
        let broadcaster = self.broadcaster.clone();
        Arc::new(move |update: NodeRuntimeProgress| {
            let payload = RuntimeStatusPayload {
                user_id: user_id.clone(),
                resource: RuntimeResourceKind::Node,
                resource_id: None,
                scope: scope.clone(),
                phase: map_phase(update.phase),
                failure_kind: update.failure_kind.map(map_failure_kind),
                message: update.message,
                status_code: update.status_code,
            };
            let payload = serde_json::to_value(payload).expect("runtime status payload should serialize");
            broadcaster.broadcast(WebSocketMessage::new("runtime.statusChanged", payload));
        })
    }
}

fn map_phase(phase: NodeRuntimeProgressPhase) -> RuntimeStatusPhase {
    match phase {
        NodeRuntimeProgressPhase::WaitingForLock => RuntimeStatusPhase::WaitingForLock,
        NodeRuntimeProgressPhase::Downloading => RuntimeStatusPhase::Downloading,
        NodeRuntimeProgressPhase::Extracting => RuntimeStatusPhase::Extracting,
        NodeRuntimeProgressPhase::Validating => RuntimeStatusPhase::Validating,
        NodeRuntimeProgressPhase::Ready => RuntimeStatusPhase::Ready,
        NodeRuntimeProgressPhase::Failed => RuntimeStatusPhase::Failed,
    }
}

fn map_failure_kind(kind: NodeRuntimeFailureKind) -> RuntimeFailureKind {
    match kind {
        NodeRuntimeFailureKind::Timeout => RuntimeFailureKind::Timeout,
        NodeRuntimeFailureKind::DownloadFailed => RuntimeFailureKind::DownloadFailed,
        NodeRuntimeFailureKind::HttpStatus => RuntimeFailureKind::HttpStatus,
        NodeRuntimeFailureKind::ChecksumMismatch => RuntimeFailureKind::ChecksumMismatch,
        NodeRuntimeFailureKind::ValidationFailed => RuntimeFailureKind::ValidationFailed,
        NodeRuntimeFailureKind::UnsupportedPlatform => RuntimeFailureKind::UnsupportedPlatform,
        NodeRuntimeFailureKind::BundledResourceMissing => RuntimeFailureKind::BundledResourceMissing,
        NodeRuntimeFailureKind::BundledResourceInvalid => RuntimeFailureKind::BundledResourceInvalid,
        NodeRuntimeFailureKind::ActivationIoFailed => RuntimeFailureKind::ActivationIoFailed,
        NodeRuntimeFailureKind::Unknown => RuntimeFailureKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use aionui_api_types::{RuntimeStatusScopeKind, WebSocketMessage};

    use super::*;

    #[derive(Default)]
    struct RecordingBroadcaster {
        events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
    }

    impl RecordingBroadcaster {
        fn events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
            self.events.lock().unwrap().clone()
        }
    }

    impl EventBroadcaster for RecordingBroadcaster {
        fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn conversation_scope() -> RuntimeStatusScope {
        RuntimeStatusScope {
            kind: RuntimeStatusScopeKind::Conversation,
            id: "conv-1".to_owned(),
        }
    }

    #[test]
    fn node_runtime_reporter_scopes_route_event_to_user() {
        let broadcaster = Arc::new(RecordingBroadcaster::default());
        let service = RuntimePrepareService::new(broadcaster.clone());
        let reporter = service.node_runtime_reporter(Some("user-1".to_owned()), conversation_scope());

        reporter.report(NodeRuntimeProgress::downloading("downloading node"));

        let events = broadcaster.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "runtime.statusChanged");
        assert_eq!(events[0].data["user_id"], "user-1");
        assert_eq!(events[0].data["resource"], "node");
        assert_eq!(events[0].data["scope"]["id"], "conv-1");
    }

    #[test]
    fn node_runtime_reporter_can_emit_global_startup_event() {
        let broadcaster = Arc::new(RecordingBroadcaster::default());
        let service = RuntimePrepareService::new(broadcaster.clone());
        let reporter = service.node_runtime_reporter(None, conversation_scope());

        reporter.report(NodeRuntimeProgress::validating("validating node"));

        let events = broadcaster.events();
        assert_eq!(events.len(), 1);
        assert!(events[0].data.get("user_id").is_none());
        assert_eq!(events[0].data["resource"], "node");
    }
}
