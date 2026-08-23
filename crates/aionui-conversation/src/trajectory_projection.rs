//! Human-facing semantic trajectory derived from the canonical journal.
//!
//! This projection is intentionally separate from `journal_transcript`: the
//! transcript is a model-replay contract, while this module folds noisy stream
//! events into stable records suitable for diagnostics.

use std::collections::{HashMap, HashSet};

use aionui_api_types::{
    PromptAttachmentDelivery, PromptAttachmentV1, RawTrajectoryEventV1, RawTrajectoryProjectionV1,
    TrajectoryOverviewV1, TrajectoryProjectionV1, TrajectoryQuery, TrajectoryRecordV1, TrajectoryTokenUsage,
};
use tracing::warn;

use crate::stream_persistence::{CanonicalEventJournal, CanonicalJournalEvent};

const SCHEMA_VERSION: u32 = 1;
const CACHE_VERSION: u32 = 5;
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;
const PREVIEW_CHARS: usize = 240;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PersistedTrajectoryV1 {
    schema_version: u32,
    conversation_id: String,
    last_sequence: u64,
    last_event_id: Option<String>,
    state: FoldState,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct FoldState {
    records: Vec<TrajectoryRecordV1>,
    inputs: HashMap<String, usize>,
    inputs_by_message: HashMap<String, usize>,
    tools: HashMap<String, usize>,
    approvals: HashMap<String, usize>,
    cancellations: HashMap<String, usize>,
    current_turn: Option<String>,
    current_turn_record: Option<usize>,
    current_compaction_record: Option<usize>,
    current_step: u64,
    merge_target: Option<(String, usize)>,
}

pub(crate) fn validate_query(query: &TrajectoryQuery) -> Result<usize, String> {
    if query.before_sequence.is_some() && query.after_sequence.is_some() {
        return Err("before_sequence and after_sequence are mutually exclusive".to_owned());
    }
    Ok(query.limit.unwrap_or(DEFAULT_LIMIT as u32).clamp(1, MAX_LIMIT as u32) as usize)
}

#[cfg(test)]
fn derive_trajectory(
    conversation_id: &str,
    events: &[CanonicalJournalEvent],
    query: &TrajectoryQuery,
) -> Result<TrajectoryProjectionV1, String> {
    page_trajectory(
        conversation_id,
        fold_records(events),
        events.last().map_or(0, |event| event.sequence),
        query,
    )
}

pub(crate) async fn load_cached_records(
    journal: &CanonicalEventJournal,
    user_id: &str,
    conversation_id: &str,
) -> Result<(Vec<TrajectoryRecordV1>, u64), std::io::Error> {
    let path = journal.trajectory_projection_path(user_id, conversation_id)?;
    let cached = match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice::<PersistedTrajectoryV1>(&bytes).ok(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => None,
    }
    .filter(|cached| cached.schema_version == CACHE_VERSION && cached.conversation_id == conversation_id);

    if let Some(mut cached) = cached {
        let checkpoint = cached.last_sequence.saturating_sub(1);
        match journal.replay_after(user_id, conversation_id, checkpoint).await {
            Ok(events) if cached.last_sequence == 0 && events.is_empty() => {
                return Ok((materialize_records(&cached.state), 0));
            }
            Ok(events)
                if events.first().is_some_and(|event| {
                    event.sequence == cached.last_sequence && Some(&event.event_id) == cached.last_event_id.as_ref()
                }) && events
                    .windows(2)
                    .all(|pair| pair[0].sequence < pair[1].sequence && pair[0].event_id != pair[1].event_id) =>
            {
                if events.len() > 1 {
                    fold_into(&mut cached.state, &events[1..]);
                    cached.last_sequence = events.last().map_or(cached.last_sequence, |event| event.sequence);
                    cached.last_event_id = events.last().map(|event| event.event_id.clone());
                    strip_details(&mut cached.state.records);
                    persist_projection(&path, &cached).await;
                }
                return Ok((materialize_records(&cached.state), cached.last_sequence));
            }
            Ok(_) | Err(_) => {}
        }
    }

    let events = journal.replay(user_id, conversation_id).await?;
    let last_sequence = events.last().map_or(0, |event| event.sequence);
    let last_event_id = events.last().map(|event| event.event_id.clone());
    let mut state = FoldState::default();
    fold_into(&mut state, &events);
    strip_details(&mut state.records);
    let projection = PersistedTrajectoryV1 {
        schema_version: CACHE_VERSION,
        conversation_id: conversation_id.to_owned(),
        last_sequence,
        last_event_id,
        state,
    };
    persist_projection(&path, &projection).await;
    Ok((materialize_records(&projection.state), last_sequence))
}

async fn persist_projection(path: &std::path::Path, projection: &PersistedTrajectoryV1) {
    if let Some(parent) = path.parent()
        && let Err(error) = tokio::fs::create_dir_all(parent).await
    {
        warn!(error = %error, "failed to create trajectory projection directory; using in-memory projection");
        return;
    }
    match serde_json::to_vec(projection) {
        Ok(bytes) => {
            if let Err(error) = tokio::fs::write(path, bytes).await {
                warn!(error = %error, "failed to persist trajectory projection; using in-memory projection");
            }
        }
        Err(error) => warn!(error = %error, "failed to serialize trajectory projection; using in-memory projection"),
    }
}

fn strip_details(records: &mut [TrajectoryRecordV1]) {
    for record in records {
        record.detail = None;
    }
}

pub(crate) fn page_trajectory(
    conversation_id: &str,
    all_records: Vec<TrajectoryRecordV1>,
    log_revision: u64,
    query: &TrajectoryQuery,
) -> Result<TrajectoryProjectionV1, String> {
    let limit = validate_query(query)?;
    let overview = overview(&all_records);
    let filtered: Vec<_> = all_records
        .into_iter()
        .filter(|record| match (query.before_sequence, query.after_sequence) {
            (Some(before), None) => record.first_sequence < before,
            (None, Some(after)) => record.last_sequence > after,
            (None, None) => true,
            (Some(_), Some(_)) => false,
        })
        .collect();
    let has_more = filtered.len() > limit;
    let mut records = if query.after_sequence.is_some() {
        filtered.into_iter().take(limit).collect::<Vec<_>>()
    } else {
        filtered
            .into_iter()
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    };
    for record in &mut records {
        record.detail = None;
    }
    let oldest_sequence = records.first().map(|record| record.first_sequence);
    let newest_sequence = records.last().map(|record| record.last_sequence);
    Ok(TrajectoryProjectionV1 {
        schema_version: SCHEMA_VERSION,
        conversation_id: conversation_id.to_owned(),
        next_before_sequence: has_more.then_some(oldest_sequence).flatten(),
        records,
        overview,
        has_more,
        oldest_sequence,
        newest_sequence,
        log_revision,
    })
}

pub(crate) fn derive_raw_trajectory(
    conversation_id: &str,
    events: &[CanonicalJournalEvent],
    query: &TrajectoryQuery,
) -> Result<RawTrajectoryProjectionV1, String> {
    let limit = validate_query(query)?;
    let filtered: Vec<_> = events
        .iter()
        .filter(|event| match (query.before_sequence, query.after_sequence) {
            (Some(before), None) => event.sequence < before,
            (None, Some(after)) => event.sequence > after,
            (None, None) => true,
            (Some(_), Some(_)) => false,
        })
        .collect();
    let has_more = filtered.len() > limit;
    let selected = if query.after_sequence.is_some() {
        filtered.into_iter().take(limit).collect::<Vec<_>>()
    } else {
        filtered
            .into_iter()
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    };
    let log_revision = events.last().map_or(0, |event| event.sequence);
    let events = selected
        .into_iter()
        .map(|event| RawTrajectoryEventV1 {
            event_id: event.event_id.clone(),
            sequence: event.sequence,
            timestamp_ms: event.timestamp,
            kind: event.kind.clone(),
            payload: event.payload.clone(),
        })
        .collect::<Vec<_>>();
    let oldest_sequence = events.first().map(|event| event.sequence);
    let newest_sequence = events.last().map(|event| event.sequence);
    Ok(RawTrajectoryProjectionV1 {
        schema_version: SCHEMA_VERSION,
        conversation_id: conversation_id.to_owned(),
        next_before_sequence: has_more.then_some(oldest_sequence).flatten(),
        events,
        has_more,
        oldest_sequence,
        newest_sequence,
        log_revision,
    })
}

pub(crate) fn find_trajectory_record(events: &[CanonicalJournalEvent], record_id: &str) -> Option<TrajectoryRecordV1> {
    let mut record = fold_records(events)
        .into_iter()
        .find(|record| record.record_id == record_id)?;
    hydrate_record_detail(&mut record, events);
    Some(record)
}

fn hydrate_record_detail(record: &mut TrajectoryRecordV1, events: &[CanonicalJournalEvent]) {
    let source_sequences = record.source_sequences.iter().copied().collect::<HashSet<_>>();
    let source_events = events
        .iter()
        .filter(|event| source_sequences.contains(&event.sequence))
        .collect::<Vec<_>>();
    let payloads = source_events
        .iter()
        .map(|event| event_data(&event.payload))
        .collect::<Vec<_>>();

    let full_input = if record.category == "input" {
        payloads
            .iter()
            .rev()
            .find_map(|payload| content(payload).or_else(|| input(payload)))
    } else {
        payloads.iter().find_map(|payload| input(payload))
    };
    let full_output = if matches!(record.category.as_str(), "assistant" | "thinking") {
        Some(
            payloads
                .iter()
                .filter_map(|payload| content(payload))
                .collect::<String>(),
        )
        .filter(|value| !value.is_empty())
    } else {
        payloads.iter().rev().find_map(|payload| output(payload))
    };

    if let Some(input) = &full_input {
        record.input_preview = Some(input.clone());
    }
    if let Some(output) = &full_output {
        record.output_preview = Some(output.clone());
    }
    record.detail = Some(serde_json::json!({
        "input": full_input,
        "output": full_output,
        "source_events": source_events
            .into_iter()
            .map(|event| serde_json::json!({
                "sequence": event.sequence,
                "event_id": event.event_id,
                "kind": event.kind,
                "payload": event.payload,
            }))
            .collect::<Vec<_>>(),
    }));
}

fn fold_records(events: &[CanonicalJournalEvent]) -> Vec<TrajectoryRecordV1> {
    let mut state = FoldState::default();
    fold_into(&mut state, events);
    materialize_records(&state)
}

fn fold_into(state: &mut FoldState, events: &[CanonicalJournalEvent]) {
    let records = &mut state.records;
    let inputs = &mut state.inputs;
    let inputs_by_message = &mut state.inputs_by_message;
    let tools = &mut state.tools;
    let approvals = &mut state.approvals;
    let cancellations = &mut state.cancellations;
    let mut current_turn = state.current_turn.take();
    let mut current_turn_record = state.current_turn_record.take();
    let mut current_compaction_record = state.current_compaction_record.take();
    let mut current_step = state.current_step;
    let mut merge_target = state.merge_target.take();

    for event in events {
        let payload = event_data(&event.payload);
        match event.kind.as_str() {
            "Start" => {
                let turn_id = string_at(payload, &["turn_id", "id"]).unwrap_or_else(|| event.event_id.clone());
                current_turn = Some(turn_id.clone());
                current_step = 0;
                merge_target = None;
                let mut record = base_record(event, "turn", "running", Some(turn_id.clone()), None);
                record.record_id = format!("turn:{turn_id}");
                current_turn_record = Some(records.len());
                records.push(record);
            }
            "Finish" | "Error" => {
                merge_target = None;
                let status = if event.kind == "Error" { "failed" } else { "completed" };
                if let Some(index) = current_turn_record {
                    update_record(&mut records[index], event, status, payload);
                    records[index].title = event.kind.clone();
                } else {
                    let turn_id = current_turn
                        .clone()
                        .or_else(|| string_at(payload, &["turn_id", "id"]))
                        .unwrap_or_else(|| event.event_id.clone());
                    let mut record = base_record(event, "turn", status, Some(turn_id.clone()), None);
                    record.record_id = format!("turn:{turn_id}");
                    records.push(record);
                }
                current_turn = None;
                current_turn_record = None;
            }
            "UserPrompt" | "Ask" => {
                merge_target = None;
                let msg_id = string_at(payload, &["msg_id"]).unwrap_or_else(|| event.event_id.clone());
                if let Some(index) = inputs_by_message.get(&msg_id).copied() {
                    let value = content(payload).unwrap_or_default();
                    records[index].input_preview = Some(preview(&value));
                    records[index].summary = preview(&value);
                    records[index].last_sequence = event.sequence;
                    records[index].source_sequences.push(event.sequence);
                    records[index].detail = Some(payload.clone());
                } else {
                    current_step = current_step.saturating_add(1);
                    let mut record = base_record(
                        event,
                        "input",
                        "applied",
                        current_turn.clone(),
                        step_id(&current_turn, current_step),
                    );
                    let value = content(payload).unwrap_or_default();
                    record.record_id = format!("input:{msg_id}");
                    record.input_preview = Some(preview(&value));
                    record.summary = preview(&value);
                    inputs_by_message.insert(msg_id, records.len());
                    records.push(record);
                }
            }
            "InputHeld" | "InputDispatching" | "InputAccepted" | "InputApplied" | "InputRejected" | "InputCanceled"
            | "InputFailed" => {
                merge_target = None;
                let input_id = string_at(payload, &["input_id"]).unwrap_or_else(|| event.event_id.clone());
                let msg_id = string_at(payload, &["msg_id"]);
                let status = match event.kind.as_str() {
                    "InputHeld" => "held",
                    "InputDispatching" => "dispatching",
                    "InputAccepted" => "accepted",
                    "InputApplied" => "applied",
                    "InputCanceled" => "canceled",
                    _ => "failed",
                };
                let existing = inputs
                    .get(&input_id)
                    .copied()
                    .or_else(|| msg_id.as_ref().and_then(|id| inputs_by_message.get(id).copied()));
                if let Some(index) = existing {
                    update_record(&mut records[index], event, status, payload);
                    records[index].input_id = Some(input_id.clone());
                    inputs.insert(input_id, index);
                    if let Some(msg_id) = msg_id {
                        inputs_by_message.insert(msg_id, index);
                    }
                } else {
                    current_step = current_step.saturating_add(1);
                    let mut record = base_record(
                        event,
                        "input",
                        status,
                        current_turn.clone().or_else(|| string_at(payload, &["turn_id"])),
                        step_id(&current_turn, current_step),
                    );
                    record.input_id = Some(input_id.clone());
                    let stable_id = msg_id.as_deref().unwrap_or(&input_id);
                    record.record_id = format!("input:{stable_id}");
                    record.input_preview = content(payload).map(|value| preview(&value));
                    inputs.insert(input_id, records.len());
                    if let Some(msg_id) = msg_id {
                        inputs_by_message.insert(msg_id, records.len());
                    }
                    records.push(record);
                }
            }
            "Thinking" | "Text" => {
                let category = if event.kind == "Thinking" {
                    "thinking"
                } else {
                    "assistant"
                };
                let value = content(payload).unwrap_or_default();
                if let Some((last_category, index)) = merge_target.as_ref()
                    && last_category == category
                    && records[*index].turn_id == current_turn
                {
                    append_record(&mut records[*index], event, &value);
                } else {
                    current_step = current_step.saturating_add(1);
                    let mut record = base_record(
                        event,
                        category,
                        "completed",
                        current_turn.clone(),
                        step_id(&current_turn, current_step),
                    );
                    record.output_preview = Some(preview(&value));
                    record.summary = preview(&value);
                    let index = records.len();
                    records.push(record);
                    merge_target = Some((category.to_owned(), index));
                }
            }
            "ToolCall" | "AcpToolCall" => {
                merge_target = None;
                let call_id = tool_call_id(payload);
                let execution_id = diagnostic_string(payload, "execution_id");
                let business_id = call_id
                    .clone()
                    .or_else(|| execution_id.clone())
                    .unwrap_or_else(|| event.event_id.clone());
                let status = tool_status(payload);
                let existing = tools
                    .get(&business_id)
                    .copied()
                    .or_else(|| call_id.as_ref().and_then(|id| tools.get(id).copied()))
                    .or_else(|| execution_id.as_ref().and_then(|id| tools.get(id).copied()));
                if let Some(index) = existing {
                    update_record(&mut records[index], event, &status, payload);
                    records[index].execution_id = execution_id.clone().or_else(|| records[index].execution_id.clone());
                    records[index].output_preview = output(payload).map(|value| preview(&value));
                    for id in [call_id, execution_id].into_iter().flatten() {
                        tools.insert(id, index);
                    }
                } else {
                    current_step = current_step.saturating_add(1);
                    let mut record = base_record(
                        event,
                        "tool",
                        &status,
                        current_turn.clone(),
                        step_id(&current_turn, current_step),
                    );
                    record.tool_call_id = call_id.clone();
                    record.execution_id = execution_id.clone();
                    record.parent_record_id = string_at(
                        payload,
                        &["parent_execution_id", "parent_call_id", "parent_tool_use_id"],
                    )
                    .map(|id| format!("tool-reference:{id}"));
                    record.title = tool_title(payload).unwrap_or_else(|| "tool".to_owned());
                    record.input_preview = input(payload).map(|value| preview(&value));
                    record.output_preview = output(payload).map(|value| preview(&value));
                    record.record_id = format!("tool:{business_id}");
                    let index = records.len();
                    tools.insert(business_id, index);
                    for id in [call_id, execution_id].into_iter().flatten() {
                        tools.insert(id, index);
                    }
                    records.push(record);
                }
            }
            "ToolGroup" => {
                merge_target = None;
                let Some(entries) = payload.as_array() else {
                    continue;
                };
                for entry in entries {
                    let Some(call_id) = string_at(entry, &["call_id", "tool_call_id"]) else {
                        continue;
                    };
                    let status = tool_status(entry);
                    if let Some(index) = tools.get(&call_id).copied() {
                        update_record(&mut records[index], event, &status, entry);
                        if let Some(description) = content(entry) {
                            records[index].summary = preview(&description);
                            records[index].output_preview = Some(preview(&description));
                        }
                    } else {
                        current_step = current_step.saturating_add(1);
                        let mut record = base_record(
                            event,
                            "tool",
                            &status,
                            current_turn.clone(),
                            step_id(&current_turn, current_step),
                        );
                        record.record_id = format!("tool:{call_id}");
                        record.tool_call_id = Some(call_id.clone());
                        record.title = tool_title(entry).unwrap_or_else(|| "tool".to_owned());
                        record.output_preview = content(entry).map(|value| preview(&value));
                        tools.insert(call_id, records.len());
                        records.push(record);
                    }
                }
            }
            "ApprovalAsked" | "ApprovalDecided" => {
                merge_target = None;
                let approval_id =
                    string_at(payload, &["request_id", "call_id"]).unwrap_or_else(|| event.event_id.clone());
                let outcome = string_at(payload, &["outcome"]);
                let status = if event.kind == "ApprovalAsked" {
                    "waiting"
                } else {
                    match outcome.as_deref() {
                        Some("rejected" | "denied") => "rejected",
                        Some("unavailable") => "degraded",
                        _ => "completed",
                    }
                };
                if let Some(index) = approvals.get(&approval_id).copied() {
                    update_record(&mut records[index], event, status, payload);
                    records[index].output_preview = outcome;
                } else {
                    let mut record = base_record(
                        event,
                        "approval",
                        status,
                        current_turn.clone(),
                        step_id(&current_turn, current_step),
                    );
                    record.record_id = format!("approval:{approval_id}");
                    record.tool_call_id = string_at(payload, &["call_id"]);
                    record.title = string_at(payload, &["tool_name"]).unwrap_or_else(|| "approval".to_owned());
                    record.output_preview = outcome;
                    approvals.insert(approval_id, records.len());
                    records.push(record);
                }
            }
            "CompactionStart" | "CompactionEnd" => {
                merge_target = None;
                let status = if event.kind.ends_with("Start") {
                    "running"
                } else {
                    "completed"
                };
                if event.kind == "CompactionEnd"
                    && let Some(index) = current_compaction_record
                {
                    update_record(&mut records[index], event, status, payload);
                    current_compaction_record = None;
                } else {
                    let mut record = base_record(event, "compaction", status, current_turn.clone(), None);
                    record.record_id = format!("compaction:{}", event.event_id);
                    current_compaction_record = (event.kind == "CompactionStart").then_some(records.len());
                    records.push(record);
                }
            }
            "ToolExecutionRecovered" => {
                merge_target = None;
                records.push(base_record(
                    event,
                    "recovery",
                    "recovered",
                    current_turn.clone(),
                    step_id(&current_turn, current_step),
                ));
            }
            "CancellationRequested"
            | "CancellationCancelling"
            | "CancellationConvergedIdle"
            | "CancellationForceTerminated"
            | "CancellationFailed" => {
                merge_target = None;
                let status = match event.kind.as_str() {
                    "CancellationRequested" | "CancellationCancelling" => "running",
                    "CancellationConvergedIdle" => "canceled",
                    "CancellationForceTerminated" => "degraded",
                    _ => "failed",
                };
                let turn_id = current_turn.clone().or_else(|| string_at(payload, &["turn_id"]));
                let cancellation_id = turn_id.clone().unwrap_or_else(|| event.event_id.clone());
                if let Some(index) = cancellations.get(&cancellation_id).copied() {
                    update_record(&mut records[index], event, status, payload);
                    records[index].title = event.kind.clone();
                } else {
                    let mut record = base_record(
                        event,
                        "cancellation",
                        status,
                        turn_id.clone(),
                        step_id(&turn_id, current_step),
                    );
                    record.record_id = format!("cancellation:{cancellation_id}");
                    cancellations.insert(cancellation_id, records.len());
                    records.push(record);
                }
            }
            "AttachmentPrepared" => {
                merge_target = None;
                let msg_id = string_at(payload, &["msg_id"]);
                let attachments = payload
                    .get("attachments")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten();
                for value in attachments {
                    let Ok(attachment) = serde_json::from_value::<PromptAttachmentV1>(value.clone()) else {
                        continue;
                    };
                    let (status, delivery) = match attachment.delivery {
                        PromptAttachmentDelivery::Pending => ("pending", "pending"),
                        PromptAttachmentDelivery::Native => ("completed", "native"),
                        PromptAttachmentDelivery::PathFallback => ("degraded", "path fallback"),
                        PromptAttachmentDelivery::Rejected => ("rejected", "rejected"),
                    };
                    let mut record = base_record(
                        event,
                        "attachment",
                        status,
                        current_turn.clone(),
                        step_id(&current_turn, current_step),
                    );
                    record.record_id = attachment.attachment_id.clone();
                    record.input_id = msg_id.clone();
                    record.title = attachment.filename.clone();
                    record.summary = format!("{} · {delivery}", attachment.filename);
                    record.input_preview = Some(attachment.mime_type.clone());
                    record.output_preview = Some(delivery.to_owned());
                    record.detail = serde_json::to_value(attachment).ok();
                    records.push(record);
                }
            }
            "AttachmentDelivery" => {
                merge_target = None;
                let Some(value) = payload.get("attachment") else {
                    continue;
                };
                let Ok(attachment) = serde_json::from_value::<PromptAttachmentV1>(value.clone()) else {
                    continue;
                };
                let (status, delivery) = attachment_record_state(attachment.delivery);
                if let Some(record) = records
                    .iter_mut()
                    .find(|record| record.record_id == attachment.attachment_id)
                {
                    update_record(record, event, status, value);
                    if record.turn_id.is_none() {
                        record.turn_id = current_turn.clone();
                        record.step_id = step_id(&current_turn, current_step);
                    }
                    record.title = attachment.filename.clone();
                    record.summary = format!("{} · {delivery}", attachment.filename);
                    record.input_preview = Some(attachment.mime_type.clone());
                    record.output_preview = Some(delivery.to_owned());
                } else {
                    let mut record = base_record(
                        event,
                        "attachment",
                        status,
                        current_turn.clone(),
                        step_id(&current_turn, current_step),
                    );
                    record.record_id = attachment.attachment_id.clone();
                    record.title = attachment.filename.clone();
                    record.summary = format!("{} · {delivery}", attachment.filename);
                    record.input_preview = Some(attachment.mime_type.clone());
                    record.output_preview = Some(delivery.to_owned());
                    record.detail = Some(value.clone());
                    records.push(record);
                }
            }
            "AcpContextUsage" => apply_usage(records, payload),
            _ => merge_target = None,
        }
    }
    state.current_turn = current_turn;
    state.current_turn_record = current_turn_record;
    state.current_compaction_record = current_compaction_record;
    state.current_step = current_step;
    state.merge_target = merge_target;
}

fn materialize_records(state: &FoldState) -> Vec<TrajectoryRecordV1> {
    let mut records = state.records.clone();
    let tool_references = records
        .iter()
        .filter(|record| record.category == "tool")
        .flat_map(|record| {
            [record.tool_call_id.as_ref(), record.execution_id.as_ref()]
                .into_iter()
                .flatten()
                .map(|id| (id.clone(), record.record_id.clone()))
        })
        .collect::<HashMap<_, _>>();
    for record in &mut records {
        let Some(reference) = record
            .parent_record_id
            .as_deref()
            .and_then(|value| value.strip_prefix("tool-reference:"))
            .map(str::to_owned)
        else {
            continue;
        };
        record.parent_record_id = tool_references.get(&reference).cloned();
    }
    records
}

fn base_record(
    event: &CanonicalJournalEvent,
    category: &str,
    status: &str,
    turn_id: Option<String>,
    step_id: Option<String>,
) -> TrajectoryRecordV1 {
    let payload = event_data(&event.payload);
    let text = content(payload).unwrap_or_else(|| event.kind.clone());
    TrajectoryRecordV1 {
        record_id: format!("{category}:{}", event.event_id),
        category: category.to_owned(),
        status: status.to_owned(),
        visibility: "host".to_owned(),
        turn_id,
        step_id,
        parent_record_id: None,
        input_id: None,
        execution_id: None,
        tool_call_id: None,
        started_at_ms: Some(event.timestamp),
        completed_at_ms: terminal_status(status).then_some(event.timestamp),
        duration_ms: None,
        title: event.kind.clone(),
        summary: preview(&text),
        input_preview: None,
        output_preview: None,
        retained_output_reference: retained_reference(payload),
        structured_content: diagnostic_value(payload, "structured_content"),
        error_code: diagnostic_value(payload, "error_code").and_then(|value| value.as_str().map(str::to_owned)),
        truncation: diagnostic_value(payload, "truncation"),
        tokens: TrajectoryTokenUsage::default(),
        first_sequence: event.sequence,
        last_sequence: event.sequence,
        source_sequences: vec![event.sequence],
        detail: Some(event.payload.clone()),
    }
}

fn attachment_record_state(delivery: PromptAttachmentDelivery) -> (&'static str, &'static str) {
    match delivery {
        PromptAttachmentDelivery::Pending => ("pending", "pending"),
        PromptAttachmentDelivery::Native => ("completed", "native"),
        PromptAttachmentDelivery::PathFallback => ("degraded", "path fallback"),
        PromptAttachmentDelivery::Rejected => ("rejected", "rejected"),
    }
}

fn update_record(
    record: &mut TrajectoryRecordV1,
    event: &CanonicalJournalEvent,
    status: &str,
    payload: &serde_json::Value,
) {
    record.status = status.to_owned();
    record.last_sequence = event.sequence;
    record.source_sequences.push(event.sequence);
    record.completed_at_ms = terminal_status(status).then_some(event.timestamp);
    record.duration_ms = duration(record.started_at_ms, record.completed_at_ms);
    record.detail = Some(payload.clone());
    record.retained_output_reference = retained_reference(payload).or_else(|| record.retained_output_reference.clone());
    record.structured_content =
        diagnostic_value(payload, "structured_content").or_else(|| record.structured_content.clone());
    record.error_code = diagnostic_value(payload, "error_code")
        .and_then(|value| value.as_str().map(str::to_owned))
        .or_else(|| record.error_code.clone());
    record.truncation = diagnostic_value(payload, "truncation").or_else(|| record.truncation.clone());
}

fn append_record(record: &mut TrajectoryRecordV1, event: &CanonicalJournalEvent, value: &str) {
    let combined = match &record.output_preview {
        Some(existing) => format!("{existing}{value}"),
        None => value.to_owned(),
    };
    record.output_preview = Some(preview(&combined));
    record.summary = preview(&combined);
    record.last_sequence = event.sequence;
    record.source_sequences.push(event.sequence);
    record.completed_at_ms = Some(event.timestamp);
    record.duration_ms = duration(record.started_at_ms, record.completed_at_ms);
}

fn apply_usage(records: &mut [TrajectoryRecordV1], payload: &serde_json::Value) {
    let Some(record) = records.iter_mut().rev().find(|record| record.category == "turn") else {
        return;
    };
    record.tokens = usage_tokens(payload);
}

fn usage_tokens(payload: &serde_json::Value) -> TrajectoryTokenUsage {
    TrajectoryTokenUsage {
        input: usage_number(payload, &["input_tokens", "inputTokens"]),
        output: usage_number(payload, &["output_tokens", "outputTokens"]),
        cached: cached_tokens(payload),
        thinking: usage_number(payload, &["thinking_tokens", "thinkingTokens", "reasoning_tokens"]),
    }
}

fn overview(records: &[TrajectoryRecordV1]) -> TrajectoryOverviewV1 {
    let turn_ids = records
        .iter()
        .filter_map(|record| record.turn_id.clone())
        .collect::<HashSet<_>>();
    let step_ids = records
        .iter()
        .filter_map(|record| record.step_id.clone())
        .collect::<HashSet<_>>();
    let started = records.iter().filter_map(|record| record.started_at_ms).min();
    let ended = records
        .iter()
        .filter_map(|record| record.completed_at_ms.or(record.started_at_ms))
        .max();
    let first_output = records
        .iter()
        .filter(|record| matches!(record.category.as_str(), "thinking" | "assistant" | "tool"))
        .filter_map(|record| record.started_at_ms)
        .min();
    let mut tokens = TrajectoryTokenUsage::default();
    for record in records {
        add_optional(&mut tokens.input, record.tokens.input);
        add_optional(&mut tokens.output, record.tokens.output);
        add_optional(&mut tokens.cached, record.tokens.cached);
        add_optional(&mut tokens.thinking, record.tokens.thinking);
    }
    TrajectoryOverviewV1 {
        turns: turn_ids.len() as u64,
        steps: step_ids.len() as u64,
        tools: records.iter().filter(|record| record.category == "tool").count() as u64,
        errors: records
            .iter()
            .filter(|record| matches!(record.status.as_str(), "failed" | "error" | "rejected"))
            .count() as u64,
        total_duration_ms: duration(started, ended),
        first_output_ms: duration(started, first_output),
        tokens,
    }
}

fn add_optional(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0).saturating_add(value));
    }
}

fn event_data(payload: &serde_json::Value) -> &serde_json::Value {
    payload.get("data").unwrap_or(payload)
}

fn string_at(payload: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| payload.get(*key).and_then(serde_json::Value::as_str))
        .map(str::to_owned)
}

fn tool_title(payload: &serde_json::Value) -> Option<String> {
    string_at(payload, &["name", "title"])
        .or_else(|| payload.get("update").and_then(|value| string_at(value, &["title"])))
}

fn number_at(payload: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| payload.get(*key).and_then(serde_json::Value::as_u64))
}

fn usage_number(payload: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    number_at(payload, keys).or_else(|| payload.get("_meta").and_then(|meta| number_at(meta, keys)))
}

fn cached_tokens(payload: &serde_json::Value) -> Option<u64> {
    if let Some(total) = usage_number(payload, &["cached_tokens", "cachedTokens"]) {
        return Some(total);
    }
    let read = usage_number(payload, &["cached_read_tokens", "cache_read_input_tokens"]);
    let written = usage_number(payload, &["cached_write_tokens", "cache_creation_input_tokens"]);
    match (read, written) {
        (None, None) => None,
        (read, written) => Some(read.unwrap_or(0).saturating_add(written.unwrap_or(0))),
    }
}

fn content(payload: &serde_json::Value) -> Option<String> {
    string_at(payload, &["content", "text", "description", "message"])
}

fn input(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("input")
        .or_else(|| payload.get("args"))
        .or_else(|| payload.pointer("/update/rawInput"))
        .filter(|value| !value.is_null())
        .map(value_text)
}

fn output(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("output")
        .or_else(|| payload.pointer("/update/rawOutput"))
        .filter(|value| !value.is_null())
        .map(value_text)
}

fn value_text(value: &serde_json::Value) -> String {
    value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string())
}

fn tool_call_id(payload: &serde_json::Value) -> Option<String> {
    string_at(payload, &["call_id", "tool_call_id"])
        .or_else(|| {
            payload
                .get("update")
                .and_then(|value| string_at(value, &["tool_call_id"]))
        })
        .or_else(|| {
            payload
                .as_array()?
                .first()
                .and_then(|value| string_at(value, &["call_id", "tool_call_id"]))
        })
}

fn tool_status(payload: &serde_json::Value) -> String {
    if let Some(entries) = payload.as_array() {
        let statuses = entries
            .iter()
            .filter_map(|entry| string_at(entry, &["status"]))
            .map(|status| status.to_ascii_lowercase())
            .collect::<Vec<_>>();
        if statuses.iter().any(|status| status == "error" || status == "failed") {
            return "failed".to_owned();
        }
        if statuses
            .iter()
            .any(|status| status == "canceled" || status == "cancelled")
        {
            return "canceled".to_owned();
        }
        if !statuses.is_empty()
            && statuses
                .iter()
                .all(|status| status == "success" || status == "completed")
        {
            return "completed".to_owned();
        }
        return "running".to_owned();
    }
    let status = string_at(payload, &["status"])
        .or_else(|| payload.get("update").and_then(|value| string_at(value, &["status"])))
        .map(|status| status.to_ascii_lowercase())
        .unwrap_or_else(|| "running".to_owned());
    match status.as_str() {
        "completed" | "success" => "completed".to_owned(),
        "error" | "failed" => "failed".to_owned(),
        "canceled" | "cancelled" => "canceled".to_owned(),
        "pending" => "pending".to_owned(),
        _ => "running".to_owned(),
    }
}

fn retained_reference(payload: &serde_json::Value) -> Option<String> {
    string_at(payload, &["reference", "retained_output_reference"]).or_else(|| {
        payload
            .get("truncation")
            .and_then(|value| string_at(value, &["reference"]))
    })
}

fn diagnostic_value(payload: &serde_json::Value, key: &str) -> Option<serde_json::Value> {
    payload.get(key).filter(|value| !value.is_null()).cloned().or_else(|| {
        let description = payload.get("description")?.as_str()?;
        serde_json::from_str::<serde_json::Value>(description)
            .ok()?
            .get(key)
            .filter(|value| !value.is_null())
            .cloned()
    })
}

fn diagnostic_string(payload: &serde_json::Value, key: &str) -> Option<String> {
    diagnostic_value(payload, key).and_then(|value| value.as_str().map(str::to_owned))
}

fn step_id(turn_id: &Option<String>, step: u64) -> Option<String> {
    turn_id.as_ref().map(|turn| format!("{turn}:step:{step}"))
}

fn duration(started: Option<i64>, completed: Option<i64>) -> Option<u64> {
    let delta = completed?.checked_sub(started?)?;
    u64::try_from(delta).ok()
}

fn terminal_status(status: &str) -> bool {
    matches!(
        status,
        "applied" | "completed" | "failed" | "canceled" | "rejected" | "recovered"
    )
}

fn preview(value: &str) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(PREVIEW_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::journal_transcript::{RequestedVisibility, derive_transcript};

    fn event(sequence: u64, kind: &str, data: serde_json::Value) -> CanonicalJournalEvent {
        CanonicalJournalEvent {
            schema_version: 1,
            runtime_epoch: String::new(),
            event_id: format!("event-{sequence}"),
            conversation_id: "conv-1".to_owned(),
            sequence,
            timestamp: i64::try_from(sequence * 10).unwrap(),
            kind: kind.to_owned(),
            payload: json!({ "data": data }),
        }
    }

    #[test]
    fn folds_input_lifecycle_and_stream_chunks() {
        let events = vec![
            event(1, "Start", json!({ "turn_id": "turn-1" })),
            event(
                2,
                "InputDispatching",
                json!({ "input_id": "input-1", "content": "hello" }),
            ),
            event(3, "InputAccepted", json!({ "input_id": "input-1" })),
            event(4, "InputApplied", json!({ "input_id": "input-1" })),
            event(5, "Thinking", json!({ "content": "ana" })),
            event(6, "Thinking", json!({ "content": "lyze" })),
            event(7, "Text", json!({ "content": "done" })),
        ];
        let projection = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        assert_eq!(
            projection
                .records
                .iter()
                .filter(|record| record.category == "input")
                .count(),
            1
        );
        assert_eq!(
            projection
                .records
                .iter()
                .find(|record| record.category == "input")
                .unwrap()
                .status,
            "applied"
        );
        assert_eq!(
            projection
                .records
                .iter()
                .find(|record| record.category == "thinking")
                .unwrap()
                .summary,
            "analyze"
        );
    }

    #[test]
    fn detail_hydrates_full_folded_output_while_pages_keep_a_preview() {
        let first = "a".repeat(200);
        let second = "b".repeat(200);
        let events = vec![
            event(1, "Text", json!({ "content": first })),
            event(2, "Text", json!({ "content": second })),
        ];

        let page = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        let paged_output = page.records[0].output_preview.as_deref().unwrap();
        assert_eq!(paged_output.chars().count(), PREVIEW_CHARS + 3);

        let detail = find_trajectory_record(&events, &page.records[0].record_id).unwrap();
        assert_eq!(
            detail.output_preview.as_deref(),
            Some(format!("{first}{second}").as_str())
        );
        assert_eq!(
            detail.detail.as_ref().unwrap()["source_events"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn detail_hydrates_full_tool_input_and_output() {
        let full_input = "q".repeat(300);
        let full_output = "r".repeat(400);
        let events = vec![
            event(
                1,
                "ToolCall",
                json!({ "call_id": "call-1", "status": "running", "input": full_input }),
            ),
            event(
                2,
                "ToolCall",
                json!({ "call_id": "call-1", "status": "completed", "output": full_output }),
            ),
        ];

        let detail = find_trajectory_record(&events, "tool:call-1").unwrap();
        assert_eq!(detail.input_preview.as_deref(), Some(full_input.as_str()));
        assert_eq!(detail.output_preview.as_deref(), Some(full_output.as_str()));
        assert_eq!(detail.detail.as_ref().unwrap()["input"], full_input);
        assert_eq!(detail.detail.as_ref().unwrap()["output"], full_output);
    }

    #[test]
    fn start_and_finish_form_one_stable_turn_boundary() {
        let events = vec![
            event(1, "Start", json!({ "turn_id": "turn-1" })),
            event(2, "Text", json!({ "content": "done" })),
            event(3, "Finish", json!({ "turn_id": "turn-1" })),
        ];

        let projection = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        let turns = projection
            .records
            .iter()
            .filter(|record| record.category == "turn")
            .collect::<Vec<_>>();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].record_id, "turn:turn-1");
        assert_eq!(turns[0].status, "completed");
        assert_eq!(turns[0].source_sequences, [1, 3]);
    }

    #[test]
    fn joins_user_prompt_to_input_lifecycle_by_message_id() {
        let events = vec![
            event(
                1,
                "InputDispatching",
                json!({ "input_id": "input-1", "msg_id": "msg-1" }),
            ),
            event(2, "UserPrompt", json!({ "msg_id": "msg-1", "content": "hello" })),
            event(3, "InputApplied", json!({ "input_id": "input-1", "msg_id": "msg-1" })),
        ];
        let projection = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        assert_eq!(projection.records.len(), 1);
        assert_eq!(projection.records[0].record_id, "input:msg-1");
        assert_eq!(projection.records[0].input_id.as_deref(), Some("input-1"));
        assert_eq!(projection.records[0].input_preview.as_deref(), Some("hello"));
        assert_eq!(projection.records[0].source_sequences, [1, 2, 3]);
    }

    #[test]
    fn tool_updates_keep_one_stable_record() {
        let diagnostics = json!({
            "execution_id": "exec-1",
            "phase": "finalize",
            "structured_content": { "rows": 2 },
            "error_code": "execution_failed",
            "truncation": { "original_bytes": 20, "output_bytes": 10, "limit_bytes": 10 }
        });
        let events = vec![
            event(
                1,
                "ToolCall",
                json!({ "call_id": "call-1", "name": "Read", "status": "running" }),
            ),
            event(
                2,
                "ToolCall",
                json!({
                    "call_id": "call-1",
                    "status": "failed",
                    "output": "partial output",
                    "description": diagnostics.to_string()
                }),
            ),
        ];
        let projection = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        assert_eq!(projection.records.len(), 1);
        assert_eq!(projection.records[0].record_id, "tool:call-1");
        assert_eq!(projection.records[0].status, "failed");
        assert_eq!(projection.records[0].execution_id.as_deref(), Some("exec-1"));
        assert_eq!(projection.records[0].output_preview.as_deref(), Some("partial output"));
        assert_eq!(projection.records[0].structured_content, Some(json!({ "rows": 2 })));
        assert_eq!(projection.records[0].error_code.as_deref(), Some("execution_failed"));
        assert_eq!(
            projection.records[0].truncation,
            Some(json!({ "original_bytes": 20, "output_bytes": 10, "limit_bytes": 10 }))
        );
        assert!(projection.records[0].detail.is_none());
        assert!(find_trajectory_record(&events, "tool:call-1").unwrap().detail.is_some());
    }

    #[test]
    fn resolves_child_tool_parent_from_execution_id() {
        let events = vec![
            event(
                1,
                "ToolCall",
                json!({ "call_id": "parent-call", "execution_id": "parent-execution", "status": "running" }),
            ),
            event(
                2,
                "ToolCall",
                json!({ "call_id": "child-call", "parent_execution_id": "parent-execution", "status": "completed" }),
            ),
        ];
        let projection = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        let child = projection
            .records
            .iter()
            .find(|record| record.tool_call_id.as_deref() == Some("child-call"))
            .unwrap();
        assert_eq!(child.parent_record_id.as_deref(), Some("tool:parent-call"));
    }

    #[test]
    fn keeps_acp_raw_input_in_the_tool_inspector_detail() {
        let events = vec![event(
            1,
            "AcpToolCall",
            json!({
                "session_id": "session-1",
                "update": {
                    "sessionUpdate": "tool_call",
                    "tool_call_id": "call-1",
                    "status": "in_progress",
                    "title": "Search",
                    "rawInput": { "query": "trajectory" }
                }
            }),
        )];

        let projection = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        assert_eq!(projection.records.len(), 1);
        assert_eq!(projection.records[0].record_id, "tool:call-1");
        assert_eq!(
            projection.records[0].input_preview.as_deref(),
            Some(r#"{"query":"trajectory"}"#)
        );
    }

    #[test]
    fn tool_group_projects_and_updates_every_member() {
        let events = vec![
            event(
                1,
                "ToolGroup",
                json!([
                    { "call_id": "call-a", "name": "Agent A", "status": "Executing" },
                    { "call_id": "call-b", "name": "Agent B", "status": "Executing" }
                ]),
            ),
            event(
                2,
                "ToolGroup",
                json!([
                    { "call_id": "call-a", "name": "Agent A", "status": "Success", "description": "done" },
                    { "call_id": "call-b", "name": "Agent B", "status": "Error", "description": "failed" }
                ]),
            ),
        ];

        let projection = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        assert_eq!(projection.records.len(), 2);
        assert_eq!(projection.records[0].record_id, "tool:call-a");
        assert_eq!(projection.records[0].status, "completed");
        assert_eq!(projection.records[0].source_sequences, [1, 2]);
        assert_eq!(projection.records[1].record_id, "tool:call-b");
        assert_eq!(projection.records[1].status, "failed");
        assert_eq!(projection.records[1].output_preview.as_deref(), Some("failed"));
    }

    #[test]
    fn reads_usage_metadata_without_inventing_missing_values() {
        let events = vec![
            event(1, "Start", json!({ "turn_id": "turn-1" })),
            event(
                2,
                "AcpContextUsage",
                json!({ "_meta": { "input_tokens": 10, "cached_read_tokens": 3, "cached_write_tokens": 2 } }),
            ),
        ];
        let projection = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        assert_eq!(projection.overview.tokens.input, Some(10));
        assert_eq!(projection.overview.tokens.cached, Some(5));
        assert_eq!(projection.overview.tokens.output, None);
    }

    #[test]
    fn unknown_events_are_raw_only() {
        let events = vec![
            event(1, "InputDispatching", json!({ "input_id": "i" })),
            event(2, "LegacyNoise", json!({})),
        ];
        let semantic = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        let raw = derive_raw_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        assert_eq!(semantic.records.len(), 1);
        assert_eq!(raw.events.len(), 2);
    }

    #[test]
    fn trajectory_derivation_does_not_change_model_transcript_projection() {
        let events = vec![
            event(1, "Start", json!({ "turn_id": "turn-1" })),
            event(2, "Thinking", json!({ "content": "private reasoning" })),
            event(3, "Text", json!({ "content": "answer" })),
            event(
                4,
                "ToolCall",
                json!({ "call_id": "call-1", "name": "Read", "status": "completed", "output": "ok" }),
            ),
            event(5, "Finish", json!({ "turn_id": "turn-1" })),
        ];
        let before = derive_transcript("conv-1", &events, RequestedVisibility::Model);

        let _trajectory = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        let after = derive_transcript("conv-1", &events, RequestedVisibility::Model);

        assert_eq!(after.model_visible_sha256, before.model_visible_sha256);
        assert_eq!(after.items, before.items);
        assert_eq!(after.tokens, before.tokens);
    }

    #[test]
    fn host_policy_changes_stay_out_of_the_default_execution_timeline() {
        let events = vec![
            event(1, "ApprovalPolicy", json!({ "policy": "never" })),
            event(2, "CompactionPolicy", json!({ "keep_n": 3 })),
            event(
                3,
                "ApprovalAsked",
                json!({ "request_id": "request-1", "call_id": "call-1" }),
            ),
        ];

        let semantic = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        let raw = derive_raw_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        assert_eq!(semantic.records.len(), 1);
        assert_eq!(semantic.records[0].category, "approval");
        assert_eq!(raw.events.len(), 3);
    }

    #[test]
    fn approval_audit_lifecycle_forms_one_control_record() {
        let events = vec![
            event(
                1,
                "ApprovalAsked",
                json!({ "request_id": "request-1", "call_id": "call-1", "tool_name": "Bash" }),
            ),
            event(2, "Permission", json!({ "call_id": "call-1" })),
            event(
                3,
                "ApprovalDecided",
                json!({ "request_id": "request-1", "call_id": "call-1", "outcome": "rejected" }),
            ),
        ];

        let projection = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        assert_eq!(projection.records.len(), 1);
        assert_eq!(projection.records[0].record_id, "approval:request-1");
        assert_eq!(projection.records[0].status, "rejected");
        assert_eq!(projection.records[0].source_sequences, [1, 3]);
    }

    #[test]
    fn compaction_start_and_end_form_one_control_record() {
        let events = vec![
            event(1, "CompactionStart", json!({})),
            event(2, "CompactionEnd", json!({})),
        ];

        let projection = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        assert_eq!(projection.records.len(), 1);
        assert_eq!(projection.records[0].status, "completed");
        assert_eq!(projection.records[0].source_sequences, [1, 2]);
    }

    #[test]
    fn cancellation_phases_form_one_control_record() {
        let events = vec![
            event(1, "CancellationRequested", json!({ "turn_id": "turn-1" })),
            event(2, "CancellationCancelling", json!({ "turn_id": "turn-1" })),
            event(3, "CancellationConvergedIdle", json!({ "turn_id": "turn-1" })),
        ];

        let projection = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        assert_eq!(projection.records.len(), 1);
        assert_eq!(projection.records[0].record_id, "cancellation:turn-1");
        assert_eq!(projection.records[0].status, "canceled");
        assert_eq!(projection.records[0].source_sequences, [1, 2, 3]);
    }

    #[test]
    fn projects_path_free_attachment_delivery_diagnostics() {
        let descriptor = json!({
            "attachment_id": "attachment:abc",
            "source": "project",
            "filename": "diagram.png",
            "mime_type": "image/png",
            "size": 123,
            "sha256": "deadbeef",
            "width": 20,
            "height": 10,
            "media_type": "image",
            "delivery": "pending"
        });
        let mut delivered = descriptor.clone();
        delivered["delivery"] = json!("native");
        let events = vec![
            event(
                1,
                "AttachmentPrepared",
                json!({
                    "msg_id": "msg-1",
                    "attachments": [descriptor]
                }),
            ),
            event(
                2,
                "AttachmentDelivery",
                json!({ "event": "attachment_delivery", "attachment": delivered }),
            ),
        ];

        let projection = derive_trajectory("conv-1", &events, &TrajectoryQuery::default()).unwrap();
        assert_eq!(projection.records.len(), 1);
        assert_eq!(projection.records[0].record_id, "attachment:abc");
        assert_eq!(projection.records[0].category, "attachment");
        assert_eq!(projection.records[0].status, "completed");
        assert_eq!(projection.records[0].input_id.as_deref(), Some("msg-1"));
        assert_eq!(projection.records[0].output_preview.as_deref(), Some("native"));
        assert_eq!(projection.records[0].source_sequences, vec![1, 2]);
        assert!(
            !serde_json::to_string(&projection.records[0])
                .unwrap()
                .contains("\\Users\\")
        );
    }

    #[test]
    fn rejects_ambiguous_cursor_query() {
        let query = TrajectoryQuery {
            before_sequence: Some(2),
            after_sequence: Some(1),
            limit: None,
        };
        assert_eq!(
            validate_query(&query).unwrap_err(),
            "before_sequence and after_sequence are mutually exclusive"
        );
    }

    #[test]
    fn clamps_page_limit_to_public_maximum() {
        let query = TrajectoryQuery {
            limit: Some(10_000),
            ..Default::default()
        };
        assert_eq!(validate_query(&query).unwrap(), MAX_LIMIT);
    }

    #[test]
    fn paging_uses_stable_sequence_boundaries() {
        let events = (1..=5)
            .map(|sequence| event(sequence, "Start", json!({ "turn_id": format!("turn-{sequence}") })))
            .collect::<Vec<_>>();
        let first = derive_trajectory(
            "conv-1",
            &events,
            &TrajectoryQuery {
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let older = derive_trajectory(
            "conv-1",
            &events,
            &TrajectoryQuery {
                before_sequence: first.oldest_sequence,
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(first.records[0].first_sequence > older.records[0].first_sequence);
        assert_ne!(first.records[0].record_id, older.records[0].record_id);
    }

    #[test]
    fn paging_keeps_a_folded_record_whole_across_the_cursor_boundary() {
        let events = vec![
            event(1, "Text", json!({ "content": "first " })),
            event(2, "Text", json!({ "content": "answer" })),
            event(3, "ToolCall", json!({ "tool_call_id": "tool-1", "name": "search" })),
        ];

        let latest = derive_trajectory(
            "conv-1",
            &events,
            &TrajectoryQuery {
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(latest.records[0].record_id, "tool:tool-1");

        let older = derive_trajectory(
            "conv-1",
            &events,
            &TrajectoryQuery {
                before_sequence: latest.oldest_sequence,
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(older.records.len(), 1);
        assert_eq!(older.records[0].record_id, "assistant:event-1");
        assert_eq!(older.records[0].summary, "first answer");
        assert_eq!(older.records[0].source_sequences, [1, 2]);
    }

    #[test]
    fn incremental_cursor_returns_the_same_record_with_its_latest_folded_state() {
        let events = vec![
            event(1, "Thinking", json!({ "content": "deep " })),
            event(2, "Thinking", json!({ "content": "thought" })),
        ];

        let initial = derive_trajectory("conv-1", &events[..1], &TrajectoryQuery::default()).unwrap();
        let incremental = derive_trajectory(
            "conv-1",
            &events,
            &TrajectoryQuery {
                after_sequence: Some(1),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(incremental.records.len(), 1);
        assert_eq!(incremental.records[0].record_id, initial.records[0].record_id);
        assert_eq!(incremental.records[0].summary, "deep thought");
        assert_eq!(incremental.records[0].source_sequences, [1, 2]);
    }

    #[tokio::test]
    async fn persisted_projection_rebuilds_after_corruption_and_tail_growth() {
        let root = tempfile::tempdir().unwrap();
        let journal = CanonicalEventJournal::new(root.path().to_path_buf());
        journal
            .append(
                "user",
                "conv-1",
                "event-1".to_owned(),
                "Text".to_owned(),
                json!({ "data": { "content": "first" } }),
            )
            .await
            .unwrap();

        let (initial, revision) = load_cached_records(&journal, "user", "conv-1").await.unwrap();
        assert_eq!(revision, 1);
        assert_eq!(initial.len(), 1);
        let path = journal.trajectory_projection_path("user", "conv-1").unwrap();
        tokio::fs::write(&path, br#"{"schema_version":0}"#).await.unwrap();
        let (rebuilt, revision) = load_cached_records(&journal, "user", "conv-1").await.unwrap();
        assert_eq!(revision, 1);
        assert_eq!(rebuilt[0].summary, "first");

        journal
            .append(
                "user",
                "conv-1",
                "event-2".to_owned(),
                "Text".to_owned(),
                json!({ "data": { "content": " second" } }),
            )
            .await
            .unwrap();
        let (grown, revision) = load_cached_records(&journal, "user", "conv-1").await.unwrap();
        assert_eq!(revision, 2);
        assert_eq!(grown.len(), 1);
        assert_eq!(grown[0].summary, "first second");
    }

    #[tokio::test]
    async fn persisted_projection_finishes_the_existing_turn_after_incremental_growth() {
        let root = tempfile::tempdir().unwrap();
        let journal = CanonicalEventJournal::new(root.path().to_path_buf());
        journal
            .append(
                "user",
                "conv-1",
                "event-1".to_owned(),
                "Start".to_owned(),
                json!({ "data": { "turn_id": "turn-1" } }),
            )
            .await
            .unwrap();
        let (running, _) = load_cached_records(&journal, "user", "conv-1").await.unwrap();
        assert_eq!(running[0].status, "running");

        journal
            .append(
                "user",
                "conv-1",
                "event-2".to_owned(),
                "Finish".to_owned(),
                json!({ "data": { "turn_id": "turn-1" } }),
            )
            .await
            .unwrap();
        let (completed, revision) = load_cached_records(&journal, "user", "conv-1").await.unwrap();

        assert_eq!(revision, 2);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].record_id, "turn:turn-1");
        assert_eq!(completed[0].status, "completed");
        assert_eq!(completed[0].source_sequences, [1, 2]);
    }
}
