use std::sync::Arc;

use aionui_api_types::{
    EnsureManagedAcpToolResponse, EnsureNodeRuntimeResponse, RuntimeFailureKind, RuntimeResourceKind,
    RuntimeStatusPayload, RuntimeStatusPhase, RuntimeStatusScope, WebSocketMessage,
};
use aionui_common::AppError;
use aionui_realtime::EventBroadcaster;
use aionui_runtime::{
    ManagedAcpToolFailureKind, ManagedAcpToolId, ManagedAcpToolProgress, ManagedAcpToolProgressPhase,
    NodeRuntimeFailureKind, NodeRuntimeProgress, NodeRuntimeProgressPhase, SharedManagedAcpToolProgressReporter,
    SharedNodeRuntimeProgressReporter, ensure_managed_acp_tool_with_reporter, ensure_node_runtime_with_reporter,
};

#[derive(Clone)]
pub struct RuntimePrepareService {
    broadcaster: Arc<dyn EventBroadcaster>,
}

impl RuntimePrepareService {
    pub fn new(broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self { broadcaster }
    }

    pub async fn ensure_node_runtime(&self, scope: RuntimeStatusScope) -> Result<EnsureNodeRuntimeResponse, AppError> {
        let reporter = self.node_runtime_reporter(scope);
        ensure_node_runtime_with_reporter(Some(reporter.as_ref()))
            .await
            .map_err(|error| AppError::BadRequest(error.to_string()))?;
        Ok(EnsureNodeRuntimeResponse { ready: true })
    }

    pub async fn ensure_managed_acp_tool(
        &self,
        scope: RuntimeStatusScope,
        tool_id: &str,
    ) -> Result<EnsureManagedAcpToolResponse, AppError> {
        let tool = ManagedAcpToolId::from_slug(tool_id)
            .ok_or_else(|| AppError::BadRequest(format!("Unsupported managed ACP tool '{tool_id}'")))?;
        let node_reporter = self.node_runtime_reporter(scope.clone());
        ensure_node_runtime_with_reporter(Some(node_reporter.as_ref()))
            .await
            .map_err(|error| AppError::BadRequest(error.to_string()))?;

        let tool_reporter = self.acp_tool_runtime_reporter(scope, tool);
        ensure_managed_acp_tool_with_reporter(tool, Some(tool_reporter.as_ref()))
            .await
            .map_err(|error| AppError::BadRequest(error.to_string()))?;
        Ok(EnsureManagedAcpToolResponse { ready: true })
    }

    fn node_runtime_reporter(&self, scope: RuntimeStatusScope) -> SharedNodeRuntimeProgressReporter {
        let broadcaster = self.broadcaster.clone();
        Arc::new(move |update: NodeRuntimeProgress| {
            let payload = RuntimeStatusPayload {
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

    fn acp_tool_runtime_reporter(
        &self,
        scope: RuntimeStatusScope,
        tool: ManagedAcpToolId,
    ) -> SharedManagedAcpToolProgressReporter {
        let broadcaster = self.broadcaster.clone();
        Arc::new(move |update: ManagedAcpToolProgress| {
            let payload = RuntimeStatusPayload {
                resource: RuntimeResourceKind::AcpTool,
                resource_id: Some(tool.slug().to_owned()),
                scope: scope.clone(),
                phase: map_acp_phase(update.phase),
                failure_kind: update.failure_kind.map(map_acp_failure_kind),
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
        NodeRuntimeFailureKind::ValidationFailed => RuntimeFailureKind::ValidationFailed,
        NodeRuntimeFailureKind::UnsupportedPlatform => RuntimeFailureKind::UnsupportedPlatform,
        NodeRuntimeFailureKind::Unknown => RuntimeFailureKind::Unknown,
    }
}

fn map_acp_phase(phase: ManagedAcpToolProgressPhase) -> RuntimeStatusPhase {
    match phase {
        ManagedAcpToolProgressPhase::WaitingForLock => RuntimeStatusPhase::WaitingForLock,
        ManagedAcpToolProgressPhase::Downloading => RuntimeStatusPhase::Downloading,
        ManagedAcpToolProgressPhase::Extracting => RuntimeStatusPhase::Extracting,
        ManagedAcpToolProgressPhase::Validating => RuntimeStatusPhase::Validating,
        ManagedAcpToolProgressPhase::Ready => RuntimeStatusPhase::Ready,
        ManagedAcpToolProgressPhase::Failed => RuntimeStatusPhase::Failed,
    }
}

fn map_acp_failure_kind(kind: ManagedAcpToolFailureKind) -> RuntimeFailureKind {
    match kind {
        ManagedAcpToolFailureKind::Timeout => RuntimeFailureKind::Timeout,
        ManagedAcpToolFailureKind::DownloadFailed => RuntimeFailureKind::DownloadFailed,
        ManagedAcpToolFailureKind::HttpStatus => RuntimeFailureKind::HttpStatus,
        ManagedAcpToolFailureKind::ChecksumMismatch => RuntimeFailureKind::ChecksumMismatch,
        ManagedAcpToolFailureKind::ValidationFailed => RuntimeFailureKind::ValidationFailed,
        ManagedAcpToolFailureKind::UnsupportedPlatform => RuntimeFailureKind::UnsupportedPlatform,
        ManagedAcpToolFailureKind::Unknown => RuntimeFailureKind::Unknown,
    }
}
