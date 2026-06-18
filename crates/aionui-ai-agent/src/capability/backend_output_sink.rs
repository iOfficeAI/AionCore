use aion_agent::output::OutputSink;
use tokio::sync::broadcast;

use crate::protocol::events::{
    AgentStreamEvent, ErrorEventData, FinishEventData, StartEventData, TextEventData, ThinkingEventData, TipType,
    TipsEventData, ToolCallEventData, ToolCallStatus,
};

pub struct BackendOutputSink {
    event_tx: broadcast::Sender<AgentStreamEvent>,
}

impl BackendOutputSink {
    pub fn new(event_tx: broadcast::Sender<AgentStreamEvent>) -> Self {
        Self { event_tx }
    }

    fn internal_call_id(tool_use_id: &str) -> Option<String> {
        let id = tool_use_id.trim();
        if id.is_empty() {
            None
        } else {
            Some(format!("aionrs-{id}"))
        }
    }
}

impl OutputSink for BackendOutputSink {
    fn emit_text_delta(&self, text: &str, _msg_id: &str) {
        let _ = self.event_tx.send(AgentStreamEvent::Text(TextEventData {
            content: text.to_owned(),
        }));
    }

    fn emit_thinking(&self, text: &str, _msg_id: &str) {
        let _ = self.event_tx.send(AgentStreamEvent::Thinking(ThinkingEventData {
            content: text.to_owned(),
            subject: None,
            duration: None,
            status: None,
        }));
    }

    fn emit_tool_call(&self, tool_use_id: &str, name: &str, input: &str) {
        let Some(call_id) = Self::internal_call_id(tool_use_id) else {
            tracing::error!(tool = name, "Cannot emit tool_call with empty tool_use_id");
            return;
        };
        if name.trim().is_empty() {
            tracing::error!(tool_use_id = %tool_use_id, "Cannot emit tool_call with empty tool name");
            return;
        }

        let parsed_input = serde_json::from_str(input).unwrap_or(serde_json::Value::String(input.to_owned()));

        tracing::debug!(
            tool_use_id = %tool_use_id,
            call_id = %call_id,
            tool = name,
            status = ?ToolCallStatus::Running,
            "Derived internal tool_call id from aionrs tool_use_id"
        );
        tracing::info!(
            tool_use_id = %tool_use_id,
            call_id = %call_id,
            tool = name,
            status = ?ToolCallStatus::Running,
            "Emitting aionrs tool_call event"
        );

        let _ = self.event_tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id,
            name: name.to_owned(),
            args: parsed_input.clone(),
            status: ToolCallStatus::Running,
            input: Some(parsed_input),
            output: None,
            description: None,
        }));
    }

    fn emit_tool_result(&self, tool_use_id: &str, name: &str, is_error: bool, content: &str) {
        let Some(call_id) = Self::internal_call_id(tool_use_id) else {
            tracing::error!(tool = name, "Cannot emit tool_result with empty tool_use_id");
            return;
        };
        if name.trim().is_empty() {
            tracing::error!(tool_use_id = %tool_use_id, "Cannot emit tool_result with empty tool name");
            return;
        }

        let status = if is_error {
            ToolCallStatus::Error
        } else {
            ToolCallStatus::Completed
        };

        tracing::info!(
            tool_use_id = %tool_use_id,
            call_id = %call_id,
            tool = name,
            status = ?status,
            "Emitting aionrs tool_result event"
        );

        let _ = self.event_tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id,
            name: name.to_owned(),
            args: serde_json::Value::Null,
            status,
            input: None,
            output: if content.is_empty() {
                None
            } else {
                Some(content.to_owned())
            },
            description: None,
        }));
    }

    fn emit_stream_start(&self, _msg_id: &str) {
        let _ = self
            .event_tx
            .send(AgentStreamEvent::Start(StartEventData { session_id: None }));
    }

    fn emit_stream_end(
        &self,
        _msg_id: &str,
        _turns: usize,
        _input_tokens: u64,
        _output_tokens: u64,
        _cache_creation_tokens: u64,
        _cache_read_tokens: u64,
    ) {
        let _ = self
            .event_tx
            .send(AgentStreamEvent::Finish(FinishEventData { session_id: None }));
    }

    fn emit_error(&self, msg: &str) {
        let _ = self
            .event_tx
            .send(AgentStreamEvent::Error(ErrorEventData::legacy(msg, None)));
    }

    fn emit_info(&self, msg: &str) {
        let _ = self.event_tx.send(AgentStreamEvent::Tips(TipsEventData {
            content: msg.to_owned(),
            tip_type: TipType::Success,
            code: None,
            params: None,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sink() -> (BackendOutputSink, broadcast::Receiver<AgentStreamEvent>) {
        let (tx, rx) = broadcast::channel(16);
        (BackendOutputSink::new(tx), rx)
    }

    #[test]
    fn emit_text_delta_sends_text_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_text_delta("hello", "msg-1");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Text(data) => assert_eq!(data.content, "hello"),
            other => panic!("Expected Text, got {:?}", other),
        }
    }

    #[test]
    fn emit_thinking_sends_thinking_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_thinking("analyzing...", "msg-1");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Thinking(data) => assert_eq!(data.content, "analyzing..."),
            other => panic!("Expected Thinking, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_call_sends_running_tool_call() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call("call_read_1", "Read", r#"{"path":"/tmp/a.txt"}"#);
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.name, "Read");
                assert_eq!(data.status, ToolCallStatus::Running);
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_call_with_empty_name_is_ignored() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call("call_bad", "   ", r#"{"path":"/tmp/a.txt"}"#);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn emit_tool_result_success_sends_completed() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_result("call_read_1", "Read", false, "file content here");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.name, "Read");
                assert_eq!(data.status, ToolCallStatus::Completed);
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_result_with_empty_name_is_ignored() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_result("call_bad", "   ", false, "content");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn emit_tool_result_error_sends_error_status() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_result("call_bash_1", "Bash", true, "command failed");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.name, "Bash");
                assert_eq!(data.status, ToolCallStatus::Error);
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn duplicate_tool_names_use_distinct_internal_call_ids() {
        let (sink, mut rx) = make_sink();

        sink.emit_tool_call("call_a", "Glob", r#"{"pattern":"*.rs"}"#);
        sink.emit_tool_call("call_b", "Glob", r#"{"pattern":"*.toml"}"#);
        sink.emit_tool_result("call_a", "Glob", false, "first");
        sink.emit_tool_result("call_b", "Glob", false, "second");

        let events = (0..4).map(|_| rx.try_recv().unwrap()).collect::<Vec<_>>();

        let mut call_ids = vec![];
        for event in events {
            match event {
                AgentStreamEvent::ToolCall(data) => call_ids.push((data.call_id, data.status)),
                other => panic!("Expected ToolCall, got {:?}", other),
            }
        }

        assert_eq!(call_ids[0].0, "aionrs-call_a");
        assert_eq!(call_ids[1].0, "aionrs-call_b");
        assert_eq!(call_ids[2].0, "aionrs-call_a");
        assert_eq!(call_ids[3].0, "aionrs-call_b");
        assert_eq!(call_ids[2].1, ToolCallStatus::Completed);
        assert_eq!(call_ids[3].1, ToolCallStatus::Completed);
    }

    #[test]
    fn emit_stream_start_sends_start_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_stream_start("msg-1");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Start(_) => {}
            other => panic!("Expected Start, got {:?}", other),
        }
    }

    #[test]
    fn emit_stream_end_sends_finish_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_stream_end("msg-1", 3, 1000, 500, 100, 200);
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Finish(_) => {}
            other => panic!("Expected Finish, got {:?}", other),
        }
    }

    #[test]
    fn emit_error_sends_error_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_error("something went wrong");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Error(data) => assert_eq!(data.message, "something went wrong"),
            other => panic!("Expected Error, got {:?}", other),
        }
    }

    #[test]
    fn emit_info_sends_tips_event() {
        let (sink, mut rx) = make_sink();
        sink.emit_info("operation completed");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::Tips(data) => {
                assert_eq!(data.content, "operation completed");
                assert_eq!(data.tip_type, TipType::Success);
            }
            other => panic!("Expected Tips, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_call_carries_input() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call("call_glob_1", "Glob", r#"{"pattern":"**/*.rs"}"#);
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.name, "Glob");
                assert_eq!(data.status, ToolCallStatus::Running);
                assert!(data.input.is_some());
                assert_eq!(data.input.unwrap()["pattern"], "**/*.rs");
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_result_carries_output_and_matching_call_id() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_call("call_glob_1", "Glob", r#"{"pattern":"**/*.rs"}"#);
        let start_event = rx.try_recv().unwrap();
        let start_call_id = match &start_event {
            AgentStreamEvent::ToolCall(data) => data.call_id.clone(),
            _ => panic!("Expected ToolCall"),
        };

        sink.emit_tool_result("call_glob_1", "Glob", false, "src/main.rs\nsrc/lib.rs");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert_eq!(data.name, "Glob");
                assert_eq!(data.status, ToolCallStatus::Completed);
                assert_eq!(data.call_id, start_call_id);
                assert_eq!(data.output.as_deref(), Some("src/main.rs\nsrc/lib.rs"));
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn emit_tool_result_empty_content_omits_output() {
        let (sink, mut rx) = make_sink();
        sink.emit_tool_result("call_glob_1", "Glob", false, "");
        let event = rx.try_recv().unwrap();
        match event {
            AgentStreamEvent::ToolCall(data) => {
                assert!(data.output.is_none());
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    #[test]
    fn no_panic_when_no_receivers() {
        let (tx, _) = broadcast::channel(16);
        let sink = BackendOutputSink::new(tx);
        sink.emit_text_delta("hello", "msg-1");
        sink.emit_thinking("thought", "msg-1");
        sink.emit_tool_call("call_read_1", "Read", "{}");
        sink.emit_tool_result("call_read_1", "Read", false, "ok");
        sink.emit_stream_start("msg-1");
        sink.emit_stream_end("msg-1", 1, 100, 50, 0, 0);
        sink.emit_error("err");
        sink.emit_info("info");
    }
}
