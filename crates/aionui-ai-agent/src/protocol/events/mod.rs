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
    AcpToolCallUpdateData, ToolCallEventData, ToolCallStatus, ToolGroupEntry, ToolGroupStatus,
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
    /// Structured question card (claude AskUserQuestion — `SessionEvent::Ask`).
    /// Its own frame, NOT an `AcpPermission`: asking is not authorizing
    /// (2026-08-04 spec). Payload: `{ session_id, request_id, questions }` where
    /// `questions` is the raw claude `questions[]` array — the cross-vendor shape
    /// (claude/qwen/grok all converged on it, 2026-08-04 captures):
    /// `[{question, header?, options:[{label, description?}], multiSelect?}]`.
    /// Answered via the confirm channel with the FULL per-question answer set;
    /// wire tag `ask`.
    Ask(serde_json::Value),
    SkillSuggest(SkillSuggestEventData),
    CronTrigger(CronTriggerEventData),
    AcpModelInfo(serde_json::Value),
    AcpModeInfo(serde_json::Value),
    AcpConfigOption(serde_json::Value),
    AcpSessionInfo(serde_json::Value),
    AcpContextUsage(serde_json::Value),
    /// Live snapshot of a client-hosted terminal (ACP `terminal/*`):
    /// `{terminal_id, command, output(cumulative), truncated, exit_status?}`.
    /// Emitted throttled while the delegated command runs, plus one final
    /// frame when it exits.
    AcpTerminalOutput(serde_json::Value),
    AcpPromptHookWarning(serde_json::Value),
    SlashCommandsUpdated(serde_json::Value),
    AvailableCommands(AvailableCommandsEventData),
    Finish(FinishEventData),
    Error(ErrorEventData),
    System(serde_json::Value),
    RequestTrace(serde_json::Value),
    SessionAssigned(SessionAssignedEventData),
    /// Intra-turn soft boundary between two independent assistant outputs.
    ///
    /// Unlike `Finish`, this does NOT terminate the turn or the relay: it only
    /// tells the relay to close the current text/thinking segment so the next
    /// batch of text starts a fresh message bubble. Emitted by the direct-CLI
    /// session pump when it suppresses a non-blocking Workflow's intermediate
    /// launch `result` (claude produces one launch output while subagents run
    /// and a separate completion output afterwards — two independent outputs
    /// under one turn). The relay consumes it internally and never forwards it
    /// to the WebSocket, so no frontend renderer is required.
    SegmentBreak,
    /// Internal-only: the backend's OWN id for the turn that just started
    /// (codex `turn/started` → `Turn.id`). The relay consumes it to stamp
    /// `messages.backend_turn_id` on every row it persists for this turn — the
    /// lookup key for `thread/fork`'s `lastTurnId` when forking mid-history.
    /// Never forwarded to the WebSocket; never user-visible output. Only the
    /// direct-CLI codex pump emits it.
    BackendTurnBound(String),
    /// Internal-only: one snapshot of a running workflow's progress.
    ///
    /// NOT forwarded to the WebSocket as-is. The relay projects it into the two
    /// frames the frontend ALREADY renders — a `tool_call` (the container row:
    /// headline in `description`, full phase tree in `output`) and a `tool_group`
    /// (one entry per workflow agent, each with its own status badge) — so no new
    /// frontend renderer is required.
    ///
    /// It exists as its own variant purely so the relay can special-case it:
    /// a plain `ToolCall`/`ToolGroup` makes the relay close the active text
    /// segment, and a workflow emits progress WHILE the assistant is streaming
    /// its "workflow started" reply (claude runs with `--include-partial-messages`,
    /// so that reply arrives as deltas). Reusing those variants would therefore
    /// shatter that reply into one bubble per progress frame. The relay's arm for
    /// this variant leaves `active_text`/`active_thinking` untouched.
    ///
    /// Never counts as user-visible turn output (see `event_is_user_visible_output`):
    /// it is an out-of-band status refresh, not the turn "saying something".
    WorkflowProgress(WorkflowProgressData),
    /// Internal-only: lifecycle echo for a user message we wrote to a direct
    /// CLI (claude `command_lifecycle` → `SessionEvent::MessageLifecycle`,
    /// verified 2.1.226 — mid-turn interjection design spec §6.1).
    /// `client_msg_id` is the uuid WE minted on the user frame, echoed back
    /// verbatim. Consumed by the conversation layer's BackgroundStreamWatcher
    /// to decide whether an agent-started follow-up turn SERVES a user message
    /// (claim it) or is a pure background continuation (leave it unclaimed);
    /// the per-turn relay consumes it silently. Never forwarded to the
    /// WebSocket; never counts as user-visible turn output.
    MessageLifecycle(MessageLifecycleData),
    /// Internal-only signal: the tolerant transport layer absorbed a CodeBuddy
    /// dialect notification (`session_end` / `compact-maxtoken`) that the stock
    /// ACP schema hard-rejects as `-32602`. Consumed by the empty-turn judgment
    /// within the turn/near-window; never counts as user-visible output and is
    /// never forwarded to the WebSocket. Mirrors `SegmentBreak`'s "relay consumes
    /// internally, never forwards" contract.
    AcpDialectSignal(AcpDialectSignalData),
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
    /// Stable identity for a tip that SUPERSEDES its predecessor: a later tip
    /// with the same key replaces the earlier one in place instead of being
    /// appended. Used by progress-style notices (codex retry attempts count up
    /// 1/5 → 2/5 → …) so the conversation shows one card, not five.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes_key: Option<String>,
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

/// Kind of CodeBuddy ACP dialect signal absorbed by the tolerant transport
/// layer before the stock ACP schema can hard-reject it as `-32602`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpDialectSignalKind {
    /// Non-standard `session_end` terminal marker (carries `stopReason:"end_turn"`).
    SessionEnd,
    /// Emergency auto-compaction / max-token pressure notification
    /// (`compact-maxtoken` message id or `codebuddy.ai/compactType` meta marker).
    TokenPressure,
}

/// Data for the internal-only [`AgentStreamEvent::WorkflowProgress`] event.
///
/// Carries both projections precomputed by the pump, so the relay stays a dumb
/// forwarder: it never has to know how a workflow renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowProgressData {
    /// The container row, keyed by the Workflow tool call's `call_id` so it
    /// updates the card the user already sees. Emitted as a `tool_call` frame.
    pub card: ToolCallEventData,
    /// One entry per workflow agent. Emitted as a `tool_group` frame.
    ///
    /// ALWAYS the FULL roster, never a delta: `persist_tool_group` replaces the
    /// whole row's content and keys it by the FIRST entry's `call_id`, so a
    /// partial list would erase the agents left out, and a reordered list would
    /// insert a second row.
    pub agents: Vec<ToolGroupEntry>,
    /// A status-only settle for a card THIS pump never opened — the CLI reported
    /// a task terminal after a resume/rebuild wiped the ledger (live 2026-08-04:
    /// an idle-killed conversation's load-gate bash finished its work, and the
    /// resumed session's `Interrupted` report found no card — the stored row
    /// spun forever). Consumers must apply it ONLY to an EXISTING stored row
    /// (update, never insert, never a fresh UI card): the same unknown-terminal
    /// shape also fires for workflow-INTERNAL refs that never had a row, and
    /// inserting for those would conjure junk cards.
    #[serde(default)]
    pub settle_only: bool,
}

/// Data for the internal-only [`AgentStreamEvent::MessageLifecycle`] event.
///
/// `phase` reuses the session layer's [`aionui_session::MessageLifecyclePhase`]
/// verbatim — the pump is a pass-through here. Re-exported below so consumers
/// of this event (the conversation layer's watcher) can match on the enum
/// without taking their own `aionui-session` dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageLifecycleData {
    /// The uuid we minted on the user frame, echoed back by the CLI.
    pub client_msg_id: String,
    pub phase: MessageLifecyclePhase,
}

pub use aionui_session::MessageLifecyclePhase;

/// Data for the internal-only [`AgentStreamEvent::AcpDialectSignal`] event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpDialectSignalData {
    pub kind: AcpDialectSignalKind,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
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
            supersedes_key: None,
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
            supersedes_key: None,
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
            parent_call_id: None,
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
            parent_call_id: Some("toolu_task".into()),
        });
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "tool_call");
        assert_eq!(json["data"]["input"]["pattern"], "**/*.rs");
        assert_eq!(json["data"]["output"], "src/main.rs\nsrc/lib.rs");
        assert_eq!(json["data"]["description"], "Search for Rust files");
        assert_eq!(json["data"]["parent_call_id"], "toolu_task");
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
            parent_call_id: None,
        });
        let json = serde_json::to_value(&event).unwrap();
        assert!(json["data"].get("input").is_none());
        assert!(json["data"].get("output").is_none());
        assert!(json["data"].get("description").is_none());
        // Absent, not null: a null would DELETE stored attribution under the
        // DB's merge-patch upsert.
        assert!(json["data"].get("parent_call_id").is_none());
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
                status: ToolGroupStatus::Success,
                description: Some("Read file".into()),
            },
            ToolGroupEntry {
                call_id: "c2".into(),
                name: "write".into(),
                status: ToolGroupStatus::Executing,
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
