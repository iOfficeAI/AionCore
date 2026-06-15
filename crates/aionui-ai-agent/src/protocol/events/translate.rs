use agent_client_protocol::schema::{
    ContentBlock, PermissionOption, PermissionOptionKind as SdkPermissionOptionKind, RequestPermissionRequest,
    SessionNotification, SessionUpdate, ToolCallContent as SdkToolCallContent, ToolCallLocation as SdkToolCallLocation,
    ToolCallStatus as SdkToolCallStatus, ToolCallUpdate as SdkToolCallUpdate, ToolKind as SdkToolKind,
};
use tracing::debug;

use super::permission::{
    AcpPermissionEventData, AcpPermissionOptionData, AcpPermissionOptionKind, AcpPermissionRequestData,
    AcpPermissionToolCall,
};
use super::session_updates::{AvailableCommandsEventData, PlanEventData, ThinkingEventData};
use super::tool_call::{
    AcpToolCallContentItem, AcpToolCallEventData, AcpToolCallKind, AcpToolCallLocationItem,
    AcpToolCallSessionUpdateKind, AcpToolCallStatus, AcpToolCallTextBlock, AcpToolCallTextBlockType,
    AcpToolCallUpdateData,
};
use super::{AgentStreamEvent, TextEventData};

/// Convert an SDK [`SessionNotification`] into zero or more [`AgentStreamEvent`]s.
pub(crate) fn session_notification_to_events(notif: &SessionNotification) -> Vec<AgentStreamEvent> {
    let session_id = notif.session_id.to_string();
    let mut events = Vec::new();

    match &notif.update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            if let ContentBlock::Text(text) = &chunk.content {
                events.push(AgentStreamEvent::Text(TextEventData {
                    content: text.text.clone(),
                }));
            }
        }

        SessionUpdate::AgentThoughtChunk(chunk) => {
            if let ContentBlock::Text(text) = &chunk.content {
                events.push(AgentStreamEvent::Thinking(ThinkingEventData {
                    content: text.text.clone(),
                    subject: None,
                    duration: None,
                    status: Some("in_progress".into()),
                }));
            }
        }

        SessionUpdate::UserMessageChunk(_chunk) => {}

        SessionUpdate::ToolCall(tc) => {
            events.push(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
                session_id,
                update: AcpToolCallUpdateData {
                    session_update: AcpToolCallSessionUpdateKind::ToolCall,
                    tool_call_id: tc.tool_call_id.to_string(),
                    status: Some(map_sdk_tool_status(&tc.status)),
                    title: Some(tc.title.clone()),
                    kind: Some(map_sdk_tool_kind(&tc.kind)),
                    raw_input: tc.raw_input.clone(),
                    raw_output: None,
                    content: map_tool_call_content(&tc.content),
                    locations: map_tool_call_locations(&tc.locations),
                },
                meta: tc.meta.clone(),
            }));
        }

        SessionUpdate::ToolCallUpdate(tcu) => {
            let mut raw_output = sanitize_raw_output(tcu.fields.raw_output.clone());
            let status = normalize_tool_status(tcu.fields.status.as_ref(), raw_output.as_ref());
            normalize_raw_output_status(&mut raw_output, status.as_ref());

            events.push(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
                session_id,
                update: AcpToolCallUpdateData {
                    session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
                    tool_call_id: tcu.tool_call_id.to_string(),
                    status,
                    title: tcu.fields.title.clone(),
                    kind: tcu.fields.kind.as_ref().map(map_sdk_tool_kind),
                    raw_input: tcu.fields.raw_input.clone(),
                    raw_output,
                    content: tcu
                        .fields
                        .content
                        .as_ref()
                        .and_then(|content| map_tool_call_content(content)),
                    locations: tcu
                        .fields
                        .locations
                        .as_ref()
                        .and_then(|locations| map_tool_call_locations(locations)),
                },
                meta: tcu.meta.clone(),
            }));
        }

        SessionUpdate::Plan(plan) => {
            let entries: Vec<serde_json::Value> = plan
                .entries
                .iter()
                .map(|e| serde_json::to_value(e).unwrap_or_default())
                .collect();

            events.push(AgentStreamEvent::Plan(PlanEventData {
                session_id: Some(session_id),
                entries,
            }));
        }

        SessionUpdate::AvailableCommandsUpdate(update) => {
            events.push(AgentStreamEvent::AvailableCommands(AvailableCommandsEventData {
                commands: update.available_commands.clone(),
            }));
        }

        SessionUpdate::CurrentModeUpdate(update) => {
            events.push(AgentStreamEvent::AcpModeInfo(
                serde_json::to_value(update).unwrap_or_default(),
            ));
        }

        SessionUpdate::ConfigOptionUpdate(update) => {
            events.push(AgentStreamEvent::AcpConfigOption(
                serde_json::to_value(update).unwrap_or_default(),
            ));
        }

        SessionUpdate::SessionInfoUpdate(update) => {
            events.push(AgentStreamEvent::AcpSessionInfo(
                serde_json::to_value(update).unwrap_or_default(),
            ));
        }

        SessionUpdate::UsageUpdate(update) => {
            events.push(AgentStreamEvent::AcpContextUsage(
                serde_json::to_value(update).unwrap_or_default(),
            ));
        }
        _ => {
            debug!("Unknown SessionUpdate variant received, skipping");
        }
    }

    events
}

pub(crate) fn permission_request_to_event_data(request: &RequestPermissionRequest) -> AcpPermissionEventData {
    AcpPermissionEventData::Request(AcpPermissionRequestData {
        session_id: request.session_id.to_string(),
        tool_call: map_permission_tool_call(&request.tool_call),
        options: request.options.iter().map(map_permission_option).collect(),
        meta: request.meta.clone(),
    })
}

fn map_sdk_tool_status(sdk: &SdkToolCallStatus) -> AcpToolCallStatus {
    match sdk {
        SdkToolCallStatus::Pending => AcpToolCallStatus::Pending,
        SdkToolCallStatus::InProgress => AcpToolCallStatus::InProgress,
        SdkToolCallStatus::Completed => AcpToolCallStatus::Completed,
        SdkToolCallStatus::Failed => AcpToolCallStatus::Failed,
        _ => AcpToolCallStatus::Pending,
    }
}

const ACP_RAW_OUTPUT_INLINE_IMAGE_LIMIT: usize = 64 * 1024;

fn sanitize_raw_output(raw_output: Option<serde_json::Value>) -> Option<serde_json::Value> {
    let mut value = raw_output?;
    sanitize_inline_image_result(&mut value);
    Some(value)
}

fn sanitize_inline_image_result(value: &mut serde_json::Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    let saved_path = obj.get("saved_path").and_then(|v| v.as_str()).map(str::to_owned);
    // Strip any oversized inline-image `result` regardless of whether the image was
    // saved to disk. Older codex versions and interrupted/failed generations may emit
    // the multi-MB base64 without a `saved_path`; that payload must never reach the
    // WebSocket broadcast or SQLite either. `saved_path` only decides whether we attach
    // the structured `image { path, mime_type, source }` object below.
    let should_omit = obj
        .get("result")
        .and_then(|v| v.as_str())
        .map(is_probably_inline_image_result)
        .unwrap_or(false);

    if !should_omit {
        return;
    }

    let result_len = obj.get("result").and_then(|v| v.as_str()).map(str::len).unwrap_or(0);
    obj.remove("result");
    obj.insert("result_omitted".to_owned(), serde_json::Value::Bool(true));
    obj.insert(
        "result_omitted_reason".to_owned(),
        serde_json::Value::String("image_base64".to_owned()),
    );
    obj.insert(
        "result_bytes".to_owned(),
        serde_json::Value::Number(serde_json::Number::from(result_len)),
    );

    if let Some(path) = saved_path {
        let mime_type = mime_type_from_image_path(&path);
        obj.insert(
            "image".to_owned(),
            serde_json::json!({
                "path": path,
                "mime_type": mime_type,
                "source": "codex_image_generation"
            }),
        );
    }
}

fn is_probably_inline_image_result(value: &str) -> bool {
    value.len() > ACP_RAW_OUTPUT_INLINE_IMAGE_LIMIT
        && (value.starts_with("iVBORw0KGgo")
            || value.starts_with("/9j/")
            || value.starts_with("UklGR")
            || value.starts_with("data:image/"))
}

fn mime_type_from_image_path(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else {
        "image/png"
    }
}

fn normalize_tool_status(
    sdk_status: Option<&SdkToolCallStatus>,
    raw_output: Option<&serde_json::Value>,
) -> Option<AcpToolCallStatus> {
    let image_saved = raw_output
        .and_then(|v| v.get("image"))
        .and_then(|v| v.get("path"))
        .and_then(|v| v.as_str())
        .is_some();

    // Only force `completed` when the image is on disk AND the agent did not already
    // report a terminal status. Codex stalls by leaving the final event as
    // `generating`/`in_progress`, but a genuine `failed` must be preserved as-is.
    match (image_saved, sdk_status.map(map_sdk_tool_status)) {
        (true, None | Some(AcpToolCallStatus::Pending | AcpToolCallStatus::InProgress)) => {
            Some(AcpToolCallStatus::Completed)
        }
        (_, status) => status,
    }
}

fn normalize_raw_output_status(raw_output: &mut Option<serde_json::Value>, status: Option<&AcpToolCallStatus>) {
    let Some(AcpToolCallStatus::Completed) = status else {
        return;
    };
    let Some(obj) = raw_output.as_mut().and_then(|v| v.as_object_mut()) else {
        return;
    };
    obj.insert("status".to_owned(), serde_json::Value::String("completed".to_owned()));
}

fn map_sdk_tool_kind(kind: &SdkToolKind) -> AcpToolCallKind {
    match kind {
        SdkToolKind::Read | SdkToolKind::Search => AcpToolCallKind::Read,
        SdkToolKind::Edit | SdkToolKind::Delete | SdkToolKind::Move => AcpToolCallKind::Edit,
        SdkToolKind::Execute
        | SdkToolKind::Think
        | SdkToolKind::Fetch
        | SdkToolKind::SwitchMode
        | SdkToolKind::Other
        | _ => AcpToolCallKind::Execute,
    }
}

fn map_sdk_permission_option_kind(kind: SdkPermissionOptionKind) -> AcpPermissionOptionKind {
    match kind {
        SdkPermissionOptionKind::AllowOnce => AcpPermissionOptionKind::AllowOnce,
        SdkPermissionOptionKind::AllowAlways => AcpPermissionOptionKind::AllowAlways,
        SdkPermissionOptionKind::RejectOnce => AcpPermissionOptionKind::RejectOnce,
        SdkPermissionOptionKind::RejectAlways => AcpPermissionOptionKind::RejectAlways,
        _ => AcpPermissionOptionKind::RejectOnce,
    }
}

fn map_permission_tool_call(tool_call: &SdkToolCallUpdate) -> AcpPermissionToolCall {
    AcpPermissionToolCall {
        tool_call_id: tool_call.tool_call_id.to_string(),
        status: tool_call.fields.status.as_ref().map(map_sdk_tool_status),
        title: tool_call.fields.title.clone(),
        kind: tool_call.fields.kind.as_ref().map(map_sdk_tool_kind),
        raw_input: tool_call.fields.raw_input.clone(),
        raw_output: tool_call.fields.raw_output.clone(),
        content: tool_call
            .fields
            .content
            .as_ref()
            .and_then(|content| map_tool_call_content(content)),
        locations: tool_call
            .fields
            .locations
            .as_ref()
            .and_then(|locations| map_tool_call_locations(locations)),
        meta: tool_call.meta.clone(),
    }
}

fn map_permission_option(option: &PermissionOption) -> AcpPermissionOptionData {
    AcpPermissionOptionData {
        option_id: option.option_id.to_string(),
        name: option.name.clone(),
        kind: map_sdk_permission_option_kind(option.kind),
        meta: option.meta.clone(),
    }
}

fn map_tool_call_content(content: &[SdkToolCallContent]) -> Option<Vec<AcpToolCallContentItem>> {
    let items: Vec<AcpToolCallContentItem> = content
        .iter()
        .filter_map(|item| match item {
            SdkToolCallContent::Content(content) => match &content.content {
                ContentBlock::Text(text) => Some(AcpToolCallContentItem::Content {
                    content: AcpToolCallTextBlock {
                        block_type: AcpToolCallTextBlockType::Text,
                        text: text.text.clone(),
                    },
                }),
                _ => None,
            },
            SdkToolCallContent::Diff(diff) => Some(AcpToolCallContentItem::Diff {
                path: diff.path.to_string_lossy().into_owned(),
                old_text: diff.old_text.clone(),
                new_text: diff.new_text.clone(),
            }),
            SdkToolCallContent::Terminal(_) => None,
            _ => None,
        })
        .collect();

    if items.is_empty() { None } else { Some(items) }
}

fn map_tool_call_locations(locations: &[SdkToolCallLocation]) -> Option<Vec<AcpToolCallLocationItem>> {
    (!locations.is_empty()).then(|| {
        locations
            .iter()
            .map(|loc| AcpToolCallLocationItem {
                path: loc.path.to_string_lossy().into_owned(),
            })
            .collect()
    })
}
