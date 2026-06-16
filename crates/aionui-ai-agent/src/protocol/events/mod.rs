pub mod permission;
pub mod session_updates;
pub mod tool_call;
pub mod translate;

use serde::{Deserialize, Serialize};

pub use aionui_api_types::AgentStreamErrorData as ErrorEventData;

pub use permission::{
    AcpPermissionEventData, AcpPermissionOptionData, AcpPermissionOptionKind, AcpPermissionRequestData,
    AcpPermissionToolCall,
};
pub use session_updates::{
    AgentStatusEventData, AvailableCommandsEventData, CronTriggerEventData, PlanEventData, SkillSuggestEventData,
    ThinkingEventData,
};
pub use tool_call::{
    AcpToolCallContentItem, AcpToolCallEventData, AcpToolCallKind, AcpToolCallLocationItem,
    AcpToolCallSessionUpdateKind, AcpToolCallStatus, AcpToolCallTextBlock, AcpToolCallTextBlockType,
    AcpToolCallUpdateData, ToolCallEventData, ToolCallStatus, ToolGroupEntry,
};
pub(crate) use translate::{permission_request_to_event_data, session_notification_to_events};

/// Events emitted by an Agent during a message processing turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentStreamEvent {
    Start(StartEventData),
    #[serde(rename = "content")]
    Text(TextEventData),
    Tips(TipsEventData),
    ToolCall(ToolCallEventData),
    AcpToolCall(AcpToolCallEventData),
    ToolGroup(Vec<ToolGroupEntry>),
    AgentStatus(AgentStatusEventData),
    Thinking(ThinkingEventData),
    Plan(PlanEventData),
    Permission(serde_json::Value),
    AcpPermission(AcpPermissionEventData),
    SkillSuggest(SkillSuggestEventData),
    CronTrigger(CronTriggerEventData),
    AcpModelInfo(serde_json::Value),
    AcpModeInfo(serde_json::Value),
    AcpConfigOption(serde_json::Value),
    AcpSessionInfo(serde_json::Value),
    AcpContextUsage(serde_json::Value),
    AcpPromptHookWarning(serde_json::Value),
    SlashCommandsUpdated(serde_json::Value),
    AvailableCommands(AvailableCommandsEventData),
    Finish(FinishEventData),
    Error(ErrorEventData),
    System(serde_json::Value),
    RequestTrace(serde_json::Value),
    SessionAssigned(SessionAssignedEventData),
}

/// Data for the `Start` event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StartEventData {
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Data for the `SessionAssigned` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAssignedEventData {
    pub session_id: String,
}

/// Data for the `Text` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEventData {
    pub content: String,
}

/// Data for the `Tips` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TipsEventData {
    pub content: String,
    #[serde(rename = "type")]
    pub tip_type: TipType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// Severity level for a tip event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TipType {
    Error,
    Info,
    Success,
    Warning,
}

/// Data for the `Finish` event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FinishEventData {
    #[serde(default)]
    pub session_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::{
        PermissionOption, PermissionOptionKind as SdkPermissionOptionKind, RequestPermissionRequest,
        SessionNotification, SessionUpdate, ToolCall as SdkToolCall, ToolCallStatus as SdkToolCallStatus,
        ToolCallUpdate as SdkToolCallUpdate, ToolCallUpdateFields, ToolKind as SdkToolKind,
    };
    use serde_json::json;

    #[test]
    fn text_event_roundtrip() {
        let event = AgentStreamEvent::Text(TextEventData {
            content: "Hello world".into(),
        });
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "content");
        assert_eq!(json["data"]["content"], "Hello world");

        let parsed: AgentStreamEvent = serde_json::from_value(json).unwrap();
        if let AgentStreamEvent::Text(data) = parsed {
            assert_eq!(data.content, "Hello world");
        } else {
            panic!("Expected Text event");
        }
    }

    #[test]
    fn tips_event_roundtrip() {
        let event = AgentStreamEvent::Tips(TipsEventData {
            content: "Something went wrong".into(),
            tip_type: TipType::Error,
            code: None,
            params: None,
        });
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "tips");
        assert_eq!(json["data"]["type"], "error");
    }

    #[test]
    fn tips_event_roundtrip_preserves_info_code_and_params() {
        let event = AgentStreamEvent::Tips(TipsEventData {
            content: "Select a slash command to continue".into(),
            tip_type: TipType::Info,
            code: Some("acp.empty_turn.choose_command".into()),
            params: Some(json!({ "command_count": 3 })),
        });
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "tips");
        assert_eq!(json["data"]["type"], "info");
        assert_eq!(json["data"]["code"], "acp.empty_turn.choose_command");
        assert_eq!(json["data"]["params"]["command_count"], 3);

        let parsed: AgentStreamEvent = serde_json::from_value(json).unwrap();
        if let AgentStreamEvent::Tips(data) = parsed {
            assert_eq!(data.content, "Select a slash command to continue");
            assert_eq!(data.tip_type, TipType::Info);
            assert_eq!(data.code.as_deref(), Some("acp.empty_turn.choose_command"));
            assert_eq!(data.params, Some(json!({ "command_count": 3 })));
        } else {
            panic!("Expected Tips event");
        }
    }

    #[test]
    fn tool_call_event_roundtrip() {
        let event = AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "call-1".into(),
            name: "read_file".into(),
            args: json!({ "path": "/tmp/a.txt" }),
            status: ToolCallStatus::Running,
            input: None,
            output: None,
            description: None,
        });
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "tool_call");
        assert_eq!(json["data"]["call_id"], "call-1");
        assert_eq!(json["data"]["status"], "running");
    }

    #[test]
    fn tool_call_event_includes_enriched_fields() {
        let event = AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "call-1".into(),
            name: "Glob".into(),
            args: json!({}),
            status: ToolCallStatus::Completed,
            input: Some(json!({ "pattern": "**/*.rs" })),
            output: Some("src/main.rs\nsrc/lib.rs".into()),
            description: Some("Search for Rust files".into()),
        });
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "tool_call");
        assert_eq!(json["data"]["input"]["pattern"], "**/*.rs");
        assert_eq!(json["data"]["output"], "src/main.rs\nsrc/lib.rs");
        assert_eq!(json["data"]["description"], "Search for Rust files");
    }

    #[test]
    fn tool_call_event_omits_none_fields() {
        let event = AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "call-1".into(),
            name: "Glob".into(),
            args: json!({}),
            status: ToolCallStatus::Running,
            input: None,
            output: None,
            description: None,
        });
        let json = serde_json::to_value(&event).unwrap();
        assert!(json["data"].get("input").is_none());
        assert!(json["data"].get("output").is_none());
        assert!(json["data"].get("description").is_none());
    }

    #[test]
    fn finish_event_roundtrip() {
        let event = AgentStreamEvent::Finish(FinishEventData {
            session_id: Some("sess-abc".into()),
        });
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "finish");
        assert_eq!(json["data"]["session_id"], "sess-abc");
    }

    #[test]
    fn error_event_roundtrip() {
        let event = AgentStreamEvent::Error(ErrorEventData::legacy("timeout", None));
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["data"]["message"], "timeout");
    }

    #[test]
    fn start_event_default_session_id() {
        let event = AgentStreamEvent::Start(StartEventData::default());
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "start");
        assert_eq!(json["data"]["session_id"], serde_json::Value::Null);
    }

    #[test]
    fn tool_group_event_roundtrip() {
        let entries = vec![
            ToolGroupEntry {
                call_id: "c1".into(),
                name: "read".into(),
                status: ToolCallStatus::Completed,
                description: Some("Read file".into()),
            },
            ToolGroupEntry {
                call_id: "c2".into(),
                name: "write".into(),
                status: ToolCallStatus::Running,
                description: None,
            },
        ];
        let event = AgentStreamEvent::ToolGroup(entries);
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "tool_group");
        let data = json["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["call_id"], "c1");
    }

    #[test]
    fn agent_status_event_roundtrip() {
        let event = AgentStreamEvent::AgentStatus(AgentStatusEventData {
            backend: "claude".into(),
            status: "running".into(),
            agent_name: Some("default".into()),
            session_id: None,
        });
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "agent_status");
        assert_eq!(json["data"]["backend"], "claude");
    }

    #[test]
    fn session_tool_call_maps_to_acp_tool_call_event() {
        let notif = SessionNotification::new(
            "sess-1",
            SessionUpdate::ToolCall(
                SdkToolCall::new("tool-1", "Terminal")
                    .kind(SdkToolKind::Execute)
                    .status(SdkToolCallStatus::Pending)
                    .raw_input(json!({ "command": "echo hi" })),
            ),
        );

        let events = session_notification_to_events(&notif);
        assert_eq!(events.len(), 1);
        let json = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(json["type"], "acp_tool_call");
        assert_eq!(json["data"]["session_id"], "sess-1");
        assert_eq!(json["data"]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(json["data"]["update"]["tool_call_id"], "tool-1");
        assert_eq!(json["data"]["update"]["title"], "Terminal");
        assert_eq!(json["data"]["update"]["kind"], "execute");
        assert_eq!(json["data"]["update"]["rawInput"]["command"], "echo hi");
    }

    #[test]
    fn session_tool_call_update_omits_missing_fields_for_frontend_merge() {
        let notif = SessionNotification::new(
            "sess-1",
            SessionUpdate::ToolCallUpdate(SdkToolCallUpdate::new(
                "tool-1",
                ToolCallUpdateFields::new().status(SdkToolCallStatus::Completed),
            )),
        );

        let events = session_notification_to_events(&notif);
        assert_eq!(events.len(), 1);
        let json = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(json["type"], "acp_tool_call");
        assert_eq!(json["data"]["update"]["sessionUpdate"], "tool_call_update");
        assert_eq!(json["data"]["update"]["tool_call_id"], "tool-1");
        assert_eq!(json["data"]["update"]["status"], "completed");
        assert!(json["data"]["update"].get("title").is_none());
        assert!(json["data"]["update"].get("rawInput").is_none());
    }

    #[test]
    fn codex_image_tool_update_omits_base64_result() {
        let large_png_base64 = format!("iVBORw0KGgo{}", "A".repeat(128 * 1024));
        let notif = SessionNotification::new(
            "sess-1",
            SessionUpdate::ToolCallUpdate(SdkToolCallUpdate::new(
                "ig_test_image",
                ToolCallUpdateFields::new()
                    .status(SdkToolCallStatus::InProgress)
                    .raw_output(json!({
                        "call_id": "ig_test_image",
                        "status": "generating",
                        "saved_path": "/Users/test/.codex/generated_images/session/ig_test_image.png",
                        "revised_prompt": "一只小猫",
                        "result": large_png_base64
                    })),
            )),
        );

        let events = session_notification_to_events(&notif);
        assert_eq!(events.len(), 1);
        let json = serde_json::to_value(&events[0]).unwrap();
        let raw_output = &json["data"]["update"]["rawOutput"];

        assert_eq!(
            raw_output["saved_path"],
            "/Users/test/.codex/generated_images/session/ig_test_image.png"
        );
        assert_eq!(
            raw_output["image"]["path"],
            "/Users/test/.codex/generated_images/session/ig_test_image.png"
        );
        assert_eq!(raw_output["result_omitted"], true);
        assert!(raw_output.get("result").is_none());
    }

    #[test]
    fn codex_image_tool_update_with_saved_path_is_completed() {
        let notif = SessionNotification::new(
            "sess-1",
            SessionUpdate::ToolCallUpdate(SdkToolCallUpdate::new(
                "ig_done_image",
                ToolCallUpdateFields::new()
                    .status(SdkToolCallStatus::InProgress)
                    .raw_output(json!({
                        "call_id": "ig_done_image",
                        "status": "generating",
                        "saved_path": "/Users/test/.codex/generated_images/session/ig_done_image.png",
                        "result": format!("iVBORw0KGgo{}", "A".repeat(128 * 1024))
                    })),
            )),
        );

        let events = session_notification_to_events(&notif);
        assert_eq!(events.len(), 1);
        let json = serde_json::to_value(&events[0]).unwrap();

        assert_eq!(json["data"]["update"]["status"], "completed");
        assert_eq!(json["data"]["update"]["rawOutput"]["status"], "completed");
    }

    #[test]
    fn codex_image_tool_update_keeps_small_text_result_in_progress() {
        let notif = SessionNotification::new(
            "sess-1",
            SessionUpdate::ToolCallUpdate(SdkToolCallUpdate::new(
                "small-result",
                ToolCallUpdateFields::new()
                    .status(SdkToolCallStatus::InProgress)
                    .raw_output(json!({
                        "status": "generating",
                        "saved_path": "/tmp/result.txt",
                        "result": "not an inline image"
                    })),
            )),
        );

        let events = session_notification_to_events(&notif);
        let json = serde_json::to_value(&events[0]).unwrap();
        let raw_output = &json["data"]["update"]["rawOutput"];

        assert_eq!(json["data"]["update"]["status"], "in_progress");
        assert_eq!(raw_output["result"], "not an inline image");
        assert!(raw_output.get("image").is_none());
        assert!(raw_output.get("result_omitted").is_none());
    }

    #[test]
    fn codex_image_tool_update_detects_image_mime_types_from_saved_path() {
        let cases = [
            ("photo.jpg", "image/jpeg", "/9j/"),
            ("photo.webp", "image/webp", "UklGR"),
            ("photo.gif", "image/gif", "data:image/gif;base64,"),
            ("photo.bin", "image/png", "iVBORw0KGgo"),
        ];

        for (file_name, expected_mime, prefix) in cases {
            let saved_path = format!("/Users/test/.codex/generated_images/session/{file_name}");
            let notif = SessionNotification::new(
                "sess-1",
                SessionUpdate::ToolCallUpdate(SdkToolCallUpdate::new(
                    "ig_mime",
                    ToolCallUpdateFields::new().raw_output(json!({
                        "saved_path": saved_path,
                        "result": format!("{prefix}{}", "A".repeat(128 * 1024))
                    })),
                )),
            );

            let events = session_notification_to_events(&notif);
            let json = serde_json::to_value(&events[0]).unwrap();

            assert_eq!(json["data"]["update"]["rawOutput"]["image"]["mime_type"], expected_mime);
            assert!(json["data"]["update"]["rawOutput"].get("result").is_none());
        }
    }

    #[test]
    fn codex_image_tool_update_omits_base64_without_saved_path() {
        let large_png_base64 = format!("iVBORw0KGgo{}", "A".repeat(128 * 1024));
        let notif = SessionNotification::new(
            "sess-1",
            SessionUpdate::ToolCallUpdate(SdkToolCallUpdate::new(
                "ig_no_path",
                ToolCallUpdateFields::new()
                    .status(SdkToolCallStatus::InProgress)
                    .raw_output(json!({
                        "call_id": "ig_no_path",
                        "status": "generating",
                        "result": large_png_base64
                    })),
            )),
        );

        let events = session_notification_to_events(&notif);
        assert_eq!(events.len(), 1);
        let json = serde_json::to_value(&events[0]).unwrap();
        let raw_output = &json["data"]["update"]["rawOutput"];

        // Oversized base64 must be stripped even though Codex did not save the file.
        assert!(raw_output.get("result").is_none());
        assert_eq!(raw_output["result_omitted"], true);
        // No saved_path means we cannot offer a path-based preview, so no image object.
        assert!(raw_output.get("image").is_none());
        // Without a saved image the status must pass through unchanged.
        assert_eq!(json["data"]["update"]["status"], "in_progress");
    }

    #[test]
    fn codex_image_tool_update_preserves_failed_status() {
        let notif = SessionNotification::new(
            "sess-1",
            SessionUpdate::ToolCallUpdate(SdkToolCallUpdate::new(
                "ig_failed_image",
                ToolCallUpdateFields::new()
                    .status(SdkToolCallStatus::Failed)
                    .raw_output(json!({
                        "call_id": "ig_failed_image",
                        "status": "failed",
                        "saved_path": "/Users/test/.codex/generated_images/session/ig_failed_image.png",
                        "result": format!("iVBORw0KGgo{}", "A".repeat(128 * 1024))
                    })),
            )),
        );

        let events = session_notification_to_events(&notif);
        assert_eq!(events.len(), 1);
        let json = serde_json::to_value(&events[0]).unwrap();

        // A terminal `failed` status must never be rewritten to `completed`.
        assert_eq!(json["data"]["update"]["status"], "failed");
        assert_eq!(json["data"]["update"]["rawOutput"]["status"], "failed");
        // The base64 payload is still stripped regardless of the failure.
        assert!(json["data"]["update"]["rawOutput"].get("result").is_none());
    }

    #[test]
    fn codex_image_tool_update_completes_saved_image_path_without_inline_result() {
        let notif = SessionNotification::new(
            "sess-1",
            SessionUpdate::ToolCallUpdate(SdkToolCallUpdate::new(
                "ig_path_only",
                ToolCallUpdateFields::new()
                    .status(SdkToolCallStatus::InProgress)
                    .raw_output(json!({
                        "call_id": "ig_path_only",
                        "status": "generating",
                        "saved_path": "/Users/test/.codex/generated_images/session/ig_path_only.png"
                    })),
            )),
        );

        let events = session_notification_to_events(&notif);
        assert_eq!(events.len(), 1);
        let json = serde_json::to_value(&events[0]).unwrap();

        assert_eq!(json["data"]["update"]["status"], "completed");
        assert_eq!(json["data"]["update"]["rawOutput"]["status"], "completed");
        assert_eq!(
            json["data"]["update"]["rawOutput"]["image"]["path"],
            "/Users/test/.codex/generated_images/session/ig_path_only.png"
        );
    }

    #[test]
    fn codex_tool_update_keeps_non_image_saved_path_in_progress() {
        let notif = SessionNotification::new(
            "sess-1",
            SessionUpdate::ToolCallUpdate(SdkToolCallUpdate::new(
                "text_result_path",
                ToolCallUpdateFields::new()
                    .status(SdkToolCallStatus::InProgress)
                    .raw_output(json!({
                        "call_id": "text_result_path",
                        "status": "generating",
                        "saved_path": "/tmp/result.txt"
                    })),
            )),
        );

        let events = session_notification_to_events(&notif);
        assert_eq!(events.len(), 1);
        let json = serde_json::to_value(&events[0]).unwrap();

        assert_eq!(json["data"]["update"]["status"], "in_progress");
        assert_eq!(json["data"]["update"]["rawOutput"]["status"], "generating");
        assert!(json["data"]["update"]["rawOutput"].get("image").is_none());
    }

    #[test]
    fn permission_request_maps_to_snake_case_event_data() {
        let request = RequestPermissionRequest::new(
            "sess-1",
            SdkToolCallUpdate::new(
                "tool-1",
                ToolCallUpdateFields::new()
                    .title("Write file")
                    .kind(SdkToolKind::Edit)
                    .raw_input(json!({ "file_path": "/tmp/a.txt" })),
            ),
            vec![
                PermissionOption::new("allow", "Allow", SdkPermissionOptionKind::AllowOnce),
                PermissionOption::new("reject", "Reject", SdkPermissionOptionKind::RejectOnce),
            ],
        );

        let event = AgentStreamEvent::AcpPermission(permission_request_to_event_data(&request));
        let json = serde_json::to_value(&event).unwrap();

        assert_eq!(json["type"], "acp_permission");
        assert_eq!(json["data"]["session_id"], "sess-1");
        assert_eq!(json["data"]["tool_call"]["tool_call_id"], "tool-1");
        assert_eq!(json["data"]["tool_call"]["raw_input"]["file_path"], "/tmp/a.txt");
        assert_eq!(json["data"]["options"][0]["option_id"], "allow");
        assert_eq!(json["data"]["options"][0]["kind"], "allow_once");
        assert!(json["data"].get("toolCall").is_none());
        assert!(json["data"]["options"][0].get("optionId").is_none());
    }

    #[test]
    fn thinking_event_roundtrip() {
        let event = AgentStreamEvent::Thinking(ThinkingEventData {
            content: "Analyzing...".into(),
            subject: Some("code review".into()),
            duration: Some(1500),
            status: Some("in_progress".into()),
        });
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "thinking");
        assert_eq!(json["data"]["duration"], 1500);
    }
}
