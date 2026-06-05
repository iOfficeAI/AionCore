use std::sync::Arc;

use aionui_api_types::{
    RuntimeFailureKind, RuntimeResourceKind, RuntimeStatusPayload, RuntimeStatusPhase, RuntimeStatusScope,
    RuntimeStatusScopeKind, WebSocketMessage,
};
use aionui_realtime::EventBroadcaster;
use aionui_runtime::{
    ManagedAcpToolFailureKind, ManagedAcpToolId, ManagedAcpToolProgress, NodeRuntimeFailureKind, NodeRuntimeProgress,
    SharedManagedAcpToolProgressReporter, SharedNodeRuntimeProgressReporter,
};

pub(crate) fn conversation_runtime_reporter(
    broadcaster: Arc<dyn EventBroadcaster>,
    conversation_id: impl Into<String>,
) -> SharedNodeRuntimeProgressReporter {
    node_runtime_reporter(
        broadcaster,
        RuntimeStatusScope {
            kind: RuntimeStatusScopeKind::Conversation,
            id: conversation_id.into(),
        },
    )
}

pub(crate) fn custom_agent_runtime_reporter(
    broadcaster: Arc<dyn EventBroadcaster>,
    scope_id: impl Into<String>,
) -> SharedNodeRuntimeProgressReporter {
    node_runtime_reporter(
        broadcaster,
        RuntimeStatusScope {
            kind: RuntimeStatusScopeKind::CustomAgent,
            id: scope_id.into(),
        },
    )
}

pub(crate) fn conversation_acp_tool_runtime_reporter(
    broadcaster: Arc<dyn EventBroadcaster>,
    conversation_id: impl Into<String>,
    tool: ManagedAcpToolId,
) -> SharedManagedAcpToolProgressReporter {
    acp_tool_runtime_reporter(
        broadcaster,
        RuntimeStatusScope {
            kind: RuntimeStatusScopeKind::Conversation,
            id: conversation_id.into(),
        },
        tool,
    )
}

fn node_runtime_reporter(
    broadcaster: Arc<dyn EventBroadcaster>,
    scope: RuntimeStatusScope,
) -> SharedNodeRuntimeProgressReporter {
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
    broadcaster: Arc<dyn EventBroadcaster>,
    scope: RuntimeStatusScope,
    tool: ManagedAcpToolId,
) -> SharedManagedAcpToolProgressReporter {
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

fn map_phase(phase: aionui_runtime::NodeRuntimeProgressPhase) -> RuntimeStatusPhase {
    match phase {
        aionui_runtime::NodeRuntimeProgressPhase::WaitingForLock => RuntimeStatusPhase::WaitingForLock,
        aionui_runtime::NodeRuntimeProgressPhase::Downloading => RuntimeStatusPhase::Downloading,
        aionui_runtime::NodeRuntimeProgressPhase::Extracting => RuntimeStatusPhase::Extracting,
        aionui_runtime::NodeRuntimeProgressPhase::Validating => RuntimeStatusPhase::Validating,
        aionui_runtime::NodeRuntimeProgressPhase::Ready => RuntimeStatusPhase::Ready,
        aionui_runtime::NodeRuntimeProgressPhase::Failed => RuntimeStatusPhase::Failed,
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

fn map_acp_phase(phase: aionui_runtime::ManagedAcpToolProgressPhase) -> RuntimeStatusPhase {
    match phase {
        aionui_runtime::ManagedAcpToolProgressPhase::WaitingForLock => RuntimeStatusPhase::WaitingForLock,
        aionui_runtime::ManagedAcpToolProgressPhase::Downloading => RuntimeStatusPhase::Downloading,
        aionui_runtime::ManagedAcpToolProgressPhase::Extracting => RuntimeStatusPhase::Extracting,
        aionui_runtime::ManagedAcpToolProgressPhase::Validating => RuntimeStatusPhase::Validating,
        aionui_runtime::ManagedAcpToolProgressPhase::Ready => RuntimeStatusPhase::Ready,
        aionui_runtime::ManagedAcpToolProgressPhase::Failed => RuntimeStatusPhase::Failed,
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
