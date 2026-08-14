//! Derive a host transcript from the canonical event journal.
//!
//! This is AionCore's equivalent of DeepSeek Harness `deriveMessages()`:
//! the journal is the source of truth, and anything model-visible must be
//! reconstructible from it. Internal stream machinery never appears in the
//! model-visible projection.
//!
//! Stored journal kinds stay as the existing `AgentStreamEvent` names so
//! v0.1.69 logs remain readable. This module only *projects* them.

use sha2::{Digest, Sha256};

use crate::stream_persistence::CanonicalJournalEvent;

const TRANSCRIPT_SCHEMA_VERSION: u32 = 3;
const SUMMARY_CHAR_LIMIT: usize = 240;
/// Host equivalent of DeepSeek Harness `dsh-compaction-tool-result-pruner`:
/// keep the newest tool results reconstructible; older ones collapse to summary.
const KEEP_RECENT_TOOL_RESULTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptVisibility {
    Model,
    Host,
}

impl TranscriptVisibility {
    fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Host => "host",
        }
    }

    fn is_at_least(self, requested: RequestedVisibility) -> bool {
        match requested {
            RequestedVisibility::Model => matches!(self, Self::Model),
            RequestedVisibility::Host => matches!(self, Self::Model | Self::Host),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestedVisibility {
    Model,
    Host,
}

impl RequestedVisibility {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("host") {
            "model" => Ok(Self::Model),
            "host" | "all" => Ok(Self::Host),
            other => Err(format!("unsupported transcript visibility '{other}'")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Host => "host",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DerivedTranscript {
    pub schema_version: u32,
    pub conversation_id: String,
    pub visibility: &'static str,
    pub items: Vec<DerivedTranscriptItem>,
    pub model_visible_count: u64,
    pub model_visible_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DerivedTranscriptItem {
    pub sequence: u64,
    pub event_id: String,
    pub journal_kind: String,
    pub transcript_kind: &'static str,
    pub visibility: &'static str,
    pub summary: String,
    /// Reconstructible model-visible payload. Tool output is already
    /// spill-governed before the event is journaled. Compacted tool
    /// results copy `summary` here so the projection stays bounded.
    pub content: String,
    pub compacted: bool,
    pub source_sequences: Vec<u64>,
}

#[derive(Debug, Clone)]
struct DraftItem {
    visibility: TranscriptVisibility,
    transcript_kind: &'static str,
    journal_kind: String,
    event_id: String,
    sequence: u64,
    summary: String,
    content: String,
    compacted: bool,
    source_sequences: Vec<u64>,
}

pub(crate) fn derive_transcript(
    conversation_id: &str,
    events: &[CanonicalJournalEvent],
    requested: RequestedVisibility,
) -> DerivedTranscript {
    let mut drafts = merge_assistant_text(events.iter().filter_map(classify_event).collect());
    compact_old_tool_results(&mut drafts);
    let model_visible_count = drafts
        .iter()
        .filter(|item| item.visibility == TranscriptVisibility::Model)
        .count() as u64;
    let model_visible_sha256 = digest_model_visible(&drafts);
    let items = drafts
        .into_iter()
        .filter(|item| item.visibility.is_at_least(requested))
        .map(|item| DerivedTranscriptItem {
            sequence: item.sequence,
            event_id: item.event_id,
            journal_kind: item.journal_kind,
            transcript_kind: item.transcript_kind,
            visibility: item.visibility.as_str(),
            summary: item.summary,
            content: item.content,
            compacted: item.compacted,
            source_sequences: item.source_sequences,
        })
        .collect();

    DerivedTranscript {
        schema_version: TRANSCRIPT_SCHEMA_VERSION,
        conversation_id: conversation_id.to_owned(),
        visibility: requested.as_str(),
        items,
        model_visible_count,
        model_visible_sha256,
    }
}

fn classify_event(event: &CanonicalJournalEvent) -> Option<DraftItem> {
    let (visibility, transcript_kind) = classify_kind(&event.kind)?;
    let content = extract_content(&event.kind, &event.payload);
    Some(DraftItem {
        visibility,
        transcript_kind,
        journal_kind: event.kind.clone(),
        event_id: event.event_id.clone(),
        sequence: event.sequence,
        summary: truncate_summary(&content),
        content,
        compacted: false,
        source_sequences: vec![event.sequence],
    })
}

fn classify_kind(kind: &str) -> Option<(TranscriptVisibility, &'static str)> {
    match kind {
        "Text" => Some((TranscriptVisibility::Model, "assistant/message")),
        "ToolCall" | "AcpToolCall" | "ToolGroup" => Some((TranscriptVisibility::Model, "tool/call")),
        "Ask" | "UserPrompt" => Some((TranscriptVisibility::Model, "user/message")),
        "Start" => Some((TranscriptVisibility::Host, "turn/start")),
        "Finish" => Some((TranscriptVisibility::Host, "turn/end")),
        "Error" => Some((TranscriptVisibility::Host, "turn/error")),
        "Thinking" | "Permission" | "AcpPermission" | "Plan" | "Tips" | "AgentStatus" | "SkillSuggest"
        | "CronTrigger" | "AvailableCommands" | "AcpTerminalOutput" | "WorkflowProgress" => {
            Some((TranscriptVisibility::Host, "host/notice"))
        }
        "SegmentBreak"
        | "BackendTurnBound"
        | "AcpDialectSignal"
        | "RequestTrace"
        | "SessionAssigned"
        | "AcpModelInfo"
        | "AcpModeInfo"
        | "AcpConfigOption"
        | "AcpSessionInfo"
        | "AcpContextUsage"
        | "SlashCommandsUpdated"
        | "System"
        | "AcpPromptHookWarning" => None,
        _ => Some((TranscriptVisibility::Host, "host/notice")),
    }
}

fn merge_assistant_text(items: Vec<DraftItem>) -> Vec<DraftItem> {
    let mut merged = Vec::new();
    for item in items {
        let can_merge = item.transcript_kind == "assistant/message"
            && merged.last().is_some_and(|last: &DraftItem| {
                last.transcript_kind == "assistant/message" && last.visibility == item.visibility
            });
        if can_merge {
            let last = merged.last_mut().expect("merge target exists");
            last.content = join_content(&last.content, &item.content);
            last.summary = truncate_summary(&last.content);
            last.source_sequences.extend(item.source_sequences);
            last.event_id = item.event_id;
            last.sequence = item.sequence;
        } else {
            merged.push(item);
        }
    }
    merged
}

fn compact_old_tool_results(items: &mut [DraftItem]) {
    let tool_indexes: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.transcript_kind == "tool/call" && item.visibility == TranscriptVisibility::Model)
        .map(|(index, _)| index)
        .collect();
    let prune_end = tool_indexes.len().saturating_sub(KEEP_RECENT_TOOL_RESULTS);
    for &index in &tool_indexes[..prune_end] {
        let item = &mut items[index];
        item.content = item.summary.clone();
        item.compacted = true;
    }
}

fn join_content(left: &str, right: &str) -> String {
    if left.is_empty() {
        return right.to_owned();
    }
    if right.is_empty() {
        return left.to_owned();
    }
    format!("{left}{right}")
}

fn extract_content(kind: &str, payload: &serde_json::Value) -> String {
    let candidates = [
        payload.pointer("/data/content"),
        payload.pointer("/content"),
        payload.pointer("/data/output"),
        payload.pointer("/output"),
        payload.pointer("/data/text"),
        payload.pointer("/text"),
        payload.pointer("/data/update/title"),
        payload.pointer("/update/title"),
        payload.pointer("/data/name"),
        payload.pointer("/name"),
        payload.pointer("/data/subject"),
        payload.pointer("/subject"),
    ];
    for candidate in candidates {
        if let Some(text) = candidate.and_then(serde_json::Value::as_str)
            && !text.is_empty()
        {
            return text.to_owned();
        }
    }
    kind.to_owned()
}

fn truncate_summary(value: &str) -> String {
    if value.chars().count() <= SUMMARY_CHAR_LIMIT {
        return value.to_owned();
    }
    let mut summary: String = value.chars().take(SUMMARY_CHAR_LIMIT).collect();
    summary.push('…');
    summary
}

fn digest_model_visible(items: &[DraftItem]) -> String {
    let mut digest = Sha256::new();
    for item in items
        .iter()
        .filter(|item| item.visibility == TranscriptVisibility::Model)
    {
        digest.update(item.transcript_kind.as_bytes());
        digest.update([0]);
        digest.update(item.content.as_bytes());
        digest.update([0]);
        for sequence in &item.source_sequences {
            digest.update(sequence.to_le_bytes());
        }
        digest.update([0xff]);
    }
    hex::encode(digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(sequence: u64, kind: &str, payload: serde_json::Value) -> CanonicalJournalEvent {
        CanonicalJournalEvent {
            schema_version: 1,
            event_id: format!("event-{sequence}"),
            conversation_id: "conv".into(),
            sequence,
            timestamp: sequence as i64,
            kind: kind.into(),
            payload,
        }
    }

    #[test]
    fn model_transcript_keeps_text_and_tools_and_drops_internal_events() {
        let events = vec![
            event(1, "Start", serde_json::json!({"type":"start","data":{}})),
            event(
                2,
                "Text",
                serde_json::json!({"type":"content","data":{"content":"hello"}}),
            ),
            event(3, "SegmentBreak", serde_json::json!({"type":"segment_break"})),
            event(4, "BackendTurnBound", serde_json::json!({"type":"backend_turn_bound"})),
            event(
                5,
                "ToolCall",
                serde_json::json!({"type":"tool_call","data":{"name":"Bash"}}),
            ),
            event(6, "Finish", serde_json::json!({"type":"finish","data":{}})),
        ];

        let model = derive_transcript("conv", &events, RequestedVisibility::Model);
        assert_eq!(model.items.len(), 2);
        assert_eq!(model.items[0].transcript_kind, "assistant/message");
        assert_eq!(model.items[0].summary, "hello");
        assert_eq!(model.items[1].transcript_kind, "tool/call");
        assert_eq!(model.items[1].summary, "Bash");
        assert_eq!(model.model_visible_count, 2);

        let host = derive_transcript("conv", &events, RequestedVisibility::Host);
        assert_eq!(host.items.len(), 4);
        assert_eq!(host.items[0].transcript_kind, "turn/start");
        assert_eq!(host.items[3].transcript_kind, "turn/end");
    }

    #[test]
    fn consecutive_text_events_merge_into_one_assistant_message() {
        let events = vec![
            event(1, "Text", serde_json::json!({"content":"hel"})),
            event(2, "Text", serde_json::json!({"content":"lo"})),
            event(3, "Thinking", serde_json::json!({"content":"hidden"})),
            event(4, "Text", serde_json::json!({"content":"world"})),
        ];
        let model = derive_transcript("conv", &events, RequestedVisibility::Model);
        assert_eq!(model.items.len(), 2);
        assert_eq!(model.items[0].summary, "hello");
        assert_eq!(model.items[0].content, "hello");
        assert_eq!(model.items[0].source_sequences, vec![1, 2]);
        assert_eq!(model.items[1].summary, "world");
        assert_eq!(model.items[1].source_sequences, vec![4]);
    }

    #[test]
    fn empty_journal_derives_an_empty_transcript() {
        let transcript = derive_transcript("conv", &[], RequestedVisibility::Host);
        assert!(transcript.items.is_empty());
        assert_eq!(transcript.model_visible_count, 0);
        assert_eq!(transcript.model_visible_sha256.len(), 64);
    }

    #[test]
    fn permission_is_host_visible_not_model_visible() {
        let events = vec![
            event(1, "Permission", serde_json::json!({"type":"permission"})),
            event(2, "AcpPermission", serde_json::json!({"type":"acp_permission"})),
            event(3, "Text", serde_json::json!({"content":"ok"})),
        ];
        let model = derive_transcript("conv", &events, RequestedVisibility::Model);
        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].summary, "ok");
        let host = derive_transcript("conv", &events, RequestedVisibility::Host);
        assert_eq!(host.items.len(), 3);
        assert!(host.items.iter().any(|item| item.journal_kind == "Permission"));
    }

    #[test]
    fn visibility_parser_rejects_unknown_values() {
        assert!(RequestedVisibility::parse(Some("model")).is_ok());
        assert!(RequestedVisibility::parse(Some("host")).is_ok());
        assert!(RequestedVisibility::parse(None).is_ok());
        assert!(RequestedVisibility::parse(Some("secret")).is_err());
    }

    #[test]
    fn user_prompt_is_model_visible_and_keeps_full_content() {
        let long = "x".repeat(SUMMARY_CHAR_LIMIT + 20);
        let events = vec![event(
            1,
            "UserPrompt",
            serde_json::json!({"type":"user_prompt","data":{"msg_id":"m1","content": long}}),
        )];
        let model = derive_transcript("conv", &events, RequestedVisibility::Model);
        assert_eq!(model.items.len(), 1);
        assert_eq!(model.items[0].transcript_kind, "user/message");
        assert_eq!(model.items[0].content, long);
        assert_eq!(model.items[0].summary.chars().count(), SUMMARY_CHAR_LIMIT + 1);
        assert!(model.items[0].summary.ends_with('…'));
    }

    #[test]
    fn tool_output_content_is_reconstructible_from_the_journal() {
        let events = vec![event(
            1,
            "ToolCall",
            serde_json::json!({
                "type":"tool_call",
                "data":{"name":"Bash","output":"exit 0\nhello world"}
            }),
        )];
        let model = derive_transcript("conv", &events, RequestedVisibility::Model);
        assert_eq!(model.items[0].summary, "exit 0\nhello world");
        assert_eq!(model.items[0].content, "exit 0\nhello world");
        assert!(!model.items[0].compacted);
    }

    #[test]
    fn older_tool_results_compact_to_summary_and_recent_ones_stay_full() {
        let events: Vec<_> = (1..=4)
            .map(|sequence| {
                let output = format!("{}-{sequence}", "x".repeat(SUMMARY_CHAR_LIMIT + 8));
                event(
                    sequence,
                    "ToolCall",
                    serde_json::json!({
                        "type": "tool_call",
                        "data": { "name": format!("tool-{sequence}"), "output": output }
                    }),
                )
            })
            .collect();
        let model = derive_transcript("conv", &events, RequestedVisibility::Model);
        assert_eq!(model.items.len(), 4);
        assert!(model.items[0].compacted);
        assert_eq!(model.items[0].content, model.items[0].summary);
        assert!(model.items[0].summary.ends_with('…'));
        assert!(model.items[0].summary.chars().count() < SUMMARY_CHAR_LIMIT + 8);
        for item in &model.items[1..] {
            assert!(!item.compacted);
            assert!(item.content.len() > SUMMARY_CHAR_LIMIT);
        }
    }

    #[test]
    fn user_and_assistant_messages_are_not_compacted() {
        let events = vec![
            event(
                1,
                "UserPrompt",
                serde_json::json!({"data":{"content":"please keep this user prompt"}}),
            ),
            event(2, "Text", serde_json::json!({"content":"assistant stays whole"})),
            event(
                3,
                "ToolCall",
                serde_json::json!({"data":{"name":"Bash","output":"old tool body"}}),
            ),
            event(
                4,
                "ToolCall",
                serde_json::json!({"data":{"name":"Bash","output":"newer tool body"}}),
            ),
            event(
                5,
                "ToolCall",
                serde_json::json!({"data":{"name":"Bash","output":"newest tool body"}}),
            ),
            event(
                6,
                "ToolCall",
                serde_json::json!({"data":{"name":"Bash","output":"latest tool body"}}),
            ),
        ];
        let model = derive_transcript("conv", &events, RequestedVisibility::Model);
        assert_eq!(model.items[0].content, "please keep this user prompt");
        assert!(!model.items[0].compacted);
        assert_eq!(model.items[1].content, "assistant stays whole");
        assert!(!model.items[1].compacted);
        assert!(model.items[2].compacted);
        assert!(!model.items[3].compacted);
        assert!(!model.items[4].compacted);
        assert!(!model.items[5].compacted);
    }
}
