//! Host-side observation pipeline for tool events emitted by agent backends.
//! `tools/pre-execute` → `tools/execute` → `tools/post-execute`.
//!
//! Agents still execute tools in their own process. This pipeline observes and
//! projects their events; it is not an authoritative execution interceptor:
//! - pre-execute: classify permission / in-flight tool frames when visible
//! - post-execute: spill oversized output, or bound it if spill fails
//!
//! Authoritative policy gates belong in native executors. Future audit hooks
//! can attach here without forking retain/bound logic in the relay.

use aionui_ai_agent::AgentStreamEvent;
use aionui_ai_agent::protocol::events::tool_call::{AcpToolCallStatus, ToolCallStatus};
use serde_json::json;

use crate::stream_persistence::OutputRetentionPolicy;

const INLINE_BOUND_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolPreExecuteDisposition {
    Skipped,
    NeedsApproval,
    InFlight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolPipelineDisposition {
    Skipped,
    Inline,
    Spilled,
    Truncated,
}

pub(crate) struct ToolEventPipeline<'a> {
    retention: Option<&'a OutputRetentionPolicy>,
}

impl<'a> ToolEventPipeline<'a> {
    pub(crate) fn new(retention: Option<&'a OutputRetentionPolicy>) -> Self {
        Self { retention }
    }

    /// Pre-execute host gate. Classification only; agents still run the tool.
    pub(crate) fn pre_execute(&self, event: &AgentStreamEvent) -> ToolPreExecuteDisposition {
        match event {
            AgentStreamEvent::Permission(_) | AgentStreamEvent::AcpPermission(_) => {
                ToolPreExecuteDisposition::NeedsApproval
            }
            AgentStreamEvent::ToolCall(data) if data.status == ToolCallStatus::Running => {
                ToolPreExecuteDisposition::InFlight
            }
            AgentStreamEvent::AcpToolCall(data)
                if matches!(
                    data.update.status,
                    Some(AcpToolCallStatus::Pending | AcpToolCallStatus::InProgress)
                ) =>
            {
                ToolPreExecuteDisposition::InFlight
            }
            _ => ToolPreExecuteDisposition::Skipped,
        }
    }

    /// Post-execute host gate. Must run before the event is journaled.
    pub(crate) async fn post_execute(
        &self,
        user_id: &str,
        conversation_id: &str,
        event: &mut AgentStreamEvent,
    ) -> Result<ToolPipelineDisposition, std::io::Error> {
        if !is_tool_result_event(event) {
            return Ok(ToolPipelineDisposition::Skipped);
        }
        let Some(policy) = self.retention else {
            if bound_large_tool_output(event) {
                return Ok(ToolPipelineDisposition::Truncated);
            }
            return Ok(ToolPipelineDisposition::Inline);
        };
        match retain_large_tool_output(policy, user_id, conversation_id, event).await {
            Ok(true) => Ok(ToolPipelineDisposition::Spilled),
            Ok(false) => Ok(ToolPipelineDisposition::Inline),
            Err(error) => {
                bound_large_tool_output(event);
                Err(error)
            }
        }
    }
}

fn is_tool_result_event(event: &AgentStreamEvent) -> bool {
    matches!(
        event,
        AgentStreamEvent::ToolCall(_) | AgentStreamEvent::AcpToolCall(_) | AgentStreamEvent::ToolGroup(_)
    )
}

async fn retain_large_tool_output(
    policy: &OutputRetentionPolicy,
    user_id: &str,
    conversation_id: &str,
    event: &mut AgentStreamEvent,
) -> Result<bool, std::io::Error> {
    async fn governed(
        policy: &OutputRetentionPolicy,
        user_id: &str,
        conversation_id: &str,
        value: &str,
    ) -> Result<Option<String>, std::io::Error> {
        Ok(policy.retain(user_id, conversation_id, value).await?.map(|retained| {
            json!({
                "preview": retained.preview,
                "size": retained.size,
                "sha256": retained.sha256,
                "reference": retained.reference,
            })
            .to_string()
        }))
    }

    let mut spilled = false;
    match event {
        AgentStreamEvent::ToolCall(data) => {
            if let Some(output) = data.output.as_mut()
                && let Some(replacement) = governed(policy, user_id, conversation_id, output).await?
            {
                *output = replacement;
                spilled = true;
            }
        }
        AgentStreamEvent::AcpToolCall(data) => {
            if let Some(raw_output) = data.update.raw_output.as_mut() {
                let serialized = serde_json::to_string(raw_output).unwrap_or_default();
                if let Some(replacement) = governed(policy, user_id, conversation_id, &serialized).await? {
                    *raw_output = serde_json::from_str(&replacement).unwrap_or(serde_json::Value::Null);
                    spilled = true;
                }
            }
            if let Some(content) = data.update.content.as_mut() {
                for item in content {
                    match item {
                        aionui_ai_agent::protocol::events::tool_call::AcpToolCallContentItem::Content { content } => {
                            if let Some(replacement) = governed(policy, user_id, conversation_id, &content.text).await?
                            {
                                content.text = replacement;
                                spilled = true;
                            }
                        }
                        aionui_ai_agent::protocol::events::tool_call::AcpToolCallContentItem::Diff {
                            old_text,
                            new_text,
                            ..
                        } => {
                            if let Some(value) = old_text.as_mut()
                                && let Some(replacement) = governed(policy, user_id, conversation_id, value).await?
                            {
                                *value = replacement;
                                spilled = true;
                            }
                            if let Some(replacement) = governed(policy, user_id, conversation_id, new_text).await? {
                                *new_text = replacement;
                                spilled = true;
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
    Ok(spilled)
}

fn bound_large_tool_output(event: &mut AgentStreamEvent) -> bool {
    fn preview(value: &str) -> (String, bool) {
        if value.len() <= INLINE_BOUND_BYTES {
            return (value.to_owned(), false);
        }
        let end = value.floor_char_boundary(INLINE_BOUND_BYTES);
        (
            format!("{}\n[output truncated: spill unavailable]", &value[..end]),
            true,
        )
    }

    let mut truncated = false;
    match event {
        AgentStreamEvent::ToolCall(data) => {
            if let Some(output) = data.output.as_mut() {
                let (next, did_bound) = preview(output);
                *output = next;
                truncated |= did_bound;
            }
        }
        AgentStreamEvent::AcpToolCall(data) => {
            if let Some(raw_output) = data.update.raw_output.as_mut() {
                let serialized = serde_json::to_string(raw_output).unwrap_or_default();
                if serialized.len() > INLINE_BOUND_BYTES {
                    let (next, _) = preview(&serialized);
                    *raw_output = json!({ "preview": next, "spill_error": true });
                    truncated = true;
                }
            }
            if let Some(content) = data.update.content.as_mut() {
                for item in content {
                    match item {
                        aionui_ai_agent::protocol::events::tool_call::AcpToolCallContentItem::Content { content } => {
                            let (next, did_bound) = preview(&content.text);
                            content.text = next;
                            truncated |= did_bound;
                        }
                        aionui_ai_agent::protocol::events::tool_call::AcpToolCallContentItem::Diff {
                            old_text,
                            new_text,
                            ..
                        } => {
                            if let Some(value) = old_text.as_mut() {
                                let (next, did_bound) = preview(value);
                                *value = next;
                                truncated |= did_bound;
                            }
                            let (next, did_bound) = preview(new_text);
                            *new_text = next;
                            truncated |= did_bound;
                        }
                    }
                }
            }
        }
        _ => {}
    }
    truncated
}

#[cfg(test)]
mod tests {
    use aionui_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

    use super::*;

    fn tool_event(output: impl Into<String>) -> AgentStreamEvent {
        AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "call-1".into(),
            name: "Bash".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            input: None,
            output: Some(output.into()),
            description: None,
        })
    }

    #[tokio::test]
    async fn small_tool_output_stays_inline() {
        let root = tempfile::tempdir().unwrap();
        let policy = OutputRetentionPolicy::with_preview_bytes(root.path().to_path_buf(), 32);
        let pipeline = ToolEventPipeline::new(Some(&policy));
        let mut event = tool_event("hello");
        let disposition = pipeline.post_execute("user", "conv", &mut event).await.unwrap();
        assert_eq!(disposition, ToolPipelineDisposition::Inline);
        match event {
            AgentStreamEvent::ToolCall(data) => assert_eq!(data.output.as_deref(), Some("hello")),
            _ => panic!("expected tool call"),
        }
    }

    #[tokio::test]
    async fn large_tool_output_is_spilled_before_journal() {
        let root = tempfile::tempdir().unwrap();
        let policy = OutputRetentionPolicy::with_preview_bytes(root.path().to_path_buf(), 8);
        let pipeline = ToolEventPipeline::new(Some(&policy));
        let mut event = tool_event("0123456789abcdef");
        let disposition = pipeline.post_execute("user", "conv", &mut event).await.unwrap();
        assert_eq!(disposition, ToolPipelineDisposition::Spilled);
        match event {
            AgentStreamEvent::ToolCall(data) => {
                let output = data.output.expect("output");
                let envelope: serde_json::Value = serde_json::from_str(&output).unwrap();
                assert_eq!(envelope["preview"], "01234567");
                assert!(envelope["reference"].as_str().unwrap().starts_with("v1_"));
            }
            _ => panic!("expected tool call"),
        }
    }

    #[test]
    fn permission_frames_need_approval_before_execute() {
        let pipeline = ToolEventPipeline::new(None);
        let event = AgentStreamEvent::Permission(serde_json::json!({"call_id":"p1"}));
        assert_eq!(pipeline.pre_execute(&event), ToolPreExecuteDisposition::NeedsApproval);
    }

    #[test]
    fn running_tool_calls_are_in_flight_before_execute() {
        let pipeline = ToolEventPipeline::new(None);
        let mut event = tool_event("partial");
        if let AgentStreamEvent::ToolCall(data) = &mut event {
            data.status = ToolCallStatus::Running;
        }
        assert_eq!(pipeline.pre_execute(&event), ToolPreExecuteDisposition::InFlight);
    }

    #[test]
    fn completed_tool_results_skip_pre_execute() {
        let pipeline = ToolEventPipeline::new(None);
        let event = tool_event("done");
        assert_eq!(pipeline.pre_execute(&event), ToolPreExecuteDisposition::Skipped);
    }

    #[tokio::test]
    async fn non_tool_events_skip_the_pipeline() {
        let pipeline = ToolEventPipeline::new(None);
        let mut event =
            AgentStreamEvent::Text(aionui_ai_agent::protocol::events::TextEventData { content: "hi".into() });
        let disposition = pipeline.post_execute("user", "conv", &mut event).await.unwrap();
        assert_eq!(disposition, ToolPipelineDisposition::Skipped);
    }

    #[tokio::test]
    async fn missing_retention_policy_bounds_oversized_output() {
        let pipeline = ToolEventPipeline::new(None);
        let mut event = tool_event("x".repeat(INLINE_BOUND_BYTES + 8));
        let disposition = pipeline.post_execute("user", "conv", &mut event).await.unwrap();
        assert_eq!(disposition, ToolPipelineDisposition::Truncated);
        match event {
            AgentStreamEvent::ToolCall(data) => {
                let output = data.output.expect("output");
                assert!(output.contains("[output truncated: spill unavailable]"));
                assert!(output.len() < INLINE_BOUND_BYTES + 80);
            }
            _ => panic!("expected tool call"),
        }
    }

    #[tokio::test]
    async fn spill_io_failure_still_forwards_bounded_output() {
        let blocker = tempfile::NamedTempFile::new().unwrap();
        let policy = OutputRetentionPolicy::with_preview_bytes(blocker.path().to_path_buf(), 8);
        let pipeline = ToolEventPipeline::new(Some(&policy));
        let mut event = tool_event("y".repeat(INLINE_BOUND_BYTES + 8));
        pipeline
            .post_execute("user", "conv", &mut event)
            .await
            .expect_err("spill should fail when the retention root is a file");
        match event {
            AgentStreamEvent::ToolCall(data) => {
                let output = data.output.expect("output");
                assert!(output.contains("[output truncated: spill unavailable]"));
            }
            _ => panic!("expected tool call"),
        }
    }
}
