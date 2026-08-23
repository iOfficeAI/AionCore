use std::sync::{Arc, RwLock};

use aion_protocol::events::{ProtocolEvent, ToolCategory, ToolStatus};
use aion_protocol::writer::ProtocolEmitter;
use aionui_common::{Confirmation, ConfirmationOption, generate_id};
use serde_json::json;
use tokio::sync::broadcast;
use tracing::debug;

use crate::protocol::events::{
    AcpPermissionEventData, AgentStreamEvent, InputLifecycleEventData, ToolCallEventData, ToolCallStatus,
};

/// Implements `ProtocolEmitter` for the aioncore context.
///
/// Bridges aionrs `ProtocolEvent` emissions to `AgentStreamEvent` on a
/// broadcast channel. Approval events drive confirmations; tool lifecycle
/// events add the stable execution id and phase that the legacy `OutputSink`
/// callbacks cannot express. Text and thinking remain owned by
/// `BackendOutputSink`.
pub struct BackendProtocolSink {
    event_tx: broadcast::Sender<AgentStreamEvent>,
    confirmations: Arc<RwLock<Vec<Confirmation>>>,
}

impl BackendProtocolSink {
    pub fn new(event_tx: broadcast::Sender<AgentStreamEvent>, confirmations: Arc<RwLock<Vec<Confirmation>>>) -> Self {
        Self {
            event_tx,
            confirmations,
        }
    }

    fn build_confirmation(call_id: &str, tool_name: &str, category: &ToolCategory, description: &str) -> Confirmation {
        let title = format!("{} wants to use: {}", category, tool_name);
        let command_type = Some(category.to_string());

        Confirmation {
            id: generate_id(),
            call_id: call_id.to_string(),
            title: Some(title),
            action: Some(tool_name.to_string()),
            description: description.to_string(),
            questions: None,
            command_type,
            options: vec![
                ConfirmationOption {
                    label: "messages.confirmation.yesAllowOnce".to_string(),
                    value: json!("proceed_once"),
                    params: None,
                },
                ConfirmationOption {
                    label: "messages.confirmation.yesAllowAlways".to_string(),
                    value: json!("proceed_always"),
                    params: None,
                },
                ConfirmationOption {
                    label: "messages.confirmation.no".to_string(),
                    value: json!("cancel"),
                    params: None,
                },
            ],
        }
    }

    fn internal_call_id(call_id: &str) -> String {
        format!("aionrs-{call_id}")
    }

    fn execution_description(execution_id: &str, phase: &str) -> String {
        json!({
            "execution_id": execution_id,
            "phase": phase,
            "enforcement": "native",
        })
        .to_string()
    }

    fn result_description(
        execution_id: &str,
        structured_content: &Option<serde_json::Value>,
        error_code: &Option<aion_types::tool::ToolExecutionErrorCode>,
        truncation: &Option<aion_types::tool::ToolResultTruncation>,
    ) -> String {
        json!({
            "execution_id": execution_id,
            "phase": "finalize",
            "enforcement": "native",
            "structured_content": structured_content,
            "error_code": error_code,
            "truncation": truncation,
        })
        .to_string()
    }
}

impl ProtocolEmitter for BackendProtocolSink {
    fn emit(&self, event: &ProtocolEvent) -> std::io::Result<()> {
        match event {
            ProtocolEvent::InputAccepted { input_id } => {
                let _ = self
                    .event_tx
                    .send(AgentStreamEvent::InputAccepted(InputLifecycleEventData {
                        input_id: input_id.clone(),
                        turn_id: None,
                        error_code: None,
                    }));
            }
            ProtocolEvent::InputApplied { input_id, turn_id } => {
                let _ = self
                    .event_tx
                    .send(AgentStreamEvent::InputApplied(InputLifecycleEventData {
                        input_id: input_id.clone(),
                        turn_id: turn_id.clone(),
                        error_code: None,
                    }));
            }
            ProtocolEvent::InputRejected { input_id, error_code } => {
                let _ = self
                    .event_tx
                    .send(AgentStreamEvent::InputRejected(InputLifecycleEventData {
                        input_id: input_id.clone(),
                        turn_id: None,
                        error_code: Some(error_code.clone()),
                    }));
            }
            ProtocolEvent::ToolRequest { call_id, tool, .. } => {
                let confirmation = Self::build_confirmation(call_id, &tool.name, &tool.category, &tool.description);

                if let Ok(mut confs) = self.confirmations.write() {
                    confs.push(confirmation.clone());
                }

                let _ = self
                    .event_tx
                    .send(AgentStreamEvent::AcpPermission(AcpPermissionEventData::Confirmation(
                        confirmation.clone(),
                    )));

                debug!(
                    call_id,
                    tool_name = %tool.name,
                    "BackendProtocolSink: emitted AcpPermission(Confirmation) event"
                );
            }

            ProtocolEvent::ToolRunning {
                call_id,
                execution_id,
                tool_name,
                ..
            } => {
                let _ = self.event_tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
                    call_id: Self::internal_call_id(call_id),
                    name: tool_name.clone(),
                    args: serde_json::Value::Null,
                    status: ToolCallStatus::Running,
                    input: None,
                    output: None,
                    description: Some(Self::execution_description(execution_id, "execute")),
                }));
            }

            ProtocolEvent::ToolResult {
                call_id,
                execution_id,
                tool_name,
                status,
                output,
                structured_content,
                error_code,
                truncation,
                ..
            } => {
                let status = match status {
                    ToolStatus::Success => ToolCallStatus::Completed,
                    ToolStatus::Error => ToolCallStatus::Error,
                };
                let _ = self.event_tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
                    call_id: Self::internal_call_id(call_id),
                    name: tool_name.clone(),
                    args: serde_json::Value::Null,
                    status,
                    input: None,
                    output: Some(output.clone()),
                    description: Some(Self::result_description(
                        execution_id,
                        structured_content,
                        error_code,
                        truncation,
                    )),
                }));
            }

            ProtocolEvent::ToolCancelled {
                call_id,
                execution_id,
                reason,
                error_code,
                ..
            } => {
                if let Ok(mut confs) = self.confirmations.write() {
                    confs.retain(|c| c.call_id != *call_id);
                }

                let _ = self.event_tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
                    call_id: Self::internal_call_id(call_id),
                    name: format!("cancelled: {reason}"),
                    args: serde_json::Value::Null,
                    status: ToolCallStatus::Error,
                    input: None,
                    output: None,
                    description: Some(
                        json!({
                            "execution_id": execution_id,
                            "phase": "cancelled",
                            "enforcement": "native",
                            "error_code": error_code,
                        })
                        .to_string(),
                    ),
                }));
            }

            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aion_protocol::events::ToolInfo;

    fn make_sink() -> (
        BackendProtocolSink,
        broadcast::Receiver<AgentStreamEvent>,
        Arc<RwLock<Vec<Confirmation>>>,
    ) {
        let (tx, rx) = broadcast::channel(16);
        let confs = Arc::new(RwLock::new(Vec::new()));
        let sink = BackendProtocolSink::new(tx, confs.clone());
        (sink, rx, confs)
    }

    #[test]
    fn tool_request_emits_permission_event() {
        let (sink, mut rx, confs) = make_sink();
        let event = ProtocolEvent::ToolRequest {
            msg_id: "m1".into(),
            call_id: "c1".into(),
            execution_id: "exec-1".into(),
            tool: ToolInfo {
                name: "Write".into(),
                category: ToolCategory::Edit,
                args: json!({"path": "/tmp/test.txt"}),
                description: "Write file /tmp/test.txt".into(),
            },
        };

        sink.emit(&event).unwrap();

        let received = rx.try_recv().unwrap();
        match received {
            AgentStreamEvent::AcpPermission(AcpPermissionEventData::Confirmation(conf)) => {
                assert_eq!(conf.call_id, "c1");
                assert!(conf.options.len() >= 3);
            }
            other => panic!("Expected AcpPermission(Confirmation), got {:?}", other),
        }

        let stored = confs.read().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].call_id, "c1");
    }

    #[test]
    fn input_lifecycle_events_preserve_correlation() {
        let (sink, mut rx, _) = make_sink();
        sink.emit(&ProtocolEvent::InputAccepted {
            input_id: "input-1".into(),
        })
        .unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            AgentStreamEvent::InputAccepted(data) if data.input_id == "input-1"
        ));

        sink.emit(&ProtocolEvent::InputApplied {
            input_id: "input-1".into(),
            turn_id: Some("turn-1".into()),
        })
        .unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            AgentStreamEvent::InputApplied(data)
                if data.input_id == "input-1" && data.turn_id.as_deref() == Some("turn-1")
        ));

        sink.emit(&ProtocolEvent::InputRejected {
            input_id: "input-2".into(),
            error_code: "too_late".into(),
        })
        .unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            AgentStreamEvent::InputRejected(data)
                if data.input_id == "input-2" && data.error_code.as_deref() == Some("too_late")
        ));
    }

    #[test]
    fn tool_running_emits_correlated_execution_state() {
        let (sink, mut rx, _) = make_sink();
        let event = ProtocolEvent::ToolRunning {
            msg_id: "m1".into(),
            call_id: "c1".into(),
            execution_id: "exec-1".into(),
            tool_name: "Write".into(),
        };

        sink.emit(&event).unwrap();
        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.call_id, "aionrs-c1");
                assert_eq!(data.status, ToolCallStatus::Running);
                assert!(data.description.as_deref().unwrap().contains("exec-1"));
            }
            other => panic!("Expected ToolCall running, got {:?}", other),
        }
    }

    #[test]
    fn tool_cancelled_removes_confirmation_and_emits_error() {
        let (sink, mut rx, confs) = make_sink();

        let req = ProtocolEvent::ToolRequest {
            msg_id: "m1".into(),
            call_id: "c1".into(),
            execution_id: "exec-1".into(),
            tool: ToolInfo {
                name: "Bash".into(),
                category: ToolCategory::Exec,
                args: json!({"command": "rm -rf /"}),
                description: "Execute: rm -rf /".into(),
            },
        };
        sink.emit(&req).unwrap();
        let _ = rx.try_recv().unwrap();

        assert_eq!(confs.read().unwrap().len(), 1);

        let cancel = ProtocolEvent::ToolCancelled {
            msg_id: "m1".into(),
            call_id: "c1".into(),
            execution_id: "exec-1".into(),
            reason: "User denied".into(),
            error_code: None,
        };
        sink.emit(&cancel).unwrap();

        let received = rx.try_recv().unwrap();
        match received {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.call_id, "aionrs-c1");
                assert_eq!(data.status, ToolCallStatus::Error);
                assert!(data.description.as_deref().unwrap().contains("exec-1"));
            }
            other => panic!("Expected ToolCall error, got {:?}", other),
        }

        assert_eq!(confs.read().unwrap().len(), 0);
    }

    #[test]
    fn tool_result_emits_correlated_terminal_state() {
        let (sink, mut rx, _) = make_sink();
        sink.emit(&ProtocolEvent::ToolResult {
            msg_id: "m1".into(),
            call_id: "c1".into(),
            execution_id: "exec-1".into(),
            tool_name: "Read".into(),
            status: ToolStatus::Error,
            output: "partial output".into(),
            output_type: aion_protocol::events::OutputType::Text,
            metadata: None,
            content_blocks: None,
            structured_content: Some(json!({ "rows": 2 })),
            error_code: Some(aion_types::tool::ToolExecutionErrorCode::ExecutionFailed),
            truncation: Some(aion_types::tool::ToolResultTruncation {
                original_bytes: 20,
                output_bytes: 10,
                limit_bytes: 10,
            }),
        })
        .unwrap();

        match rx.try_recv().unwrap() {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.call_id, "aionrs-c1");
                assert_eq!(data.status, ToolCallStatus::Error);
                assert_eq!(data.output.as_deref(), Some("partial output"));
                let diagnostics: serde_json::Value =
                    serde_json::from_str(data.description.as_deref().unwrap()).unwrap();
                assert_eq!(diagnostics["structured_content"], json!({ "rows": 2 }));
                assert_eq!(diagnostics["error_code"], "execution_failed");
                assert_eq!(diagnostics["truncation"]["original_bytes"], 20);
            }
            other => panic!("Expected ToolCall result, got {:?}", other),
        }
    }

    #[test]
    fn other_events_are_ignored() {
        let (sink, mut rx, _) = make_sink();
        let event = ProtocolEvent::StreamStart { msg_id: "m1".into() };

        sink.emit(&event).unwrap();

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn no_panic_when_no_receivers() {
        let (tx, _) = broadcast::channel(16);
        let confs = Arc::new(RwLock::new(Vec::new()));
        let sink = BackendProtocolSink::new(tx, confs);
        let event = ProtocolEvent::ToolRequest {
            msg_id: "m1".into(),
            call_id: "c1".into(),
            execution_id: "exec-1".into(),
            tool: ToolInfo {
                name: "Read".into(),
                category: ToolCategory::Info,
                args: json!({}),
                description: "Read file".into(),
            },
        };
        sink.emit(&event).unwrap();
    }

    #[test]
    fn confirmation_has_three_options() {
        let conf =
            BackendProtocolSink::build_confirmation("c1", "Write", &ToolCategory::Edit, "Write file /tmp/test.txt");
        assert_eq!(conf.options.len(), 3);
        assert_eq!(conf.options[0].value, json!("proceed_once"));
        assert_eq!(conf.options[1].value, json!("proceed_always"));
        assert_eq!(conf.options[2].value, json!("cancel"));
    }
}
